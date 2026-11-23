//! `tasks [projects...] [--task ID] [--status S] ...`: task status listing,
//! plus `--supersede`/`--acknowledge` outcome-record correction and
//! `--reconcile`/`--close-stranded`/`--reclaimable`'s own write legs.
//!
//! `--supersede`/`--acknowledge` are ported to `mv_state::tasks`
//! (`outcome_targets`/`append_supersede_if_absent`/
//! `append_acknowledge_if_absent`, mirroring `accounts._outcome_targets`/
//! `append_supersede_if_absent`/`append_acknowledge_if_absent`): a dry run
//! by default, with `--yes` required to actually write.
//! `--close-stranded`/`--reclaimable` are the same shape, ported to
//! [`mv_state::tasks::close_stranded`]/[`mv_state::tasks::reclaimable_panes`]
//! (mirroring `tasks.close_stranded`/`runner.reclaimable_panes` — Part A of
//! `multiverse/tasks.py`'s stranded-task machinery). Part B runs through
//! [`mv_state::tasks::sweep_project_with_tasks`]: auto-supersede first,
//! then lease-expire the remaining AWAITING tasks. Auto-supersede itself
//! has no elapsed-time gate, matching Python's `converge_stale`.
//!
//! The read-only listing below reconstructs each task's history straight
//! from `routing.log` (`dispatch`/`started`/`result` events) plus the task
//! directory's own `status` file, classifying every task as `recorded` (a
//! `result` event closes the loop — shown with `#<line>`, the routing-log
//! line that recorded it), `unrecorded` (the status file reached a
//! terminal value but no `result` event was ever logged for it — a
//! dispatcher that died or raced ahead of the agent), `running` (a
//! `started` pane is working or has no reported state), `stalled` (a live
//! pane explicitly not working), or `awaiting` (nothing terminal and no
//! live pane either — uncertain, not dead). This mirrors `cmd_tasks`
//! (`cli.py:7112`, spec-cli.md). The task/role/account/state widths are
//! minimum widths, with explicit spaces between fields. Keeping the
//! separators separate from padding preserves readability when a task,
//! role or account exceeds its column's width. Worker table widths
//! are independent of these task-listing fields.
//!
//! Pane liveness goes through [`AgentPanes::list_checked`]: `None` is a
//! failed probe (unfinished tasks stay RUNNING) and `Some` (possibly
//! empty) is a confirmed listing. Writes — auto-reconcile, `--reconcile`,
//! `--reclaimable --yes`, blocking-prompt trips, hub seat-out sync — stay
//! in this command and in [`mv_state::tasks::reconcile_project`]. Classification
//! reuses the listing snapshot; reclamation still requires a fresh idle/done
//! observation with the exact dispatched name before a pane can close.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::roles::Role;
use mv_engine::breaker::{self, BreakerKind};
use mv_engine::limits;
use mv_engine::mv_herdr::envelope::AgentState;
use mv_engine::runner::naming;
use mv_state::activity::{self, ProjectLog as ActivityLog};
use mv_state::index::{records as index_records, Selector};
use mv_state::tasks::{
    read_writer_status_verbose, AgentPanes, HerdrAgents, LiveAgentStatus, ReconcileRequest,
    StatusRead, TaskRef, TaskStatus,
};
use time::OffsetDateTime;

use crate::exit::{Failure, UNKNOWN};

use super::current_project_root;
use super::dispatch::ReclaimShellCheck;
use super::doctor::{project_path, state_dir_from_config};

const DASH: &str = "\u{2014}";
const ID_WIDTH: usize = 26;
const ROLE_WIDTH: usize = 10;
const ACCOUNT_WIDTH: usize = 10;
const STATE_WIDTH: usize = 11;

/// Everything this reader keeps from one task's `routing.log` history.
/// `role`/`account` come from the most recent `dispatch` record (ported
/// from the old `latest_dispatch` helper this replaces); `started_pane`,
/// `pane_event_pane`, and `result_status`/`result_line` are `None` until
/// their event is seen, exactly like a task that never got that far.
#[derive(Debug, Default, Clone)]
struct TaskEvents {
    role: String,
    account: String,
    agent: String,
    branch: String,
    dispatched_at: Option<OffsetDateTime>,
    started_at: Option<OffsetDateTime>,
    started_pane: Option<String>,
    /// `event=pane`'s own `pane=` field — written the instant the pane
    /// exists, before `started_pane` (`record_pane`, Scout 67a619 finding
    /// 1). The only pane this reader has for a task killed between pane
    /// creation and `agent start` (card `20261118-b-c328a3` M2) — see
    /// [`TaskEvents::effective_pane`]'s fallback.
    pane_event_pane: Option<String>,
    result_status: Option<String>,
    /// 1-indexed line in `routing.log`, shown as `#<N>` next to a
    /// `recorded` task so a human can jump straight to the record. Stale
    /// convergence retains the prior ordinal (or None) until the next read.
    result_line: Option<usize>,
    corrected: bool,
    acknowledged: Vec<usize>,
}

impl TaskEvents {
    /// The pane to treat as "where this task is running": `started_pane`
    /// when `agent start` recorded one, else the earlier, durable
    /// `event=pane` line — the only pane recorded for a task killed
    /// between pane creation and `agent start` (card `20261118-b-c328a3`
    /// M2). Every direct `started_pane` read in this module that means
    /// "the task's live pane" goes through this one helper instead, so
    /// the fallback cannot drift between call sites again (card
    /// `20261118-b-9226e1`).
    fn effective_pane(&self) -> Option<&str> {
        self.started_pane
            .as_deref()
            .or(self.pane_event_pane.as_deref())
    }
}

/// One pass over `routing.log`'s task-lifecycle events
/// (`dispatch`/`started`/`result`/`supersede`, everything
/// `index::Selector::Tasks` already parses), keyed by task id, plus the
/// order each task id was first seen in — the routing log's own dispatch
/// order, which `build_rows` lists in rather than alphabetically (card
/// `20261102-b-a5782d`). A missing or unreadable log is simply "no
/// history for any task", the same fail-open posture the rest of this
/// read-only command takes.
///
/// A `dispatch` event resets every field for its task id, same as
/// `mv_state::tasks::task_events`'s own fold (added porting reconcile,
/// card `20261102-b-fde23e`/`20261102-b-1b2d80`) and Python's `read_log`:
/// a fix round that reuses a task id must not inherit an earlier round's
/// pane or outcome. A `supersede` event overwrites `result_status` and
/// `result_line` the same way a later `result` would (card
/// `20261102-b-28afef`), mirroring `mv_state::tasks::task_events`'s own
/// `supersede` arm — a closer's corrected outcome record must win over
/// the pre-correction one this reader would otherwise keep showing. This
/// reader keeps its own fold rather than reusing that one directly — its
/// `TaskEvents` carries `account` and a display `result_line` ordinal
/// that function's own `SweepTask` has no use for, and duplicating the
/// few lines of fold logic here costs less than reshaping either shape
/// to fit the other.
fn gather_task_events(root: &Path) -> (HashMap<String, TaskEvents>, Vec<String>) {
    let log_path = root.join(".multiverse/routing.log");
    let mut by_task: HashMap<String, TaskEvents> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let Ok(records) = index_records(&log_path, Selector::Tasks) else {
        return (by_task, order);
    };
    for record in &records {
        let Ok(parsed) = record.parse() else {
            continue;
        };
        let Some(task_id) = parsed.fields.get("task").cloned() else {
            continue;
        };
        if !by_task.contains_key(&task_id) {
            order.push(task_id.clone());
        }
        match parsed.event() {
            "dispatch" => {
                by_task.insert(
                    task_id,
                    TaskEvents {
                        role: parsed.fields.get("role").cloned().unwrap_or_default(),
                        account: parsed.fields.get("account").cloned().unwrap_or_default(),
                        agent: parsed.fields.get("agent").cloned().unwrap_or_default(),
                        branch: parsed.fields.get("branch").cloned().unwrap_or_default(),
                        dispatched_at: mv_core::time::parse_flexible(&parsed.when).ok(),
                        started_at: None,
                        started_pane: None,
                        pane_event_pane: None,
                        result_status: None,
                        result_line: None,
                        corrected: false,
                        acknowledged: Vec::new(),
                    },
                );
            }
            "pane" => {
                let entry = by_task.entry(task_id).or_default();
                entry.pane_event_pane = parsed.fields.get("pane").cloned();
            }
            "started" => {
                let entry = by_task.entry(task_id).or_default();
                entry.started_pane = parsed.fields.get("pane").cloned();
                entry.started_at = mv_core::time::parse_flexible(&parsed.when).ok();
            }
            "result" => {
                let entry = by_task.entry(task_id).or_default();
                entry.result_status = parsed.fields.get("status").cloned();
                entry.result_line = Some(record.entry.ordinal as usize + 1);
            }
            "supersede" => {
                let entry = by_task.entry(task_id).or_default();
                entry.result_status = parsed.fields.get("status").cloned();
                entry.result_line = Some(record.entry.ordinal as usize + 1);
                entry.corrected = true;
            }
            "acknowledged" => {
                if let Some(line) = parsed
                    .fields
                    .get("acknowledges")
                    .and_then(|line| line.parse().ok())
                {
                    by_task.entry(task_id).or_default().acknowledged.push(line);
                }
            }
            _ => {}
        }
    }
    (by_task, order)
}

/// A raw `routing.log` `role=` value, translated to its current display
/// name — mirroring `commands::board_data::display_role`'s own translation
/// of the same on-disk shape for the board's own cards (card
/// `20261109-b-2a3755`). A legacy on-disk key (`fixer`, `librarian`,
/// `orchestrator`, …) renders as its current config key; anything else
/// (an already-current key, or a string that is neither) round-trips
/// unchanged. `routing.log` itself is never touched — only what this
/// reader prints.
fn display_role(raw: &str) -> String {
    match mv_config::roles::Role::from_any_config_key(raw) {
        Ok(role) => role.config_key().to_string(),
        Err(_) => raw.to_string(),
    }
}

/// One rendered row, already resolved to display text.
struct Row {
    task_id: String,
    role: String,
    account: String,
    state: &'static str,
    suffix: String,
    flagged: bool,
    note: Option<String>,
}

fn display_row(row: &Row) -> (String, Option<String>) {
    let marker = if row.flagged { "  * " } else { "    " };
    let line = format!(
        "{marker}{:ID_WIDTH$} {:ROLE_WIDTH$} {:ACCOUNT_WIDTH$} {:STATE_WIDTH$} {}",
        mv_board::layout::sanitize_title(&row.task_id),
        mv_board::layout::sanitize_title(&row.role),
        mv_board::layout::sanitize_title(&row.account),
        mv_board::layout::sanitize_title(row.state),
        mv_board::layout::sanitize_title(&row.suffix),
    );
    let note = row
        .note
        .as_deref()
        .map(|note| format!("      {}", mv_board::layout::sanitize_title(note)));
    (line, note)
}

/// Classify one task from its `routing.log` history, status file, and the
/// fleet's current live panes. Mirrors `cmd_tasks`'s four outcomes: a
/// `result` event closes the loop (`recorded`); a terminal status file
/// with no matching `result` event is `unrecorded` (flagged); a live pane
/// with nothing terminal yet is `running` while working or unclassified,
/// and `stalled` (flagged) when explicitly not working; anything else is
/// `awaiting` (flagged) — uncertain, not dead, because the result (if
/// any) is sitting unread in a pane this reader never had a way to check.
///
/// A fifth outcome, `garbled` (flagged), always wins over `running`/
/// `stalled`/`awaiting`: a status file holding a token outside
/// `done`/`blocked`/`failed` is never shown as merely running, no matter
/// what the live pane reports — card `20261117-b-437eeb`. `--close-stranded`
/// can record a terminal outcome for it (see `is_garbled_status_candidate`
/// in `mv_state::tasks`).
fn classify(
    events: Option<&TaskEvents>,
    file_status: Option<TaskStatus>,
    unrecognized_status: Option<&str>,
    live: Option<&[LiveAgentStatus]>,
) -> (&'static str, String, bool, Option<String>) {
    if let Some(events) = events {
        if let Some(status) = &events.result_status {
            let flagged = status != "done"
                && !events.corrected
                && !events
                    .result_line
                    .is_some_and(|line| events.acknowledged.contains(&line));
            let suffix = match events.result_line {
                Some(line) => format!("{status} #{line}"),
                None => status.clone(),
            };
            return ("recorded", suffix, flagged, None);
        }
    }
    if let Some(raw) = unrecognized_status {
        let token = mv_board::layout::truncate(raw, 80);
        let note = format!(
            "status file holds an unrecognized value ('{token}'): neither empty (still running) \
             nor one of done/blocked/failed — needs a human look, not an assumption either way. \
             `--close-stranded` can record a terminal outcome for it."
        );
        return ("garbled", DASH.to_string(), true, Some(note));
    }
    // A kill between pane creation and `agent start` leaves only
    // `event=pane` (`started_pane` is `None`), but Herdr still lists
    // that pane live under the dispatched agent — falling back to it
    // here is what keeps that crash window classifying running/stalled
    // instead of a lying "awaiting" (card `20261118-b-c328a3` M2).
    let started_pane = events.and_then(TaskEvents::effective_pane);
    let pane_is_live = live.is_some_and(|agents| {
        started_pane.is_some_and(|pane| agents.iter().any(|agent| agent.pane_id == pane))
    });

    if let Some(status) = file_status {
        let status = status.as_str();
        let working = live.is_some_and(|agents| {
            agents.iter().any(|agent| {
                Some(agent.pane_id.as_str()) == started_pane
                    && agent.status == Some(AgentState::Working)
            })
        });
        let note = if working {
            format!("status file says '{status}', but pane is still working: a live fix round, \
                     not a lingering finish — `--reconcile` leaves it alone until the pane settles.")
        } else if started_pane.is_some() && live.is_some() && !pane_is_live {
            format!(
                "status file says '{status}', but pane gone, status {status} — `--reconcile` \
                     leaves it alone until the pane settles."
            )
        } else {
            format!("finished as '{status}' after its dispatch gave up. `--reconcile` records it.")
        };
        return ("unrecorded", status.to_string(), true, Some(note));
    }

    // A failed `agent list` is not "nothing live": unfinished work stays
    // RUNNING until we can actually ask. Confirmed-empty (`Some(&[])`) is
    // the only listing that may promote a no-status task to AWAITING.
    if live.is_none() {
        return ("running", DASH.to_string(), true, None);
    }

    if pane_is_live {
        let status = live.and_then(|agents| {
            agents
                .iter()
                .find(|agent| Some(agent.pane_id.as_str()) == started_pane)
                .and_then(|agent| agent.status)
        });
        if let Some(status) = status.filter(|status| *status != AgentState::Working) {
            let pane = started_pane.expect("live pane has a recorded id");
            let note = format!(
                "live in pane '{pane}' but agent is live but not working ({}): not dead and \
                 not still spending — go look at the pane.",
                status.as_str()
            );
            return ("stalled", DASH.to_string(), true, Some(note));
        }
        return ("running", DASH.to_string(), true, None);
    }
    let note = if started_pane.is_some() {
        "no status file and recorded pane is not among live panes: uncertain, not dead. The \
         result, if any, is in its pane."
            .to_string()
    } else {
        "no status file and no agent recorded for this dispatch: uncertain, not dead. The \
         result, if any, is in its pane."
            .to_string()
    };
    ("awaiting", DASH.to_string(), true, Some(note))
}

/// Resolve the single project `--supersede`/`--acknowledge` corrects a
/// record in: an explicitly named project (refusing more than one, same as
/// `cmd_tasks`), else the cwd-inferred project, else a refusal naming
/// `flag` rather than silently spanning the fleet (`cli.py:7206-7267`).
fn resolve_single_project(args: &ArgMatches, flag: &str, verb: &str) -> Result<PathBuf, Failure> {
    let explicit: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    if !explicit.is_empty() {
        if explicit.len() != 1 {
            return Err(Failure::Config(format!(
                "{flag} {verb} one project; name exactly one (got {}: {}).",
                explicit.len(),
                explicit.join(", ")
            )));
        }
        let config = super::load_config(args)?;
        return super::project_root_by_name(&config, &explicit[0]);
    }
    super::current_project(args)
        .map(|(_, root)| root)
        .map_err(|_| {
            Failure::Config(format!(
                "no project named, and cwd is outside every configured project: refusing to \
             {flag} across the whole fleet silently. Name the project, or run this from \
             inside one."
            ))
        })
}

/// `"  - line {ordinal} ({when})"` per known outcome line, newest first, or
/// the Python-matching placeholder when there are none
/// (`accounts.UnknownSupersedeTargetError`/`UnknownAcknowledgeTargetError`).
fn known_lines(when_by_ordinal: &std::collections::BTreeMap<u64, String>) -> String {
    if when_by_ordinal.is_empty() {
        return "  (no outcome records at all)".to_string();
    }
    when_by_ordinal
        .iter()
        .map(|(ordinal, when)| format!("  - line {ordinal} ({when})"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `repr()`-shaped quoting for the `--supersede` dry-run preview line,
/// also used by backlog cwd-scope diagnostics, matching Python for a
/// plain, printable reason.
pub(super) fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Mirrors `mv_state::tasks`'s own injection guard at the flag boundary, so
/// a bad `--supersede`/`--status`/`--state`/`--acknowledge` value is
/// refused with a usage exit code before `supersede`/`acknowledge` ever
/// reaches the state layer — never relying on that layer's own refusal
/// alone to be the only thing standing between a bad flag and a corrupted
/// routing-log line (card `20261116-b-5f49bd`).
fn invalid_log_field(flag: &str, value: &str) -> Option<String> {
    if value.contains(['\t', '\r', '\n', '=']) {
        return Some(format!(
            "{flag}: must not contain a tab, carriage return, line feed, or '=' — any of those \
             could smuggle an extra field into the tab-separated routing-log line (got {value:?})"
        ));
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return Some(format!(
            "{flag}: must not have a leading or trailing space (got {value:?})"
        ));
    }
    None
}

fn read_only_mode(args: &ArgMatches) -> bool {
    args.get_flag("read-only") || std::env::var("MULTIVERSE_READ_ONLY").as_deref() == Ok("1")
}

/// `tasks --supersede TASK --supersedes N --status S --state TEXT --reason
/// TEXT [--revisions N] [--yes]`: correct one wrong outcome record
/// (`_cmd_tasks_supersede`, `cli.py:7782`).
fn supersede(args: &ArgMatches) -> Result<(), Failure> {
    let task = args
        .get_one::<String>("supersede")
        .expect("checked by caller")
        .clone();
    let (Some(&supersedes), Some(status), Some(state)) = (
        args.get_one::<i64>("supersedes"),
        args.get_one::<String>("status"),
        args.get_one::<String>("state"),
    ) else {
        return Err(Failure::Config(
            "--supersede requires --supersedes, --status and --state.".into(),
        ));
    };
    let status = status.clone();
    let state = state.clone();
    let reason = args
        .get_one::<String>("reason")
        .cloned()
        .unwrap_or_default();
    let revisions = *args.get_one::<i64>("revisions").unwrap_or(&0);
    let yes = args.get_flag("yes");

    for (flag, value) in [
        ("--supersede", task.as_str()),
        ("--status", status.as_str()),
        ("--state", state.as_str()),
    ] {
        if let Some(message) = invalid_log_field(flag, value) {
            eprintln!("tasks: {message}");
            return Err(Failure::Reported(UNKNOWN));
        }
    }

    if reason.is_empty() {
        return Err(Failure::Operation(format!(
            "refusing to supersede {task}@line {supersedes} with no reason: a correction with \
             no evidence is the same unsupported assertion this mechanism exists to fix"
        )));
    }
    let root = resolve_single_project(args, "--supersede", "corrects one record in")?;
    let targets = mv_state::tasks::outcome_targets(&root, &task);
    let supersedes_ord = u64::try_from(supersedes)
        .ok()
        .filter(|ord| targets.targetable.contains(ord));
    let Some(supersedes_ord) = supersedes_ord else {
        return Err(Failure::Operation(format!(
            "{task} has no outcome record at line {supersedes} to supersede.\nknown outcome \
             record lines for {task}:\n{}",
            known_lines(&targets.when_by_ordinal)
        )));
    };
    let attempted = (status.clone(), state.clone());
    if let Some(existing) = targets.applied.get(&supersedes_ord) {
        if existing == &attempted {
            println!(
                "{task}@line {supersedes_ord}: already superseded with status={} state={} — \
                 nothing to write.",
                existing.0, existing.1
            );
            return Ok(());
        }
        let when = targets
            .when_by_ordinal
            .get(&supersedes_ord)
            .cloned()
            .unwrap_or_default();
        return Err(Failure::Operation(format!(
            "{task}@line {supersedes_ord} ({when}) was already superseded with \
             status={} state={}; refusing to also record status={} state={} for the same \
             target — supersede the corrector instead, not the same target twice",
            existing.0, existing.1, attempted.0, attempted.1
        )));
    }

    let (current_status, current_state) =
        mv_state::tasks::current_outcome_fields(&root, supersedes_ord);
    let when = targets
        .when_by_ordinal
        .get(&supersedes_ord)
        .cloned()
        .unwrap_or_default();
    println!("{task}@line {supersedes_ord}");
    println!("  current:  when={when} status={current_status} state={current_state}");
    println!(
        "  proposed: status={status} state={state} reason={} revisions={revisions}",
        python_repr(&reason)
    );

    if !yes || read_only_mode(args) {
        println!("  dry run: nothing was written. Add --yes to record this correction.");
        return Ok(());
    }

    let appended = mv_state::tasks::append_supersede_if_absent(
        &root,
        &task,
        supersedes_ord,
        &status,
        &state,
        &reason,
        revisions,
        None,
    )
    .map_err(|e| Failure::Operation(e.to_string()))?;
    if appended {
        println!("  recorded the correction for {task}@line {supersedes_ord}.");
    } else {
        println!("  already recorded; nothing written for {task}@line {supersedes_ord}.");
    }
    // Card 20261122-b-40cb46: a correction to `done` also brings the lane's
    // own status file into line with the routing log, when the worktree
    // the task ran in (its own, or the one a `dispatch --into` round
    // adopted) still exists — so nothing that still reads that file sees
    // the pre-correction verdict. Idempotent, so a repeat of an
    // already-recorded correction repairs a file a hand edit left behind.
    if status == "done" {
        match mv_state::worktrees::rewrite_lane_status(&root, &task, &status) {
            Ok(Some(path)) => println!("  rewrote {} to done.", path.display()),
            Ok(None) => {}
            Err(error) => {
                return Err(Failure::Operation(format!(
                    "recorded the correction for {task}@line {supersedes_ord}, but could not \
                     rewrite its lane status file: {error}"
                )))
            }
        }
    }
    Ok(())
}

/// `tasks --acknowledge TASK --acknowledges N [--yes]`: mark one outcome
/// record reviewed, never touching its verdict
/// (`_cmd_tasks_acknowledge`, `cli.py:7901`).
fn acknowledge(args: &ArgMatches) -> Result<(), Failure> {
    let task = args
        .get_one::<String>("acknowledge")
        .expect("checked by caller")
        .clone();
    let Some(&acknowledges) = args.get_one::<i64>("acknowledges") else {
        return Err(Failure::Config(
            "--acknowledge requires --acknowledges.".into(),
        ));
    };
    let yes = args.get_flag("yes");

    if let Some(message) = invalid_log_field("--acknowledge", &task) {
        eprintln!("tasks: {message}");
        return Err(Failure::Reported(UNKNOWN));
    }

    let root = resolve_single_project(args, "--acknowledge", "marks one record in")?;
    let targets = mv_state::tasks::outcome_targets(&root, &task);
    let acknowledges_ord = u64::try_from(acknowledges)
        .ok()
        .filter(|ord| targets.targetable.contains(ord));
    let Some(acknowledges_ord) = acknowledges_ord else {
        return Err(Failure::Operation(format!(
            "{task} has no outcome record at line {acknowledges} to acknowledge.\nknown \
             outcome record lines for {task}:\n{}",
            known_lines(&targets.when_by_ordinal)
        )));
    };

    let (current_status, current_state) =
        mv_state::tasks::current_outcome_fields(&root, acknowledges_ord);
    let when = targets
        .when_by_ordinal
        .get(&acknowledges_ord)
        .cloned()
        .unwrap_or_default();
    println!("{task}@line {acknowledges_ord}");
    println!("  current: when={when} status={current_status} state={current_state}");

    if !yes || read_only_mode(args) {
        println!("  dry run: nothing was written. Add --yes to record this acknowledgement.");
        return Ok(());
    }

    let appended =
        mv_state::tasks::append_acknowledge_if_absent(&root, &task, acknowledges_ord, None)
            .map_err(|e| Failure::Operation(e.to_string()))?;
    if appended {
        println!("  recorded the acknowledgement for {task}@line {acknowledges_ord}.");
    } else {
        println!("  already recorded; nothing written for {task}@line {acknowledges_ord}.");
    }
    Ok(())
}

/// The command's public entry point: delegates to [`run_with`], the real
/// body, wired to the real [`HerdrAgents`] pane source. See `run_with`'s
/// own doc comment for why the split exists.
pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    run_with(args, &mut HerdrAgents)
}

/// Resolves `tasks`'s requested project tokens the same way `backlog
/// list`'s own tokens resolve (`backlog::resolve_named_project`): a literal
/// `[projects.<name>]` key first, else `.`, a relative path, or an
/// absolute path resolved against `cwd` (card `20261117-b-486a55` — `tasks`
/// used to require an exact config key, with no path handling at all, so
/// `tasks .` failed with "no such project: ." even from inside a
/// configured project's own directory). Returns the resolved
/// [`mv_config::load::Project`] for every token that resolves, and every
/// token that resolves to nothing, in request order.
fn resolve_requested_projects<'a>(
    config: &mv_config::load::Config,
    cwd: Option<&Path>,
    requested: &'a [String],
) -> (Vec<mv_config::load::Project>, Vec<&'a String>) {
    let mut resolved = Vec::with_capacity(requested.len());
    let mut unknown = Vec::new();
    for name in requested {
        match super::backlog::resolve_named_project(config, cwd, name) {
            Some(project) => resolved.push(project),
            None => unknown.push(name),
        }
    }
    (resolved, unknown)
}

/// `run`'s own body, with the one live seam (`--reconcile`'s pane-liveness
/// check) injected — mirrors `sweep::run_with`'s split for the same reason:
/// a test can hand this a scripted [`AgentPanes`] instead of `HerdrAgents`,
/// so `cargo test` never spawns a real Herdr.
fn run_with(args: &ArgMatches, agents: &mut impl AgentPanes) -> Result<(), Failure> {
    if args.get_one::<String>("supersede").is_some() {
        return supersede(args);
    }
    if args.get_one::<String>("acknowledge").is_some() {
        return acknowledge(args);
    }

    let reconcile = args.get_flag("reconcile");
    if args.get_one::<String>("task").is_some() && !reconcile {
        return Err(Failure::Config(
            "--task only makes sense with --reconcile.".to_string(),
        ));
    }
    let close_stranded = args.get_flag("close-stranded");
    let reclaimable = args.get_flag("reclaimable");

    let named: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();

    if !named.is_empty() {
        let config = super::load_config(args)?;
        let cwd = std::env::current_dir().ok();
        let (resolved, unknown) = resolve_requested_projects(&config, cwd.as_deref(), &named);
        if !unknown.is_empty() {
            let mut known: Vec<&str> = config.projects.keys().map(String::as_str).collect();
            known.sort_unstable();
            let known = if known.is_empty() {
                "(none)".to_string()
            } else {
                known.join(", ")
            };
            let unknown = unknown
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(Failure::Config(format!(
                "no such project: {unknown}; configured: {known} (see `multiverse projects`)"
            )));
        }
        let names: Vec<String> = resolved.iter().map(|p| p.name.clone()).collect();
        if args.get_one::<String>("task").is_some() && names.len() != 1 {
            return Err(Failure::Config(format!(
                "--task reconciles one task in one project; name exactly one (got {}: {}).",
                names.len(),
                names.join(", ")
            )));
        }
        if reconcile && !read_only_mode(args) {
            sync_seat_outages_if_hub_in_scope(&config, &names, args);
        }
        for project in &resolved {
            match project_path(project) {
                Some(root) => list(root, Some(&project.name), args, agents, Some(&config))?,
                None => {
                    println!("\n{}", project.name);
                    println!("    (path not yet configured)");
                }
            }
        }
        return Ok(());
    }

    if let Ok((name, root)) = super::current_project(args) {
        if name == mv_state::activity::HUB_PROJECT_NAME {
            return list_hub_widened(args, agents);
        }
        // Python `cmd_tasks` (`cli.py:7176-7180`) prints this preamble
        // with `print(...)` — one trailing newline, no extra blank line.
        // Found via `conformance/diff_fuzz.py` seed 9000.
        println!(
            "(cwd is inside project '{name}' — scoping to it. Name other projects, or run \
             from outside any project, for the whole fleet.)"
        );
        let config = super::load_config(args).ok();
        return list(&root, Some(&name), args, agents, config.as_ref());
    }
    // `current_project` failed only because cwd isn't inside any project
    // *this* config registers — not because no config loaded at all.
    // `current_project_root(args)` now refuses outright in exactly that
    // case (card `20261106-b-fd2d3e` folded the config-vouching check into
    // the walk itself, replacing the `config_available(args)` guard this
    // call site used to need by hand), so falling through to the
    // config-driven fleet listing below when it errs matches Python's
    // `cmd_tasks` (`cli.py:7222-7223`), which has no cwd-walk notion at all
    // and widens to every configured project the same way when cwd
    // resolves to none of them. Only when no config could be loaded at
    // all — the walk's one remaining legitimate use, e.g. Herdr invoking
    // this as a global-context plugin action with no multiverse config
    // present (card `20261102-b-2691da`) — does the walk still apply.
    if let Ok(root) = current_project_root(args) {
        return list(&root, None, args, agents, None);
    }
    if reconcile {
        return Err(Failure::Config(
            "no project named, and cwd is outside every configured project: refusing to \
             --reconcile the whole fleet silently. Name the project(s) to reconcile, or run \
             this from inside one."
                .to_string(),
        ));
    }
    if close_stranded {
        return Err(Failure::Config(
            "no project named, and cwd is outside every configured project: refusing to \
             --close-stranded the whole fleet silently. Name the project(s), or run this \
             from inside one."
                .to_string(),
        ));
    }
    if reclaimable {
        return Err(Failure::Config(
            "no project named, and cwd is outside every configured project: refusing to \
             --reclaimable the whole fleet silently. Name the project(s), or run this from \
             inside one."
                .to_string(),
        ));
    }
    // Not inside, or at, any configured project, and no `.multiverse/`
    // ancestor either — e.g. Herdr invoking this as a global-context
    // plugin action from the plugin checkout directory. The help text
    // above promises "the whole fleet" for exactly this case; fall back
    // to it instead of failing (card 20261102-b-2691da).
    list_fleet(args, agents)
}

/// Every configured project's task list, one after another — the fleet
/// view `run` falls back to when cwd resolves to no project at all. Mirrors
/// `board`'s own no-project-in-scope fallback
/// (`board_data::LiveSource::resolve_scope`'s `BoardScope::Default` arm):
/// show every configured project rather than erroring.
fn list_fleet(args: &ArgMatches, agents: &mut impl AgentPanes) -> Result<(), Failure> {
    let config = super::load_config(args)?;
    if config.projects.is_empty() {
        return Err(Failure::Config(
            "no projects configured; run `multiverse add` or `multiverse new`".to_owned(),
        ));
    }
    refuse_task_unless_one_project(args, config.projects.len(), &config)?;
    if args.get_flag("reconcile") && !read_only_mode(args) {
        let names: Vec<String> = config.project_order.clone();
        sync_seat_outages_if_hub_in_scope(&config, &names, args);
    }
    println!(
        "(cwd is outside any project — showing the whole fleet: {} project(s).)",
        config.projects.len()
    );
    list_configured_projects(&config, args, agents)
}

/// Every configured project's task list, with no preamble — the silent
/// widening Python's `cmd_tasks` applies when cwd is inside the hub
/// project itself (`HUB_PROJECT_NAME`, `cli.py` ~7166-7179): the hub is
/// the fleet operator's own vantage point, not "a project" to scope down
/// to, so a bare `tasks`/`tasks --reconcile` run from inside it must see
/// every project, with no "scoping to it" preamble implying anything was
/// skipped. Mirrors the same rule `backlog::resolve_list_projects`
/// (`backlog.rs:284`) and `board_data::LiveSource::resolve_scope`
/// (`board_data.rs:574`) already apply via the same constant (card
/// `20261102-b-f507d9`).
fn list_hub_widened(args: &ArgMatches, agents: &mut impl AgentPanes) -> Result<(), Failure> {
    let config = super::load_config(args)?;
    if config.projects.is_empty() {
        return Err(Failure::Config(
            "no projects configured; run `multiverse add` or `multiverse new`".to_owned(),
        ));
    }
    refuse_task_unless_one_project(args, config.projects.len(), &config)?;
    if args.get_flag("reconcile") && !read_only_mode(args) {
        let names: Vec<String> = config.project_order.clone();
        sync_seat_outages_if_hub_in_scope(&config, &names, args);
    }
    list_configured_projects(&config, args, agents)
}

fn refuse_task_unless_one_project(
    args: &ArgMatches,
    count: usize,
    config: &mv_config::load::Config,
) -> Result<(), Failure> {
    if args.get_one::<String>("task").is_none() || count == 1 {
        return Ok(());
    }
    let names = config.project_order.join(", ");
    Err(Failure::Config(format!(
        "--task reconciles one task in one project; name exactly one (got {count}: {names})."
    )))
}

/// Every configured project, in `config.toml`'s own declaration order
/// rather than `config.projects`' alphabetical `BTreeMap` order — card
/// `20261102-b-d10803`, the same gap `backlog::projects_in_declaration_
/// order` (`backlog.rs`, card `20261102-b-624f17`) already fixed for
/// `backlog list`: Python's `cmd_tasks` iterates `config.projects.values
/// ()`, a plain `dict` that keeps the config file's own insertion order
/// (`cli.py:7172,7223`). `project_order` already guarantees every key in
/// `projects` appears exactly once (`mv_config::load::Config::
/// project_order`'s own doc comment), so the lookup below cannot miss.
fn projects_in_declaration_order(
    config: &mv_config::load::Config,
) -> Vec<(&str, &mv_config::load::Project)> {
    config
        .project_order
        .iter()
        .filter_map(|name| {
            config
                .projects
                .get(name)
                .map(|project| (name.as_str(), project))
        })
        .collect()
}

/// The loop both [`list_fleet`] and [`list_hub_widened`] share: every
/// configured project's task list, one after another, blank-line
/// separated.
fn list_configured_projects(
    config: &mv_config::load::Config,
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
) -> Result<(), Failure> {
    for (name, project) in projects_in_declaration_order(config) {
        match project_path(project) {
            Some(root) => list(root, Some(name), args, agents, Some(config))?,
            None => {
                println!("\n{name}");
                println!("    (path not yet configured)");
            }
        }
    }
    Ok(())
}

/// Every row `list` would print for `root`, plus how many need attention —
/// pulled out of `list` itself so the task universe this builds from
/// (`routing.log`, never a raw directory scan — see its own doc comment)
/// is directly testable without capturing stdout.
fn build_rows(
    root: &Path,
    args: &ArgMatches,
    live: Option<&[LiveAgentStatus]>,
    outcome_lines: Option<&HashMap<String, Option<usize>>>,
) -> (Vec<Row>, usize) {
    let only_task = args.get_one::<String>("task").map(String::as_str);

    // The task universe comes from `routing.log`, mirroring Python's
    // `read_log` — never a raw scan of `.multiverse/tasks`. A directory
    // scan also matched a stray loose file sitting directly under
    // `tasks/` (never inside its own `<id>/` directory — its filename
    // alone still satisfies the generic task-id pattern) and a task
    // directory whose dispatch predates the routing log's earliest
    // surviving entry; Python's reader never learns about either one,
    // since both are invisible to `read_log` by construction (card
    // `20261102-b-1b2d80`, the same root cause `mv_state::tasks::
    // sweep_tasks` had).
    //
    // Listed in the order each task id was first seen in the log — its
    // own dispatch order — not alphabetically, matching `cmd_tasks`
    // (card `20261102-b-a5782d`: an alphabetical sort here used to
    // reorder every listing relative to Python's).
    let (mut task_events, entries) = gather_task_events(root);
    if let Some(outcome_lines) = outcome_lines {
        for (id, events) in &mut task_events {
            if let Some(line) = outcome_lines.get(id) {
                events.result_line = *line;
            }
        }
    }

    let mut rows = Vec::new();
    let mut needs_look = 0usize;
    for task_id in &entries {
        if let Some(only) = only_task {
            if task_id != only {
                continue;
            }
        }
        let events = task_events.get(task_id);
        let lane_id = events
            .map(|events| {
                events
                    .branch
                    .strip_prefix("multiverse/")
                    .unwrap_or(task_id.as_str())
            })
            .unwrap_or(task_id);
        let since = events.and_then(|e| e.started_at.or(e.dispatched_at));
        let (file_status, unrecognized_status) = match read_writer_status_verbose(
            root,
            TaskRef {
                task_id,
                lane_task_id: lane_id,
            },
            since,
        ) {
            StatusRead::Terminal(status) => (Some(status), None),
            StatusRead::Unrecognized(raw) => (None, Some(raw)),
            StatusRead::Absent => (None, None),
        };

        let (state, suffix, flagged, mut note) =
            classify(events, file_status, unrecognized_status.as_deref(), live);
        if state == "running" {
            // A healthy, working agent (or one Herdr can't currently be
            // asked about at all — the `live.is_none()` case `classify`
            // also routes here) is never "possibly frozen" on age alone —
            // a Builder routinely runs for hours. Scout 342332 finding
            // 4's own age-only gate here (reverted: frozen-pane rework)
            // flagged nearly every healthy lane. The real signal lives in
            // the `stalled` branch below, where Herdr actually reports the
            // pane idle/done — not working.
            if let Some(events) = events {
                let elapsed = events
                    .dispatched_at
                    .map(|stamp| mv_core::time::now_utc() - stamp)
                    .filter(|elapsed| !elapsed.is_negative())
                    .map(mv_state::tasks::format_elapsed)
                    .unwrap_or_else(|| "unknown".into());
                note = Some(format!("dispatched {elapsed} ago, still running as {} — and still spending. Nothing here will stop it.", python_repr(&events.agent)));
            }
        } else if state == "stalled" {
            // Frozen-pane rework (Scout 342332 finding 4, reworked): gate
            // "possibly frozen" on a real signal, not age alone. Signal
            // (a) — Herdr reports the agent idle/done (ready for input,
            // not working) while no terminal status file exists — is the
            // host-sleep shape seen on 2026-11-17: an idle prompt, no
            // status, hours old. `blocked`/`unknown` keep `classify`'s
            // plain stalled note unconditionally: `blocked` is ambiguous
            // without more context and `unknown` proves nothing either
            // way (`AgentState`'s own doc comment). Reuses
            // `mv_board::layout::STALE_AFTER_MINUTES`, the same threshold
            // `board_data`'s stale-pane check already applies, rather
            // than a second number for the same kind of silence.
            if let Some(events) = events {
                let status = live.and_then(|agents| {
                    agents
                        .iter()
                        .find(|agent| Some(agent.pane_id.as_str()) == events.effective_pane())
                        .and_then(|agent| agent.status)
                });
                if let Some(status) = status.filter(|status| status.is_ready_for_input()) {
                    let dispatched_elapsed = events
                        .dispatched_at
                        .map(|stamp| mv_core::time::now_utc() - stamp)
                        .filter(|elapsed| !elapsed.is_negative());
                    if dispatched_elapsed.is_some_and(|elapsed| {
                        elapsed > time::Duration::minutes(mv_board::layout::STALE_AFTER_MINUTES)
                    }) {
                        let elapsed = dispatched_elapsed
                            .map(mv_state::tasks::format_elapsed)
                            .unwrap_or_else(|| "unknown".into());
                        note = Some(format!(
                            "possibly frozen: agent reports {} with no status file for \
                             {elapsed} \u{2014} not acted on; try `herdr agent prompt {}` or \
                             `dispatch --into {task_id}`.",
                            status.as_str(),
                            events.agent,
                        ));
                    }
                }
            }
        }
        if flagged {
            needs_look += 1;
        }
        let (role, account) = events
            .map(|events| (display_role(&events.role), events.account.clone()))
            .unwrap_or_else(|| ("-".to_string(), "-".to_string()));
        rows.push(Row {
            task_id: task_id.clone(),
            role,
            account,
            state,
            suffix,
            flagged,
            note,
        });
    }
    (rows, needs_look)
}

// Python `cmd_tasks` (`cli.py:7668-7669`) sorts on `(priority, created_at)`
// only, with no id tie-break; `sorted()` is stable, so two items sharing a
// priority and a second-granularity `created_at` keep `read_items`'s own
// first-seen/log order. `sort_by` here is stable too, so matching that key
// exactly (no `item` id third term) reproduces the same fallback -- a
// previous version tie-broke by `item` id, a random-looking hex suffix
// unrelated to creation order, and so diverged from Python on every such
// tie (seed 12000: two same-second P2 items printed in a different order).
fn sort_open_backlog(items: &mut [mv_state::backlog::BacklogItem]) {
    items.sort_by(|a, b| (&a.priority, &a.created_at).cmp(&(&b.priority, &b.created_at)));
}

/// Reuse one already-observed listing for a helper's classification pass.
/// Later probes still reach Herdr so pane teardown always gets fresh evidence.
struct ObservedAgents<'a, A> {
    agents: &'a mut A,
    first: Option<Option<Vec<LiveAgentStatus>>>,
}

impl<'a, A: AgentPanes> ObservedAgents<'a, A> {
    fn new(agents: &'a mut A, live: Option<&[LiveAgentStatus]>) -> Self {
        Self {
            agents,
            first: Some(live.map(<[_]>::to_vec)),
        }
    }
}

impl<A: AgentPanes> AgentPanes for ObservedAgents<'_, A> {
    fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
        self.list_checked()
            .unwrap_or_default()
            .into_iter()
            .map(|agent| mv_state::tasks::LiveAgent {
                pane_id: agent.pane_id,
            })
            .collect()
    }

    fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
        self.first
            .take()
            .unwrap_or_else(|| self.agents.list_checked())
    }

    fn close(&mut self, pane: &str) -> bool {
        self.agents.close(pane)
    }

    fn read_visible(&mut self, pane: &str) -> Option<String> {
        self.agents.read_visible(pane)
    }

    fn confirm_live(
        &mut self,
        pane_id: &str,
        name: &str,
    ) -> Option<mv_state::tasks::LiveConfirmation> {
        self.agents.confirm_live(pane_id, name)
    }

    fn pane_label(&mut self, pane_id: &str) -> Option<String> {
        self.agents.pane_label(pane_id)
    }
}

fn list(
    root: &Path,
    project_name: Option<&str>,
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    config: Option<&mv_config::load::Config>,
) -> Result<(), Failure> {
    let live = agents.list_checked();
    let read_only = read_only_mode(args);
    let explicit_reconcile = args.get_flag("reconcile");
    let no_reconcile = args.get_flag("no-reconcile");

    // Automatic listing records UNRECORDED, closes settled panes, and
    // converges stale AWAITING tasks before print — Python
    // `cli.py:7314-7358`. Explicit `--reconcile` prints first, then writes.
    let outcome_lines = if !read_only && !no_reconcile && !explicit_reconcile {
        Some(auto_reconcile_project(
            root,
            args,
            agents,
            config,
            live.as_deref(),
        ))
    } else {
        None
    };

    let merged_backlog = if !read_only && !no_reconcile {
        let tasks = mv_state::tasks::sweep_tasks(root);
        mv_state::tasks::close_merged_backlog_links(root, &tasks, true)
    } else {
        Vec::new()
    };

    let (rows, needs_look) = build_rows(root, args, live.as_deref(), outcome_lines.as_ref());
    // Python `cmd_tasks` (`cli.py:7368-7381`) is silent when a project has
    // no dispatched tasks *and* no open backlog — it never prints
    // "no tasks found". Open backlog is a section of this listing, not a
    // fake task row. Found via `conformance/diff_fuzz.py` seed 9000.
    let mut open_items = mv_state::backlog::read_items(root, true);
    sort_open_backlog(&mut open_items);

    if !rows.is_empty() || !open_items.is_empty() || !merged_backlog.is_empty() {
        if let Some(name) = project_name {
            // Python `cmd_tasks` (`cli.py:7381`) prints `f"\n{project.name}"`
            // so a blank line always precedes the project header when the
            // project has anything to show. Found via
            // `conformance/diff_fuzz.py` seed 9000.
            println!("\n{name}");
        }
        for (item, evidence, _) in &merged_backlog {
            println!(" backlog {item} closed (evidence {evidence})");
        }
        for row in &rows {
            let (line, note) = display_row(row);
            println!("{line}");
            if let Some(note) = note {
                println!("{note}");
            }
            probe_blocking_prompt(root, row, args, agents, config, live.as_deref());
        }
    }
    if !read_only {
        if explicit_reconcile {
            reconcile_and_report(
                root,
                project_name,
                &rows,
                args,
                agents,
                config,
                live.as_deref(),
            );
        }
        if args.get_flag("close-stranded") {
            close_stranded_and_report(root, &rows, args, agents, live.as_deref());
        }
        if args.get_flag("reclaimable") {
            reclaimable_and_report(root, args, agents, config, live.as_deref());
        }
    }
    // Python `cmd_tasks` (`cli.py:7662-7663`) prints `open backlog:` after
    // the `--reconcile`/`--close-stranded`/`--reclaimable` write-leg
    // reports above, not before them — this used to print it inside the
    // row-listing block, ahead of those reports, which put it in the wrong
    // place relative to `would close`/`NOT closed`/`stale bookkeeping`
    // lines. Found via `conformance/diff_fuzz.py` seed 50015.
    if !open_items.is_empty() {
        println!("  open backlog:");
        for item in &open_items {
            println!("    {} {} {}", item.item, item.priority, item.title);
        }
    }
    if needs_look > 0 {
        println!();
        println!("{needs_look} task(s) marked * need a look.");
    }
    Ok(())
}

/// `--close-stranded`'s own write leg, run after the listing above —
/// mirrors `cmd_tasks`'s own order: a task with a live agent is named as
/// refused before anything closes (it is `RUNNING` or `STALLED`, not
/// stranded — a task with a recoverable status file is `unrecorded`,
/// `--reconcile`'s own, untouched here), then every determination
/// [`mv_state::tasks::close_stranded`] computed is printed with its
/// evidence, dry-run or not.
///
/// `STALLED` is refused alongside `RUNNING` (card `20261106-b-2c3d2f`
/// Decision 1): the listing just called this row's pane live but not
/// `Working`, and closing it in the same invocation would record an
/// outcome for an agent the listing itself still shows live.
fn close_stranded_and_report(
    root: &Path,
    rows: &[Row],
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    live: Option<&[LiveAgentStatus]>,
) {
    if live.is_none() {
        return;
    }
    let live_agent_rows: Vec<&str> = rows
        .iter()
        .filter(|row| matches!(row.state, "running" | "stalled"))
        .map(|row| row.task_id.as_str())
        .collect();
    if !live_agent_rows.is_empty() {
        println!(
            "  refusing to close {} task(s) with a live agent — not stranded: {}",
            live_agent_rows.len(),
            live_agent_rows.join(", ")
        );
    }

    let apply = args.get_flag("yes");
    let mut observed = ObservedAgents::new(agents, live);
    let closures = mv_state::tasks::close_stranded(root, apply, &mut observed);
    for closure in &closures {
        let verb = if closure.recorded {
            "closed"
        } else if !apply {
            "would close"
        } else {
            "already recorded"
        };
        println!(
            "  {verb} {} as {} (state={})\n      evidence: {}",
            closure.task_id, closure.status, closure.state, closure.evidence
        );
    }
    if !closures.is_empty() && !apply {
        println!("  dry run: nothing was written. Add --yes to record these outcomes.");
    }
}

/// `--reclaimable`'s own write leg, run after the listing above — mirrors
/// `cmd_tasks`'s own order. `--yes` re-lists and only closes a pane that
/// is still idle/done under the same agent; WORKING/BLOCKED/unknown fail
/// closed (`cli.py:7583-7616` / `_reclaim_pane_confirmation`).
///
/// Card `20261115-b-9815fe`: the reap is checked only after a candidate's
/// own pane is actually confirmed `"closed"` this pass — never for `"NOT
/// closed"`, where the pane safety check itself refused, and the process
/// this lane still shows may belong to an occupant this run never
/// confirmed gone. Without `--yes` nothing closes, so every candidate's
/// own lingering process is previewed as "would reap" there, the same
/// shape `--prune --reap --dry-run` already uses. Card
/// `20261122-b-78bcf9`: `--yes` alone now reaps a just-closed lane's
/// leftovers (closing the pane is what made them leftovers), and `--reap`
/// also covers a stale entry whose pane was already gone.
///
/// Card `20261122-b-4ad223`: a candidate on a Claude seat whose pane
/// footer still shows a running background shell is printed `kept …
/// waiting-on-shell` and never closed, `--yes` or not — a recorded lane
/// can still have its own suite running. A Claude pane whose text cannot
/// be read is kept too. `mv_state::tasks::reclaimable_panes` cannot apply
/// this itself (mv-state sits below the mv-engine classifier), so the
/// candidates are filtered here, by the same
/// [`super::dispatch::reclaim_shell_check`] the pre-dispatch reclaim uses.
/// `config` of `None` (no loadable config) skips the check.
fn reclaimable_and_report(
    root: &Path,
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    config: Option<&mv_config::load::Config>,
    live: Option<&[LiveAgentStatus]>,
) {
    if live.is_none() {
        return;
    }
    let apply = args.get_flag("yes");
    let reap = args.get_flag("reap");
    let mut observed = ObservedAgents::new(agents, live);
    let (candidates, stale) = mv_state::tasks::reclaimable_panes(root, &mut observed);

    // Card `20261122-b-78bcf9`: every candidate's lane is now checked for
    // leftovers once its pane closes (or, without `--yes`, previewed), so
    // the task snapshot is needed whenever there is any candidate at all.
    let lane_tasks = if reap || !candidates.is_empty() {
        mv_state::tasks::sweep_tasks(root)
    } else {
        Vec::new()
    };
    let shell_checks: Vec<ReclaimShellCheck> = candidates
        .iter()
        .map(|candidate| match config {
            Some(config) => {
                super::dispatch::reclaim_shell_check(config, &lane_tasks, candidate, agents)
            }
            None => ReclaimShellCheck::Clear,
        })
        .collect();
    let closable = shell_checks
        .iter()
        .filter(|check| **check == ReclaimShellCheck::Clear)
        .count();

    let confirmed = if apply && closable > 0 {
        agents.list_checked()
    } else {
        None
    };
    let ancestors = if reap || !candidates.is_empty() {
        mv_engine::dockerreport::own_ancestor_pids()
    } else {
        Vec::new()
    };
    for (candidate, shell_check) in candidates.iter().zip(&shell_checks) {
        let kept_because = match shell_check {
            ReclaimShellCheck::Clear => None,
            ReclaimShellCheck::WaitingOnShell => {
                Some("waiting-on-shell: a background shell is still running")
            }
            ReclaimShellCheck::Unreadable => {
                Some("pane could not be read to rule out a running background shell")
            }
        };
        if let Some(reason) = kept_because {
            println!(
                "  kept {} ({}, agent={}, pane={}) — {reason}",
                candidate.task_id,
                display_role(&candidate.role),
                python_repr(candidate.agent.as_deref().unwrap_or("")),
                python_repr(&candidate.pane)
            );
            continue;
        }
        let verb = if !apply {
            "would close"
        } else if reclaim_pane_is_safe(candidate, confirmed.as_deref())
            && agents.close(&candidate.pane)
        {
            "closed"
        } else {
            "NOT closed"
        };
        // Python (`cli.py:7617-7619`) prints `agent={task.agent!r}` ahead
        // of `pane={task.pane!r}` — both missing-field-absent-as-`''` and
        // single-quoted, not `{:?}`'s always-double-quoted Debug form.
        // Found via `conformance/diff_fuzz.py` seed 50015.
        println!(
            "  {verb} {} ({}, agent={}, pane={})",
            candidate.task_id,
            display_role(&candidate.role),
            python_repr(candidate.agent.as_deref().unwrap_or("")),
            python_repr(&candidate.pane)
        );
        if apply && verb == "NOT closed" {
            println!(
                "      pane could not be confirmed idle under the same agent twice — left open \
                 even with --yes"
            );
        }
        if verb == "closed" || !apply {
            for note in reap_lane_notes(
                root,
                &lane_tasks,
                &candidate.task_id,
                apply,
                reap,
                &ancestors,
            ) {
                println!("{note}");
            }
        }
    }
    if closable > 0 && !apply {
        println!("  dry run: nothing was closed. Add --yes to close these panes.");
    }

    if !stale.is_empty() {
        println!("  stale bookkeeping — pane already gone, holds no slot:");
        for candidate in &stale {
            println!(
                "    {} ({}, agent={}, pane={})",
                candidate.task_id,
                display_role(&candidate.role),
                python_repr(candidate.agent.as_deref().unwrap_or("")),
                python_repr(&candidate.pane)
            );
            if apply {
                mv_state::tasks::append_stale_pane_if_absent(
                    root,
                    &candidate.task_id,
                    &candidate.pane,
                );
            }
            // Card `20261122-b-78bcf9`: a pane already gone is the same dead
            // lane a close just made — `--reap` lists (and with `--yes`
            // ends) its leftovers too, where it used to skip the lane.
            if reap {
                for note in reap_lane_notes(
                    root,
                    &lane_tasks,
                    &candidate.task_id,
                    apply,
                    true,
                    &ancestors,
                ) {
                    println!("  {note}");
                }
            }
        }
    }
}

/// `candidate`'s own lane's lingering host processes: the
/// `20261115-b-9815fe` reaper's one call site in this file. `tasks` is a
/// snapshot already read once by the caller — `sweep_tasks` reads the
/// whole routing log, so this stays one read per `--reclaimable` pass, not
/// one per candidate. `apply` gates signaling exactly the way
/// `verb == "closed"`/`!apply` already gate calling this at all: `true`
/// reaps for real, `false` only previews "would reap".
///
/// Card `20261122-b-78bcf9`: the caller only reaches this once the lane's
/// pane is closed (or, previewing, would be) or already gone, so the lane
/// is dead and [`super::worktrees::leftover_protected`]'s short list
/// applies — a `zsh -c` left behind is ended like the `tail -f` beside it.
/// `--yes` reaps after its own close whether or not `--reap` was given;
/// `explicit` (`--reap`) only decides whether a failed process probe is
/// worth a warning line, so a plain `--reclaimable` prints nothing new
/// unless the lane really holds a leftover. A lane with no worktree on disk
/// is never probed at all.
fn reap_lane_notes(
    root: &Path,
    tasks: &[mv_state::tasks::SweepTask],
    task_id: &str,
    apply: bool,
    explicit: bool,
    ancestors: &[u32],
) -> Vec<String> {
    let Some(task) = tasks.iter().find(|task| task.task_id == task_id) else {
        return Vec::new();
    };
    let lane_path = root
        .join(".multiverse/worktrees")
        .join(mv_state::tasks::lane_task_id(task));
    if !lane_path.is_dir() {
        return Vec::new();
    }
    let Some(snapshot) = mv_engine::dockerreport::process_cwds() else {
        return if explicit {
            vec!["      warning: process-cwd probe unknown".to_string()]
        } else {
            Vec::new()
        };
    };
    let mut notes = Vec::new();
    for process in mv_engine::dockerreport::processes_under(&lane_path, &snapshot) {
        let label = mv_engine::dockerreport::process_label(&process);
        if super::worktrees::leftover_protected(&process, ancestors) {
            notes.push(format!(
                "      lane process {label} (protected, never reaped)"
            ));
        } else if apply {
            notes.push(format!("      reaping {label}"));
            let outcome = super::worktrees::end_leftover(&process);
            notes.push(format!(
                "        {}",
                super::worktrees::reap_outcome_text(outcome)
            ));
        } else {
            notes.push(format!("      would reap {label}"));
        }
    }
    notes
}

/// `--reconcile`'s own write leg, run after the listing above has already
/// printed — mirrors `cmd_tasks`'s own order: the rows above still show
/// each task's pre-reconcile state (an `unrecorded` row keeps its "—
/// `--reconcile` records it" note even for the task this call is about to
/// record), and only this trailing line reports what actually got written,
/// exactly as `tasklog.reconcile`'s own summary does.
///
/// `--task` narrows which task is recorded (`mv_state::tasks::
/// reconcile_settled`'s `only_task`); unlike Python, it also narrows which
/// row the listing above printed in the first place (`list`'s own
/// `only_task` filter, pre-existing and unrelated to this call) — see this
/// task's result.md for that one shape mismatch.
fn reclaim_pane_is_safe(
    candidate: &mv_state::tasks::ReclaimCandidate,
    live: Option<&[LiveAgentStatus]>,
) -> bool {
    let Some(live) = live else {
        return false;
    };
    let Some(current) = live.iter().find(|agent| agent.pane_id == candidate.pane) else {
        return false;
    };
    if candidate.agent.as_deref().unwrap_or("") != current.name.as_deref().unwrap_or("") {
        return false;
    }
    current.status.is_some_and(AgentState::is_ready_for_input)
}

fn reconcile_and_report(
    root: &Path,
    project_name: Option<&str>,
    rows: &[Row],
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    config: Option<&mv_config::load::Config>,
    live: Option<&[LiveAgentStatus]>,
) {
    let only_task = args.get_one::<String>("task").map(String::as_str);
    if let Some(task_id) = only_task {
        match rows.iter().find(|row| row.task_id == task_id) {
            None => println!(
                "  --task {} not found among this project's dispatched tasks",
                python_repr(task_id)
            ),
            Some(row) if row.state != "unrecorded" => println!(
                "  --task {} is {}, not unrecorded — nothing for --reconcile to do",
                python_repr(task_id),
                row.state
            ),
            _ => {}
        }
    }
    let (lease_minutes, overrides) = lease_settings(config);
    let mut observed = ObservedAgents::new(agents, live);
    let outcome = mv_state::tasks::reconcile_project(
        root,
        ReconcileRequest {
            only_task,
            close_panes: true,
            expire_stale: false,
            lease_minutes,
            lease_overrides: &overrides,
            now: mv_core::time::now_utc(),
            require_live: false,
        },
        &mut observed,
    );
    if !outcome.recorded.is_empty() {
        println!(
            "  recorded {} outcome(s) that were finished but unlogged",
            outcome.recorded.len()
        );
    }
    report_orphan_panes(root, project_name, config, live);
}

/// Current role codes a live agent's dispatch-time name could carry —
/// duplicated from `dispatch::role_short` (private to that file, and
/// outside this task's write scope) rather than shared, since both are
/// nine-arm matches on the same nine [`Role`] variants and neither is
/// likely to drift without the other noticing in review.
fn lane_role_short(role: Role) -> &'static str {
    match role {
        Role::Nexus => "nexu",
        Role::Scout => "scou",
        Role::Builder => "buil",
        Role::Archivist => "arch",
        Role::Critic => "crit",
        Role::Seer => "seer",
        Role::Council => "coun",
        Role::Artisan => "arti",
        Role::Watcher => "watc",
    }
}

/// Every Herdr agent name `naming::agent_name` could mint for one of
/// `project_name`'s own dispatched lanes — current role/seat codes only,
/// instance 1 through [`naming::MAX_NAME_INSTANCE`], the same budget
/// `naming::unique_agent_name` retries against: a hand-copied, truncated
/// range here once stopped at instance 9 and silently missed a collision
/// past it (card `20261118-b-c328a3` M4). [`Role::Nexus`] is skipped even
/// when a chain names it:
/// Nexus's own pane is never dispatched through `routing.log` the way a
/// task's lane is, so it is not a "lane" this orphan check is about.
///
/// Returns an empty set — never a panic or a guess — when `config` is
/// absent or does not know `project_name`; every caller already treats
/// that as "nothing to report" (`tasks --reconcile`'s orphan-pane line,
/// card `20261118-b-fde912`).
fn project_lane_agent_names(
    project_name: &str,
    config: Option<&mv_config::load::Config>,
) -> HashSet<String> {
    let mut names = HashSet::new();
    let Some(config) = config else {
        return names;
    };
    let Some(project) = config.projects.get(project_name) else {
        return names;
    };
    let short = project.short.as_deref().unwrap_or("");
    for (role, chain) in &project.chains {
        if *role == Role::Nexus {
            continue;
        }
        for account_name in chain {
            let Some(account) = config.accounts.get(account_name) else {
                continue;
            };
            for instance in 1..=naming::MAX_NAME_INSTANCE {
                let suffix = instance.to_string();
                let instance_arg = (instance > 1).then_some(suffix.as_str());
                if let Ok(name) = naming::agent_name(
                    &project.name,
                    lane_role_short(*role),
                    instance_arg,
                    &account.short,
                    short,
                ) {
                    names.insert(name);
                }
            }
        }
    }
    names
}

/// `--reconcile`'s orphan-pane line (card `20261118-b-fde912` step 1,
/// report-only): a live Herdr agent whose name matches `project_name`'s
/// own lane naming but whose pane [`mv_state::tasks::current_pane_agents`]
/// either has no current record of, or currently records for a different
/// agent name. Printed, never acted on — no close, no prompt, no write —
/// one line per orphan, nothing when there are none, so every existing
/// conformance scenario's output stays byte-identical (PARITY FIRST: this
/// is new Rust-only reporting with no Python reference, and the one way
/// to keep it from leaking into scenarios that never meant to exercise it
/// is to stay silent until there is something real to say).
///
/// Checking the agent name along with the pane, not just the pane id by
/// itself, is what catches a pane Herdr recycles for a different,
/// never-dispatched agent after the one this project's log recorded for
/// it is gone — a plain pane-id membership test (the old `known_panes`)
/// kept reading that pane as "known" forever once any dispatch round had
/// ever named it (card `20261118-b-c328a3` M1).
fn report_orphan_panes(
    root: &Path,
    project_name: Option<&str>,
    config: Option<&mv_config::load::Config>,
    live: Option<&[LiveAgentStatus]>,
) {
    let (Some(project_name), Some(live)) = (project_name, live) else {
        return;
    };
    let lane_names = project_lane_agent_names(project_name, config);
    if lane_names.is_empty() {
        return;
    }
    let current_panes = mv_state::tasks::current_pane_agents(root);
    let mut orphans: Vec<&LiveAgentStatus> = live
        .iter()
        .filter(|agent| {
            agent
                .name
                .as_deref()
                .is_some_and(|name| lane_names.contains(name))
                && !current_panes
                    .get(&agent.pane_id)
                    .zip(agent.name.as_ref())
                    .is_some_and(|(recorded, name)| recorded.contains(name))
        })
        .collect();
    orphans.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    for orphan in orphans {
        println!(
            "  orphan pane {} (agent {}): no task record — close it in Herdr if it is not \
             yours",
            orphan.pane_id,
            orphan.name.as_deref().unwrap_or("")
        );
    }
}

fn auto_reconcile_project(
    root: &Path,
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    config: Option<&mv_config::load::Config>,
    live: Option<&[LiveAgentStatus]>,
) -> HashMap<String, Option<usize>> {
    if live.is_none() {
        return HashMap::new();
    }
    let (before, _) = gather_task_events(root);
    let (lease_minutes, overrides) = lease_settings(config);
    let mut observed = ObservedAgents::new(agents, live);
    let outcome = mv_state::tasks::reconcile_project(
        root,
        ReconcileRequest {
            only_task: None,
            close_panes: !args.get_flag("reclaimable"),
            expire_stale: !args.get_flag("close-stranded"),
            lease_minutes,
            lease_overrides: &overrides,
            now: mv_core::time::now_utc(),
            require_live: true,
        },
        &mut observed,
    );
    // Python re-reads outcomes recorded by reconciliation, then converges
    // stale tasks in memory without updating their outcome ordinals. Keep
    // those prior ordinals (including None) for this invocation's listing.
    let mut outcome_lines: HashMap<String, Option<usize>> = before
        .into_iter()
        .map(|(id, events)| (id, events.result_line))
        .collect();
    if !outcome.recorded.is_empty() {
        // A newly reconciled BLOCKED task can also be superseded in this
        // pass. Retain its reconciliation result's line, before the later
        // correction, just as Python's intermediate re-read does.
        for record in index_records(&root.join(".multiverse/routing.log"), Selector::Tasks)
            .unwrap_or_default()
        {
            let Ok(parsed) = record.parse() else {
                continue;
            };
            if parsed.event() == "result" {
                if let Some(id) = parsed.fields.get("task") {
                    if outcome.recorded.contains(id) {
                        outcome_lines.insert(id.clone(), Some(record.entry.ordinal as usize + 1));
                    }
                }
            }
        }
    }
    outcome_lines
}

fn lease_settings(config: Option<&mv_config::load::Config>) -> (u32, HashMap<String, u32>) {
    let Some(config) = config else {
        return (60, HashMap::new());
    };
    let overrides = config
        .accounts
        .iter()
        .filter_map(|(name, account)| {
            account
                .awaiting_lease_minutes_override
                .map(|minutes| (name.clone(), minutes))
        })
        .collect();
    (config.awaiting_lease_minutes, overrides)
}

fn breaker_kind_token(kind: BreakerKind) -> &'static str {
    match kind {
        BreakerKind::Weekly => "weekly",
        BreakerKind::Budget => "budget",
        BreakerKind::Rolling => "rolling",
    }
}

fn probe_blocking_prompt(
    root: &Path,
    row: &Row,
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    config: Option<&mv_config::load::Config>,
    live: Option<&[LiveAgentStatus]>,
) {
    if !matches!(row.state, "running" | "stalled") {
        return;
    }
    let Some(live) = live else {
        return;
    };
    let (events, _) = gather_task_events(root);
    let Some(events) = events.get(&row.task_id) else {
        return;
    };
    let Some(pane) = events.effective_pane() else {
        return;
    };
    if events.agent.is_empty() {
        return;
    }
    let Some(current) = live.iter().find(|agent| agent.pane_id == pane) else {
        return;
    };
    if current.status == Some(AgentState::Working) {
        return;
    }
    let Some(config) = config else {
        return;
    };
    let Some(account) = config.accounts.get(&events.account) else {
        return;
    };
    let Some(text) = agents.read_visible(pane) else {
        return;
    };
    let Some(verdict) = limits::classify(&text, &account.kind, None) else {
        return;
    };
    if read_only_mode(args) {
        return;
    }
    let state_dir = state_dir_from_config(&config.source_path);
    let _ = breaker::trip(
        &state_dir,
        &account.pool,
        mv_core::time::now_utc(),
        breaker::TripRequest {
            vendor: &account.vendor,
            kind: verdict.kind,
            rule: &verdict.rule,
            caller: "tasks",
            evidence_path: None,
            pane_text: Some(&text),
            project_root: Some(root),
            manual: false,
        },
    );
    let stamp = mv_core::time::now_stamp();
    let line = format!(
        "{stamp}\tevent=limit\ttask={}\trule={}\tkind={}",
        row.task_id,
        verdict.rule,
        breaker_kind_token(verdict.kind)
    );
    let _ = mv_state::log::ProjectLog::new(root).append(&line);
}

/// Pull `(pool, detail)` out of a `SEAT OUT:` report body, or `None`.
/// Mirrors `activity.parse_seat_out`.
fn parse_seat_out(body: &str) -> Option<(String, String)> {
    let rest = body.strip_prefix("SEAT OUT:")?.trim();
    let em = rest.find('—').map(|i| (i, "—"));
    let hyphen = rest.find(" - ").map(|i| (i, " - "));
    let (pos, sep) = match (em, hyphen) {
        (Some(a), Some(b)) if a.0 <= b.0 => a,
        (Some(_), Some(b)) => b,
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    let pool = rest[..pos].trim();
    let detail = rest[pos + sep.len()..].trim();
    if pool.is_empty() || detail.is_empty() {
        None
    } else {
        Some((pool.to_string(), detail.to_string()))
    }
}

fn sync_seat_outages_if_hub_in_scope(
    config: &mv_config::load::Config,
    named: &[String],
    _args: &ArgMatches,
) {
    if !named
        .iter()
        .any(|name| name == mv_state::activity::HUB_PROJECT_NAME)
    {
        return;
    }
    let Some(hub) = config.projects.get(mv_state::activity::HUB_PROJECT_NAME) else {
        return;
    };
    let Some(hub_root) = hub.path.value().map(Path::new) else {
        return;
    };
    if let Err(err) = sync_seat_outages(config, hub_root) {
        println!(
            "\nseat outage sync ({}): {err}",
            mv_state::activity::HUB_PROJECT_NAME
        );
    }
}

fn sync_seat_outages(config: &mv_config::load::Config, hub_root: &Path) -> Result<(), String> {
    let log = ActivityLog::open(mv_state::activity::HUB_PROJECT_NAME.to_string(), hub_root);
    let state_dir = hub_root.join(".multiverse");
    let page =
        activity::read_inbox(&state_dir, &log, usize::MAX, false).map_err(|e| e.to_string())?;
    let breaker_dir = state_dir_from_config(&config.source_path);
    let now = mv_core::time::now_utc();
    let mut tripped: Vec<(String, String, String)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for message in &page.messages {
        let Some((pool, detail)) = parse_seat_out(&message.body) else {
            continue;
        };
        let Some(account) = config
            .accounts
            .values()
            .find(|account| account.pool == pool)
        else {
            skipped.push((
                message.id.clone(),
                format!("no configured account uses pool {pool:?}"),
            ));
            continue;
        };
        match breaker::trip(
            &breaker_dir,
            &pool,
            now,
            breaker::TripRequest {
                vendor: &account.vendor,
                kind: BreakerKind::Budget,
                rule: "",
                caller: "tasks",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: false,
            },
        ) {
            Ok(_) => tripped.push((pool, detail, message.id.clone())),
            Err(err) => skipped.push((message.id.clone(), err.to_string())),
        }
    }
    if !tripped.is_empty() {
        let names = tripped
            .iter()
            .map(|(pool, detail, _)| format!("{pool} — {detail}"))
            .collect::<Vec<_>>()
            .join("; ");
        println!(
            "\nseat outage sync ({})",
            mv_state::activity::HUB_PROJECT_NAME
        );
        println!("  fenced: {names}");
        let body = format!("SEAT OUT FENCED: {names}");
        let mut recipients = Vec::new();
        for (project_name, project) in &config.projects {
            if project_name == mv_state::activity::HUB_PROJECT_NAME {
                continue;
            }
            let Some(path) = project.path.value() else {
                continue;
            };
            let may_send = config
                .activity_pairs
                .get(mv_state::activity::HUB_PROJECT_NAME)
                .is_some_and(|allowed| allowed.iter().any(|p| p == project_name));
            recipients.push((
                ActivityLog::open(project_name.clone(), Path::new(path)),
                may_send,
            ));
        }
        let msg_id = format!("seat-out-fenced:{}", {
            let mut ids: Vec<&str> = tripped.iter().map(|(_, _, id)| id.as_str()).collect();
            ids.sort_unstable();
            ids.join(",")
        });
        let _ = activity::broadcast(activity::BroadcastRequest {
            from_project: mv_state::activity::HUB_PROJECT_NAME,
            body: &body,
            ask: false,
            recipients: &recipients,
            msg_id: &msg_id,
            redact: &mv_core::redact::redact,
        });
    }
    for (id, reason) in &skipped {
        let _ = id;
        println!("  seat report not actioned: {reason}");
    }
    if let Some(seq) = page.through_seq {
        let _ = activity::acknowledge(&state_dir, &log, seq, page.messages.len() as u64);
    }
    recheck_fenced_seats(config, &breaker_dir);
    Ok(())
}

fn recheck_fenced_seats(config: &mv_config::load::Config, breaker_dir: &Path) {
    let entries = mv_state::breaker_store::load(breaker_dir);
    let now = mv_core::time::now_utc();
    let mut recovered = Vec::new();
    for (pool, entry) in &entries {
        let status = breaker::read_status(Some(entry), None, now);
        if !status.open {
            continue;
        }
        let Some(account) = config
            .accounts
            .values()
            .find(|account| &account.pool == pool)
        else {
            continue;
        };
        if account.vendor != "local" {
            continue;
        }
        if breaker::reset(breaker_dir, pool, now, "seat recovery probe").is_ok() {
            recovered.push(pool.clone());
        }
    }
    if !recovered.is_empty() {
        println!("  recovered: {}", recovered.join(", "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::command;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;
    use crate::commands::worktrees::lane_child::LaneChild;
    use mv_state::tasks::LiveAgent;

    /// Card `20261114-b-74029b`: `probe_blocking_prompt` (this file's own
    /// `breaker::trip` call, line ~1355) reaches
    /// `mv_config::load::config_state_dir_from_env`, the same failure
    /// mode `commands::sweep`/`commands::dispatch`/`commands::council`/
    /// `commands::review_policy`/`commands::seat` each close with a
    /// guard of this same name — this module had none, so the one test
    /// that reaches that call (`blocking_prompt_trips_the_pool_and_
    /// records_event_limit`) relied on whatever `$MULTIVERSE_STATE_DIR`
    /// happened to already be set to in this process.
    struct StateDirEnvGuard {
        _herdr: HerdrEnvGuard,
        _dir: tempfile::TempDir,
        previous_state_dir: Option<std::ffi::OsString>,
        previous_plugin_state_dir: Option<std::ffi::OsString>,
        previous_config: Option<std::ffi::OsString>,
    }

    impl StateDirEnvGuard {
        fn new() -> Self {
            let herdr = HerdrEnvGuard::unreachable();
            let dir = tempfile::tempdir().unwrap();
            let previous_state_dir = std::env::var_os("MULTIVERSE_STATE_DIR");
            let previous_plugin_state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
            let previous_config = std::env::var_os("MULTIVERSE_CONFIG");
            std::env::set_var("MULTIVERSE_STATE_DIR", dir.path());
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", dir.path());
            std::env::remove_var("MULTIVERSE_CONFIG");
            Self {
                _herdr: herdr,
                _dir: dir,
                previous_state_dir,
                previous_plugin_state_dir,
                previous_config,
            }
        }
    }

    impl Drop for StateDirEnvGuard {
        fn drop(&mut self) {
            match self.previous_state_dir.take() {
                Some(value) => std::env::set_var("MULTIVERSE_STATE_DIR", value),
                None => std::env::remove_var("MULTIVERSE_STATE_DIR"),
            }
            match self.previous_plugin_state_dir.take() {
                Some(value) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", value),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            match self.previous_config.take() {
                Some(value) => std::env::set_var("MULTIVERSE_CONFIG", value),
                None => std::env::remove_var("MULTIVERSE_CONFIG"),
            }
        }
    }

    #[test]
    fn card68e9f5_unknown_live_pane_remains_running() {
        let events = TaskEvents {
            started_pane: Some("w1:p1".into()),
            ..TaskEvents::default()
        };
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: None,
        }];
        assert_eq!(
            classify(Some(&events), None, None, Some(&live)).0,
            "running"
        );
        assert_eq!(classify(Some(&events), None, None, Some(&[])).0, "awaiting");
    }

    /// Card `20261118-b-c328a3` M2: a kill between pane creation and
    /// `agent start` leaves only `event=pane` recorded (`started_pane` is
    /// `None`), but Herdr still lists the pane live under the dispatched
    /// agent. `classify` must read `pane_event_pane` the same way
    /// `started_pane` — a live, working agent on that pane reads as
    /// `running`, not the old lying `awaiting`. Revert check: drop the
    /// `.or(events.pane_event_pane.as_deref())` fallback in `classify` and
    /// this fails (`awaiting`, not `running`).
    #[test]
    fn classify_falls_back_to_the_pane_event_when_started_never_landed() {
        let events = TaskEvents {
            pane_event_pane: Some("w1:p1".into()),
            ..TaskEvents::default()
        };
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Working),
        }];
        assert_eq!(
            classify(Some(&events), None, None, Some(&live)).0,
            "running"
        );
        assert_eq!(classify(Some(&events), None, None, Some(&[])).0, "awaiting");
    }

    #[test]
    fn display_role_translates_a_legacy_key_and_passes_unknown_through() {
        assert_eq!(display_role("fixer"), "builder");
        assert_eq!(display_role("builder"), "builder");
        assert_eq!(display_role("archaeologist"), "archaeologist");
    }

    #[test]
    fn nonworking_live_panes_are_stalled_with_the_python_note() {
        let events = TaskEvents {
            started_pane: Some("w1:p2".into()),
            ..Default::default()
        };
        for status in [
            AgentState::Idle,
            AgentState::Done,
            AgentState::Blocked,
            AgentState::Unknown,
        ] {
            let live = [LiveAgentStatus {
                pane_id: "w1:p2".into(),
                name: None,
                status: Some(status),
            }];
            let (state, suffix, flagged, note) = classify(Some(&events), None, None, Some(&live));
            assert_eq!(state, "stalled", "{}", status.as_str());
            assert_eq!(suffix, DASH);
            assert!(flagged);
            assert_eq!(note.unwrap(), format!(
                "live in pane 'w1:p2' but agent is live but not working ({}): not dead and not still spending — go look at the pane.", status.as_str()
            ));
        }
        let live = [LiveAgentStatus {
            pane_id: "w1:p2".into(),
            name: None,
            status: Some(AgentState::Working),
        }];
        assert_eq!(
            classify(Some(&events), None, None, Some(&live)),
            ("running", DASH.into(), true, None)
        );
    }

    fn matches(words: &[&str]) -> clap::ArgMatches {
        let m = command()
            .try_get_matches_from(std::iter::once("multiverse").chain(words.iter().copied()))
            .unwrap();
        m.subcommand_matches("tasks").unwrap().clone()
    }

    /// Scripted stand-in for [`HerdrAgents`] — same seam `sweep.rs`'s own
    /// tests use, so `--reconcile`'s pane-liveness check never spawns a
    /// real Herdr under `cargo test`.
    struct FakeAgents {
        panes: Vec<LiveAgentStatus>,
        fail_next_probes: usize,
        visible: HashMap<String, String>,
        probes: usize,
        /// `pane get`'s own `label`, by pane id — card `20261106-b-
        /// 9d70c7`'s occupancy gate. A pane with no entry here models a
        /// fake that was never told to answer this probe, same posture
        /// as the trait default: `pane_label` returns `None`.
        labels: HashMap<String, String>,
        /// `agent get`'s own `cwd`, by pane id — the gate's second
        /// witness, read through `confirm_live`.
        cwds: HashMap<String, std::path::PathBuf>,
    }

    impl FakeAgents {
        fn new(panes: &[&str]) -> Self {
            Self {
                panes: panes
                    .iter()
                    .map(|pane_id| LiveAgentStatus {
                        pane_id: pane_id.to_string(),
                        name: None,
                        status: None,
                    })
                    .collect(),
                fail_next_probes: 0,
                visible: HashMap::new(),
                probes: 0,
                labels: HashMap::new(),
                cwds: HashMap::new(),
            }
        }

        fn fail_next(mut self, times: usize) -> Self {
            self.fail_next_probes = times;
            self
        }

        fn with_status(mut self, pane_id: &str, status: AgentState) -> Self {
            if let Some(agent) = self.panes.iter_mut().find(|a| a.pane_id == pane_id) {
                agent.status = Some(status);
            } else {
                self.panes.push(LiveAgentStatus {
                    pane_id: pane_id.to_string(),
                    name: None,
                    status: Some(status),
                });
            }
            self
        }

        fn with_visible(mut self, pane_id: &str, text: &str) -> Self {
            self.visible.insert(pane_id.to_string(), text.to_string());
            self
        }

        fn with_label(mut self, pane_id: &str, label: &str) -> Self {
            self.labels.insert(pane_id.to_string(), label.to_string());
            self
        }

        fn with_cwd(mut self, pane_id: &str, cwd: impl Into<std::path::PathBuf>) -> Self {
            self.cwds.insert(pane_id.to_string(), cwd.into());
            self
        }
    }

    impl AgentPanes for FakeAgents {
        fn list(&mut self) -> Vec<LiveAgent> {
            self.probes += 1;
            self.panes
                .iter()
                .map(|agent| LiveAgent {
                    pane_id: agent.pane_id.clone(),
                })
                .collect()
        }
        fn close(&mut self, pane_id: &str) -> bool {
            self.panes.retain(|agent| agent.pane_id != pane_id);
            true
        }
        fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
            self.probes += 1;
            if self.fail_next_probes > 0 {
                self.fail_next_probes -= 1;
                return None;
            }
            Some(self.panes.clone())
        }
        fn read_visible(&mut self, pane_id: &str) -> Option<String> {
            self.visible.get(pane_id).cloned()
        }
        fn pane_label(&mut self, pane_id: &str) -> Option<String> {
            self.labels.get(pane_id).cloned()
        }
        fn confirm_live(
            &mut self,
            pane_id: &str,
            name: &str,
        ) -> Option<mv_state::tasks::LiveConfirmation> {
            let agent = self
                .panes
                .iter()
                .find(|a| a.pane_id == pane_id && a.name.as_deref().unwrap_or("") == name)?;
            Some(mv_state::tasks::LiveConfirmation {
                ready: agent.status.is_some_and(AgentState::is_ready_for_input),
                state_change_seq: None,
                cwd: self.cwds.get(pane_id).cloned(),
                terminal_id: None,
            })
        }
    }

    fn live_ids(ids: &[&str]) -> Vec<LiveAgentStatus> {
        ids.iter()
            .map(|pane_id| LiveAgentStatus {
                pane_id: pane_id.to_string(),
                name: None,
                status: None,
            })
            .collect()
    }

    /// A lane directory `reclaim_lane_checks`'s own clean-lane gate will
    /// actually treat as clean: `lane_porcelain` runs `git status
    /// --porcelain` in it and fail-closes (excludes the candidate from
    /// `reclaimable_panes` entirely) on anything but a real, clean git
    /// checkout — a plain directory with no `.git` makes that query fail
    /// and the candidate silently vanish, which cost this file one
    /// confusing failure before landing on this helper.
    fn init_clean_lane(lane: &Path) -> std::path::PathBuf {
        std::fs::create_dir_all(lane).unwrap();
        assert!(std::process::Command::new("git")
            .current_dir(lane)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        lane.to_path_buf()
    }

    /// Removed on drop, including on panic (card `20261115-b-3e0fde`: the
    /// plain `PathBuf` this replaced only cleaned up on each test's own
    /// trailing `remove_dir_all` call, so an earlier assertion failure left
    /// it behind in `TMPDIR`). `Deref`s to `Path` so every existing call
    /// site keeps reading `&dir`/`dir.join(...)` unchanged.
    struct TempDir(std::path::PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-tasks-test-{tag}-{}-{}",
            std::process::id(),
            tag.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    /// A `--config` file registering one project named `name` at
    /// `project_dir`.
    fn write_config(dir: &Path, project_dir: &Path, name: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[projects.{name}]\npath = {:?}\n",
                project_dir.to_str().unwrap()
            ),
        )
        .unwrap();
        path
    }

    fn matches_cfg(words: &[&str], config_path: &Path) -> clap::ArgMatches {
        let mut full: Vec<&str> = words.to_vec();
        full.push("--config");
        let config_str = config_path.to_str().unwrap();
        full.push(config_str);
        matches(&full)
    }

    fn write_result(root: &Path, task: &str) {
        mv_state::log::ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=result\ttask={task}\tstatus=done\t\
                 state=merged\trevisions=0"
            ))
            .unwrap();
    }

    /// Serializes the tests below that must change the real process cwd —
    /// one to exercise `current_project_root()`'s raw ancestor walk
    /// end-to-end, another (`tasks_dot_lists_the_cwd_project`, card
    /// `20261117-b-486a55`) to exercise `.` resolving against cwd through
    /// `resolve_requested_projects`. No other test in this crate changes
    /// cwd or depends on an unmodified one (every other call into
    /// `current_project`/`current_project_root` either names its project
    /// explicitly, fails before project resolution, or pins an explicit
    /// `root` straight into `list`/`build_rows`), so the only hazard this
    /// guards against is two such tests racing each other. Restores the
    /// previous cwd on drop, even on panic, mirroring
    /// `worktrees::herdr_env`'s process-global lock for the same kind of
    /// ambient state.
    struct CwdGuard {
        previous: PathBuf,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl CwdGuard {
        fn set(dir: &Path) -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let lock = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.previous);
        }
    }

    #[test]
    fn bare_tasks_in_a_sandboxed_nested_lane_never_writes_the_real_project_it_sits_inside() {
        // Task 20261106-fixer-dabb0d: a dev lane's sandbox config is
        // nested *inside* a real project's checkout (the dev/use split),
        // and registers no project at the lane's own path. Before this
        // fix, `current_project(args)` failing for that reason — not
        // because the config failed to load — still fell through to
        // `current_project_root()`'s raw ancestor walk, which found the
        // real project's own `.multiverse/` one level up and handed it
        // straight to `list`'s auto-reconcile, with no config in hand to
        // say that root was never registered. `run_with` (not `run`) is
        // used so this reaches that real cwd-dependent path with a
        // `FakeAgents` instead of a real Herdr, the same seam every other
        // `--reconcile`-adjacent test here already uses.
        let dir = temp_dir("nested-sandbox");
        let real_root = dir.join("real-project");
        let lane_root = real_root.join(".multiverse").join("worktrees").join("lane");
        std::fs::create_dir_all(&lane_root).unwrap();

        // The real project has one dispatched, unrecorded task: a
        // terminal status file with no matching `event=result` yet —
        // exactly what an auto-reconcile pass would write an outcome for,
        // proving a write reached this root were one to occur.
        mv_state::log::ProjectLog::new(&real_root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-real01\t\
                 branch=\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        std::fs::create_dir_all(real_root.join(".multiverse/tasks/20261101-fixer-real01")).unwrap();
        std::fs::write(
            real_root
                .join(".multiverse/tasks/20261101-fixer-real01")
                .join("status"),
            "done\n",
        )
        .unwrap();
        // `is_tasks_reconcile_candidate` also requires a usable `result.md`
        // for a `done` outcome before it will record anything — without
        // this, auto-reconcile has nothing to write regardless of which
        // root it runs against, which would make this test pass for the
        // wrong reason.
        std::fs::write(
            real_root
                .join(".multiverse/tasks/20261101-fixer-real01")
                .join("result.md"),
            "# Result\ndone\n",
        )
        .unwrap();
        let real_log = real_root.join(".multiverse/routing.log");
        let before = std::fs::read_to_string(&real_log).unwrap();

        // The lane's own sandbox config registers a project elsewhere
        // entirely — never the lane's own path, never the real project's.
        let elsewhere = dir.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let config = write_config(&dir, &elsewhere, "elsewhere");

        let guard = CwdGuard::set(&lane_root);
        let args = matches_cfg(&["tasks"], &config);
        let result = run_with(&args, &mut FakeAgents::new(&[]));
        drop(guard);

        assert!(result.is_ok(), "{:?}", result.err());
        assert_eq!(
            std::fs::read_to_string(&real_log).unwrap(),
            before,
            "a sandboxed lane nested inside a real project must never auto-reconcile \
             (write to) that real project"
        );
    }

    // `--close-stranded`/`--reclaimable` are ported (card 20261102-b-fde23e)
    // to `mv_state::tasks::close_stranded`/`reclaimable_panes` — no longer a
    // blanket refusal, so there is no early, pre-project-resolution check
    // left to exercise safely through `run()` the way the old version of
    // this test did (`run()`'s own `HerdrAgents` and `current_project_root`'s
    // cwd walk are exactly what a test must never reach unmocked — see
    // `reconcile_task_without_the_flag_refuses_before_touching_any_project`'s
    // own comment for why). Their real behavior is covered by the
    // `list`-level tests below, which pin an explicit `root` and a
    // `FakeAgents`, the same seam every `--reconcile` test already uses.

    #[test]
    fn reconcile_task_without_the_flag_refuses_before_touching_any_project() {
        // Checked ahead of any project resolution (`run()`'s own early
        // `--task`/`--reconcile` validation) — safe to exercise with no
        // `--config` for the same reason the test above is: this must never
        // reach `current_project`/`current_project_root()`, which would
        // otherwise resolve to whatever real project happens to contain
        // this test binary's working directory.
        let args = matches(&["tasks", "--task", "20250101-fixer-aaaaaa"]);
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err
            .to_string()
            .contains("--task only makes sense with --reconcile"));
    }

    #[test]
    fn supersede_requires_supersedes_status_and_state() {
        let args = matches(&["tasks", "--supersede", "lane"]);
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err.to_string().contains("--supersede requires"));
    }

    #[test]
    fn acknowledge_requires_acknowledges() {
        let args = matches(&["tasks", "--acknowledge", "lane"]);
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err.to_string().contains("--acknowledge requires"));
    }

    #[test]
    fn supersede_refuses_more_than_one_named_project() {
        let args = matches(&[
            "tasks",
            "one",
            "two",
            "--supersede",
            "lane",
            "--supersedes",
            "1",
            "--status",
            "done",
            "--state",
            "settled",
            "--reason",
            "r",
        ]);
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err
            .to_string()
            .contains("corrects one record in one project; name exactly one (got 2: one, two)"));
    }

    #[test]
    fn supersede_dry_run_writes_nothing_and_yes_appends_once() {
        let dir = temp_dir("supersede-dry-run");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_result(&project, "20261101-fixer-aaaaaa");
        let config = write_config(&dir, &project, "sandbox");
        let log_path = project.join(".multiverse/routing.log");

        let before = std::fs::read_to_string(&log_path).unwrap();
        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--supersede",
                "20261101-fixer-aaaaaa",
                "--supersedes",
                "1",
                "--status",
                "blocked",
                "--state",
                "reconciled-by-merge",
                "--reason",
                "the merge never happened",
            ],
            &config,
        );
        run(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            before,
            "a dry run must not write"
        );

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--supersede",
                "20261101-fixer-aaaaaa",
                "--supersedes",
                "1",
                "--status",
                "blocked",
                "--state",
                "reconciled-by-merge",
                "--reason",
                "the merge never happened",
                "--yes",
            ],
            &config,
        );
        run(&args).unwrap();
        let after = std::fs::read_to_string(&log_path).unwrap();
        assert!(after.contains("event=supersede"));
        assert!(after.contains("supersedes=1"));

        // Recording the same correction again appends nothing further.
        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--supersede",
                "20261101-fixer-aaaaaa",
                "--supersedes",
                "1",
                "--status",
                "blocked",
                "--state",
                "reconciled-by-merge",
                "--reason",
                "the merge never happened",
                "--yes",
            ],
            &config,
        );
        run(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            after,
            "the same correction twice appends once"
        );
    }

    /// Card 20261122-b-40cb46: `--supersede --status done --yes` also
    /// rewrites the task's status file in the worktree it ran in — here an
    /// adopted `dispatch --into` round, whose own id has no worktree — and
    /// a dry run leaves that file alone.
    #[test]
    fn supersede_to_done_rewrites_the_lane_status_file() {
        let dir = temp_dir("supersede-rewrites-lane");
        let project = dir.join("project");
        let owner = "20261101-builder-aaaaaa";
        let adopted = "20261101-builder-bbbbbb";
        let status_path = project
            .join(".multiverse/worktrees")
            .join(owner)
            .join(".multiverse/tasks")
            .join(adopted)
            .join("status");
        std::fs::create_dir_all(status_path.parent().unwrap()).unwrap();
        std::fs::write(&status_path, "blocked\n").unwrap();
        let log = mv_state::log::ProjectLog::new(&project);
        log.append(&format!(
            "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={adopted}\trole=builder\t\
             branch=multiverse/{owner}"
        ))
        .unwrap();
        log.append(&format!(
            "2026-11-01T00:10:00+00:00\tevent=result\ttask={adopted}\tstatus=blocked\t\
             state=reconciled\trevisions=0"
        ))
        .unwrap();
        let config = write_config(&dir, &project, "sandbox");
        let words = |yes: bool| {
            let mut words = vec![
                "tasks",
                "sandbox",
                "--supersede",
                adopted,
                "--supersedes",
                "2",
                "--status",
                "done",
                "--state",
                "verified",
                "--reason",
                "the nexus re-ran the checks",
            ];
            if yes {
                words.push("--yes");
            }
            matches_cfg(&words, &config)
        };

        run(&words(false)).unwrap();
        assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "blocked\n");

        run(&words(true)).unwrap();
        assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "done\n");
    }

    #[test]
    fn supersede_refuses_an_unknown_ordinal() {
        let dir = temp_dir("supersede-unknown");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_result(&project, "20261101-fixer-bbbbbb");
        let config = write_config(&dir, &project, "sandbox");

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--supersede",
                "20261101-fixer-bbbbbb",
                "--supersedes",
                "99",
                "--status",
                "blocked",
                "--state",
                "reconciled-by-merge",
                "--reason",
                "r",
                "--yes",
            ],
            &config,
        );
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 1);
        assert!(err.to_string().contains("has no outcome record at line 99"));
    }

    #[test]
    fn acknowledge_dry_run_writes_nothing_and_yes_appends_once() {
        let dir = temp_dir("acknowledge-dry-run");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_result(&project, "20261101-fixer-cccccc");
        let config = write_config(&dir, &project, "sandbox");
        let log_path = project.join(".multiverse/routing.log");

        let before = std::fs::read_to_string(&log_path).unwrap();
        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--acknowledge",
                "20261101-fixer-cccccc",
                "--acknowledges",
                "1",
            ],
            &config,
        );
        run(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            before,
            "a dry run must not write"
        );

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--acknowledge",
                "20261101-fixer-cccccc",
                "--acknowledges",
                "1",
                "--yes",
            ],
            &config,
        );
        run(&args).unwrap();
        let after = std::fs::read_to_string(&log_path).unwrap();
        assert!(after.contains("event=acknowledged"));

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--acknowledge",
                "20261101-fixer-cccccc",
                "--acknowledges",
                "1",
                "--yes",
            ],
            &config,
        );
        run(&args).unwrap();
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            after,
            "acknowledging twice appends once"
        );
    }

    #[test]
    fn supersede_rejects_a_tab_in_state_before_writing_anything() {
        let dir = temp_dir("supersede-field-injection");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_result(&project, "20261101-fixer-dddddd");
        let config = write_config(&dir, &project, "sandbox");
        let log_path = project.join(".multiverse/routing.log");
        let before = std::fs::read_to_string(&log_path).unwrap();

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--supersede",
                "20261101-fixer-dddddd",
                "--supersedes",
                "1",
                "--status",
                "blocked",
                "--state",
                "a\tstatus=X",
                "--reason",
                "r",
                "--yes",
            ],
            &config,
        );
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 2);
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            before,
            "a --state value that could smuggle a field must write nothing"
        );
    }

    #[test]
    fn acknowledge_rejects_a_tab_in_task_before_writing_anything() {
        let dir = temp_dir("acknowledge-field-injection");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        write_result(&project, "20261101-fixer-eeeeee");
        let config = write_config(&dir, &project, "sandbox");
        let log_path = project.join(".multiverse/routing.log");
        let before = std::fs::read_to_string(&log_path).unwrap();

        let args = matches_cfg(
            &[
                "tasks",
                "sandbox",
                "--acknowledge",
                "20261101-fixer-eeeeee\tstatus=X",
                "--acknowledges",
                "1",
                "--yes",
            ],
            &config,
        );
        let err = run(&args).unwrap_err();
        assert_eq!(err.code(), 2);
        assert_eq!(
            std::fs::read_to_string(&log_path).unwrap(),
            before,
            "a --acknowledge value that could smuggle a field must write nothing"
        );
    }

    #[test]
    fn lists_task_directories_by_status_absence_means_running() {
        let root = temp_dir("list");
        let tasks_dir = root.join(".multiverse/tasks");
        // The listing's task universe comes from `routing.log` (mirroring
        // Python's `read_log`), not from `.multiverse/tasks` directly — so
        // every task here needs its own `dispatch` line, matching what a
        // real dispatch always writes before anything else.
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-abcdef\t\
                 branch=\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:01+00:00\tevent=dispatch\ttask=20261101-fixer-123456\t\
                 branch=\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        std::fs::create_dir_all(tasks_dir.join("20261101-fixer-abcdef")).unwrap();
        std::fs::write(
            tasks_dir.join("20261101-fixer-abcdef").join("status"),
            "done\n",
        )
        .unwrap();
        std::fs::create_dir_all(tasks_dir.join("20261101-fixer-123456")).unwrap();

        let args = matches(&["tasks"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let args = matches(&["tasks", "--status", "running"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let args = matches(&["tasks", "--task", "20261101-fixer-abcdef"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
    }

    #[test]
    fn a_loose_file_and_a_pre_log_directory_never_become_rows() {
        // Card 20261102-b-1b2d80: a large real project
        // had 34 stray `<id>-result.md` files sitting directly under
        // `.multiverse/tasks/` rather than inside their own `<id>/`
        // directory, and another real project had task
        // directories whose dispatch predated the routing log's earliest
        // surviving entry. A directory scan picked up both as if they were
        // real tasks — the generic task-id pattern a loose file's own name
        // can satisfy, and a directory on disk existing independently of
        // any log entry. Neither should ever become a row once the task
        // universe is sourced from the log instead (`build_rows`'s own doc
        // comment).
        let root = temp_dir("list-phantom-tasks");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-real01\t\
                 branch=\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        // The loose file: same directory level as a real task dir, never
        // its own `<id>/status` layout.
        write(
            &root.join(".multiverse/tasks/20260101-fixer-ffffff-result.md"),
            "leftover content from some earlier mistake\n",
        );
        // The pre-log directory: a real `<id>/status` layout, but no
        // routing-log entry of its own at all.
        write(
            &root.join(".multiverse/tasks/20250101-explorer-999999/status"),
            "done",
        );

        let args = matches(&["tasks"]);
        let (rows, needs_look) = build_rows(&root, &args, Some(&[]), None);
        let ids: Vec<&str> = rows.iter().map(|row| row.task_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["20261101-fixer-real01"],
            "only the logged task may ever become a row"
        );
        assert_eq!(needs_look, 1, "the one real task is still awaiting/flagged");
    }

    #[test]
    fn a_missing_task_directory_is_an_empty_list_not_an_error() {
        let root = temp_dir("missing");
        let args = matches(&["tasks"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
    }

    #[test]
    fn open_backlog_is_listed_when_there_are_no_dispatched_tasks() {
        // Python `cmd_tasks` (`cli.py:7662-7680`) prints an `open backlog:`
        // section; Rust used to print `no tasks found` and hide the cards.
        // Found via `conformance/diff_fuzz.py` seed 9000.
        let root = temp_dir("open-backlog");
        mv_state::backlog::add(
            &root,
            "fix the thing",
            "sandbox",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let items = mv_state::backlog::read_items(&root, true);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "fix the thing");
        assert_eq!(items[0].priority, "P2");
        let args = matches(&["tasks"]);
        let (rows, _) = build_rows(&root, &args, Some(&[]), None);
        assert!(rows.is_empty(), "backlog cards are not dispatched tasks");
    }

    #[test]
    fn open_backlog_ties_break_by_creation_order_not_item_id() {
        // Python's tie-break for two open items sharing a priority and a
        // second-granularity `created_at` is `sorted()`'s own stability,
        // i.e. creation/log order. A previous Rust version added the
        // item id as an explicit third sort key, which tie-broke by the
        // id's random-looking hex suffix instead -- unrelated to creation
        // order, and observed to disagree with Python at seed 12000.
        use time::macros::datetime;

        let root = temp_dir("open-backlog-tie-break");
        let when = datetime!(2026-11-03 12:00:00 UTC);
        mv_state::backlog::add(
            &root,
            "first created",
            "sandbox",
            "",
            None,
            None,
            None,
            "P2",
            Some(when),
        )
        .unwrap();
        mv_state::backlog::add(
            &root,
            "second created",
            "sandbox",
            "",
            None,
            None,
            None,
            "P2",
            Some(when),
        )
        .unwrap();

        let mut items = mv_state::backlog::read_items(&root, true);
        assert_eq!(
            items[0].created_at, items[1].created_at,
            "ties require equal timestamps"
        );
        sort_open_backlog(&mut items);
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["first created", "second created"],
        );
    }

    #[test]
    fn open_backlog_ties_break_by_creation_order_even_when_item_ids_sort_the_other_way() {
        // The test above builds its two items through `backlog::add`,
        // whose random-hex `item` id makes it a coin flip against the bug
        // this guards: the old third sort key (`item` id) agreed with
        // creation order about as often as it disagreed. Pin the ids
        // explicitly, in the opposite order from creation, so a
        // reintroduced `item`-id tie-break fails this test every run, not
        // only at seed 12000.
        fn item(id: &str, title: &str) -> mv_state::backlog::BacklogItem {
            mv_state::backlog::BacklogItem {
                item: id.to_string(),
                state: "open".to_string(),
                priority: "P2".to_string(),
                title: title.to_string(),
                why: String::new(),
                due: String::new(),
                role: String::new(),
                source: String::new(),
                by: String::new(),
                evidence: String::new(),
                created_at: "2026-11-03T12:00:00+00:00".to_string(),
                updated_at: "2026-11-03T12:00:00+00:00".to_string(),
                annotations: Vec::new(),
                options: Vec::new(),
                recommendation: None,
                task_id: String::new(),
            }
        }

        let mut items = vec![
            item("zzzzzz", "first created"),
            item("aaaaaa", "second created"),
        ];
        sort_open_backlog(&mut items);
        assert_eq!(
            items.iter().map(|i| i.title.as_str()).collect::<Vec<_>>(),
            vec!["first created", "second created"],
            "creation order must win the tie even when the item id sorts the other way"
        );
    }

    fn write_dispatch_log(root: &Path, task: &str) {
        mv_state::log::ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={task}\tbranch=\trole=explorer"
            ))
            .unwrap();
    }

    #[test]
    fn reconcile_records_a_settled_unrecorded_task_and_reports_the_count() {
        // Was refused outright before this task ported `--reconcile`
        // (card 20261102-b-fde23e); now it should actually write the
        // missing outcome and say so, mirroring `cmd_tasks`'s own
        // `recorded N outcome(s) that were finished but unlogged` line.
        let root = temp_dir("reconcile-settled");
        write_dispatch_log(&root, "20261101-explorer-aaaaaa");
        std::fs::create_dir_all(root.join(".multiverse/tasks/20261101-explorer-aaaaaa")).unwrap();
        std::fs::write(
            root.join(".multiverse/tasks/20261101-explorer-aaaaaa/status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            root.join(".multiverse/tasks/20261101-explorer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        )
        .unwrap();

        let args = matches(&["tasks", "--reconcile"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains(
                "event=result\ttask=20261101-explorer-aaaaaa\tstatus=done\tstate=reconciled"
            ),
            "the settled task's outcome must actually be recorded: {log:?}"
        );
    }

    #[test]
    fn reconcile_records_a_done_task_with_no_result_md_and_it_stops_needing_a_look() {
        // Scout `20261117-scout-9c0832` rank 2: before this fix,
        // `is_tasks_reconcile_candidate` silently refused a `done` status
        // with no usable `result.md`, so the row stayed `unrecorded`/
        // flagged "* need a look" forever — even though `classify`'s own
        // note claimed `--reconcile` would fix it. No command could ever
        // clear it short of hand-editing the task directory.
        let root = temp_dir("reconcile-done-without-result");
        write_dispatch_log(&root, "20261101-explorer-ffffff");
        std::fs::create_dir_all(root.join(".multiverse/tasks/20261101-explorer-ffffff")).unwrap();
        std::fs::write(
            root.join(".multiverse/tasks/20261101-explorer-ffffff/status"),
            "done",
        )
        .unwrap();
        // Deliberately no result.md at all.

        let before = matches(&["tasks"]);
        let (_, needs_look_before) = build_rows(&root, &before, Some(&[]), None);
        assert_eq!(
            needs_look_before, 1,
            "an unrecorded done task with no result.md starts out flagged"
        );

        let args = matches(&["tasks", "--reconcile"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains(
                "event=result\ttask=20261101-explorer-ffffff\tstatus=done\tstate=done-without-result"
            ),
            "the done-but-no-result task must be recorded, carrying that annotation: {log:?}"
        );

        let after = matches(&["tasks"]);
        let (_, needs_look_after) = build_rows(&root, &after, Some(&[]), None);
        assert_eq!(
            needs_look_after, 0,
            "once --reconcile has recorded it, the task must no longer need a look"
        );
    }

    #[test]
    fn reconcile_with_read_only_never_writes() {
        let root = temp_dir("reconcile-read-only");
        write_dispatch_log(&root, "20261101-explorer-bbbbbb");
        std::fs::create_dir_all(root.join(".multiverse/tasks/20261101-explorer-bbbbbb")).unwrap();
        std::fs::write(
            root.join(".multiverse/tasks/20261101-explorer-bbbbbb/status"),
            "done",
        )
        .unwrap();
        let before = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();

        let args = matches(&["tasks", "--reconcile", "--read-only"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let after = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(before, after, "--read-only must never record an outcome");
    }

    #[test]
    fn reconcile_with_an_unknown_task_reports_not_found() {
        let root = temp_dir("reconcile-task-not-found");
        write_dispatch_log(&root, "20261101-explorer-cccccc");

        let args = matches(&["tasks", "--reconcile", "--task", "20261101-explorer-zzzzzz"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
        // Nothing to assert on stdout here without capturing it; the real
        // assertion is that this does not panic and records nothing.
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("state=reconciled"));
    }

    #[test]
    fn reconcile_with_task_filters_to_only_that_task() {
        let root = temp_dir("reconcile-task-filter");
        write_dispatch_log(&root, "20261101-explorer-dddddd");
        write_dispatch_log(&root, "20261101-explorer-eeeeee");
        for task in ["20261101-explorer-dddddd", "20261101-explorer-eeeeee"] {
            std::fs::create_dir_all(root.join(".multiverse/tasks").join(task)).unwrap();
            std::fs::write(
                root.join(".multiverse/tasks").join(task).join("status"),
                "done",
            )
            .unwrap();
            std::fs::write(
                root.join(".multiverse/tasks").join(task).join("result.md"),
                "# Result\ndone.\n",
            )
            .unwrap();
        }

        let args = matches(&["tasks", "--reconcile", "--task", "20261101-explorer-dddddd"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains("task=20261101-explorer-dddddd\tstatus=done\tstate=reconciled"));
        assert!(!log.contains("task=20261101-explorer-eeeeee\tstatus=done\tstate=reconciled"));
    }

    #[test]
    fn close_stranded_records_an_awaiting_task_and_leaves_a_running_one_alone() {
        // Card 20261102-b-fde23e: was refused outright before this task
        // ported `--close-stranded`.
        let root = temp_dir("close-stranded-cli");
        write_dispatch_log(&root, "20261101-explorer-aaaaaa");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-bbbbbb\t\
                 branch=multiverse/20261101-fixer-bbbbbb\trole=fixer",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=started\ttask=20261101-fixer-bbbbbb\tpane=w1:p1",
            )
            .unwrap();

        let args = matches(&["tasks", "--close-stranded", "--yes"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&["w1:p1"]), None).is_ok());

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains(
            "event=result\ttask=20261101-explorer-aaaaaa\tstatus=failed\tstate=no-branch-expected"
        ));
        assert!(
            !log.contains("task=20261101-fixer-bbbbbb\tstatus="),
            "a task with a live agent is RUNNING, not stranded, and must never be closed"
        );
    }

    #[test]
    fn close_stranded_dry_run_writes_nothing() {
        let root = temp_dir("close-stranded-dry-run");
        write_dispatch_log(&root, "20261101-explorer-cccccc");
        let before = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();

        let args = matches(&["tasks", "--close-stranded"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let after = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(before, after, "a dry run must never write");
    }

    #[test]
    fn close_stranded_with_read_only_never_writes() {
        let root = temp_dir("close-stranded-read-only");
        write_dispatch_log(&root, "20261101-explorer-gggggg");
        let before = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();

        let args = matches(&["tasks", "--close-stranded", "--yes", "--read-only"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());

        let after = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(before, after, "--read-only must never record an outcome");
    }

    #[test]
    fn reclaimable_closes_a_live_recorded_clean_pane_with_yes() {
        let root = temp_dir("reclaimable-cli");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-dddddd\t\
                 branch=main\trole=fixer",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261101-fixer-dddddd\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1",
            )
            .unwrap();

        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_label("w1:p1", "fixer · 20261101-fixer-dddddd");
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert!(
            agents.list().is_empty(),
            "a live, recorded, clean-lane pane must be closed"
        );
    }

    #[test]
    fn reclaimable_dry_run_closes_nothing() {
        let root = temp_dir("reclaimable-dry-run");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-eeeeee\t\
                 branch=main\trole=fixer",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261101-fixer-eeeeee\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1",
            )
            .unwrap();

        let args = matches(&["tasks", "--reclaimable"]);
        let mut agents = FakeAgents::new(&["w1:p1"]);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert_eq!(
            agents.list(),
            vec![mv_state::tasks::LiveAgent {
                pane_id: "w1:p1".to_string()
            }],
            "a dry run must never close a pane"
        );
    }

    /// Card `20261115-b-9815fe`: `--reclaimable --reap` without `--yes` is
    /// "would-reap" only — the pane preview is "would close" (unchanged),
    /// and the same candidate's lane process is only ever previewed, never
    /// signaled.
    #[test]
    fn reclaimable_reap_without_yes_previews_and_signals_nothing() {
        let root = temp_dir("reclaimable-reap-dry");
        let task_id = "20261101-fixer-ffffff";
        mv_state::log::ProjectLog::new(&root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
                 branch=multiverse/{task_id}\trole=fixer"
            ))
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(&format!(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask={task_id}\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1"
            ))
            .unwrap();
        // `reclaim_lane_checks`'s own clean-lane gate treats a still-live
        // pane with no terminal status file as a live occupant it must
        // protect (`lane_clean_for_reclaim`) — a real dispatched task
        // always has this file too, so write it here for the same reason
        // `init_clean_lane` exists: without it the candidate silently
        // never reaches `reclaimable_panes` at all.
        write(
            &root.join(".multiverse/tasks").join(task_id).join("status"),
            "done\n",
        );
        let lane = init_clean_lane(&root.join(".multiverse/worktrees").join(task_id));
        let mut child = LaneChild::sleep_in(&lane);

        let args = matches(&["tasks", "--reclaimable", "--reap"]);
        let mut agents = FakeAgents::new(&["w1:p1"]);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert_eq!(
            agents.list(),
            vec![mv_state::tasks::LiveAgent {
                pane_id: "w1:p1".to_string()
            }],
            "--reap without --yes must never close the pane"
        );
        assert!(
            child.not_signalled(),
            "--reap without --yes must never signal the lane process"
        );
    }

    /// Proof item: after a successful close, `--reap` ends that lane's own
    /// lingering process (SIGTERM, re-checked identity, SIGKILL).
    #[test]
    fn reclaimable_reap_with_yes_ends_the_lane_process_once_the_pane_closes() {
        let root = temp_dir("reclaimable-reap-yes");
        let task_id = "20261101-fixer-gggggg";
        mv_state::log::ProjectLog::new(&root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
                 branch=multiverse/{task_id}\trole=fixer"
            ))
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(&format!(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask={task_id}\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1"
            ))
            .unwrap();
        write(
            &root.join(".multiverse/tasks").join(task_id).join("status"),
            "done\n",
        );
        let lane = init_clean_lane(&root.join(".multiverse/worktrees").join(task_id));
        let mut child = LaneChild::sleep_in(&lane);

        let args = matches(&["tasks", "--reclaimable", "--yes", "--reap"]);
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_label("w1:p1", &format!("fixer · {task_id}"))
            .with_cwd("w1:p1", lane.clone());
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert!(
            agents.list().is_empty(),
            "a live, recorded, clean-lane pane must still be closed"
        );
        assert!(
            child.signalled_within(30).is_some(),
            "--reap --yes must signal the lane process"
        );
    }

    /// Whether this host's process cwd probe can see `pid` with its cwd in
    /// `lane` — `false`, after printing why, where it cannot (a sandbox
    /// hiding `lsof`/`/proc`), so a test needing it skips rather than fails.
    fn probe_sees(lane: &Path, pid: u32) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let seen = mv_engine::dockerreport::process_cwds().is_some_and(|processes| {
                mv_engine::dockerreport::processes_under(lane, &processes)
                    .iter()
                    .any(|process| process.pid == pid)
            });
            if seen {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("skipped: this host's process cwd probe cannot see spawned pid {pid}");
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    fn recorded_done_lane(root: &Path, task_id: &str) -> PathBuf {
        mv_state::log::ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
                 branch=multiverse/{task_id}\trole=fixer"
            ))
            .unwrap();
        mv_state::log::ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask={task_id}\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1"
            ))
            .unwrap();
        write(
            &root.join(".multiverse/tasks").join(task_id).join("status"),
            "done\n",
        );
        init_clean_lane(&root.join(".multiverse/worktrees").join(task_id))
    }

    /// Card `20261122-b-78bcf9`: `--reclaimable --yes` with no `--reap`
    /// still ends the lane's leftover once it has closed that lane's pane
    /// — the `tail -f` a Claude lane leaves behind used to outlive the
    /// close and block `--merge`.
    #[test]
    fn reclaimable_yes_without_reap_ends_the_lane_leftover_after_the_close() {
        let root = temp_dir("reclaimable-yes-leftover");
        let task_id = "20261101-fixer-hhhhhh";
        let lane = recorded_done_lane(&root, task_id);
        let mut child = LaneChild::sleep_in(&lane);
        if !probe_sees(&lane, child.id()) {
            return;
        }

        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_label("w1:p1", &format!("fixer · {task_id}"))
            .with_cwd("w1:p1", lane.clone());
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert!(agents.list().is_empty(), "the pane must still be closed");
        assert!(
            child.signalled_within(30).is_some(),
            "--reclaimable --yes must end the closed lane's leftover"
        );
    }

    /// Card `20261122-b-78bcf9`: without `--yes` nothing closes and the
    /// leftover is only ever previewed, `--reap` or not.
    #[test]
    fn reclaimable_without_yes_never_signals_a_leftover() {
        let root = temp_dir("reclaimable-no-yes-leftover");
        let task_id = "20261101-fixer-iiiiii";
        let lane = recorded_done_lane(&root, task_id);
        let mut child = LaneChild::sleep_in(&lane);

        let args = matches(&["tasks", "--reclaimable"]);
        let mut agents = FakeAgents::new(&["w1:p1"]);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert!(
            child.not_signalled(),
            "a preview must never signal the lane's leftover"
        );
    }

    /// Card `20261122-b-78bcf9`: a recorded lane whose pane is already gone
    /// lands in "stale bookkeeping", which `--reap` used to skip entirely;
    /// `--reap --yes` now ends its leftover there too.
    #[test]
    fn reclaimable_reap_yes_ends_a_stale_lane_leftover() {
        let root = temp_dir("reclaimable-stale-leftover");
        let task_id = "20261101-fixer-jjjjjj";
        let lane = recorded_done_lane(&root, task_id);
        let mut child = LaneChild::sleep_in(&lane);
        if !probe_sees(&lane, child.id()) {
            return;
        }

        let args = matches(&["tasks", "--reclaimable", "--reap", "--yes"]);
        let mut agents = FakeAgents::new(&["w9:p9"]);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert!(
            child.signalled_within(30).is_some(),
            "--reap --yes must end a stale lane's leftover"
        );
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn minimal_config(projects_toml: &str) -> String {
        format!(
            "[accounts.seat]\n\
             kind = \"codex\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:test\"\n\n\
             {projects_toml}\n\
             [roles.scout]\n\
             write_scope = \"none\"\n\
             effort = \"low\"\n\
             on_exhausted = \"queue\"\n\
             skills = []\n\n\
             [roles.scout.models]\n\
             codex = \"gpt-test\"\n"
        )
    }

    /// Card `20261102-b-d10803`: `config.projects` is a `BTreeMap`, so
    /// iterating it directly would print `alpha` before `zeta` regardless
    /// of which one `config.toml` declared first — `projects_in_
    /// declaration_order` must return the file's own order instead.
    #[test]
    fn projects_in_declaration_order_follows_the_config_file_not_the_alphabet() {
        let base = temp_dir("declaration-order");
        let zeta = base.join("zeta");
        let alpha = base.join("alpha");
        std::fs::create_dir_all(&zeta).unwrap();
        std::fs::create_dir_all(&alpha).unwrap();

        let config_path = base.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.zeta]\n\
                 path = \"{}\"\n\n\
                 [projects.zeta.chains]\n\
                 scout = [\"seat\"]\n\n\
                 [projects.alpha]\n\
                 path = \"{}\"\n\n\
                 [projects.alpha.chains]\n\
                 scout = [\"seat\"]\n\n",
                zeta.display(),
                alpha.display()
            )),
        );

        let args = matches(&["tasks", "--config", config_path.to_str().unwrap()]);
        let config = super::super::load_config(&args).unwrap();
        let names: Vec<&str> = projects_in_declaration_order(&config)
            .into_iter()
            .map(|(name, _)| name)
            .collect();
        assert_eq!(names, vec!["zeta", "alpha"]);
    }

    /// The fallback `run` takes when cwd resolves to no project at all —
    /// not a configured project's path, and no `.multiverse/` ancestor
    /// either, which is exactly how Herdr invokes a global-context action
    /// (card 20261102-b-2691da). Every configured project must still show
    /// up, not just the one `cwd` happens to be nearest to.
    #[test]
    fn list_fleet_covers_every_configured_project() {
        let base = temp_dir("fleet-covers-every-project");
        let alpha = base.join("alpha");
        let beta = base.join("beta");
        write(
            &alpha
                .join(".multiverse/tasks/20261101-fixer-aaaaaa")
                .join("status"),
            "done\n",
        );
        std::fs::create_dir_all(&beta).unwrap();

        let config_path = base.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.alpha]\n\
                 path = \"{}\"\n\n\
                 [projects.alpha.chains]\n\
                 scout = [\"seat\"]\n\n\
                 [projects.beta]\n\
                 path = \"{}\"\n\n\
                 [projects.beta.chains]\n\
                 scout = [\"seat\"]\n\n",
                alpha.display(),
                beta.display()
            )),
        );

        let args = matches(&["tasks", "--config", config_path.to_str().unwrap()]);
        assert!(list_fleet(&args, &mut FakeAgents::new(&[])).is_ok());
    }

    /// Card 20261102-b-f507d9: `run` had no hub-widens-to-fleet rule, so a
    /// bare `tasks`/`tasks --reconcile` run from inside the hub project's
    /// own directory only scoped to that one project. `list_hub_widened`
    /// is the silent (no-preamble) widening `run` falls back to for that
    /// case instead of `list`-ing just the hub; this pins that it covers
    /// every configured project the same way `list_fleet` does.
    #[test]
    fn list_hub_widened_covers_every_configured_project() {
        let base = temp_dir("hub-widened-covers-every-project");
        let alpha = base.join("alpha");
        let beta = base.join("beta");
        write(
            &alpha
                .join(".multiverse/tasks/20261101-fixer-aaaaaa")
                .join("status"),
            "done\n",
        );
        std::fs::create_dir_all(&beta).unwrap();

        let config_path = base.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.alpha]\n\
                 path = \"{}\"\n\n\
                 [projects.alpha.chains]\n\
                 scout = [\"seat\"]\n\n\
                 [projects.beta]\n\
                 path = \"{}\"\n\n\
                 [projects.beta.chains]\n\
                 scout = [\"seat\"]\n\n",
                alpha.display(),
                beta.display()
            )),
        );

        let args = matches(&["tasks", "--config", config_path.to_str().unwrap()]);
        assert!(list_hub_widened(&args, &mut FakeAgents::new(&[])).is_ok());
    }

    #[test]
    fn list_fleet_reports_an_unfilled_project_path_instead_of_erroring() {
        let base = temp_dir("fleet-unfilled-path");
        let config_path = base.join("config.toml");
        write(
            &config_path,
            &minimal_config(
                "[projects.alpha]\n\
                 path = \"<<PLACEHOLDER: path>>\"\n\n",
            ),
        );

        let args = matches(&["tasks", "--config", config_path.to_str().unwrap()]);
        assert!(list_fleet(&args, &mut FakeAgents::new(&[])).is_ok());
    }

    #[test]
    fn list_fleet_on_an_empty_fleet_is_a_config_error_not_a_panic() {
        let base = temp_dir("fleet-no-projects");
        let config_path = base.join("config.toml");
        write(&config_path, &minimal_config(""));

        let args = matches(&["tasks", "--config", config_path.to_str().unwrap()]);
        let err = list_fleet(&args, &mut FakeAgents::new(&[])).unwrap_err();
        assert!(err.to_string().contains("no projects configured"));
    }

    #[test]
    fn a_result_event_is_recorded_with_its_routing_log_line() {
        let root = temp_dir("recorded");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=fixer\taccount=old-seat\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        let (events, _order) = gather_task_events(&root);
        let (state, suffix, flagged, note) = classify(
            events.get("20250101-fixer-aaaaaa"),
            Some(TaskStatus::Done),
            None,
            Some(&[]),
        );
        assert_eq!(state, "recorded");
        assert_eq!(suffix, "done #3");
        assert!(!flagged);
        assert!(note.is_none());
    }

    /// Card `20261118-b-c328a3` M2: the same crash window
    /// `close_stranded_leaves_a_live_pane_alone_when_only_event_pane_was_recorded`
    /// (`mv_state::tasks`) covers for `close_stranded` — a kill between
    /// pane creation and `agent start` leaves only `event=pane`, no
    /// `event=started` — must also classify `running`, not `awaiting`, in
    /// this listing's own `gather_task_events`/`classify` pipeline.
    /// Revert check: drop the `"pane"` arm in `gather_task_events` (or its
    /// fallback in `classify`) and this fails (`awaiting`, not `running`).
    #[test]
    fn a_pane_only_dispatch_classifies_running_not_awaiting() {
        let root = temp_dir("pane-only-classifies-running");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-paneonl\t\
             branch=multiverse/20260101-fixer-paneonl\trole=fixer\tagent=agent-a\n\
             2026-01-01T00:00:30+00:00\tevent=pane\ttask=20260101-fixer-paneonl\tpane=w1:p1\n",
        );
        let (events, _order) = gather_task_events(&root);
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: Some("agent-a".into()),
            status: Some(AgentState::Working),
        }];
        let (state, _suffix, flagged, note) = classify(
            events.get("20260101-fixer-paneonl"),
            None,
            None,
            Some(&live),
        );
        assert_eq!(state, "running");
        assert!(flagged);
        assert!(note.is_none());
    }

    #[test]
    fn a_supersede_event_overrides_the_result_it_corrects_status_and_line() {
        // Card 20261102-b-28afef: `event=supersede` fell into the `_ => {}`
        // catch-all, so a closer's correction of a wrong `result` record
        // (e.g. `failed` corrected to `done`) never reached this listing —
        // it kept showing the pre-correction status and routing-log line.
        let root = temp_dir("superseded");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=fixer\taccount=old-seat\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=failed\n\
             2020-01-02T00:00:00+00:00\tevent=supersede\ttask=20250101-fixer-aaaaaa\tsupersedes=3\tstatus=done\tstate=reconciled-by-merge\treason=lane merged\n",
        );
        let (events, _order) = gather_task_events(&root);
        let (state, suffix, flagged, note) = classify(
            events.get("20250101-fixer-aaaaaa"),
            Some(TaskStatus::Done),
            None,
            Some(&[]),
        );
        assert_eq!(state, "recorded");
        assert_eq!(suffix, "done #4");
        assert!(!flagged);
        assert!(note.is_none());
    }

    #[test]
    fn a_redispatch_reusing_a_task_id_does_not_inherit_the_earlier_rounds_pane_or_outcome() {
        // Card 20261102-b-a5782d: a new `event=dispatch` for a reused task
        // id used to leave the previous round's `started`/`result` fields
        // in place (only `role`/`account` got overwritten), so a second
        // round still live would wrongly show as `recorded`/`unrecorded`
        // off the first round's own result line.
        let root = temp_dir("redispatch-reset");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\trole=fixer\taccount=old-seat\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-fixer-aaaaaa\tpane=w1:p1\n\
             2026-01-01T00:02:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\n\
             2026-01-02T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\trole=fixer\taccount=new-seat\n\
             2026-01-02T00:01:00+00:00\tevent=started\ttask=20260101-fixer-aaaaaa\tpane=w2:p2\n",
        );
        let (events, _order) = gather_task_events(&root);
        let entry = events.get("20260101-fixer-aaaaaa").unwrap();
        assert_eq!(entry.account, "new-seat");
        assert_eq!(entry.started_pane.as_deref(), Some("w2:p2"));
        assert!(
            entry.result_status.is_none(),
            "the new round has no result of its own yet, and must not keep the old round's"
        );
    }

    #[test]
    fn a_running_row_is_marked_and_names_its_dispatch_age() {
        let root = temp_dir("running-note");
        let stamp = mv_core::time::format_seconds_utc(
            mv_core::time::now_utc() - time::Duration::minutes(2),
        );
        write(&root.join(".multiverse/routing.log"), &format!(
            "{stamp}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=builder\n{stamp}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n"));
        let args = matches(&["tasks"]);
        let live = live_ids(&["w1:p1"]);
        let (rows, count) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(count, 1);
        assert!(rows[0].flagged);
        assert_eq!(rows[0].note.as_deref(), Some("dispatched 2 min ago, still running as '' — and still spending. Nothing here will stop it."));
    }

    /// Frozen-pane rework (Scout 342332 finding 4): a healthy *working*
    /// agent is never "possibly frozen" on age alone, however long it
    /// runs — the bug the original, age-only version of this change had
    /// (every healthy multi-hour Builder lane flagged). Four hours old,
    /// still `working`: plain RUNNING note, unchanged.
    #[test]
    fn a_working_row_four_hours_old_is_not_flagged_possibly_frozen() {
        let root = temp_dir("running-working");
        let stamp =
            mv_core::time::format_seconds_utc(mv_core::time::now_utc() - time::Duration::hours(4));
        write(&root.join(".multiverse/routing.log"), &format!(
            "{stamp}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=builder\tagent=sb-buil-cx\n{stamp}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n"));
        let args = matches(&["tasks"]);
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Working),
        }];
        let (rows, count) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(rows[0].state, "running");
        assert_eq!(count, 1);
        assert_eq!(
            rows[0].note.as_deref(),
            Some(
                "dispatched 4 hr ago, still running as 'sb-buil-cx' — and still spending. \
                 Nothing here will stop it."
            )
        );
    }

    /// Signal (a) of the frozen-pane rework: Herdr reports the agent
    /// idle/done (ready for input, not working) while no terminal status
    /// file exists — the host-sleep shape seen on 2026-11-17 (idle
    /// prompt, no status, hours old). Past the shared stall threshold,
    /// this is "possibly frozen", naming the signal. Revert check: drop
    /// the `is_ready_for_input()` gate above and keep only the elapsed
    /// check, and this test still passes — the gate must be checked
    /// against `a_working_row_four_hours_old_is_not_flagged_possibly_frozen`
    /// instead, which would then fail.
    #[test]
    fn an_idle_row_four_hours_old_is_marked_possibly_frozen() {
        let root = temp_dir("stalled-idle-frozen");
        let stamp =
            mv_core::time::format_seconds_utc(mv_core::time::now_utc() - time::Duration::hours(4));
        write(&root.join(".multiverse/routing.log"), &format!(
            "{stamp}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=builder\tagent=sb-buil-cx\n{stamp}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n"));
        let args = matches(&["tasks"]);
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Idle),
        }];
        let (rows, count) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(rows[0].state, "stalled");
        assert_eq!(count, 1);
        assert!(rows[0].flagged);
        assert_eq!(
            rows[0].note.as_deref(),
            Some(
                "possibly frozen: agent reports idle with no status file for 4 hr \u{2014} not \
                 acted on; try `herdr agent prompt sb-buil-cx` or `dispatch --into \
                 20250101-fixer-aaaaaa`."
            )
        );
    }

    /// Card `20261118-b-9226e1`: the same frozen-pane note, but for a task
    /// killed between pane creation and `agent start` — `routing.log`
    /// holds only `event=pane`, never `event=started`, so `started_pane`
    /// is `None` and the note builder must fall back to
    /// `TaskEvents::effective_pane` the same way `classify` already does.
    /// Revert check: read `events.started_pane.as_deref()` directly in the
    /// "stalled" branch's live-status lookup instead of
    /// `events.effective_pane()`, and this fails — no live agent ever
    /// matches `None`, so the note stays the plain `classify` stalled note
    /// instead of "possibly frozen".
    #[test]
    fn an_idle_row_four_hours_old_via_pane_event_only_is_marked_possibly_frozen() {
        let root = temp_dir("stalled-idle-frozen-pane-event");
        let stamp =
            mv_core::time::format_seconds_utc(mv_core::time::now_utc() - time::Duration::hours(4));
        write(&root.join(".multiverse/routing.log"), &format!(
            "{stamp}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=builder\tagent=sb-buil-cx\n{stamp}\tevent=pane\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n"));
        let args = matches(&["tasks"]);
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Idle),
        }];
        let (rows, count) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(rows[0].state, "stalled");
        assert_eq!(count, 1);
        assert!(rows[0].flagged);
        assert_eq!(
            rows[0].note.as_deref(),
            Some(
                "possibly frozen: agent reports idle with no status file for 4 hr \u{2014} not \
                 acted on; try `herdr agent prompt sb-buil-cx` or `dispatch --into \
                 20250101-fixer-aaaaaa`."
            )
        );
    }

    /// The threshold side of signal (a): the same idle, no-status-file
    /// pane, but still under the stall threshold, is not yet "possibly
    /// frozen" — it keeps `classify`'s plain stalled note, same as every
    /// idle/done/blocked/unknown pane always has.
    #[test]
    fn an_idle_row_under_the_stall_threshold_is_not_flagged_possibly_frozen() {
        let root = temp_dir("stalled-idle-fresh");
        let stamp = mv_core::time::format_seconds_utc(
            mv_core::time::now_utc() - time::Duration::minutes(2),
        );
        write(&root.join(".multiverse/routing.log"), &format!(
            "{stamp}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=builder\tagent=sb-buil-cx\n{stamp}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n"));
        let args = matches(&["tasks"]);
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Idle),
        }];
        let (rows, count) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(rows[0].state, "stalled");
        assert_eq!(count, 1);
        assert_eq!(
            rows[0].note.as_deref(),
            Some(
                "live in pane 'w1:p1' but agent is live but not working (idle): not dead and \
                 not still spending — go look at the pane."
            )
        );
    }

    #[test]
    fn a_settled_unrecorded_task_uses_the_python_finished_note() {
        let events = TaskEvents {
            started_pane: Some("w1:p1".into()),
            ..Default::default()
        };
        for status in [AgentState::Idle, AgentState::Done, AgentState::Unknown] {
            let live = [LiveAgentStatus {
                pane_id: "w1:p1".into(),
                name: None,
                status: Some(status),
            }];
            let (_, _, _, note) =
                classify(Some(&events), Some(TaskStatus::Done), None, Some(&live));
            assert_eq!(
                note.as_deref(),
                Some("finished as 'done' after its dispatch gave up. `--reconcile` records it.")
            );
        }
    }

    #[test]
    fn status_flag_is_supersede_only_and_never_filters_the_plain_listing() {
        // Python's `--status` (`cli.py:966-971`) is read only by
        // `_cmd_tasks_supersede` (`cli.py:7803,7835,7864,7882`) — a plain
        // `tasks --status done` never reaches that function and so never
        // filters anything; `args.status` is simply unused on that path.
        // Found via `conformance/diff_fuzz.py` seed 50000.
        let root = temp_dir("status-filter");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=done\trole=builder\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=done\tstatus=done\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=blocked\trole=builder\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=blocked\tstatus=blocked\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=unfinished\trole=builder\n",
        );
        let args = matches(&["tasks", "--status", "done"]);
        let (rows, needs_look) = build_rows(&root, &args, Some(&[]), None);
        assert_eq!(
            rows.iter()
                .map(|row| row.task_id.as_str())
                .collect::<Vec<_>>(),
            ["done", "blocked", "unfinished"]
        );
        assert_eq!(needs_look, 2);
    }

    #[test]
    fn attention_count_tracks_unreviewed_outcome_records() {
        let root = temp_dir("attention-count");
        let log = root.join(".multiverse/routing.log");
        let history = "2020-01-01T00:00:00Z\tevent=dispatch\ttask=task\trole=builder\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=task\tstatus=blocked\n";
        write(&log, history);
        let args = matches(&["tasks"]);
        let (rows, needs_look) = build_rows(&root, &args, Some(&[]), None);
        assert!(rows[0].flagged);
        assert_eq!(needs_look, 1);
        write(
            &log,
            &format!(
                "{history}2020-01-01T00:00:00Z\tevent=acknowledged\ttask=task\tacknowledges=2\n"
            ),
        );
        assert_eq!(build_rows(&root, &args, Some(&[]), None).1, 0);
        write(&log, &format!("{history}2020-01-01T00:00:00Z\tevent=supersede\ttask=task\tstatus=failed\tsupersedes=2\treason=reviewed\n"));
        assert_eq!(build_rows(&root, &args, Some(&[]), None).1, 0);
    }

    #[test]
    fn rows_are_listed_in_dispatch_order_not_alphabetically() {
        // Card 20261102-b-a5782d: `build_rows` used to sort task ids
        // alphabetically, reordering every listing relative to Python's
        // own `cmd_tasks`, which preserves routing-log dispatch order.
        let root = temp_dir("dispatch-order");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-zzzzzz\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\trole=fixer\n",
        );
        let args = matches(&["tasks"]);
        let (rows, _needs_look) = build_rows(&root, &args, Some(&[]), None);
        let ids: Vec<&str> = rows.iter().map(|row| row.task_id.as_str()).collect();
        assert_eq!(ids, vec!["20260101-fixer-zzzzzz", "20260101-fixer-aaaaaa"]);
    }

    #[test]
    fn a_terminal_status_with_no_result_event_is_unrecorded_and_flagged() {
        let events = TaskEvents {
            role: "oracle".into(),
            account: "old-seat".into(),
            started_pane: Some("w2:p1".into()),
            ..Default::default()
        };
        let (state, suffix, flagged, note) =
            classify(Some(&events), Some(TaskStatus::Done), None, Some(&[]));
        assert_eq!(state, "unrecorded");
        assert_eq!(suffix, "done");
        assert!(flagged);
        assert_eq!(
            note.unwrap(),
            "status file says 'done', but pane gone, status done — `--reconcile` leaves it \
             alone until the pane settles."
        );
    }

    #[test]
    fn a_missing_recorded_pane_uses_the_python_awaiting_note() {
        let events = TaskEvents {
            started_pane: Some("w1:p3".into()),
            ..Default::default()
        };
        let (state, suffix, flagged, note) = classify(Some(&events), None, None, Some(&[]));
        assert_eq!(state, "awaiting");
        assert_eq!(suffix, DASH);
        assert!(flagged);
        assert_eq!(note.as_deref(), Some(
            "no status file and recorded pane is not among live panes: uncertain, not dead. The result, if any, is in its pane."
        ));
    }

    #[test]
    fn a_bare_dispatch_with_nothing_else_is_awaiting_and_flagged() {
        let events = TaskEvents {
            role: "reviewer".into(),
            account: "old-seat".into(),
            ..Default::default()
        };
        let (state, suffix, flagged, note) = classify(Some(&events), None, None, Some(&[]));
        assert_eq!(state, "awaiting");
        assert_eq!(suffix, DASH);
        assert!(flagged);
        assert_eq!(
            note.unwrap(),
            "no status file and no agent recorded for this dispatch: uncertain, not dead. The \
             result, if any, is in its pane."
        );
    }

    #[test]
    fn a_live_pane_with_nothing_terminal_yet_is_running_and_flagged() {
        let events = TaskEvents {
            role: "fixer".into(),
            account: "old-seat".into(),
            started_pane: Some("w1:p1".into()),
            ..Default::default()
        };
        let live = live_ids(&["w1:p1"]);
        let (state, suffix, flagged, note) = classify(Some(&events), None, None, Some(&live));
        assert_eq!(state, "running");
        assert_eq!(suffix, DASH);
        assert!(flagged);
        assert!(note.is_none());
    }

    #[test]
    fn a_failed_agent_list_leaves_an_unfinished_task_running() {
        let events = TaskEvents {
            role: "fixer".into(),
            account: "old-seat".into(),
            started_pane: Some("w1:p1".into()),
            ..Default::default()
        };
        let (state, suffix, flagged, note) = classify(Some(&events), None, None, None);
        assert_eq!(state, "running");
        assert_eq!(suffix, DASH);
        assert!(flagged);
        assert!(note.is_none());
    }

    #[test]
    fn parse_seat_out_takes_the_earliest_separator() {
        assert_eq!(
            parse_seat_out("SEAT OUT: codex:default — rolling 429"),
            Some(("codex:default".into(), "rolling 429".into()))
        );
        assert_eq!(
            parse_seat_out("SEAT OUT: grok:fast - banner"),
            Some(("grok:fast".into(), "banner".into()))
        );
        assert_eq!(
            parse_seat_out("SEAT OUT: pool — detail with - hyphen"),
            Some(("pool".into(), "detail with - hyphen".into()))
        );
        assert!(parse_seat_out("chatter mentioning SEAT OUT: later").is_none());
        assert!(parse_seat_out("SEAT OUT: nosep").is_none());
    }

    fn seed_unrecorded_done(root: &Path, task: &str) {
        mv_state::log::ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={task}\tbranch=\trole=explorer"
            ))
            .unwrap();
        std::fs::create_dir_all(root.join(".multiverse/tasks").join(task)).unwrap();
        std::fs::write(
            root.join(".multiverse/tasks").join(task).join("status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            root.join(".multiverse/tasks").join(task).join("result.md"),
            "# Result\ndone.\n",
        )
        .unwrap();
    }

    #[test]
    fn a_failed_probe_keeps_an_unfinished_listed_task_running() {
        let root = temp_dir("probe-failed-running");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-rrrrrr\t\
                 branch=\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=started\ttask=20261101-fixer-rrrrrr\tpane=w1:p1",
            )
            .unwrap();
        let args = matches(&["tasks", "--no-reconcile"]);
        let mut agents = FakeAgents::new(&[]).fail_next(1);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        let (rows, _) = build_rows(&root, &args, None, None);
        assert_eq!(rows[0].state, "running");
    }

    #[test]
    fn writer_status_in_the_listing_prefers_the_lane_worktree() {
        let root = temp_dir("list-writer-status");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-wwwwww\t\
                 branch=multiverse/20261101-fixer-wwwwww\trole=fixer\taccount=test-seat",
            )
            .unwrap();
        write(
            &root.join(".multiverse/tasks/20261101-fixer-wwwwww/status"),
            "blocked",
        );
        write(
            &root
                .join(".multiverse/worktrees/20261101-fixer-wwwwww")
                .join(".multiverse/tasks/20261101-fixer-wwwwww/status"),
            "done",
        );
        let args = matches(&["tasks", "--no-reconcile"]);
        let (rows, _) = build_rows(&root, &args, Some(&[]), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "unrecorded");
        assert_eq!(rows[0].suffix, "done");
    }

    #[test]
    fn a_garbled_status_token_is_never_shown_as_running_even_with_a_live_pane() {
        // Card 20261117-b-437eeb (Scout 9c0832 rank 5): a corrupted status
        // write ("donedone") must never render the same as "running" just
        // because Herdr still reports the pane live and working — that is
        // exactly the case this listing most needs to flag, since neither
        // `--reconcile` nor (pre-fix) `--close-stranded` could ever force
        // a terminal recording for it.
        let root = temp_dir("list-garbled-status");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-garbld\t\
             branch=multiverse/20260101-fixer-garbld\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-fixer-garbld\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-garbld/status"),
            "donedone",
        );
        let live = [LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Working),
        }];
        let args = matches(&["tasks", "--no-reconcile"]);
        let (rows, needs_look) = build_rows(&root, &args, Some(&live), None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, "garbled");
        assert!(rows[0].flagged);
        assert_eq!(needs_look, 1);
        let note = rows[0]
            .note
            .as_deref()
            .expect("a garbled row always has a note");
        assert!(note.contains("donedone"), "note was: {note}");
    }

    #[test]
    fn named_unknown_project_is_a_config_error() {
        let dir = temp_dir("named-unknown");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config = write_config(&dir, &project, "sandbox");
        let args = matches_cfg(&["tasks", "nope"], &config);
        let err = run_with(&args, &mut FakeAgents::new(&[])).unwrap_err();
        assert_eq!(err.code(), 3);
        let message = err.to_string();
        assert!(message.contains("no such project: nope"));
        assert!(message.contains("configured: sandbox"));
        assert!(message.contains("see `multiverse projects`"));
    }

    /// Two configured projects, `a` and `b`, each a real directory under
    /// `dir` so a path token has something on disk to resolve against —
    /// mirrors `backlog`'s own `two_project_config` test fixture.
    fn two_project_config(dir: &Path) -> (mv_config::load::Config, PathBuf, PathBuf) {
        let project_a = dir.join("project-a");
        let project_b = dir.join("project-b");
        std::fs::create_dir_all(&project_a).unwrap();
        std::fs::create_dir_all(&project_b).unwrap();
        let toml = format!(
            "[projects.a]\npath = \"{}\"\n[projects.b]\npath = \"{}\"\n",
            project_a.display(),
            project_b.display(),
        );
        let config = mv_config::load::load_config_str(&toml, &dir.join("config.toml")).unwrap();
        (config, project_a, project_b)
    }

    #[test]
    fn resolve_requested_projects_matches_a_literal_configured_name() {
        let dir = temp_dir("resolve-requested-literal");
        let (config, _a, _b) = two_project_config(&dir);
        let requested = vec!["a".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, None, &requested);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "a");
        assert!(unknown.is_empty());
    }

    /// Card `20261117-b-486a55`: `tasks .` used to fail with "no such
    /// project: ." because this lookup was a bare `contains_key` match
    /// with no path handling at all — the revert check for this task ran
    /// with `resolve_requested_projects` stubbed back down to that bare
    /// check, and this test failed exactly as expected before the real
    /// fix was restored.
    #[test]
    fn resolve_requested_projects_resolves_dot_to_the_cwd_project() {
        let dir = temp_dir("resolve-requested-dot");
        let (config, project_a, _b) = two_project_config(&dir);
        let requested = vec![".".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&project_a), &requested);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "a");
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_resolves_a_relative_path() {
        let dir = temp_dir("resolve-requested-relative");
        let (config, _a, _b) = two_project_config(&dir);
        let requested = vec!["project-b".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "b");
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_resolves_an_absolute_path() {
        let dir = temp_dir("resolve-requested-absolute");
        let (config, project_a, _b) = two_project_config(&dir);
        let requested = vec![project_a.display().to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].name, "a");
        assert!(unknown.is_empty());
    }

    #[test]
    fn resolve_requested_projects_reports_an_unknown_token() {
        let dir = temp_dir("resolve-requested-unknown");
        let (config, _a, _b) = two_project_config(&dir);
        let requested = vec!["no-such-project".to_string()];
        let (resolved, unknown) = resolve_requested_projects(&config, Some(&dir), &requested);
        assert!(resolved.is_empty());
        assert_eq!(unknown, vec![&requested[0]]);
    }

    /// End-to-end through `run_with` itself (not just the resolver): `tasks
    /// .` from inside a configured project's own directory must list that
    /// project, the same as naming it literally would. Uses [`CwdGuard`]
    /// like `bare_tasks_in_a_sandboxed_nested_lane_never_writes_the_real_
    /// project_it_sits_inside` above — two tests in this file now change
    /// cwd, and both go through the same process-global lock.
    #[test]
    fn tasks_dot_lists_the_cwd_project() {
        let dir = temp_dir("tasks-dot-lists-cwd-project");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        seed_unrecorded_done(&project, "20261101-explorer-cccccc");
        let config = write_config(&dir, &project, "sandbox");
        let guard = CwdGuard::set(&project);
        let args = matches_cfg(&["tasks", ".", "--reconcile"], &config);
        let result = run_with(&args, &mut FakeAgents::new(&[]));
        drop(guard);
        assert!(result.is_ok(), "{:?}", result.err());
        let log = std::fs::read_to_string(project.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains("state=reconciled"), "{log}");
    }

    #[test]
    fn named_projects_are_the_listing_scope() {
        let dir = temp_dir("named-scope");
        let alpha = dir.join("alpha");
        let beta = dir.join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        seed_unrecorded_done(&alpha, "20261101-explorer-aaaaaa");
        seed_unrecorded_done(&beta, "20261101-explorer-bbbbbb");
        let config_path = dir.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.alpha]\npath = {:?}\n\n[projects.alpha.chains]\nscout = [\"seat\"]\n\n\
                 [projects.beta]\npath = {:?}\n\n[projects.beta.chains]\nscout = [\"seat\"]\n",
                alpha.to_str().unwrap(),
                beta.to_str().unwrap()
            )),
        );
        let args = matches_cfg(&["tasks", "alpha", "--reconcile"], &config_path);
        run_with(&args, &mut FakeAgents::new(&[])).unwrap();
        let alpha_log = std::fs::read_to_string(alpha.join(".multiverse/routing.log")).unwrap();
        let beta_log = std::fs::read_to_string(beta.join(".multiverse/routing.log")).unwrap();
        assert!(alpha_log.contains("state=reconciled"));
        assert!(!beta_log.contains("state=reconciled"));
    }

    #[test]
    fn plain_tasks_records_an_unrecorded_done_task() {
        let root = temp_dir("auto-reconcile");
        seed_unrecorded_done(&root, "20261101-explorer-aaaaaa");
        let args = matches(&["tasks"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains(
                "event=result\ttask=20261101-explorer-aaaaaa\tstatus=done\tstate=reconciled"
            ),
            "plain tasks must auto-reconcile: {log:?}"
        );
    }

    #[test]
    fn reused_observation_does_not_replace_the_fresh_teardown_probe() {
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Working);
        let initial = vec![LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: None,
            status: Some(AgentState::Idle),
        }];
        let mut observed = ObservedAgents::new(&mut agents, Some(&initial));
        assert_eq!(
            observed.list_checked().unwrap()[0].status,
            Some(AgentState::Idle)
        );
        assert_eq!(
            observed.list_checked().unwrap()[0].status,
            Some(AgentState::Working)
        );
        assert_eq!(agents.probes, 1);
    }

    #[test]
    fn failed_listing_never_closes_stranded_or_reclaimable_tasks() {
        for flag in ["--close-stranded", "--reclaimable"] {
            let root = temp_dir("failed-write-listing");
            write_dispatch_log(&root, "20261101-fixer-aaaaaa");
            let log = root.join(".multiverse/routing.log");
            let before = std::fs::read(&log).unwrap();
            let args = matches(&["tasks", flag, "--yes"]);
            let mut agents = FakeAgents::new(&[]).fail_next(1);
            list(&root, None, &args, &mut agents, None).unwrap();
            assert_eq!(std::fs::read(&log).unwrap(), before);
            assert_eq!(agents.probes, 1);
        }
    }

    #[test]
    fn close_stranded_reuses_the_listings_single_herdr_probe() {
        let root = temp_dir("close-stranded-probes");
        write_dispatch_log(&root, "20261101-fixer-aaaaaa");
        let args = matches(&["tasks", "--close-stranded", "--yes"]);
        let mut agents = FakeAgents::new(&[]);
        list(&root, None, &args, &mut agents, None).unwrap();
        assert_eq!(agents.probes, 1);
    }

    #[test]
    fn close_stranded_confirms_a_recorded_pane_idle_with_a_fresh_probe_before_closing() {
        // Card `20261106-b-6b25fd`: the doc comment on `ObservedAgents`
        // ("later probes still reach Herdr") and on `close_settled_panes`
        // ("Close each just-recorded pane only when a *second* listing
        // still shows it idle/done") both claim a genuine fresh Herdr
        // round-trip immediately before the close, mirroring Python's
        // `_close_settled_panes` re-check. `FakeAgents::probes` counts
        // every `list`/`list_checked` call that actually reaches the
        // scripted agent — including a second one that only repeats the
        // first's own answer — so a regression that let `ObservedAgents`
        // hand back its cached first snapshot a second time would drop
        // this to 1 while still closing the pane.
        let root = temp_dir("close-stranded-settle-probe");
        seed_unrecorded_done(&root, "20261101-explorer-aaaaaa");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=started\ttask=20261101-explorer-aaaaaa\t\
                 pane=w1:p1",
            )
            .unwrap();
        let args = matches(&["tasks", "--close-stranded", "--yes"]);
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Idle);
        list(&root, None, &args, &mut agents, None).unwrap();
        assert_eq!(
            agents.probes, 2,
            "the top listing, reused once, plus one genuinely fresh re-check before the close"
        );
        assert!(
            agents.panes.is_empty(),
            "the settled pane must actually close"
        );
    }

    #[test]
    fn reclaimable_never_closes_a_pane_without_matching_dispatch_name() {
        let root = temp_dir("reclaim-no-name");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=scout\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\n",
        );
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Done);
        agents.panes[0].name = Some("sandbox-worker-cx".into());
        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        list(&root, None, &args, &mut agents, None).unwrap();
        assert_eq!(agents.panes.len(), 1);
        // Card 20261108-b-37df85: the name mismatch is now caught while
        // `reclaimable_panes` builds its candidate list (the pane falls
        // to `stale`, never `reclaimable`), so there is no longer a
        // confirmation re-probe to skip a close with — just the one
        // top-level listing.
        assert_eq!(agents.probes, 1);
    }

    #[test]
    fn reclaimable_never_closes_a_pane_reused_by_a_different_live_agent() {
        // Card 20261108-b-37df85 (the mail-reported incident): task T was
        // dispatched under agent `x-fix-cl` at pane `w1:p1`. Herdr now
        // reports a different agent, `y-buil-cl`, answering at that same
        // pane id — a reuse after restart, not T's own session. `--yes`
        // must never issue a pane-close for it, and the listing must not
        // offer it as something that would close.
        let root = temp_dir("reclaim-reused-pane");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261108-fixer-111111\t\
                 branch=main\trole=fixer\tagent=x-fix-cl",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261108-fixer-111111\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1",
            )
            .unwrap();
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Idle);
        agents.panes[0].name = Some("y-buil-cl".into());
        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        list(&root, None, &args, &mut agents, None).unwrap();
        assert_eq!(
            agents.panes.len(),
            1,
            "a pane id reused by a different live agent must never be closed"
        );
    }

    #[test]
    fn reclaimable_closes_a_pane_when_the_recorded_agent_still_matches() {
        let root = temp_dir("reclaim-matching-agent");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261108-fixer-222222\t\
                 branch=main\trole=fixer\tagent=x-fix-cl",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261108-fixer-222222\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1",
            )
            .unwrap();
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_label("w1:p1", "fixer · 20261108-fixer-222222");
        agents.panes[0].name = Some("x-fix-cl".into());
        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        list(&root, None, &args, &mut agents, None).unwrap();
        assert!(
            agents.panes.is_empty(),
            "a pane still hosting the task's own dispatched agent must still be closed"
        );
    }

    /// Card `20261122-b-4ad223` / `20261122-b-e98304`: a recorded, clean
    /// Claude lane whose pane is live and done but whose footer still
    /// shows a running background shell is kept, never closed, even with
    /// `--yes`; the same pane with the footer segment gone is closed as
    /// before. Revert check: make `reclaimable_and_report`'s
    /// `shell_checks` always `Clear` and the first pane closes.
    #[test]
    fn reclaimable_keeps_a_claude_pane_still_waiting_on_its_shell() {
        let shell_footer = include_str!(
            "../../../../tests/fixtures/stall-cause/claude-shell-still-running-idle.txt"
        );
        for (footer, closes) in [
            (shell_footer.to_string(), false),
            (shell_footer.replace(" · 1 shell", ""), true),
        ] {
            let root = temp_dir("reclaim-waiting-on-shell");
            mv_state::log::ProjectLog::new(&root)
                .append(
                    "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261122-builder-333333\t\
                     branch=main\trole=builder\tagent=x-buil-cl\taccount=claude-a",
                )
                .unwrap();
            mv_state::log::ProjectLog::new(&root)
                .append(
                    "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261122-builder-333333\t\
                     status=done\tstate=done\trevisions=0\tpane=w1:p1",
                )
                .unwrap();
            let toml = format!(
                "[accounts.claude-a]\nkind = \"claude\"\nshort = \"cl\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"/tmp\"\npool = \"claude:a\"\n\
                 [projects.x]\npath = \"{}\"\n",
                root.display()
            );
            let config =
                mv_config::load::load_config_str(&toml, &root.join("config.toml")).unwrap();
            let mut agents = FakeAgents::new(&["w1:p1"])
                .with_status("w1:p1", AgentState::Done)
                .with_label("w1:p1", "builder · 20261122-builder-333333")
                .with_visible("w1:p1", &footer);
            agents.panes[0].name = Some("x-buil-cl".into());
            let args = matches(&["tasks", "--no-reconcile", "--reclaimable", "--yes"]);
            list(&root, None, &args, &mut agents, Some(&config)).unwrap();
            assert_eq!(
                agents.panes.is_empty(),
                closes,
                "footer {footer:?}: a pane still running its shell must be kept"
            );
        }
    }

    #[test]
    fn no_reconcile_leaves_an_unrecorded_done_task_unlogged() {
        let root = temp_dir("no-reconcile");
        seed_unrecorded_done(&root, "20261101-explorer-bbbbbb");
        let args = matches(&["tasks", "--no-reconcile"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("state=reconciled"));
    }

    #[test]
    fn auto_reconcile_skips_when_herdr_is_down() {
        let root = temp_dir("auto-herdr-down");
        seed_unrecorded_done(&root, "20261101-explorer-cccccc");
        let args = matches(&["tasks"]);
        let mut agents = FakeAgents::new(&[]).fail_next(1);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("state=reconciled"));
    }

    #[test]
    fn reclaimable_yes_never_closes_a_working_pane() {
        let root = temp_dir("reclaimable-working");
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-ffffff\t\
                 branch=main\trole=fixer",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&root)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=result\ttask=20261101-fixer-ffffff\tstatus=done\t\
                 state=done\trevisions=0\tpane=w1:p1",
            )
            .unwrap();
        let args = matches(&["tasks", "--reclaimable", "--yes"]);
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Working);
        assert!(list(&root, None, &args, &mut agents, None).is_ok());
        assert_eq!(
            agents.list(),
            vec![mv_state::tasks::LiveAgent {
                pane_id: "w1:p1".to_string()
            }],
        );
    }

    #[test]
    fn blocking_prompt_trips_the_pool_and_records_event_limit() {
        let _state = StateDirEnvGuard::new();
        let dir = temp_dir("blocking-prompt");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        mv_state::log::ProjectLog::new(&project)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-llllll\t\
                 branch=\trole=fixer\taccount=seat\tagent=codex-1",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&project)
            .append(
                "2026-11-01T00:01:00+00:00\tevent=started\ttask=20261101-fixer-llllll\tpane=w1:p1",
            )
            .unwrap();
        let config_path = dir.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.sandbox]\npath = {:?}\n\n[projects.sandbox.chains]\nscout = [\"seat\"]\n",
                project.to_str().unwrap()
            )),
        );
        let args = matches_cfg(&["tasks", "sandbox", "--no-reconcile"], &config_path);
        let config = super::super::load_config(&args).unwrap();
        let rolling = "exceeded retry limit, last status: 429 Too Many Requests";
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_visible("w1:p1", rolling);
        assert!(list(&project, Some("sandbox"), &args, &mut agents, Some(&config)).is_ok());
        let log = std::fs::read_to_string(project.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=limit\ttask=20261101-fixer-llllll\trule="),
            "expected event=limit: {log:?}"
        );
    }

    /// Card `20261118-b-9226e1`: the same probe, but for a task killed
    /// between pane creation and `agent start` — `routing.log` holds only
    /// `event=pane`, never `event=started`. `probe_blocking_prompt` must
    /// fall back to `TaskEvents::effective_pane` the same way `classify`
    /// does, or it reads `None` and returns before ever looking at the
    /// pane. Revert check: read `events.started_pane.as_deref()` directly
    /// at the top of `probe_blocking_prompt` instead of
    /// `events.effective_pane()`, and this fails — the probe returns
    /// early and no `event=limit` is ever recorded.
    #[test]
    fn blocking_prompt_reads_the_pane_event_pane_when_started_never_landed() {
        let _state = StateDirEnvGuard::new();
        let dir = temp_dir("blocking-prompt-pane-event");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        mv_state::log::ProjectLog::new(&project)
            .append(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-fixer-llllll\t\
                 branch=\trole=fixer\taccount=seat\tagent=codex-1",
            )
            .unwrap();
        mv_state::log::ProjectLog::new(&project)
            .append("2026-11-01T00:00:30+00:00\tevent=pane\ttask=20261101-fixer-llllll\tpane=w1:p1")
            .unwrap();
        let config_path = dir.join("config.toml");
        write(
            &config_path,
            &minimal_config(&format!(
                "[projects.sandbox]\npath = {:?}\n\n[projects.sandbox.chains]\nscout = [\"seat\"]\n",
                project.to_str().unwrap()
            )),
        );
        let args = matches_cfg(&["tasks", "sandbox", "--no-reconcile"], &config_path);
        let config = super::super::load_config(&args).unwrap();
        let rolling = "exceeded retry limit, last status: 429 Too Many Requests";
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_status("w1:p1", AgentState::Idle)
            .with_visible("w1:p1", rolling);
        assert!(list(&project, Some("sandbox"), &args, &mut agents, Some(&config)).is_ok());
        let log = std::fs::read_to_string(project.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=limit\ttask=20261101-fixer-llllll\trule="),
            "expected event=limit: {log:?}"
        );
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    #[test]
    fn listing_closes_a_backlog_item_whose_linked_writer_merged() {
        let root = temp_dir("close-merged-cli");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "main@example.invalid"]);
        git(&root, &["config", "user.name", "Main Test"]);
        std::fs::write(root.join("README"), "seed\n").unwrap();
        git(&root, &["add", "README"]);
        git(&root, &["commit", "-q", "-m", "seed"]);
        let base = std::process::Command::new("git")
            .current_dir(&root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
        git(
            &root,
            &["branch", "multiverse/20260101-fixer-aaaaaa", &base],
        );
        git(
            &root,
            &[
                "update-ref",
                "refs/multiverse/base/20260101-fixer-aaaaaa",
                &base,
            ],
        );
        git(
            &root,
            &["checkout", "-q", "multiverse/20260101-fixer-aaaaaa"],
        );
        std::fs::write(root.join("work.txt"), "a\n").unwrap();
        git(&root, &["add", "work.txt"]);
        git(&root, &["commit", "-q", "-m", "work"]);
        git(&root, &["checkout", "-q", "main"]);
        git(
            &root,
            &[
                "merge",
                "--no-ff",
                "-q",
                "-m",
                "merge lane",
                "multiverse/20260101-fixer-aaaaaa",
            ],
        );
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=multiverse/20260101-fixer-aaaaaa\trole=fixer\n\
             2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();
        let item = mv_state::backlog::add(
            &root,
            "port auto_supersede",
            "nexus",
            "tracked",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        mv_state::backlog::link_task(&root, &item.item, "20260101-fixer-aaaaaa", None).unwrap();

        let args = matches(&["tasks"]);
        assert!(list(&root, None, &args, &mut FakeAgents::new(&[]), None).is_ok());
        let items = mv_state::backlog::read_items(&root, false);
        let reread = items.iter().find(|i| i.item == item.item).unwrap();
        assert_eq!(reread.state, "done");
    }

    #[test]
    fn long_account_names_keep_a_separator_before_the_task_state() {
        let row = Row {
            task_id: "20250101-fixer-aaaaaa".into(),
            role: "fixer".into(),
            account: "explorer-seat".into(),
            state: "recorded",
            suffix: "done #3".into(),
            flagged: false,
            note: None,
        };
        assert_eq!(
            display_row(&row).0,
            format!(
                "    {:26} {:10} {:10} {:11} done #3",
                row.task_id, row.role, row.account, row.state
            )
        );
    }

    #[test]
    fn display_row_strips_osc_52_from_role_account_suffix_and_note() {
        let row = Row {
            task_id: "20261102-fixer-abc123".into(),
            role: "Build\u{1b}]52;c;EVIL\u{07}er".into(),
            account: "clau\u{1b}]0;pwned\u{07}de".into(),
            state: "running",
            suffix: "\u{1b}[2Jok".into(),
            flagged: true,
            note: Some("see pane\u{1b}]52;c;EVIL\u{07}".into()),
        };
        let (line, note) = display_row(&row);
        assert!(!line.contains('\u{1b}'));
        assert!(!line.contains('\u{07}'));
        assert!(!line.contains("[2J"));
        assert!(line.contains("Builder"));
        assert!(line.contains("claude"));
        let note = note.expect("note");
        assert!(!note.contains('\u{1b}'));
        assert!(!note.contains("]52;"));
        assert!(note.contains("see pane"));
    }
}

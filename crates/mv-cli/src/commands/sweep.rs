//! `multiverse sweep`: reconcile stale tasks and reclaim idle fleet
//! resources across every configured project, optionally running one
//! maintenance tick.
//!
//! The actual reconcile/reclaim read-and-write now lives in
//! `mv_state::tasks::sweep_project` (`crates/mv-state/src/tasks.rs`),
//! which this file injects [`mv_state::tasks::HerdrAgents`] into for a
//! real run — `mv-cli` still does not depend on `mv-herdr` directly
//! (`doctor.rs`'s own doc comment: "Herdr access is routed through
//! mv-engine, never straight from mv-cli"); the one real exception is
//! this crate's own direct-shell probes in `doctor.rs`/`workers.rs`/
//! `tasks.rs`, none of which this file reuses. `mv-state` already depends
//! on `mv-herdr`, so the live-pane probe and pane-close call happen
//! there, behind the `AgentPanes` seam `scratch::ScratchPanes`/
//! `HerdrPanes` already established — this file only ever sees the
//! trait, never the subprocess.
//!
//! Per the brief: sweep always records (reconciles) and reclaims, even
//! while the fleet is halted; it only skips `--tick`'s maintenance pass
//! while halted — it never resumes paused work.
//!
//! Finding 1 (critic-sweep-board.md), card `20261102-b-4f86db`: `--tick`
//! now sends the hub's own heartbeat — Python's `SWEEP_TICK_PROMPT` via
//! `_sweep_tick_orchestrator` (`cli.py:11281-11480`) — to each project's
//! Nexus orchestrator, through the same two primitives card
//! `20261102-b-a01bbc` added for `start --reseed`:
//! [`mv_engine::fleet_start::FleetState::orchestrator_agent`] to find the
//! live pane by identity, and a heartbeat-specific landing confirmation
//! ([`mv_engine::runner::deliver_heartbeat`], not
//! [`mv_engine::runner::deliver_prompt_to`] — see that function's own doc
//! comment for why a tick must never resend). `--stale-only` and
//! `--verbose` are both wired to their real Python meanings now too (see
//! below). `hostsleep` (card `20261102-b-fd9159`, task
//! `20261102-fixer-c628da`) now is: [`mv_engine::hostsleep::report`] is
//! called once per `run_with` pass, host-wide ahead of the per-project
//! loop, against the `multiverse` hub project's own root (skipped when
//! that project is not configured).
//!
//! Task `20261103-fixer-ce23dd`: the Codex-pool recovery probe
//! (`cli.py:11573-11636`'s `_sweep_tick_codex_probe`) is now ported as
//! [`mv_engine::limits::sweep_tick_codex_probe`] — once per pool, before
//! any project's own tick, gated the same way as the block above it in
//! this file: `--tick`, `!dry_run`, and this pass's own `halted` snapshot.
//! Card `20261103-b-bb7230`: `_sweep_tick_gauge`'s opportunistic
//! quota-gauge read is now ported too — see [`tick_gauge`] and its own
//! call site in [`run_with`]'s per-project loop, and
//! [`mv_engine::limits::Gauge`]/`has_gauge_reader`/`read_gauge`/
//! `sweep_tick_gauge`/`record_gauge` for the reader half.
//!
//! Task `20261103-fixer-53315d` (card `20261103-b-c5d832`): the blocked
//! half of Python's `AskUserQuestion` picker reporting
//! (`_detect_and_report_picker`/`_report_blocked_picker`,
//! `cli.py:11136-11215`) is now ported as [`parse_orchestrator_picker`]
//! and [`report_blocked_picker`], called from [`tick_outcome`]'s own
//! `live.is_blocked()` branch, on top of the notify-state *episode* table
//! ([`mv_state::notify_episodes`] — first_seen/alerted per key, not the
//! rate-limiter half `mv_state::notify` already ported).
//!
//! Task `20261103-fixer-74c03d` (card `20261103-b-15d3ca`): the rest of
//! `notify.picker_alert` is now wired too —
//! [`mv_state::notify_episodes::report_and_alert_picker_episode`]'s own
//! hub activity post ([`post_picker_activity`], `mv_state::activity::
//! send_message`) and ntfy push ([`push_picker_ntfy`], reusing
//! `commands::notify`'s own `push_with_token` / `resolve_topic` /
//! `resolve_token`), gated on the
//! `_mike_is_idle` presence check (`cli.py:11093-11108`, one-minute
//! threshold — [`PICKER_MIKE_IDLE_AFTER_SECONDS`], reusing
//! `commands::presence`'s own HID idle probe) that decides whether either
//! ever fires. `apply` (`!dry_run`) now reaches all the way down: a
//! `--dry-run` sweep still reads the pane and reports what the episode
//! table would say, but persists nothing and never pages, matching every
//! other dry-run leg in this file. See this task's `result.md` for the
//! remaining gap: Python's own `episode_key`/`episode_state` outbox
//! replay gating (`mv_state::notify`'s own doc comment scopes this out).
//!
//! The reported tick outcome itself is unchanged (`"skipped (blocked)"`,
//! verbatim) — Python's own `_sweep_tick_orchestrator` returns that same
//! literal regardless of what `_report_blocked_picker` does as a side
//! effect, so this port's episode bookkeeping and alerting are likewise
//! invisible in the printed line. The `done`/menu half of Python's picker
//! detection (`cli.py:11426-11441`) now uses the shared
//! [`dialog_or_menu_reason`] before the dry-run/send branch: menus report
//! their episode from that same read and return `skipped (menu)` without
//! prompting or pressing keys. A confirmed clear pane closes its episode;
//! dialogs and failed reads leave it untouched.
//! Host-wide Docker and disk-floor reports now run once per
//! sweep pass — tick or not, dry-run or not, including while halted — after
//! the project summaries and before ticks, via
//! [`mv_engine::hostdisk::sweep_report`], matching Python's `cmd_sweep`
//! host blocks (`cli.py:12991-13015`). The hub's low-disk notice (card
//! `20261103-b-94fa09`) is still apply-only.
//!
//! - `--stale-only` skips ticking a project whose orchestrator is still
//!   cycling on its own — its `activity.lastseen` marker
//!   ([`mv_state::activity::read_marker`]) younger than
//!   [`mv_board::layout::STALE_AFTER_MINUTES`] — printing
//!   `skipped (fresh, Nm)` for it instead of ticking.
//! - `--verbose` is still accepted but changes nothing: every failure this
//!   file or [`mv_state::tasks::sweep_project`] can hit already fails open
//!   (an unreadable log, Herdr unreachable) rather than raising, so there
//!   is no traceback for it to add under a `sweep error:` line the way
//!   Python's `_print_sweep_error` has one to print.
//!
//! Finding 4 (part 2 of `converge_stale`): the reviewer-over-timeout line
//! (`cli.py:12860-12881`) is ported below (`reviewer_over_timeout_line`),
//! deliberately checking the live listing last so the common case (nothing
//! overdue) adds no Herdr call beyond what `sweep_project` already made —
//! every pinned `sweep-*` conformance transcript scripts an exact call
//! count. `nudge_idle_lanes_to_finish`/`resume_stuck_lanes`/
//! `cap_long_running_lanes` (one-shot claim-file nudge/resume/cap prompts)
//! are tracked as backlog `20261102-b-b4ede0`.
//!
//! Task `20261102-fixer-c628da` (card `20261102-b-0d8b17`): three more
//! `converge_stale`/reconciliation legs are ported as summary fragments,
//! joined with `; ` on each project's single line, matching Python:
//!
//! - `superseded: ...` — [`mv_state::tasks::auto_supersede`] (`cli.py:
//!   12841-12849`), named per task, same shape as `expired`'s own
//!   (uncounted) sibling. Runs inside `sweep_project` itself, ahead of
//!   `expire_awaiting`, so a task it claims is never also lease-expired
//!   in the same pass — see that function's own doc comment.
//! - `closed backlog: ...` — [`close_merged_backlog_links`] below mirrors
//!   `cli._close_merged_backlog_links`: a backlog item linked to a task
//!   now recorded `done` (and not itself an `auto_supersede` rewrite —
//!   `SUPERSEDED_MERGED`/`SUPERSEDED_REDISPATCHED` are the task-log's own
//!   correction, not this task's outcome) whose branch has independently
//!   landed is closed with that commit as evidence.
//! - `closed: ...` / `possibly answered by ...` —
//!   [`mv_state::activity::find_unlinked_replies`] (`cli.py:12900-12936`):
//!   an open ask this project holds that a later, unlinked ordinary
//!   message to the same asker already answered in words. A message that
//!   references the ask (`#N`, an exact phrase, or strong word overlap)
//!   closes it via [`mv_state::activity::close_ask`]; one that does not is
//!   only reported, never closed automatically. A peeked but unacknowledged
//!   inbox is reported via [`mv_state::activity::mail_cursor_behind`].
//!
//! critic-sweep-board.md findings fixed previously, in `sweep_project`
//! (task `20261102-fixer-40e851`/`20261102-fixer-4dbd49`): finding 2 — a
//! failed Herdr listing ([`mv_state::tasks::AgentPanes::list_checked`])
//! now fails the whole pass open instead of reading as "no panes live";
//! finding 3/7 — reclaim's clean-lane check now covers a writer's own
//! `multiverse/<id>` lane (not only a bare `main` dispatch), a close is
//! confirmed against a second, fresh listing showing the *same* agent
//! name in the *same* idle/done status the first listing saw, and this
//! file takes a fleet-wide lock (`try_acquire_sweep_lock`) around every
//! non-dry-run pass; finding 4 (part 1) — a genuinely stranded AWAITING
//! task past its account's lease is recorded `failed`
//! ([`mv_state::tasks::expire_awaiting`]) and named in an `expired` summary
//! fragment only when nonzero.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use clap::ArgMatches;
use mv_config::load::{Config, Project};
use mv_engine::fleet_start::FleetState;
use mv_engine::mv_herdr::argv as herdr_argv;
use mv_engine::runner::{
    deliver_heartbeat, dialog_or_menu_reason, Heartbeat, Herdr, ProcessHerdr, RunnerError, Timing,
};
use mv_state::activity::HUB_PROJECT_NAME;
use mv_state::backlog::{done as backlog_done, read_items as read_backlog_items, BacklogError};
use mv_state::tasks::{
    is_writing_role, lane_task_id, merged_evidence, AgentPanes, HerdrAgents, SweepTask,
    SUPERSEDED_MERGED, SUPERSEDED_REDISPATCHED,
};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use crate::exit::Failure;

/// Adapted from Python's `SWEEP_TICK_PROMPT` (`cli.py:10926-10930`) — the one
/// and only place this text is written, so a cold read of the pane can
/// never mistake it for Mike's own words (see that constant's own doc
/// comment for the 2026-10-20 incident this guards against). Unlike
/// Python's frozen reference, this no longer names the seed file directly:
/// a project's own seed may be at either name depending on when it was
/// last written (see [`mv_engine::nexus_seed::existing_seed_path`]).
const SWEEP_TICK_PROMPT: &str = "Hub cycle tick (hub nudge, not Mike): run one pass of your \
     seed's Every cycle (mail, reconcile, reclaim, board, backlog). Not a \
     recurring job.";

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    run_with(
        args,
        &mut HerdrAgents,
        &mut ProcessHerdr,
        &mut mv_engine::hostdisk::sweep_report,
        ProcessProbe::Live,
    )
}

/// One host process observed by the read-only cwd probe. Never killed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LaneProcess {
    pid: u32,
    cwd: PathBuf,
    command: String,
    cpu_percent: String,
}

/// How this sweep pass learns about host processes. Tests inject a snapshot
/// so the live `ps`/`lsof` probe never runs against the real host.
enum ProcessProbe {
    Live,
    #[cfg(test)]
    Snapshot(Option<Vec<LaneProcess>>),
}

fn process_label(process: &LaneProcess) -> String {
    let command: String = process
        .command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let command: String = command.chars().take(160).collect();
    format!("pid {} {command}", process.pid)
}

/// Python's `process_cwd_snapshot`: processes, or `None` (unknown) on
/// probe failure; never kill anything.
fn process_cwd_snapshot() -> Option<Vec<LaneProcess>> {
    probe_process_cwds().ok()
}

fn probe_process_cwds() -> Result<Vec<LaneProcess>, ()> {
    let stdout =
        mv_engine::dockerreport::host_probe("ps", &["-axo", "pid=,pcpu=,comm="]).ok_or(())?;
    let mut metadata = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid) = parts.next().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let Some(cpu) = parts.next() else {
            continue;
        };
        let command = parts.collect::<Vec<_>>().join(" ");
        if command.is_empty() {
            continue;
        }
        metadata.insert(pid, (command, cpu.to_string()));
    }
    let mut cwds: HashMap<u32, PathBuf> = HashMap::new();
    if cfg!(target_os = "macos") {
        let stdout =
            mv_engine::dockerreport::host_probe("lsof", &["-a", "-d", "cwd", "-Fpcn"]).ok_or(())?;
        let mut pid = None;
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix('p') {
                pid = rest.parse().ok();
            } else if let Some(rest) = line.strip_prefix("n/") {
                if let Some(pid) = pid {
                    cwds.insert(pid, PathBuf::from(format!("/{rest}")));
                }
            }
        }
    } else if cfg!(target_os = "linux") {
        for pid in metadata.keys().copied() {
            if let Ok(cwd) = std::fs::read_link(format!("/proc/{pid}/cwd")) {
                cwds.insert(pid, cwd);
            }
        }
    } else {
        return Err(());
    }
    let mut rows: Vec<LaneProcess> = cwds
        .into_iter()
        .filter_map(|(pid, cwd)| {
            let (command, cpu_percent) = metadata.get(&pid)?.clone();
            Some(LaneProcess {
                pid,
                cwd,
                command,
                cpu_percent,
            })
        })
        .collect();
    rows.sort_by_key(|p| p.pid);
    Ok(rows)
}

fn resolve_process_probe(probe: &ProcessProbe) -> Option<Vec<LaneProcess>> {
    match probe {
        ProcessProbe::Live => process_cwd_snapshot(),
        #[cfg(test)]
        ProcessProbe::Snapshot(rows) => rows.clone(),
    }
}

/// Sum of every `"host: slept ..."` line's own minutes this pass printed —
/// Python's `hostsleep.total_sleep_minutes`, read back out of the already-
/// formatted lines rather than a parallel raw-minutes field on
/// [`mv_engine::hostsleep::SleepReport`] (another lane's file). Both of
/// [`mv_engine::hostsleep::report_with`]'s own line shapes carry the
/// minutes right after the first `(`: `"... (N min, ...)"` and the
/// capped-window `"... (N+ min, ...)"`; a capped window's `+` floor counts
/// as its floor value, matching the line itself, and a line this cannot
/// parse contributes `0` rather than a guess (critic-codex-areas.md H1).
fn sleep_minutes_from_lines(lines: &[String]) -> i64 {
    lines
        .iter()
        .filter_map(|line| {
            let digits: String = line
                .split_once('(')?
                .1
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            digits.parse::<i64>().ok()
        })
        .sum()
}

/// Python's `closed_lane_processes`: only recorded lanes, excluding a lane
/// with any active occupant.
fn closed_lane_processes(
    project: &Path,
    tasks: &[SweepTask],
    processes: &[LaneProcess],
) -> Vec<(String, LaneProcess)> {
    let mut closed: HashMap<PathBuf, String> = HashMap::new();
    let mut active: HashSet<PathBuf> = HashSet::new();
    for task in tasks {
        let root = project
            .join(".multiverse/worktrees")
            .join(lane_task_id(task));
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if task.recorded {
            closed.entry(root).or_insert_with(|| task.task_id.clone());
        } else {
            active.insert(root);
        }
    }
    let mut lingering = Vec::new();
    for process in processes {
        let Ok(cwd) = process.cwd.canonicalize() else {
            continue;
        };
        let mut cursor = Some(cwd.as_path());
        while let Some(root) = cursor {
            if closed.contains_key(root) && !active.contains(root) {
                lingering.push((closed[root].clone(), process.clone()));
                break;
            }
            cursor = root.parent();
        }
    }
    lingering.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.pid.cmp(&b.1.pid)));
    lingering
}

/// Python's closed-lane process suffix (`cli.py:12943-12954`).
fn lane_process_notes(
    project: &Path,
    tasks: &[SweepTask],
    processes: Option<&[LaneProcess]>,
) -> Vec<String> {
    let Some(processes) = processes else {
        return vec!["warning: process-cwd probe unknown".to_string()];
    };
    let lingering = closed_lane_processes(project, tasks, processes);
    if lingering.is_empty() {
        return Vec::new();
    }
    let shown = lingering.len().min(5);
    let named: Vec<String> = lingering
        .iter()
        .take(shown)
        .map(|(task_id, process)| {
            format!(
                "{task_id} {} {}%",
                process_label(process),
                process.cpu_percent
            )
        })
        .collect();
    let mut line = format!("lane processes still running: {}", named.join(", "));
    if lingering.len() > 5 {
        line.push_str(&format!(" (+{} more)", lingering.len() - 5));
    }
    vec![line]
}

/// Persist the sweep timestamp the board's LAST CYCLE card reads
/// (`board_data::read_orchestrator_pass`). Python's `_record_orchestrator_pass`
/// (`cli.py:2782-2790`) writes this after every real non-dry-run pass, at
/// `{project}/.multiverse/orchestrator-pass`, tab-separated
/// `reconcile=<stamp>\tbacklog=<stamp>\n` with second-precision UTC
/// `+00:00` stamps. Best-effort: a write failure must not fail the sweep.
fn record_orchestrator_pass(project_root: &Path, now: OffsetDateTime) {
    let state_dir = project_root.join(".multiverse");
    if std::fs::create_dir_all(&state_dir).is_err() {
        return;
    }
    let stamp = mv_core::time::format_seconds_utc(now);
    let body = format!("reconcile={stamp}\tbacklog={stamp}\n");
    let _ = mv_core::atomic::write_atomic(
        state_dir.join("orchestrator-pass").as_path(),
        body.as_bytes(),
    );
}

/// Python's `needs a decision: <id> (done, <detail>, <hours>h)` suffix
/// (`cli.py:12882-12899`), one line per dirty-done task, sorted by id.
fn dirty_done_notes(root: &Path, tasks: &[SweepTask], now: OffsetDateTime) -> Vec<String> {
    let mut tasks: Vec<&SweepTask> = tasks.iter().collect();
    tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    tasks
        .into_iter()
        .filter_map(|task| {
            let detail = mv_state::tasks::dirty_done_detail(root, task)?;
            let hours = task
                .dispatched_at
                .map(|at| (now - at).whole_hours().max(0).to_string())
                .unwrap_or_else(|| "?".to_string());
            Some(format!(
                "needs a decision: {} (done, {detail}, {hours}h)",
                task.task_id
            ))
        })
        .collect()
}

/// The one summary line every project prints, regardless of `--dry-run`,
/// `--tick`, or halt state — pulled out as a pure function so its exact
/// shape (pinned by every `sweep-*` conformance scenario) is testable
/// without capturing stdout.
fn project_summary_line(
    name: &str,
    report: &mv_state::tasks::SweepReport,
    notes: &[String],
) -> String {
    let mut line = format!(
        "{name}: in progress {} -> {}, recorded {}, reclaimed {}",
        report.in_progress_before, report.in_progress_after, report.recorded, report.reclaimed,
    );
    for note in notes {
        line.push_str("; ");
        line.push_str(note);
    }
    line
}

/// `{account name: minutes}`, from every configured account whose own
/// `awaiting_lease_minutes_override` is set — [`mv_state::tasks::
/// expire_awaiting`]'s per-account lease, consulted before the fleet-wide
/// default (finding 4).
fn lease_overrides(config: &mv_config::load::Config) -> std::collections::HashMap<String, u32> {
    config
        .accounts
        .values()
        .filter_map(|account| {
            account
                .awaiting_lease_minutes_override
                .map(|minutes| (account.name.clone(), minutes))
        })
        .collect()
}

/// `--stale-only`'s own skip, when this project's orchestrator has looked
/// at its own mail recently enough that the hub heartbeat is not the only
/// thing keeping it cycling. Mirrors `cmd_sweep`'s own check
/// (`cli.py:13101-13109`): `activity.lastseen`'s own marker
/// ([`mv_state::activity::read_marker`]) read straight off the project's
/// `.multiverse` directory — never `activity.last_seen_at`'s own module,
/// which this crate has not ported as a separate public wrapper — younger
/// than [`mv_board::layout::STALE_AFTER_MINUTES`]. `None` means this
/// project is not fresh (or has never looked at all) and ticking should
/// proceed normally.
fn stale_only_skip(path: &Path, now: OffsetDateTime) -> Option<String> {
    let (_seq, when) = mv_state::activity::read_marker(&path.join(".multiverse"))?;
    let seen = mv_core::time::parse_flexible(&when).ok()?;
    let elapsed = now - seen;
    if elapsed < time::Duration::minutes(mv_board::layout::STALE_AFTER_MINUTES) {
        Some(format!(
            "skipped (fresh, {}m)",
            elapsed.whole_minutes().max(0)
        ))
    } else {
        None
    }
}

/// Finding 1 (critic-sweep-board.md), card `20261102-b-4f86db`: one
/// project's `--tick` outcome, mirroring `_sweep_tick_orchestrator`
/// (`cli.py:11281-11480`) as closely as this crate's existing primitives
/// allow. Finds the Nexus orchestrator by identity
/// ([`FleetState::orchestrator_agent`], the same lookup `start --reseed`
/// uses), reports `working`/`blocked`/`unknown` without reading the pane
/// at all (Herdr's own status is enough to rule those out), and otherwise
/// sends [`SWEEP_TICK_PROMPT`] through [`deliver_heartbeat`] — which
/// itself refuses (dialog/menu) rather than typing into a parked pane, and
/// never resends once Herdr has accepted the send. The shared vendor-dialog/
/// menu classifier also gates dry runs and reports picker episodes from
/// the same pane read. `dry_run` stops short of the send, the same posture
/// `_reconcile_and_reclaim`'s own `apply` flag already takes elsewhere in
/// this file.
///
/// `now` timestamps picker episodes in both the blocked and done/menu
/// branches, matching Python's `_sweep_tick_orchestrator`.
fn tick_outcome(
    project: &Project,
    path: &Path,
    config: &Config,
    herdr: &mut impl Herdr,
    dry_run: bool,
    timing: &Timing,
    now: OffsetDateTime,
) -> String {
    if config.accounts.values().all(|account| !account.interactive) {
        return "skipped (no Nexus)".to_string();
    }
    let fleet = match FleetState::fetch(herdr, true) {
        Ok(fleet) => fleet,
        Err(_) => return "skipped (herdr unreachable)".to_string(),
    };
    let account = config.accounts.values().find(|account| account.interactive);
    let seat = account.map(|a| a.short.as_str()).unwrap_or("");
    let short = project.short.as_deref().unwrap_or("");
    let Some((agent_name, live)) =
        mv_engine::fleet_start::find_orchestrator_agent(&fleet, &project.name, path, seat, short)
    else {
        return "skipped (no Nexus)".to_string();
    };
    if live.is_working() {
        return "skipped (working)".to_string();
    }
    if live.is_blocked() {
        report_blocked_picker(project, config, &agent_name, herdr, now, !dry_run);
        return "skipped (blocked)".to_string();
    }
    if !live.is_ready_for_input() {
        return "skipped (unknown)".to_string();
    }
    let Ok(read) = herdr.call(
        &herdr_argv::agent_read(&agent_name, Some("visible"), None),
        StdDuration::from_secs(30),
    ) else {
        return "skipped (unreadable)".to_string();
    };
    if !read.ok() || read.stdout.trim().is_empty() {
        return "skipped (unreadable)".to_string();
    }
    let kind = account.map(|account| account.kind.as_str()).unwrap_or("");
    if let Some(reason) = dialog_or_menu_reason(&read.stdout, kind) {
        if reason == "menu" {
            report_picker_text(project, config, &read.stdout, now, !dry_run);
            return "skipped (menu)".to_string();
        }
        return "skipped (dialog)".to_string();
    }
    close_picker_episode(project, config, !dry_run);
    if dry_run {
        return "ticked".to_string();
    }
    match deliver_heartbeat(
        herdr,
        &agent_name,
        &live.pane_id,
        SWEEP_TICK_PROMPT,
        live.state_change_seq,
        timing,
    ) {
        Ok(Heartbeat::Landed) => "ticked".to_string(),
        Ok(Heartbeat::Unconfirmed { last_status }) => {
            format!("ticked (unconfirmed: still {last_status})")
        }
        Err(RunnerError::NeedsAttention { reason, .. }) => format!("skipped ({reason})"),
        Err(error) => format!("skipped ({error})"),
    }
}

/// `notify.HID_IDLE_AFTER_SECONDS`: the picker alert's own one-minute
/// presence threshold — distinct from `commands::presence`'s own
/// five-minute `PRESENCE_AWAY_AFTER_SECONDS`, which gates `multiverse
/// presence`'s "away" verdict, not this alert.
const PICKER_MIKE_IDLE_AFTER_SECONDS: f64 = 60.0;

/// `notify.picker_alert`'s own message body, verbatim (`notify.py:1576-1581`).
fn picker_alert_body(
    project_name: &str,
    question: &str,
    options: &[String],
    recommended: &str,
) -> String {
    let options_block = options
        .iter()
        .map(|option| format!("- {option}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Project: {project_name}\nQuestion: {question}\nOptions:\n\
         {options_block}\nRecommended option: {recommended}"
    )
}

/// `hashlib.sha256(key.encode("utf-8")).hexdigest()` — the activity
/// message's own stable id, so a retried post of the same episode is
/// idempotent against [`mv_state::activity::ProjectLog`]'s own
/// append-if-absent dedupe rather than landing twice.
fn picker_message_id(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The hub activity post half of `notify.picker_alert` (`notify.py:1583-
/// 1599`): `project_name -> "multiverse"`, gated on the same
/// `[activity.pairs]` permission `commands::activity::send` evaluates
/// before calling [`mv_state::activity::send_message`] itself — this
/// module has no CLI args to read that gate from, so it recomputes it off
/// `config.activity_pairs` directly. Any outcome short of `Sent`/
/// `AlreadySent` (an outright refusal, or an I/O error) is "not posted",
/// matching Python's own `except Exception: pass`.
fn post_picker_activity(
    config: &Config,
    hub_path: &Path,
    project_name: &str,
    key: &str,
    body: &str,
) -> bool {
    let may_send = config
        .activity_pairs
        .get(project_name)
        .is_some_and(|allowed| allowed.iter().any(|to| to == HUB_PROJECT_NAME));
    let hub_log = mv_state::activity::ProjectLog::open(HUB_PROJECT_NAME, hub_path);
    matches!(
        mv_state::activity::send_message(
            &hub_log,
            &picker_message_id(key),
            project_name,
            HUB_PROJECT_NAME,
            body,
            false,
            None,
            may_send,
            &mv_core::redact::redact_text,
        ),
        Ok(mv_state::activity::SendOutcome::Sent { .. }
            | mv_state::activity::SendOutcome::AlreadySent)
    )
}

/// The ntfy push half of `notify.picker_alert` (`notify.py:1601-1613`):
/// `priority="high"`, same as Python's own call — this is the ntfy header
/// value, not [`mv_state::notify::Priority`]'s cap-bypass flag (only the
/// literal `"urgent"` string bypasses the per-project cap in Python too,
/// so `"high"` takes the ordinary [`mv_state::notify::Priority::Normal`]
/// path here). Reuses `commands::notify`'s own topic/token resolution and
/// curl push rather than a second transport of its own. `true` covers both
/// `Sent` and `Queued` — Python's own `outcome in {"sent", "queued"}` —
/// since a queued copy still counts as this episode's one page.
///
/// `episode_key`/`episode_state` are set the same way Python's own
/// `picker_alert` call sets them (`notify.py:1605-1610`:
/// `episode_key=key, episode_state=str(_state_path(hub.path))`) — what
/// lets `mv_state::notify::flush` drop this exact alert, not resend it,
/// once the picker it was for has resolved (card `20261103-b-a2215b`). No
/// hub project configured leaves both `None`, matching every other
/// `notify_episodes` caller's "a config with no multiverse project has
/// nowhere to keep this state" posture — this send then falls back to an
/// ordinary, never-stale one rather than failing outright.
fn push_picker_ntfy(config: &Config, project_name: &str, key: &str, body: &str) -> bool {
    let state_dir = state_dir_from_config(&config.source_path);
    let topic = super::notify::resolve_topic(&config.source_path);
    let token = super::notify::resolve_token(&config.source_path);
    let episode_state = config
        .projects
        .get(HUB_PROJECT_NAME)
        .and_then(project_path)
        .map(|hub_path| {
            mv_state::notify_episodes::state_path(hub_path)
                .to_string_lossy()
                .into_owned()
        });
    let record = mv_state::notify::QueuedNotification {
        project: project_name.to_string(),
        title: format!("multiverse: {project_name} picker waiting"),
        message: body.to_string(),
        priority: "high".to_string(),
        dedup_key: Some(key.to_string()),
        queued_at: mv_core::time::now_stamp(),
        kind: None,
        subject: None,
        tags: Vec::new(),
        episode_key: Some(key.to_string()),
        episode_state,
    };
    mv_state::notify::send(
        &state_dir,
        &record,
        mv_state::notify::Priority::Normal,
        &mut |record| match token.as_deref() {
            Some(token) => {
                super::notify::push_with_token(topic.as_deref(), Some(token), record, "high", &[])
            }
            None => super::notify::push(topic.as_deref(), record, "high", &[]),
        },
    )
    .is_ok_and(|result| !matches!(result.outcome, mv_state::notify::Outcome::Suppressed))
}

/// Card `20261103-b-15d3ca`: the rest of Python's `_report_blocked_picker`
/// (`cli.py:11174-11207`) — [`mv_state::notify_episodes::
/// report_and_alert_picker_episode`]'s own hub activity post and ntfy push,
/// plus the `_mike_is_idle` presence gate (`cli.py:11093-11108`) that
/// decides whether either ever fires. Reads `agent_name`'s own pane and
/// opens/extends/closes its picker episode in the hub project's
/// [`mv_state::notify_episodes`] table exactly as the prior port (card
/// `20261103-b-c5d832`) did. A failed read (Herdr error, or a non-`ok`
/// result) leaves any open episode untouched, exactly as Python's own
/// `try/except` around that read does — a read failure says nothing about
/// whether the picker is still there. No hub project configured is a
/// silent no-op, the same posture every `notify_episodes` caller takes
/// (`notify.py`'s own `picker_alert`: "a config with no multiverse project
/// has nowhere to keep this state").
///
/// `apply` is `!dry_run` at the one call site below — `_sweep_tick_
/// orchestrator`'s own `apply` parameter, threaded through
/// `_detect_and_report_picker`'s `mike_idle = idle() if apply ... else
/// False` and into `picker_alert`'s/`close_picker_episode`'s own
/// `dry_run=not apply`. A dry run still reads the pane and still computes
/// what the episode table *would* say, but persists nothing and never
/// pages — matching every other `--dry-run` leg in this file.
///
/// Does not touch the returned tick outcome at all — see this file's own
/// module doc for why that is deliberate, not an oversight.
fn report_blocked_picker(
    project: &Project,
    config: &Config,
    agent_name: &str,
    herdr: &mut impl Herdr,
    now: OffsetDateTime,
    apply: bool,
) {
    let Some(hub) = config.projects.get(HUB_PROJECT_NAME) else {
        return;
    };
    if project_path(hub).is_none() {
        return;
    }
    let Ok(read) = herdr.call(
        &herdr_argv::agent_read(agent_name, Some("visible"), None),
        StdDuration::from_secs(30),
    ) else {
        return;
    };
    if !read.ok() {
        return;
    }
    report_picker_text(project, config, &read.stdout, now, apply);
}

fn close_picker_episode(project: &Project, config: &Config, apply: bool) {
    if let Some(hub_path) = config.projects.get(HUB_PROJECT_NAME).and_then(project_path) {
        mv_state::notify_episodes::close_picker_episode_on_disk(hub_path, &project.name, apply);
    }
}

/// Report from the already-read text so a done/menu picker is never
/// re-read between classification and episode bookkeeping.
fn report_picker_text(
    project: &Project,
    config: &Config,
    text: &str,
    now: OffsetDateTime,
    apply: bool,
) {
    let Some(hub_path) = config.projects.get(HUB_PROJECT_NAME).and_then(project_path) else {
        return;
    };
    match parse_orchestrator_picker(text) {
        Some((question, options, recommended)) => {
            let mike_idle = apply
                && super::presence::hid_idle_seconds()
                    .is_some_and(|seconds| seconds >= PICKER_MIKE_IDLE_AFTER_SECONDS);
            let body = picker_alert_body(&project.name, &question, &options, &recommended);
            let key = mv_state::notify_episodes::picker_key(&project.name, &question);
            mv_state::notify_episodes::report_and_alert_picker_episode(
                hub_path,
                &project.name,
                &question,
                now,
                apply,
                mike_idle,
                || post_picker_activity(config, hub_path, &project.name, &key, &body),
                || push_picker_ntfy(config, &project.name, &key, &body),
            );
        }
        None => {
            mv_state::notify_episodes::close_picker_episode_on_disk(hub_path, &project.name, apply);
        }
    }
}

/// `cli.PICKER_TAIL_LINES`: only the pane's last few non-blank lines are
/// ever considered a picker menu.
const PICKER_TAIL_LINES: usize = 30;

/// `cli._PICKER_POINTERS`, verbatim.
const PICKER_POINTERS: [&str; 4] = ["❯", "›", ">", "»"];

/// `cli._PICKER_FOOTER_MARKERS`, verbatim (the same fixed vocabulary
/// `mv_engine::sweep_nudges::DIALOG_MENU_MARKERS` already ports for the
/// nudge path's own menu-tail scan — duplicated here rather than shared,
/// since nothing in this crate currently exposes that constant outside
/// `mv-engine`).
const PICKER_FOOTER_MARKERS: [&str; 5] = [
    "enter to select",
    "esc to cancel",
    "enter confirm",
    "esc not now",
    "↑/↓ choose",
];

fn starts_with_any(text: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| text.starts_with(prefix))
}

/// `cli._PICKER_NUMBER_RE.match`: `^\s*(\d+)[.)]\s+(?P<option>.+?)\s*$`.
/// Returns the trimmed option text when `line` is a numbered row.
fn picker_numbered_option(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let digits_len = trimmed
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(trimmed.len());
    if digits_len == 0 {
        return None;
    }
    let mut rest = trimmed[digits_len..].chars();
    let separator = rest.next()?;
    if separator != '.' && separator != ')' {
        return None;
    }
    let after_separator = rest.as_str();
    if after_separator == after_separator.trim_start() {
        // `\s+` requires at least one whitespace char right after the
        // separator.
        return None;
    }
    let option = after_separator.trim();
    if option.is_empty() {
        None
    } else {
        Some(option)
    }
}

/// `cli._PICKER_RECOMMENDED_RE`: `\s+\((?:recommended|default)\)\s*$`,
/// case-insensitive. Returns `(text with the marker and its leading
/// whitespace removed, whether a marker was found)` — `candidate`
/// unchanged when there is none, matching `re.sub`'s own no-op on no
/// match.
fn strip_recommended_marker(candidate: &str) -> (String, bool) {
    let right_trimmed = candidate.trim_end();
    for marker in ["(recommended)", "(default)"] {
        if let Some(marker_start) = right_trimmed.len().checked_sub(marker.len()) {
            if right_trimmed.is_char_boundary(marker_start)
                && right_trimmed[marker_start..].eq_ignore_ascii_case(marker)
            {
                let before = &right_trimmed[..marker_start];
                if before.ends_with(char::is_whitespace) {
                    return (before.trim_end().to_string(), true);
                }
            }
        }
    }
    (candidate.to_string(), false)
}

/// `cli._is_picker_free_text_option`: whether `candidate` is Claude's
/// free-text entry row (`"Type something…"`/`"‹Type something else›"`),
/// not a real option.
fn is_picker_free_text_option(candidate: &str) -> bool {
    candidate
        .trim_matches(|c| c == '‹' || c == '›' || c == ' ')
        .trim()
        .to_lowercase()
        .starts_with("type something")
}

/// `cli._parse_orchestrator_picker` (`cli.py:10974-11069`), verbatim —
/// extract a question, its options, and the recommended one from an
/// `AskUserQuestion` picker's pane text. `None` when the tail of `text`
/// does not look like one (too few non-footer lines, no question line
/// found, or fewer than two real options survive both the main parse and
/// the compact-picker fallback).
fn parse_orchestrator_picker(text: &str) -> Option<(String, Vec<String>, String)> {
    let mut lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    if lines.len() > PICKER_TAIL_LINES {
        lines = lines.split_off(lines.len() - PICKER_TAIL_LINES);
    }
    let footer = lines
        .iter()
        .position(|line| {
            let lower = line.to_lowercase();
            PICKER_FOOTER_MARKERS
                .iter()
                .any(|marker| lower.contains(marker))
        })
        .unwrap_or(lines.len());
    let menu = &lines[..footer];
    if menu.len() < 3 {
        return None;
    }

    let mut question_index: Option<usize> = None;
    let mut question = String::new();
    for (index, &line) in menu.iter().enumerate() {
        let candidate = line.trim_start_matches(['?', '？', ' ']).trim();
        if starts_with_any(line, &["?", "？"]) && !candidate.is_empty() {
            question_index = Some(index);
            question = candidate.to_string();
        } else if line.contains('?') && !starts_with_any(line, &PICKER_POINTERS) {
            question_index = Some(index);
            question = line.to_string();
        } else if line.to_lowercase().starts_with("question:") {
            question_index = Some(index);
            question = line
                .split_once(':')
                .map(|(_, rest)| rest)
                .unwrap_or("")
                .trim()
                .to_string();
        }
    }
    let question_index = question_index?;
    if question.is_empty() {
        return None;
    }

    let mut wrap_start = question_index;
    while wrap_start > 0 {
        let prior = menu[wrap_start - 1];
        if starts_with_any(prior, &PICKER_POINTERS)
            || picker_numbered_option(prior).is_some()
            || starts_with_any(prior, &["-", "•"])
        {
            break;
        }
        wrap_start -= 1;
    }
    if wrap_start < question_index {
        let mut parts: Vec<&str> = menu[wrap_start..question_index].to_vec();
        parts.push(&question);
        question = parts.join(" ");
    }
    question = question.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut options: Vec<String> = Vec::new();
    let mut recommended: Option<String> = None;
    for &line in &menu[question_index + 1..] {
        let selected = starts_with_any(line, &PICKER_POINTERS);
        let mut candidate = line
            .trim_start_matches(|c| "❯›>» ".contains(c))
            .trim()
            .to_string();
        let numbered = picker_numbered_option(&candidate).map(str::to_string);
        if let Some(option) = &numbered {
            candidate = option.trim().to_string();
        }
        let (stripped, marked_recommended) = strip_recommended_marker(&candidate);
        candidate = stripped;
        if candidate.is_empty() {
            continue;
        }
        let folded = candidate.to_lowercase();
        if folded == "options:" || folded == "answers:" {
            continue;
        }
        if folded.starts_with("recommended:") {
            recommended = Some(
                candidate
                    .split_once(':')
                    .map(|(_, rest)| rest)
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            );
            continue;
        }
        if folded.starts_with("question:") || folded.starts_with("answer:") {
            continue;
        }
        if is_picker_free_text_option(&candidate) {
            continue;
        }
        if marked_recommended || (selected && recommended.is_none()) {
            recommended = Some(candidate.clone());
        }
        if starts_with_any(line, &PICKER_POINTERS)
            || numbered.is_some()
            || starts_with_any(line, &["-", "•"])
        {
            candidate = candidate
                .trim_start_matches(|c| "-• ".contains(c))
                .trim()
                .to_string();
            options.push(candidate);
        }
    }

    if options.len() < 2 {
        options = menu[question_index + 1..]
            .iter()
            .filter(|line| {
                !line.is_empty()
                    && !{
                        let lower = line.to_lowercase();
                        PICKER_FOOTER_MARKERS
                            .iter()
                            .any(|marker| lower.contains(marker))
                    }
                    && !is_picker_free_text_option(
                        line.trim_start_matches(|c| "❯›>» -•".contains(c)).trim(),
                    )
            })
            .map(|line| line.to_string())
            .collect();
        recommended = options.first().cloned();
    }
    if options.len() < 2 {
        return None;
    }
    let recommended = match recommended {
        Some(value) if !value.is_empty() => value,
        _ => options[0].clone(),
    };
    Some((question, options, recommended))
}

/// Card `20261103-b-bb7230` (leg 2 of this module's own "not ported"
/// list): `cli._sweep_tick_gauge`, the opportunistic quota-gauge read off
/// this project's own orchestrator pane — the passive counterpart, for a
/// pane that is *started* rather than dispatched, to the dispatch path's
/// own gauge capture. Returns the single global interactive account's own
/// `(pool, Gauge)` when there is one to read; `None` covers every
/// "nothing to show" case Python's own function does (no interactive
/// account at all — this checkout, like Python, allows at most one,
/// enforced by `mv_config::load`'s own `validate_interactive_seats` — no
/// gauge reader for that account's `kind`, no live orchestrator pane for
/// *this* project, or a pane read that came back empty or gauge-less).
///
/// Reuses the exact same orchestrator-by-identity lookup `tick_outcome`
/// above makes ([`FleetState::orchestrator_agent`]) rather than threading
/// a shared fetch through both — this crate's own per-call `FleetState::
/// fetch` is already the convention `tick_outcome` itself uses, and no
/// existing `sweep-*` conformance scenario configures an interactive
/// account, so this adds no Herdr call to any pinned transcript.
fn tick_gauge(
    project: &Project,
    path: &Path,
    config: &Config,
    herdr: &mut impl Herdr,
) -> Option<(String, mv_engine::limits::Gauge)> {
    let account = config
        .accounts
        .values()
        .find(|account| account.interactive)?;
    if !mv_engine::limits::has_gauge_reader(&account.kind) {
        return None;
    }
    let fleet = FleetState::fetch(herdr, true).ok()?;
    let short = project.short.as_deref().unwrap_or("");
    let (agent_name, _) = mv_engine::fleet_start::find_orchestrator_agent(
        &fleet,
        &project.name,
        path,
        &account.short,
        short,
    )?;
    let gauge = mv_engine::limits::sweep_tick_gauge(herdr, &agent_name, &account.kind)?;
    Some((account.pool.clone(), gauge))
}

/// Finding 4 (part 2 of `converge_stale`): the reviewer-over-timeout
/// report line (`cli.py:12860-12881`) — a live (pane still listed),
/// not-yet-recorded Critic task past `[review_policy].timeout_minutes`
/// since it was last known to be running. Mirrors Python's own
/// `t.state in (RUNNING, STALLED)` with the one simplification this
/// crate's task model already makes elsewhere in this file (no ported
/// STALLED/startup-grace distinction): "still has an open pane" stands in
/// for both. `None` when nothing is overdue.
fn reviewer_over_timeout_from_tasks(
    tasks: &[mv_state::tasks::SweepTask],
    timeout_minutes: u32,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
) -> Option<String> {
    let limit = time::Duration::minutes(i64::from(timeout_minutes));
    // Find every not-yet-recorded Critic past the timeout by dispatch
    // time alone *before* ever asking Herdr for a listing: every pinned
    // `sweep-*` conformance transcript scripts an exact Herdr call count,
    // and the overwhelming common case (no overdue Critic at all) must
    // add no call beyond what `sweep_project` already made this pass.
    let mut candidates: Vec<&mv_state::tasks::SweepTask> = tasks
        .iter()
        .filter(|task| task.role == "reviewer" && !task.recorded)
        .filter(|task| {
            task.started_at
                .or(task.dispatched_at)
                .is_some_and(|last_seen| now - last_seen > limit)
        })
        .collect();
    if candidates.is_empty() {
        return None;
    }
    let live = agents.list();
    candidates.retain(|task| {
        task.pane
            .as_deref()
            .is_some_and(|pane| live.iter().any(|agent| agent.pane_id == pane))
    });
    let mut overdue: Vec<String> = candidates
        .iter()
        .map(|task| {
            let last_seen = task
                .started_at
                .or(task.dispatched_at)
                .expect("filtered to Some above");
            format!(
                "{} ({})",
                task.task_id,
                mv_state::tasks::format_elapsed(now - last_seen)
            )
        })
        .collect();
    overdue.sort();
    if overdue.is_empty() {
        None
    } else {
        Some(overdue.join(", "))
    }
}

/// Close a backlog item whose linked task is now recorded `done` and whose
/// own branch has independently landed. Mirrors `cli._close_merged_backlog_links`
/// — returns `(item id, evidence sha, recorded)` for every item closed (or,
/// under `apply=false`, that would be).
///
/// The branch disposition is deliberately checked only for a linked
/// *writer* task already recorded `done`, so an unrelated recorded-blocked
/// task — especially one with a live human-facing pane — is never cleared
/// by this housekeeping pass. [`mv_state::tasks::auto_supersede`] rewrites
/// a stale BLOCKED/AWAITING record's own status to `done` in place once
/// its branch lands too — a correction to the task log's own bookkeeping,
/// not this task's own outcome — so a task whose `outcome_state` is
/// [`SUPERSEDED_MERGED`]/[`SUPERSEDED_REDISPATCHED`] is excluded here even
/// though its `status` now reads `done`: that is the one place the
/// pre-supersede verdict (superseded vs. genuinely done) still survives.
fn close_merged_backlog_links(
    root: &Path,
    tasks: &[SweepTask],
    apply: bool,
) -> Vec<(String, String, bool)> {
    let linked: Vec<_> = read_backlog_items(root, false)
        .into_iter()
        .filter(|item| !item.task_id.is_empty() && item.state != "done")
        .collect();
    if linked.is_empty() {
        return Vec::new();
    }
    let by_id: HashMap<&str, &SweepTask> = tasks.iter().map(|t| (t.task_id.as_str(), t)).collect();

    let mut closed = Vec::new();
    for item in &linked {
        let Some(&task) = by_id.get(item.task_id.as_str()) else {
            continue;
        };
        if task.status != Some(mv_state::tasks::TaskStatus::Done) {
            continue;
        }
        let outcome_state = task.outcome_state.as_deref().unwrap_or("");
        if outcome_state == SUPERSEDED_MERGED || outcome_state == SUPERSEDED_REDISPATCHED {
            continue;
        }
        if !is_writing_role(&task.role) {
            continue;
        }
        let Some(evidence) = merged_evidence(root, lane_task_id(task)) else {
            continue;
        };
        let mut recorded = false;
        if apply {
            match backlog_done(root, &item.item, &evidence, None) {
                Ok(_) => recorded = true,
                Err(BacklogError::TerminalItem { .. }) => continue,
                Err(_) => continue,
            }
        }
        closed.push((item.item.clone(), evidence, recorded));
    }
    closed
}

/// Card `20261103-fixer-2faef1` finding 1: `matched` comes from `mail_index`,
/// loaded once before this pass's per-project loop (so eight holders do not
/// re-parse the same eight logs) — but `holder_log` can have moved on since
/// that load, closed by a concurrent `multiverse reply`/`close_ask` landing
/// while an earlier project in this same pass was still being swept.
/// `close_ask` itself never re-checks, so writing straight from the stale
/// `matched` list would append a second, spurious close. Re-reads
/// `holder_log` fresh for each item immediately before writing it, and
/// returns only the asks this call actually closed, so the printed line
/// matches what was written.
fn close_still_open_unlinked_replies(
    holder_log: &mv_state::activity::ProjectLog,
    matched: Vec<mv_state::activity::UnlinkedReply>,
) -> Vec<mv_state::activity::UnlinkedReply> {
    let mut closed = Vec::new();
    for item in matched {
        if mv_state::activity::is_closed(holder_log, item.ask.seq).unwrap_or(false) {
            continue;
        }
        let _ = mv_state::activity::close_ask(
            holder_log,
            item.ask.seq,
            &format!(
                "answered by unlinked send #{} to {}",
                item.answer.seq, item.ask.from_project
            ),
            &|s: &str| s.to_string(),
        );
        closed.push(item);
    }
    closed
}

/// Reuse the sweep's initial roster to rule out a missing Nexus before
/// fetching workspace/pane facts for a tick or gauge. Like Python's
/// `_orchestrator_agent_of`, a name must match exactly once; accept legacy
/// `orch` names as well as the current `nexu` names during the cutover.
fn orchestrator_listed(
    project: &Project,
    config: &Config,
    live: &[mv_state::tasks::LiveAgentStatus],
) -> bool {
    let Some(account) = config.accounts.values().find(|account| account.interactive) else {
        return false;
    };
    let short = project.short.as_deref().unwrap_or("");
    for name in [
        mv_engine::fleet_start::nexus_agent_name(&project.name, &account.short, short),
        mv_engine::fleet_start::nexus_legacy_agent_name(&project.name, &account.short, short),
    ] {
        let Ok(name) = name else { continue };
        let matches = live
            .iter()
            .filter(|agent| agent.name.as_deref() == Some(name.as_str()))
            .count();
        if matches > 0 {
            return matches == 1;
        }
    }
    false
}

/// Whether `tasks_snapshot` (below) must re-read the log rather than reuse
/// this pass's in-memory `tasks`. Card `20261103-fixer-2faef1` finding 3:
/// `report`'s four counters name every write `sweep_project` itself can
/// make, but the nudge leg that runs between the snapshot and this check
/// writes too — a stuck-resume escalation's `record_stranded_closure`
/// (`sweep_nudges.rs:519`) appends `event=result status=failed` on a pass
/// where all four of `report`'s counters stay zero.
fn tasks_snapshot_needs_reread(
    report: &mv_state::tasks::SweepReport,
    nudges: &mv_engine::sweep_nudges::NudgeOutcome,
) -> bool {
    report.recorded > 0
        || report.expired > 0
        || report.reclaimed > 0
        || !report.superseded.is_empty()
        || nudges.resume.iter().any(|r| r.failed)
}

fn run_with(
    args: &ArgMatches,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
    host_report: &mut impl FnMut(&Config, bool, OffsetDateTime) -> Vec<String>,
    processes: ProcessProbe,
) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        return Err(Failure::Config(
            "no fleet state configured; run `multiverse doctor`".to_owned(),
        ));
    }
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;
    let state_dir = state_dir_from_config(&config_path);
    let halt_state = mv_state::halt::read_halt_state(&state_dir);
    let halted = halt_state.halted;

    let dry_run = args.get_flag("dry-run");
    let tick = args.get_flag("tick");

    let mut projects: Vec<&Project> = config.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    if projects.is_empty() {
        return Err(Failure::Config(
            "no fleet state configured; run `multiverse doctor`".to_owned(),
        ));
    }

    // Finding 7 (critic-sweep-board.md): a fleet-wide lock, so two
    // overlapping hub ticks never both record/reclaim the same pass.
    // Mirrors `cmd_sweep`/`_sweep_lock`: a dry run never takes it (a
    // preview needs no mutual exclusion), and a lock this process cannot
    // even open fails open rather than refusing to sweep at all — the
    // lock exists to avoid a double-write race, not to gate every run on
    // its own filesystem health.
    let _lock_guard = if dry_run {
        None
    } else {
        match mv_state::tasks::try_acquire_sweep_lock(&state_dir) {
            Ok(mv_state::tasks::SweepLock::Acquired(guard)) => Some(guard),
            Ok(mv_state::tasks::SweepLock::Held { pid, since }) => {
                println!("another sweep is running (pid {pid}, since {since}); skipped");
                return Ok(());
            }
            Err(_) => None,
        }
    };

    // Python `_sweep` (`cli.py:12592-12594`) prints `_fleet_halt_line`
    // only `if halt is not None` — an unhalted fleet gets no banner line
    // at all here (unlike `doctor`, which does echo every run with the
    // same helper). Found via `conformance/diff_fuzz.py` seed 50001.
    if halted {
        println!("{}", mv_state::halt::format_halt_line(&halt_state));
    }

    let lease_minutes = config.awaiting_lease_minutes;
    let overrides = lease_overrides(&config);
    let now = mv_core::time::now_utc();
    let stale_only = args.get_flag("stale-only");
    let timing = Timing::default();
    let review_timeout_minutes = config.review_policy.timeout_minutes;

    // `find_unlinked_replies`'s own `replied` scan is wider than the
    // projects this pass actually sweeps (mirrors `open_ask_messages`'s
    // `all_projects` — a reply lands in the *asker's* own log, which can
    // be any configured project, not only the one currently being swept).
    let all_project_roots: Vec<(String, PathBuf)> = config
        .projects
        .values()
        .filter_map(|p| project_path(p).map(|path| (p.name.clone(), path.to_path_buf())))
        .collect();
    // One pass over every routing.log for unlinked-reply detection, not
    // one per holder × peer × open ask.
    let mail_index = mv_state::activity::MailIndex::load(&all_project_roots);

    // Python takes one `process_cwd_snapshot()` before the per-project
    // loop (`cli.py:12657`) and reuses it; tests inject a snapshot so the
    // live probe never runs against the real host.
    let lane_processes = resolve_process_probe(&processes);
    let mut recorded_tasks: BTreeMap<String, String> = BTreeMap::new();
    let mut running_worktrees: Vec<PathBuf> = Vec::new();

    // Card `20261102-b-fd9159`: host-wide, not per-project (`hostsleep.py`'s
    // own placement, [`mv_engine::hostsleep::report`]'s own doc comment) —
    // run once here, against the `multiverse` hub project's own root, and
    // skip entirely when that project is not configured (nowhere to keep
    // the watermark/queue). Never affects this command's own exit code:
    // every failure inside `report` already collapses to an empty,
    // unauthoritative [`mv_engine::hostsleep::SleepReport`].
    //
    // Critic-codex-areas.md H1: captured ahead of the per-project loop
    // below (Python's own placement, `cli.py:12629-12648`) as this pass's
    // own sleep total, read back out of the captured lines —
    // `hostsleep::SleepReport` carries no raw-minutes field of its own,
    // the same list-only shape `hostsleep.total_sleep_minutes` reads on
    // the Python side — so [`mv_state::tasks::expire_awaiting`] can
    // subtract it from a task's wall-clock elapsed before the lease gates
    // on it, instead of reading a `now` that silently includes however
    // long the host was unconscious.
    let mut sleep_minutes: i64 = 0;
    let mut hostsleep_lines = Vec::new();
    if let Some(hub_path) = config.projects.get("multiverse").and_then(project_path) {
        let sleep_report = mv_engine::hostsleep::report(hub_path, now, !dry_run);
        sleep_minutes = sleep_minutes_from_lines(&sleep_report.lines);
        hostsleep_lines = sleep_report.lines;
    }

    // Card `20261105-b-421801` gap 1: one Herdr listing for the whole
    // reconcile/reclaim pass, never one per project — Python's own
    // `listing = run(herdr.agent_list())` (`cli.py:12604`, docstring at
    // `12661`), fetched once here and threaded into every project's own
    // [`mv_state::tasks::sweep_project_with_live`] call below. `agents`
    // itself is still passed to each call too: a non-dry-run reclaim still
    // takes its own *second* listing, immediately before closing that
    // project's own candidates (Python's `_confirm_reclaim_candidates`,
    // never shared across projects either).
    let live = agents.list_checked();
    for project in &projects {
        let Some(path) = project_path(project) else {
            // An unfilled `<<PLACEHOLDER: ...>>` path has nothing on disk
            // to sweep; `doctor` already flags this project separately.
            continue;
        };
        let (report, tasks) = mv_state::tasks::sweep_project_with_live(
            path,
            dry_run,
            lease_minutes,
            &overrides,
            now,
            sleep_minutes,
            live.as_deref(),
            agents,
        );
        // Python: `if not args.dry_run and live is not None: _record_orchestrator_pass`
        // (`cli.py:12698-12699`). A dry-run preview must not stamp; a real
        // pass always has a live-agents seam here (FakeAgents in tests,
        // HerdrAgents in production), matching `live is not None`.
        if !dry_run {
            record_orchestrator_pass(path, now);
        }
        let mut notes = Vec::new();
        // Keep all project findings on the same summary line, in Python's
        // order; host-wide reports and tick outcomes follow separately.
        if !report.awaiting_gone.is_empty() {
            let named: Vec<String> = report
                .awaiting_gone
                .iter()
                .map(|t| format!("{} ({})", t.task_id, t.age))
                .collect();
            notes.push(format!("awaiting, pane gone: {}", named.join(", ")));
        }
        if !report.awaiting_partial.is_empty() {
            let named: Vec<String> = report
                .awaiting_partial
                .iter()
                .map(|t| format!("{} ({})", t.task_id, t.age))
                .collect();
            notes.push(format!("awaiting, partial result: {}", named.join(", ")));
        }
        // Card `20261102-b-b4ede0`: the three one-shot claim-then-act
        // nudges ([`mv_engine::sweep_nudges`]'s own module doc comment has
        // the full narrowing list) — a halted fleet forces `apply=false`
        // the same way `--dry-run` does (Python's own `nudge_halt`/
        // `resume_halt`/`cap_halt` re-reads, folded here into the one
        // `halted` this loop already captured at the top of the run: see
        // that module's doc comment for why a halt arriving mid-pass is
        // not re-checked three times here). Printed ahead of the
        // supersede/backlog-close/unlinked-reply legs below: those three
        // settle records for work already finished, this settles lanes
        // still running — see this task's result.md for why the relative
        // order of these two groups could not be pinned to an exact
        // `converge_stale` line range.
        let nudges_apply = !dry_run && !halted;
        let nudges =
            mv_engine::sweep_nudges::run(path, &tasks, &config, nudges_apply, now, agents, herdr);
        // Python's standalone stuck visibility (`cli.py:12730-12740`): printed
        // on every confirmed-stuck pass, including after the one-shot resume
        // has already fired, ahead of the acting nudge/resume/cap fragments.
        for note in &nudges.stuck {
            notes.push(note.clone());
        }
        let visible_as = |dry_run: bool, halted: bool, sent: bool| dry_run || halted || sent;
        let mut nudged_ids: Vec<&str> = nudges
            .finish
            .iter()
            .filter(|n| visible_as(dry_run, halted, n.sent))
            .map(|n| n.task_id.as_str())
            .collect();
        nudged_ids.sort_unstable();
        if !nudged_ids.is_empty() {
            let label = if dry_run || halted {
                "would nudge"
            } else {
                "nudged"
            };
            notes.push(format!("{label}: {}", nudged_ids.join(", ")));
        }
        let mut resumed_ids: Vec<&str> = nudges
            .resume
            .iter()
            .filter(|r| visible_as(dry_run, halted, r.sent))
            .map(|r| r.task_id.as_str())
            .collect();
        resumed_ids.sort_unstable();
        if !resumed_ids.is_empty() {
            let label = if dry_run || halted {
                "would resume"
            } else {
                "resumed"
            };
            notes.push(format!("{label}: {}", resumed_ids.join(", ")));
        }
        let mut capped_ids: Vec<&str> = nudges
            .cap
            .iter()
            .filter(|c| visible_as(dry_run, halted, c.sent))
            .map(|c| c.task_id.as_str())
            .collect();
        capped_ids.sort_unstable();
        if !capped_ids.is_empty() {
            let label = if dry_run || halted {
                "would cap"
            } else {
                "capped"
            };
            notes.push(format!("{label}: {}", capped_ids.join(", ")));
        }

        if !report.superseded.is_empty() {
            let named: Vec<String> = report
                .superseded
                .iter()
                .map(|s| format!("{} ({})", s.task_id, s.state))
                .collect();
            notes.push(format!("superseded: {}", named.join(", ")));
        }

        // Named per task with its own free-text reason — Python's
        // `expired: <task> (<reason>)` fragment (`cli.py:12850-12851`),
        // same upgrade from a bare count [`report.superseded`] already got
        // (card `20261102-b-0d8b17`).
        if !report.expired_tasks.is_empty() {
            let named: Vec<String> = report
                .expired_tasks
                .iter()
                .map(|e| format!("{} ({})", e.task_id, e.state))
                .collect();
            notes.push(format!("expired: {}", named.join(", ")));
        }

        // Card `20260929-b-fbd213` (part B): closing a backlog item linked
        // to a task `auto_supersede` just left (or already found) `done`,
        // whose own branch has independently landed. Reads a fresh
        // snapshot rather than `report`'s own internal one, so this always
        // sees whatever `sweep_project` just wrote above (reconcile,
        // supersede, expiry) — a `--dry-run` pass, which wrote nothing,
        // sees exactly what it started with, same as Python's own
        // in-place-mutated-`found` posture under `apply=False`.
        // Re-read only when this pass actually wrote: reconcile/expiry
        // do not mutate the in-memory snapshot, so a fresh log walk is
        // required to see those records. A no-write pass (the common
        // case) reuses `tasks` as Python's in-place `found` would. See
        // `tasks_snapshot_needs_reread`'s own doc comment for the nudge
        // leg's write, folded into this condition too.
        let tasks_snapshot = if tasks_snapshot_needs_reread(&report, &nudges) {
            mv_state::tasks::sweep_tasks(path)
        } else {
            tasks.clone()
        };
        let merged_backlog = close_merged_backlog_links(path, &tasks_snapshot, !dry_run);
        if !merged_backlog.is_empty() {
            let label = if dry_run { "would close" } else { "closed" };
            let named: Vec<String> = merged_backlog
                .iter()
                .map(|(item_id, sha, _recorded)| format!("{item_id} (evidence {sha})"))
                .collect();
            notes.push(format!("{label} backlog: {}", named.join(", ")));
        }

        if let Some(named) =
            reviewer_over_timeout_from_tasks(&tasks, review_timeout_minutes, now, agents)
        {
            notes.push(format!("reviewer over timeout: {named}"));
        }
        for note in dirty_done_notes(path, &tasks, now) {
            notes.push(note);
        }

        // Card `20261021-fixer-23c807`: an open ask this project holds
        // that a later, unlinked ordinary message to the same asker may
        // have already answered in words. Best-effort, like every other
        // leg in this loop — an unreadable log anywhere in the fleet
        // skips this one project's line rather than failing the pass.
        if let Ok(index) = &mail_index {
            let unlinked = mv_state::activity::find_unlinked_replies_indexed(&project.name, index);
            let (matched, possible): (Vec<_>, Vec<_>) =
                unlinked.into_iter().partition(|item| item.references_ask);
            if !matched.is_empty() {
                let label = if dry_run { "would close" } else { "closed" };
                let matched = if dry_run {
                    matched
                } else {
                    let holder_log =
                        mv_state::activity::ProjectLog::open(project.name.as_str(), path);
                    close_still_open_unlinked_replies(&holder_log, matched)
                };
                if !matched.is_empty() {
                    let named: Vec<String> = matched
                        .iter()
                        .map(|item| {
                            format!(
                                "#{} ({}, answered by unlinked send #{})",
                                item.ask.seq, item.ask.from_project, item.answer.seq
                            )
                        })
                        .collect();
                    notes.push(format!("{label}: {}", named.join(", ")));
                }
            }
            if !possible.is_empty() {
                let named: Vec<String> = possible
                    .iter()
                    .map(|item| {
                        format!(
                            "#{} (ask #{} from {})",
                            item.answer.seq, item.ask.seq, item.ask.from_project
                        )
                    })
                    .collect();
                notes.push(format!("possibly answered by {}", named.join(", ")));
            }
        }
        if let Some(behind) = mv_state::activity::mail_cursor_behind(path) {
            notes.push(format!("mail cursor behind by {behind}"));
        }
        for note in lane_process_notes(path, &tasks_snapshot, lane_processes.as_deref()) {
            notes.push(note);
        }
        println!("{}", project_summary_line(&project.name, &report, &notes));
        for task in &tasks_snapshot {
            if task.recorded {
                recorded_tasks.insert(task.task_id.clone(), project.name.clone());
            } else {
                let lane = path.join(".multiverse/worktrees").join(lane_task_id(task));
                if lane.is_dir() {
                    running_worktrees.push(lane);
                }
            }
        }
    }
    // Python prints host facts once after every project's summary: Docker,
    // captured sleep intervals, disk, then stale scratch. Sleep was probed
    // before reconciliation so leases can account for the same intervals.
    let host_lines = host_report(&config, !dry_run, now);
    let disk_start = host_lines
        .iter()
        .position(|line| line.starts_with("disk:"))
        .unwrap_or(host_lines.len());
    for line in host_lines[..disk_start]
        .iter()
        .chain(&hostsleep_lines)
        .chain(&host_lines[disk_start..])
    {
        println!("{line}");
    }

    // Card `20261019-fixer-29a027`: host-wide stale temp report, after the
    // per-project loop so it can reuse this pass's recorded task ids.
    // Unit tests skip the live `/tmp` scan (injected size tests live in
    // `hostdisk::stale_temp_report_with`); the production binary always runs it.
    #[cfg(not(test))]
    {
        for line in mv_engine::hostdisk::stale_temp_report(
            &config,
            &recorded_tasks,
            now,
            &running_worktrees,
        ) {
            println!("{line}");
        }
    }
    #[cfg(test)]
    {
        let _ = (recorded_tasks, running_worktrees, now, &config);
    }
    // Finding `cli.py:12495-12507`/`13081-13097` (leg 3 of this module's own
    // "not ported" list): a stale, drained Codex gauge can block all new
    // work, which also stops the session-log writes that would otherwise
    // refresh it. Once per pool, before any project's own tick, spend at
    // most one cheap `codex exec` recovery probe
    // ([`mv_engine::limits::sweep_tick_codex_probe`] carries its own
    // 30-minute cross-process dedupe, so a probe already recorded by a
    // concurrent sweep is skipped here too). Gated on `--tick` — nothing
    // resembling this runs on a bare `sweep` — `!dry_run` (this execs a
    // live completion, real spend, never previewed), and this pass's own
    // `halted` snapshot; unlike Python's own re-read immediately before
    // this block, this reuses the single snapshot `tick_outcome`'s own call
    // site below already does, rather than adding a second, this-leg-only
    // freshness check this file does not otherwise make (see this task's
    // result.md).
    if tick && !dry_run && !halted {
        let mut probed_pools: HashSet<&str> = HashSet::new();
        let mut accounts: Vec<&mv_config::load::Account> = config.accounts.values().collect();
        accounts.sort_by(|a, b| a.name.cmp(&b.name));
        for account in accounts {
            if probed_pools.contains(account.pool.as_str()) {
                continue;
            }
            if mv_engine::limits::sweep_tick_codex_probe(
                account,
                config.dispatch_min_quota_percent,
                &state_dir,
                now,
            ) {
                probed_pools.insert(account.pool.as_str());
            }
        }
    }

    for project in projects {
        let Some(path) = project_path(project) else {
            continue;
        };
        // Card `20261103-b-bb7230`: runs unconditionally ahead of the
        // `--stale-only` skip below, same as Python's own placement — a
        // stale-only-skipped project's orchestrator pane is still worth
        // reading, since this never types into or sends anything. Gated on
        // `!halted` only (matching the `if args.tick and halt is None`
        // guard Python wraps this whole leg in), never on `dry_run`: the
        // *read* always happens; only the persisted write below is a
        // write. Python collects every project's reading into one `pool ->
        // Gauge` map and persists it once after the whole loop so two
        // projects sharing the global interactive seat write that pool's
        // store at most once per pass; writing immediately here reaches
        // the same final on-disk state without holding that map, since a
        // later project's reading for the same pool would simply overwrite
        // an earlier one's either way.
        let listed = live
            .as_deref()
            .is_some_and(|live| orchestrator_listed(project, &config, live));
        if tick && !halted && listed {
            if let Some((pool, gauge)) = tick_gauge(project, path, &config, herdr) {
                if !dry_run {
                    mv_engine::limits::record_gauge(&state_dir, &pool, &gauge, now);
                }
            }
        }
        if tick {
            let fresh_skip = stale_only.then(|| stale_only_skip(path, now)).flatten();
            let outcome = if halted {
                "skipped (fleet halted)".to_string()
            } else if live.is_none() {
                "skipped (herdr unreachable)".to_string()
            } else if let Some(skip) = fresh_skip {
                skip
            } else if !listed {
                close_picker_episode(project, &config, !dry_run);
                "skipped (no Nexus)".to_string()
            } else {
                tick_outcome(project, path, &config, herdr, dry_run, &timing, now)
            };
            println!("{}: {outcome}", project.name);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    fn run_with(
        args: &clap::ArgMatches,
        agents: &mut impl mv_state::tasks::AgentPanes,
        herdr: &mut impl mv_engine::runner::Herdr,
    ) -> Result<(), crate::exit::Failure> {
        super::run_with(
            args,
            agents,
            herdr,
            &mut |_, _, _| Vec::new(),
            super::ProcessProbe::Snapshot(Some(Vec::new())),
        )
    }

    use super::*;
    use crate::args;
    use mv_herdr::argv::HerdrCommand;
    use mv_herdr::envelope::CommandResult;
    use mv_state::tasks::LiveAgent;
    use serde_json::json;
    use std::time::Duration;

    #[test]
    fn sleep_minutes_from_lines_sums_both_line_shapes_and_floors_a_capped_window_critic_h1() {
        let lines = vec![
            "host: slept 2026-11-04T01:00:00Z-2026-11-04T02:05:00Z (65 min, battery)".to_string(),
            "host: slept from before 2026-11-04T00:00:00Z-2026-11-04T02:00:00Z (120+ min, AC)"
                .to_string(),
            "not a sleep line at all".to_string(),
        ];
        assert_eq!(sleep_minutes_from_lines(&lines), 65 + 120);
        assert_eq!(sleep_minutes_from_lines(&[]), 0);
    }

    struct FakeAgents {
        panes: Vec<String>,
    }

    impl AgentPanes for FakeAgents {
        fn list(&mut self) -> Vec<LiveAgent> {
            self.panes
                .iter()
                .map(|pane_id| LiveAgent {
                    pane_id: pane_id.clone(),
                })
                .collect()
        }
        fn close(&mut self, pane_id: &str) -> bool {
            self.panes.retain(|pane| pane != pane_id);
            true
        }
    }

    /// [`FakeAgents`] carries no agent name, which `AgentPanes::list_checked`'s
    /// own default (card `20261103-b-695405`) reports as `name: None` — fine
    /// for every test above that never reaches a nudge/resume/cap send, but
    /// `sweep_nudges`'s own fresh-listing identity re-check (added for that
    /// same card) now requires the pane's live agent name to equal the
    /// dispatching task's recorded `agent=`, so a test asserting an actual
    /// send must be able to report one. `(pane_id, name)` pairs, not the
    /// `AgentState` every entry reports `Working` regardless of what the
    /// send path itself gates on elsewhere (readiness, if any, is checked
    /// off the same listing before this one's ever consulted).
    struct NamedFakeAgents {
        panes: Vec<(String, String)>,
    }

    impl AgentPanes for NamedFakeAgents {
        fn list(&mut self) -> Vec<LiveAgent> {
            self.panes
                .iter()
                .map(|(pane_id, _)| LiveAgent {
                    pane_id: pane_id.clone(),
                })
                .collect()
        }
        fn close(&mut self, pane_id: &str) -> bool {
            self.panes.retain(|(pane, _)| pane != pane_id);
            true
        }
        fn list_checked(&mut self) -> Option<Vec<mv_state::tasks::LiveAgentStatus>> {
            Some(
                self.panes
                    .iter()
                    .map(|(pane_id, name)| mv_state::tasks::LiveAgentStatus {
                        pane_id: pane_id.clone(),
                        name: Some(name.clone()),
                        status: Some(mv_herdr::envelope::AgentState::Working),
                    })
                    .collect(),
            )
        }
    }

    /// A [`Herdr`] that must never be called — every existing test below
    /// either never ticks, or only ticks a halted/no-orchestrator/fresh
    /// project, none of which ever reach Herdr.
    struct NullHerdr;

    impl Herdr for NullHerdr {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            panic!("unexpected Herdr call: {:?}", command.argv);
        }
    }

    /// A single-project, single-pane fake backing finding 1's own tick
    /// tests: one workspace/pane/agent triple an operator would see for a
    /// freshly opened project, plus a scripted landing sequence for
    /// `agent get` (`deliver_heartbeat`'s own polling loop).
    struct TickFake {
        project_root: std::path::PathBuf,
        agent_status: &'static str,
        pane_text: &'static str,
        landed_status: &'static str,
        prompt_calls: u32,
    }

    impl Herdr for TickFake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let ok = |payload: serde_json::Value| CommandResult {
                argv: command.argv.clone(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
                payload: Some(payload),
                protocol_error: None,
            };
            match (command.argv[1].as_str(), command.argv[2].as_str()) {
                ("workspace", "list") => Ok(ok(json!({"result": {"workspaces": [
                    {"workspace_id": "w1", "label": "sandbox"},
                ]}}))),
                ("pane", "list") => Ok(ok(json!({"result": {"panes": [
                    {"pane_id": "w1:p1", "workspace_id": "w1", "cwd": self.project_root.to_string_lossy()},
                ]}}))),
                ("agent", "list") => Ok(ok(json!({"result": {"agents": [
                    {"name": "sandbox-nexu-al", "pane_id": "w1:p1", "agent_status": self.agent_status, "state_change_seq": 1},
                ]}}))),
                ("pane", "read") | ("agent", "read") => Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: self.pane_text.to_string(),
                    stderr: String::new(),
                    payload: None,
                    protocol_error: None,
                }),
                ("agent", "prompt") => {
                    self.prompt_calls += 1;
                    Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    })
                }
                ("agent", "get") => Ok(ok(json!({"result": {"agent": {
                    "name": "sandbox-nexu-al",
                    "pane_id": "w1:p1",
                    "agent_status": self.landed_status,
                    "state_change_seq": 2,
                }}}))),
                other => panic!("unexpected call {other:?}"),
            }
        }
    }

    #[test]
    fn summary_line_matches_every_sweep_conformance_scenario() {
        use mv_state::tasks::SweepReport;

        assert_eq!(
            project_summary_line(
                "sandbox",
                &SweepReport {
                    in_progress_before: 1,
                    in_progress_after: 1,
                    recorded: 1,
                    reclaimed: 0,

                    expired: 0,
                    ..Default::default()
                },
                &[],
            ),
            "sandbox: in progress 1 -> 1, recorded 1, reclaimed 0",
            "sweep-dry-run-preview"
        );
        assert_eq!(
            project_summary_line(
                "sandbox",
                &SweepReport {
                    in_progress_before: 1,
                    in_progress_after: 0,
                    recorded: 1,
                    reclaimed: 0,

                    expired: 0,
                    ..Default::default()
                },
                &[],
            ),
            "sandbox: in progress 1 -> 0, recorded 1, reclaimed 0",
            "sweep-reconcile-records-outcome"
        );
        assert_eq!(
            project_summary_line("sandbox", &SweepReport::default(), &[]),
            "sandbox: in progress 0 -> 0, recorded 0, reclaimed 0",
            "sweep-under-halt-tick-skipped"
        );
        assert_eq!(
            project_summary_line(
                "sandbox",
                &SweepReport {
                    in_progress_before: 0,
                    in_progress_after: 0,
                    recorded: 0,
                    reclaimed: 1,

                    expired: 0,
                    ..Default::default()
                },
                &[],
            ),
            "sandbox: in progress 0 -> 0, recorded 0, reclaimed 1",
            "sweep-reclaim-closes-clean-pane"
        );
    }

    #[test]
    fn summary_line_joins_project_notes_exactly_like_python() {
        let report = mv_state::tasks::SweepReport {
            in_progress_before: 2,
            in_progress_after: 2,
            ..Default::default()
        };
        let notes = [
            "awaiting, pane gone: task-a (2 hr), task-b (1 hr)".to_string(),
            "expired: task-a (lease expired), task-b (lease expired)".to_string(),
            "warning: process-cwd probe unknown".to_string(),
        ];
        for (count, expected) in [
            (0, "sandbox: in progress 2 -> 2, recorded 0, reclaimed 0"),
            (1, "sandbox: in progress 2 -> 2, recorded 0, reclaimed 0; awaiting, pane gone: task-a (2 hr), task-b (1 hr)"),
            (3, "sandbox: in progress 2 -> 2, recorded 0, reclaimed 0; awaiting, pane gone: task-a (2 hr), task-b (1 hr); expired: task-a (lease expired), task-b (lease expired); warning: process-cwd probe unknown"),
        ] {
            assert_eq!(project_summary_line("sandbox", &report, &notes[..count]), expected);
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("sweep").unwrap().clone()
    }

    #[test]
    fn unconfigured_sweep_needs_config() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-unconfigured-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let config_arg = dir.join("missing-config.toml");
        let mut agents = FakeAgents { panes: Vec::new() };
        let err = run_with(
            &matches(&["sweep", "--config", &config_arg.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap_err();
        assert_eq!(err.code(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn write_sandbox_config(
        dir: &std::path::Path,
        project_root: &std::path::Path,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n",
                project_root.display()
            ),
        )
        .unwrap();
        config_path
    }

    /// Same as [`write_sandbox_config`], with one interactive seat
    /// configured — the tick's own precondition for ever looking for a
    /// Nexus orchestrator at all.
    fn write_sandbox_config_with_interactive_seat(
        dir: &std::path::Path,
        project_root: &std::path::Path,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n\n\
                 [accounts.alpha]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"<<PLACEHOLDER: x>>\"\n\
                 command = \"claude-interactive\"\n\
                 interactive = true\n",
                project_root.display()
            ),
        )
        .unwrap();
        config_path
    }

    fn load_sandbox(config_path: &std::path::Path) -> mv_config::load::Config {
        mv_config::load::load_config(config_path).unwrap()
    }

    #[test]
    fn reports_a_per_project_summary_line_and_reconciles_a_settled_task() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-reconcile-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa"))
            .unwrap();
        std::fs::write(
            project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
        )
        .unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let log = std::fs::read_to_string(project_root.join(".multiverse/routing.log")).unwrap();
        assert!(log.lines().last().unwrap().contains("state=reconciled"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_sweep_stamps_orchestrator_pass_byte_identical_to_python() {
        let dir = sandbox_project_root();
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut FakeAgents { panes: Vec::new() },
            &mut NullHerdr,
        )
        .unwrap();
        let marker = project_root.join(".multiverse/orchestrator-pass");
        let body = std::fs::read_to_string(&marker).unwrap();
        let (reconcile, rest) = body
            .strip_suffix('\n')
            .unwrap()
            .split_once('\t')
            .expect("tab-separated reconcile/backlog");
        let (backlog_key, stamp) = rest.split_once('=').unwrap();
        assert_eq!(backlog_key, "backlog");
        assert_eq!(reconcile, format!("reconcile={stamp}"));
        assert_eq!(body, format!("reconcile={stamp}\tbacklog={stamp}\n"));
        assert_eq!(stamp.len(), "2026-11-05T12:00:00+00:00".len());
        assert!(stamp.ends_with("+00:00"), "{stamp}");
        mv_core::time::parse_flexible(stamp).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_dry_run_sweep_does_not_stamp_orchestrator_pass() {
        let dir = sandbox_project_root();
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);
        run_with(
            &matches(&[
                "sweep",
                "--dry-run",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut FakeAgents { panes: Vec::new() },
            &mut NullHerdr,
        )
        .unwrap();
        assert!(
            !project_root.join(".multiverse/orchestrator-pass").exists(),
            "a dry-run must not stamp orchestrator-pass"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_done_dirty_writing_lane_needs_a_decision() {
        let dir = sandbox_project_root();
        let root = dir.join("sandbox");
        std::fs::create_dir_all(root.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\trole=fixer\taccount=old-seat\tbranch=multiverse/20250101-fixer-aaaaaa\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
        )
        .unwrap();
        let lane = root.join(".multiverse/worktrees/20250101-fixer-aaaaaa");
        std::fs::create_dir_all(&lane).unwrap();
        for args in [
            ["init", "-q", "-b", "main"].as_slice(),
            ["config", "user.email", "lane@example.invalid"].as_slice(),
            ["config", "user.name", "Lane Test"].as_slice(),
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&lane)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        std::fs::write(lane.join("uncommitted.txt"), "dirty\n").unwrap();

        let tasks = mv_state::tasks::sweep_tasks(&root);
        let now = time::OffsetDateTime::from_unix_timestamp(1_609_459_200).unwrap(); // 2021-01-01
        let notes = dirty_done_notes(&root, &tasks, now);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with("needs a decision: 20250101-fixer-aaaaaa (done, "),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("uncommitted.txt"), "{}", notes[0]);
        assert!(notes[0].ends_with("h)"), "{}", notes[0]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn closed_lane_processes_are_reported_and_capped_and_unknown_warns() {
        let dir = sandbox_project_root();
        let root = dir.join("sandbox");
        let task_id = "20250101-fixer-aaaaaa";
        std::fs::create_dir_all(root.join(".multiverse/tasks").join(task_id)).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=fixer\taccount=old-seat\tbranch=multiverse/{task_id}\n\
                 2020-01-01T00:05:00+00:00\tevent=started\ttask={task_id}\tpane=w1:p1\n\
                 2020-01-01T01:00:00+00:00\tevent=result\ttask={task_id}\tstatus=done\tstate=done\trevisions=0\n"
            ),
        )
        .unwrap();
        let lane = root.join(".multiverse/worktrees").join(task_id);
        std::fs::create_dir_all(lane.join("nested")).unwrap();
        let tasks = mv_state::tasks::sweep_tasks(&root);
        assert!(tasks.iter().any(|t| t.recorded), "{tasks:?}");

        let rows: Vec<LaneProcess> = (0..8)
            .map(|i| LaneProcess {
                pid: 4200 + i,
                cwd: lane.join("nested"),
                command: "yes".into(),
                cpu_percent: "99.5".into(),
            })
            .collect();
        let notes = lane_process_notes(&root, &tasks, Some(&rows));
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].starts_with(&format!(
                "lane processes still running: {task_id} pid 4200 yes 99.5%"
            )),
            "{}",
            notes[0]
        );
        assert!(notes[0].contains("pid 4204 yes"), "{}", notes[0]);
        assert!(!notes[0].contains("pid 4205 yes"), "{}", notes[0]);
        assert!(notes[0].contains("(+3 more)"), "{}", notes[0]);

        let sibling = LaneProcess {
            pid: 9,
            cwd: PathBuf::from(format!("{}-other", lane.display())),
            command: "yes".into(),
            cpu_percent: "1.0".into(),
        };
        let mut mixed = rows.clone();
        mixed.push(sibling);
        assert_eq!(
            closed_lane_processes(&root, &tasks, &mixed)
                .iter()
                .map(|(id, p)| (id.as_str(), p.pid))
                .collect::<Vec<_>>(),
            (0..8).map(|i| (task_id, 4200 + i)).collect::<Vec<_>>()
        );

        assert_eq!(
            lane_process_notes(&root, &tasks, None),
            ["warning: process-cwd probe unknown"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_running_lane_process_is_not_reported() {
        let dir = sandbox_project_root();
        let root = dir.join("sandbox");
        let task_id = "20250101-fixer-bbbbbb";
        std::fs::create_dir_all(root.join(".multiverse/tasks").join(task_id)).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=fixer\taccount=old-seat\tbranch=multiverse/{task_id}\n\
                 2020-01-01T00:05:00+00:00\tevent=started\ttask={task_id}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let lane = root.join(".multiverse/worktrees").join(task_id);
        std::fs::create_dir_all(&lane).unwrap();
        let tasks = mv_state::tasks::sweep_tasks(&root);
        assert!(tasks.iter().all(|t| !t.recorded), "{tasks:?}");
        let rows = vec![LaneProcess {
            pid: 4200,
            cwd: lane.clone(),
            command: "yes".into(),
            cpu_percent: "99.5".into(),
        }];
        assert!(lane_process_notes(&root, &tasks, Some(&rows)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Guards `MULTIVERSE_PMSET_BIN` behind the crate-wide process-env lock
    /// (`commands::worktrees::herdr_env`) so writers of this var serialize
    /// with every other remaining env-mutating test in this binary.
    struct PmsetBinGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl PmsetBinGuard {
        fn set(value: &std::path::Path) -> Self {
            let lock = crate::commands::worktrees::herdr_env::lock();
            let previous = std::env::var("MULTIVERSE_PMSET_BIN").ok();
            std::env::set_var("MULTIVERSE_PMSET_BIN", value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for PmsetBinGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("MULTIVERSE_PMSET_BIN", value),
                None => std::env::remove_var("MULTIVERSE_PMSET_BIN"),
            }
        }
    }

    /// A scripted `pmset -g log` stand-in: exits 0 with an empty log, so
    /// `mv_engine::hostsleep::report`'s probe succeeds without shelling out
    /// to the real macOS-only binary (AGENTS.md: never exercise the real
    /// fleet, or anything it would reach, during tests).
    fn write_fake_pmset(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("fake-pmset.sh");
        std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    #[test]
    fn card_fd9159_hostsleep_report_runs_once_host_wide_against_the_hub_project() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-hostsleep-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        let hub_root = dir.join("hub");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(hub_root.join(".multiverse")).unwrap();
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n\n[projects.multiverse]\npath = \"{}\"\n",
                project_root.display(),
                hub_root.display(),
            ),
        )
        .unwrap();
        let _pmset = PmsetBinGuard::set(&write_fake_pmset(&dir.join("bin")));

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        // `apply = !dry_run` persisted both hostsleep markers for the hub
        // project — proof `report()` actually ran, not just compiled in.
        assert!(mv_state::hostsleep_store::read_timestamp(
            &hub_root,
            mv_state::hostsleep_store::PMSET_STATE_FILENAME
        )
        .is_some());
        // Never written for a project that is not the configured hub.
        assert!(mv_state::hostsleep_store::read_timestamp(
            &project_root,
            mv_state::hostsleep_store::PMSET_STATE_FILENAME
        )
        .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn card_fd9159_a_dry_run_never_persists_the_hostsleep_markers() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-hostsleep-dry-run-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let hub_root = dir.join("hub");
        std::fs::create_dir_all(hub_root.join(".multiverse")).unwrap();
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[projects.multiverse]\npath = \"{}\"\n", hub_root.display()),
        )
        .unwrap();
        let _pmset = PmsetBinGuard::set(&write_fake_pmset(&dir.join("bin")));

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&[
                "sweep",
                "--config",
                &config_path.to_string_lossy(),
                "--dry-run",
            ]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        assert!(mv_state::hostsleep_store::read_timestamp(
            &hub_root,
            mv_state::hostsleep_store::PMSET_STATE_FILENAME
        )
        .is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fleet_wide_lease_minutes_override_reaches_expire_awaiting() {
        // Finding 4: `[review_policy]`-adjacent fleet config
        // (`awaiting_lease_minutes`) must actually reach
        // `mv_state::tasks::expire_awaiting`, not just live in `Config`
        // unused.
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-expire-awaiting-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
        )
        .unwrap();
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "awaiting_lease_minutes = 1\n[projects.sandbox]\npath = \"{}\"\n",
                project_root.display()
            ),
        )
        .unwrap();

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let log = std::fs::read_to_string(project_root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.lines().last().unwrap().contains("status=failed"),
            "a 1-minute fleet lease must have already expired this long-stranded task"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tick_under_halt_prints_a_skip_line_per_project() {
        let dir =
            std::env::temp_dir().join(format!("mv-cli-sweep-test-halt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(&project_root).unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);
        let state_dir = state_dir_from_config(&config_path);
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("halt"),
            "{\"reason\": \"testing\", \"since\": \"2026-01-01T00:00:00+00:00\"}\n",
        )
        .unwrap();

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&[
                "sweep",
                "--tick",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sweep_already_running_skips_the_whole_pass_instead_of_racing_it() {
        // Finding 7: a fleet-wide lock, so two overlapping hub ticks never
        // both reconcile/reclaim the same pass. Holding the lock exactly
        // the way a concurrent sweep would, this run must touch nothing —
        // the stale task below would otherwise be reconciled.
        let dir =
            std::env::temp_dir().join(format!("mv-cli-sweep-test-locked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa"))
            .unwrap();
        std::fs::write(
            project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            project_root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        )
        .unwrap();
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
        )
        .unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);
        let state_dir = state_dir_from_config(&config_path);

        let held = mv_state::tasks::try_acquire_sweep_lock(&state_dir).unwrap();
        let mv_state::tasks::SweepLock::Acquired(_guard) = held else {
            panic!("the lock must be free at the start of this test");
        };

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let log = std::fs::read_to_string(project_root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log.lines().count(),
            2,
            "a sweep that lost the lock must touch nothing"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- tick_outcome (finding 1, card 20261102-b-4f86db) ------------------

    /// Card `20261103-b-08ec29`: a raw PID-plus-nanos name alone can collide
    /// when two sweep tests run as threads of the same cargo-test binary and
    /// land on the same tick, the same bug class fixed in
    /// `commands::worktrees`'s own `unique_suffix()`
    /// (card `20261102-b-9235c6`) — so this pairs the nanos with a
    /// process-wide atomic counter the same way.
    fn sandbox_project_root() -> std::path::PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-tick-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn initial_roster_requires_one_named_current_or_legacy_nexus() {
        let root = sandbox_project_root();
        let config_path = write_sandbox_config_with_interactive_seat(&root.join("config"), &root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];
        assert!(!orchestrator_listed(project, &config, &[]));
        for (name, expected) in [
            ("sandbox-nexu-al", true),
            ("sandbox-orch-al", true),
            ("sandbox-buil-al", false),
            ("other-nexu-al", false),
        ] {
            let live = mv_state::tasks::LiveAgentStatus {
                pane_id: "w1:p1".to_string(),
                name: Some(name.to_string()),
                status: Some(mv_herdr::envelope::AgentState::Working),
            };
            assert_eq!(
                orchestrator_listed(project, &config, std::slice::from_ref(&live)),
                expected
            );
            assert!(!orchestrator_listed(
                project,
                &config,
                &[live.clone(), live]
            ));
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tick_outcome_sends_the_heartbeat_once_and_confirms_landing() {
        let project_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "idle",
            pane_text: "Ready >",
            landed_status: "working",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            mv_core::time::now_utc(),
        );
        assert_eq!(outcome, "ticked");
        assert_eq!(fake.prompt_calls, 1);

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn tick_outcome_dry_run_never_sends_but_still_reports_ticked() {
        let project_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "idle",
            pane_text: "Ready >",
            landed_status: "idle",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            true,
            &Timing::default(),
            mv_core::time::now_utc(),
        );
        assert_eq!(outcome, "ticked");
        assert_eq!(
            fake.prompt_calls, 0,
            "a dry run must never call `agent prompt`"
        );

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn tick_outcome_reports_no_orchestrator_without_an_interactive_account() {
        let project_root = sandbox_project_root();
        let config_path = write_sandbox_config(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        // NullHerdr panics on any call: no interactive seat means this must
        // never even try to list workspaces/panes/agents.
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut NullHerdr,
            false,
            &Timing::default(),
            mv_core::time::now_utc(),
        );
        assert_eq!(outcome, "skipped (no Nexus)");

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn tick_outcome_reports_working_and_blocked_without_resending() {
        let project_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        for (status, expected) in [
            ("working", "skipped (working)"),
            ("unknown", "skipped (unknown)"),
        ] {
            let mut fake = TickFake {
                project_root: project_root.clone(),
                agent_status: status,
                pane_text: "Ready >",
                landed_status: "idle",
                prompt_calls: 0,
            };
            let outcome = tick_outcome(
                project,
                &project_root,
                &config,
                &mut fake,
                false,
                &Timing::default(),
                mv_core::time::now_utc(),
            );
            assert_eq!(outcome, expected, "status {status}");
            assert_eq!(fake.prompt_calls, 0);
        }

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn tick_outcome_refuses_to_type_into_a_trust_dialog() {
        let project_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "idle",
            pane_text: "Do you trust the files in this folder?",
            landed_status: "idle",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            mv_core::time::now_utc(),
        );
        assert_eq!(outcome, "skipped (dialog)");
        assert_eq!(fake.prompt_calls, 0);

        let _ = std::fs::remove_dir_all(&project_root);
    }

    // --- blocked picker reporting (card 20261103-b-c5d832) ------------------

    /// Same as [`write_sandbox_config_with_interactive_seat`], plus a
    /// `multiverse` hub project at `hub_root` — [`report_blocked_picker`]'s
    /// own precondition for ever opening an episode at all.
    fn write_sandbox_config_with_hub(
        dir: &std::path::Path,
        project_root: &std::path::Path,
        hub_root: &std::path::Path,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::create_dir_all(hub_root).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n\n\
                 [projects.multiverse]\npath = \"{}\"\n\n\
                 [accounts.alpha]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"<<PLACEHOLDER: x>>\"\n\
                 command = \"claude-interactive\"\n\
                 interactive = true\n",
                project_root.display(),
                hub_root.display(),
            ),
        )
        .unwrap();
        config_path
    }

    const PICKER_PANE_TEXT: &str = "How should I close the download check?\n\
         ❯ 1. Click it\n  2. Leave it open\n\n\
         Enter to select · ↑/↓ to navigate · Esc to cancel\n";

    #[test]
    fn a_done_picker_is_never_ticked_and_preserves_its_episode() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];
        let start = time::macros::datetime!(2026-10-28 12:00:00 UTC);
        let key = "picker_blocked|sandbox|How should I close the download check?";
        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "done",
            pane_text: PICKER_PANE_TEXT,
            landed_status: "idle",
            prompt_calls: 0,
        };

        // Dry-run still detects menus but cannot open an episode.
        assert_eq!(
            tick_outcome(
                project,
                &project_root,
                &config,
                &mut fake,
                true,
                &Timing::default(),
                start
            ),
            "skipped (menu)"
        );
        assert!(mv_state::notify_episodes::load(&hub_root).0.is_empty());
        for (status, minutes, expected) in [
            ("done", 0, "skipped (menu)"),
            ("blocked", 5, "skipped (blocked)"),
            ("done", 15, "skipped (menu)"),
        ] {
            fake.agent_status = status;
            assert_eq!(
                tick_outcome(
                    project,
                    &project_root,
                    &config,
                    &mut fake,
                    false,
                    &Timing::default(),
                    start + time::Duration::minutes(minutes)
                ),
                expected
            );
            let (table, _) = mv_state::notify_episodes::load(&hub_root);
            assert_eq!(
                table[key].first_seen,
                mv_core::time::format_seconds_utc(start)
            );
        }
        assert_eq!(
            fake.prompt_calls, 0,
            "a picker must never receive tick text"
        );
        // TickFake panics on every unsupported command, including send-keys.
        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn done_pane_classification_only_closes_confirmed_clear_episodes() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let mut config = load_sandbox(&config_path);
        config.accounts.get_mut("alpha").unwrap().kind = "codex".into();
        let project = &config.projects["sandbox"];
        let now = time::macros::datetime!(2026-10-28 12:00:00 UTC);
        for (text, expected, closes) in [
            ("Ready >", "ticked", true),
            ("Enter to select · Esc to cancel", "skipped (menu)", true),
            (
                "Do you trust the files in this folder?",
                "skipped (dialog)",
                false,
            ),
            (
                include_str!("../../../../tests/fixtures/rate-limit/codex-out-of-credits.txt"),
                "skipped (dialog)",
                false,
            ),
            (
                include_str!("../../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt"),
                "skipped (dialog)",
                false,
            ),
            ("", "skipped (unreadable)", false),
        ] {
            mv_state::notify_episodes::report_picker_episode(
                &hub_root,
                "sandbox",
                "Old question?",
                now,
            );
            for dry_run in [true, false] {
                let mut fake = TickFake {
                    project_root: project_root.clone(),
                    agent_status: "done",
                    pane_text: text,
                    landed_status: "working",
                    prompt_calls: 0,
                };
                assert_eq!(
                    tick_outcome(
                        project,
                        &project_root,
                        &config,
                        &mut fake,
                        dry_run,
                        &Timing::default(),
                        now
                    ),
                    expected,
                    "text={text:?}, dry_run={dry_run}"
                );
                assert_eq!(
                    fake.prompt_calls,
                    u32::from(!dry_run && expected == "ticked")
                );
                assert_eq!(
                    mv_state::notify_episodes::load(&hub_root).0.is_empty(),
                    closes && !dry_run
                );
            }
        }
        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn failed_done_pane_reads_never_prompt_or_close_the_episode() {
        struct FailedRead(TickFake);
        impl Herdr for FailedRead {
            fn call(
                &mut self,
                command: &HerdrCommand,
                timeout: Duration,
            ) -> Result<CommandResult, RunnerError> {
                if command.argv[1..3] == ["agent", "read"] {
                    return Err(RunnerError::Refused("fake read failure".into()));
                }
                self.0.call(command, timeout)
            }
        }
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];
        let now = time::macros::datetime!(2026-10-28 12:00:00 UTC);
        mv_state::notify_episodes::report_picker_episode(
            &hub_root,
            "sandbox",
            "Old question?",
            now,
        );
        let before = mv_state::notify_episodes::load(&hub_root).0;
        let mut fake = FailedRead(TickFake {
            project_root: project_root.clone(),
            agent_status: "done",
            pane_text: PICKER_PANE_TEXT,
            landed_status: "idle",
            prompt_calls: 0,
        });
        for dry_run in [true, false] {
            assert_eq!(
                tick_outcome(
                    project,
                    &project_root,
                    &config,
                    &mut fake,
                    dry_run,
                    &Timing::default(),
                    now
                ),
                "skipped (unreadable)"
            );
            assert_eq!(fake.0.prompt_calls, 0);
            assert_eq!(mv_state::notify_episodes::load(&hub_root).0, before);
        }
        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn a_blocked_picker_opens_an_episode_and_keeps_first_seen_across_a_flap() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];
        let key = "picker_blocked|sandbox|How should I close the download check?";

        let start = time::macros::datetime!(2026-10-28 12:00:00 UTC);
        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "blocked",
            pane_text: PICKER_PANE_TEXT,
            landed_status: "idle",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            start,
        );
        assert_eq!(outcome, "skipped (blocked)");
        let (table, _) = mv_state::notify_episodes::load(&hub_root);
        let first_seen = table
            .get(key)
            .expect("the picker episode was opened")
            .first_seen
            .clone();
        assert!(!table[key].alerted, "freshly opened, not yet min-age-gated");

        // A re-sighting 15 minutes later keeps the original first_seen,
        // still short of the 30-minute alert threshold.
        let flapped = start + time::Duration::minutes(15);
        tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            flapped,
        );
        let (table, _) = mv_state::notify_episodes::load(&hub_root);
        assert_eq!(
            table[key].first_seen, first_seen,
            "a re-sighting of the same picker must not reset first_seen"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn a_blocked_pane_with_no_picker_leaves_no_episode_and_a_failed_read_leaves_it_untouched() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "blocked",
            pane_text: "Waiting on a slow build to finish...",
            landed_status: "idle",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            mv_core::time::now_utc(),
        );
        assert_eq!(outcome, "skipped (blocked)");
        let (table, _) = mv_state::notify_episodes::load(&hub_root);
        assert!(table.is_empty(), "no picker text means no episode opens");

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn a_blocked_pane_closes_a_previously_open_episode_once_the_picker_is_gone() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];
        let now = mv_core::time::now_utc();

        mv_state::notify_episodes::report_picker_episode(
            &hub_root,
            "sandbox",
            "Old question?",
            now,
        );
        let (table, _) = mv_state::notify_episodes::load(&hub_root);
        assert!(
            !table.is_empty(),
            "sanity: the episode exists before the tick"
        );

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "blocked",
            pane_text: "Nothing picker-shaped here.",
            landed_status: "idle",
            prompt_calls: 0,
        };
        let outcome = tick_outcome(
            project,
            &project_root,
            &config,
            &mut fake,
            false,
            &Timing::default(),
            now,
        );
        assert_eq!(outcome, "skipped (blocked)");
        let (table, _) = mv_state::notify_episodes::load(&hub_root);
        assert!(
            table.is_empty(),
            "a confirmed clear pane closes the episode"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    // --- picker alert wiring (card 20261103-b-15d3ca) -----------------------

    #[test]
    fn picker_alert_body_matches_pythons_own_format() {
        let body = picker_alert_body(
            "sandbox",
            "How should I close the download check?",
            &["Click it".to_string(), "Leave it open".to_string()],
            "Click it",
        );
        assert_eq!(
            body,
            "Project: sandbox\n\
             Question: How should I close the download check?\n\
             Options:\n\
             - Click it\n\
             - Leave it open\n\
             Recommended option: Click it"
        );
    }

    #[test]
    fn picker_message_id_matches_pythons_sha256_hexdigest() {
        // `hashlib.sha256(b"picker_blocked|sandbox|Q?").hexdigest()`,
        // computed independently (`shasum -a 256`) and pinned here so this
        // id can never silently drift from Python's own `msg_id`.
        assert_eq!(
            picker_message_id("picker_blocked|sandbox|Q?"),
            "416ff9e97721ab1d67e47e5fa8d13ecfc79655f34f1180d29289f2599501010e"
        );
    }

    #[test]
    fn post_picker_activity_succeeds_via_the_implicit_hub_grant() {
        // Decided change 2 (`20261108-b-81cb2b`): a configured `multiverse`
        // project is implicitly activity-paired with every other project,
        // so a project -> hub send needs no explicit `[activity.pairs]`
        // grant anymore. This replaces
        // `post_picker_activity_is_refused_without_an_activity_pairs_grant`,
        // whose premise (no grant exists) is now false for any config that
        // configures a hub project at all — `write_sandbox_config_with_hub`
        // writes no `[activity.pairs]` table, so the grant here is entirely
        // implicit.
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);

        let posted = post_picker_activity(&config, &hub_root, "sandbox", "key", "body");
        assert!(
            posted,
            "a configured hub implicitly grants sandbox -> multiverse"
        );
        let hub_log = mv_state::activity::ProjectLog::open(HUB_PROJECT_NAME, &hub_root);
        assert_eq!(hub_log.inbox_messages().unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn post_picker_activity_is_refused_when_no_hub_project_is_configured() {
        // `merge_hub_activity_pairs` (`mv-config`) only grants the implicit
        // pairing when `[projects.multiverse]` is configured at all; once it
        // is, every other project gets the grant unconditionally — there is
        // no longer a way to configure a hub and still have `may_send` read
        // false for some other project. The one refusal path
        // `post_picker_activity`'s own `may_send` check still supports is
        // this one: no `[projects.multiverse]` table, so
        // `config.activity_pairs` carries nothing for `sandbox` at all.
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path = project_root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n",
                project_root.display()
            ),
        )
        .unwrap();
        let config = load_sandbox(&config_path);

        let posted = post_picker_activity(&config, &hub_root, "sandbox", "key", "body");
        assert!(!posted, "no multiverse project is configured at all");
        let hub_log = mv_state::activity::ProjectLog::open(HUB_PROJECT_NAME, &hub_root);
        assert!(hub_log.inbox_messages().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn post_picker_activity_lands_once_and_is_idempotent_on_retry() {
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path = project_root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n\n\
                 [projects.multiverse]\npath = \"{}\"\n\n\
                 [activity]\npairs = {{ sandbox = [\"multiverse\"] }}\n",
                project_root.display(),
                hub_root.display(),
            ),
        )
        .unwrap();
        let config = load_sandbox(&config_path);

        assert!(post_picker_activity(
            &config,
            &hub_root,
            "sandbox",
            "picker_blocked|sandbox|Q?",
            "the body"
        ));
        assert!(
            post_picker_activity(
                &config,
                &hub_root,
                "sandbox",
                "picker_blocked|sandbox|Q?",
                "the body"
            ),
            "a retry of the same key is the same message id, already sent"
        );
        let hub_log = mv_state::activity::ProjectLog::open(HUB_PROJECT_NAME, &hub_root);
        let messages = hub_log.inbox_messages().unwrap();
        assert_eq!(messages.len(), 1, "idempotent retry must not duplicate");
        assert_eq!(messages[0].body, "the body");

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    #[test]
    fn push_picker_ntfy_queues_when_no_topic_is_configured() {
        // No `ntfy-topic` file next to `config.toml`, matching every other
        // test in this crate: the push closure never reaches a real
        // network, but `notify::send` still treats the attempt as
        // delivered ("queued"), the same as Python's own unreachable-push
        // outcome.
        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        let config = load_sandbox(&config_path);

        assert!(push_picker_ntfy(
            &config,
            "sandbox",
            "picker_blocked|sandbox|Q?",
            "the body"
        ));
        let state_dir = state_dir_from_config(&config.source_path);
        let outbox = mv_state::notify::load_outbox(&state_dir.join("notify-outbox.jsonl")).unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].project, "sandbox");
        assert_eq!(outbox[0].message, "the body");
        assert_eq!(
            outbox[0].episode_key.as_deref(),
            Some("picker_blocked|sandbox|Q?")
        );
        assert_eq!(
            outbox[0].episode_state.as_deref(),
            Some(
                mv_state::notify_episodes::state_path(&hub_root)
                    .to_string_lossy()
                    .into_owned()
                    .as_str()
            )
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    struct NtfyUrlGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl NtfyUrlGuard {
        fn set(value: &str) -> Self {
            let lock = crate::commands::worktrees::herdr_env::lock();
            let previous = std::env::var("MULTIVERSE_NTFY_URL").ok();
            std::env::set_var("MULTIVERSE_NTFY_URL", value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for NtfyUrlGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("MULTIVERSE_NTFY_URL", value),
                None => std::env::remove_var("MULTIVERSE_NTFY_URL"),
            }
        }
    }

    #[test]
    fn push_picker_ntfy_sends_authorization_bearer_when_a_token_is_configured() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::sync::{Arc, Mutex};

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(None::<String>));
        let seen_thread = Arc::clone(&seen);
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(false).expect("blocking accept");
            let (mut stream, _) = listener.accept().expect("stub ntfy accepts");
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                match stream.read(&mut tmp) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let text = String::from_utf8_lossy(&buf);
            let auth = text.lines().find_map(|line| {
                let line = line.trim_end_matches('\r');
                line.split_once(':').and_then(|(name, value)| {
                    name.eq_ignore_ascii_case("Authorization")
                        .then(|| value.trim().to_string())
                })
            });
            *seen_thread.lock().unwrap() = auth;
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });

        let project_root = sandbox_project_root();
        let hub_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_hub(&project_root.join("config"), &project_root, &hub_root);
        std::fs::write(config_path.with_file_name("ntfy-topic"), "test-topic\n").unwrap();
        std::fs::write(
            config_path.with_file_name("ntfy-token"),
            "test-token-cf8f26\n",
        )
        .unwrap();
        let config = load_sandbox(&config_path);
        let _guard = NtfyUrlGuard::set(&format!("http://{addr}"));

        assert!(push_picker_ntfy(
            &config,
            "sandbox",
            "picker_blocked|sandbox|Q?",
            "the body"
        ));
        server.join().expect("stub ntfy thread");
        let auth = seen.lock().unwrap().clone();
        assert_eq!(
            auth.as_deref(),
            Some("Bearer test-token-cf8f26"),
            "picker-alert push must send the configured token"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&hub_root);
    }

    // --- parse_orchestrator_picker (`cli._parse_orchestrator_picker`) -------

    #[test]
    fn picker_recommended_marker_wins_even_off_the_pointer_line() {
        let text = "Which fix should ship first?\n\
             1. Patch the parser (Recommended)\n\
            ❯ 2. Rewrite the module\n\
              3. Type something else.\n\n\
            Enter to select · ↑/↓ to navigate · Esc to cancel\n";
        let (_question, _options, recommended) = parse_orchestrator_picker(text).unwrap();
        assert_eq!(recommended, "Patch the parser");
    }

    #[test]
    fn picker_type_something_row_excluded_from_two_row_compact_options() {
        let text = "Should we deploy now?\n\
            ‹Yes, deploy›\n\
            No, wait\n\
            Type something else\n\n\
            Enter to select · Esc to cancel\n";
        let (_question, options, _recommended) = parse_orchestrator_picker(text).unwrap();
        assert!(!options.iter().any(|o| o == "Type something else"));
        assert_eq!(options.len(), 2);
    }

    #[test]
    fn picker_question_wraps_across_lines_without_losing_its_prefix() {
        let text = "How should I handle the extremely long download verification step\n\
            that keeps timing out on slow connections?\n\
            ❯ 1. Retry with backoff\n\
              2. Skip verification\n\n\
            Enter to select · Esc to cancel\n";
        let (question, _options, _recommended) = parse_orchestrator_picker(text).unwrap();
        assert_eq!(
            question,
            "How should I handle the extremely long download verification step \
             that keeps timing out on slow connections?"
        );
    }

    #[test]
    fn non_picker_pane_text_parses_as_none() {
        assert!(parse_orchestrator_picker("Ready >").is_none());
        assert!(parse_orchestrator_picker("").is_none());
    }

    // --- tick_gauge (card 20261103-b-bb7230) --------------------------------

    #[test]
    fn tick_gauge_reads_the_interactive_seats_own_orchestrator_pane() {
        let project_root = sandbox_project_root();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "working",
            pane_text: "You've used 91% of your weekly limit · resets Oct 19, 2026, 5:30 PM (IST)",
            landed_status: "working",
            prompt_calls: 0,
        };
        let (pool, gauge) = tick_gauge(project, &project_root, &config, &mut fake).unwrap();
        assert_eq!(pool, "alpha", "pool defaults to the account name");
        assert_eq!(gauge.percent_left, 9.0);
        assert_eq!(fake.prompt_calls, 0, "a gauge read must never prompt");

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn tick_gauge_is_none_without_an_interactive_account() {
        let project_root = sandbox_project_root();
        let config_path = write_sandbox_config(&project_root.join("config"), &project_root);
        let config = load_sandbox(&config_path);
        let project = &config.projects["sandbox"];

        // NullHerdr panics on any call: no interactive seat means this must
        // never ask Herdr anything, same contract as `tick_outcome`'s own
        // "no Nexus" case.
        assert_eq!(
            tick_gauge(project, &project_root, &config, &mut NullHerdr),
            None
        );

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn a_tick_persists_the_interactive_seats_gauge_reading_but_only_without_dry_run() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        std::fs::write(project_root.join(".multiverse/routing.log"), "").unwrap();
        let config_path =
            write_sandbox_config_with_interactive_seat(&project_root.join("config"), &project_root);
        let state_dir = state_dir_from_config(&config_path);

        let mut agents = NamedFakeAgents {
            panes: vec![("w1:p1".to_string(), "sandbox-nexu-al".to_string())],
        };
        let mut fake = TickFake {
            project_root: project_root.clone(),
            agent_status: "idle",
            pane_text: "You've used 91% of your weekly limit · resets Oct 19, 2026, 5:30 PM (IST)",
            landed_status: "idle",
            prompt_calls: 0,
        };
        run_with(
            &matches(&[
                "sweep",
                "--tick",
                "--dry-run",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut fake,
        )
        .unwrap();
        assert!(
            mv_state::gauge::load(&state_dir).is_empty(),
            "a dry run must read but never persist"
        );

        run_with(
            &matches(&[
                "sweep",
                "--tick",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut fake,
        )
        .unwrap();
        let gauges = mv_state::gauge::load(&state_dir);
        let entry = &gauges["alpha"];
        assert_eq!(entry.percent_left, Some(9.0));
        assert_eq!(entry.label.as_deref(), Some("weekly limit"));
        assert_eq!(entry.vendor.as_deref(), Some("claude"));

        let _ = std::fs::remove_dir_all(&project_root);
    }

    // --- stale_only_skip ----------------------------------------------------

    #[test]
    fn stale_only_skip_is_none_when_the_project_has_never_looked() {
        let project_root = sandbox_project_root();
        assert_eq!(
            stale_only_skip(&project_root, mv_core::time::now_utc()),
            None
        );
        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn stale_only_skip_names_a_recently_seen_project_as_fresh() {
        let project_root = sandbox_project_root();
        let state_dir = project_root.join(".multiverse");
        std::fs::create_dir_all(&state_dir).unwrap();
        let now = mv_core::time::now_utc();
        mv_state::activity::write_marker(&state_dir, 1).unwrap();
        let skip = stale_only_skip(&project_root, now).unwrap();
        assert!(skip.starts_with("skipped (fresh, "), "{skip}");
        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn stale_only_skip_is_none_once_the_marker_is_old() {
        let project_root = sandbox_project_root();
        let state_dir = project_root.join(".multiverse");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("activity.lastseen"),
            "1\t2000-01-01T00:00:00+00:00\n",
        )
        .unwrap();
        assert_eq!(
            stale_only_skip(&project_root, mv_core::time::now_utc()),
            None
        );
        let _ = std::fs::remove_dir_all(&project_root);
    }

    // --- reviewer_over_timeout_line (finding 4, part 2) ---------------------

    #[test]
    fn reviewer_over_timeout_line_names_a_long_running_reviewer_pane() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let old = mv_core::time::now_utc() - time::Duration::hours(2);
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            format!(
                "{}\tevent=dispatch\ttask=20250101-reviewer-aaaaaa\tbranch=main\trole=reviewer\n\
                 {}\tevent=started\ttask=20250101-reviewer-aaaaaa\tpane=w1:p1\n",
                mv_core::time::format_seconds_utc(old),
                mv_core::time::format_seconds_utc(old),
            ),
        )
        .unwrap();
        let mut agents = FakeAgents {
            panes: vec!["w1:p1".to_string()],
        };
        let tasks = mv_state::tasks::sweep_tasks(&project_root);
        let line =
            reviewer_over_timeout_from_tasks(&tasks, 30, mv_core::time::now_utc(), &mut agents)
                .unwrap();
        assert!(line.starts_with("20250101-reviewer-aaaaaa ("), "{line}");

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn reviewer_over_timeout_line_is_none_once_the_pane_is_gone() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let old = mv_core::time::now_utc() - time::Duration::hours(2);
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            format!(
                "{}\tevent=dispatch\ttask=20250101-reviewer-aaaaaa\tbranch=main\trole=reviewer\n\
                 {}\tevent=started\ttask=20250101-reviewer-aaaaaa\tpane=w1:p1\n",
                mv_core::time::format_seconds_utc(old),
                mv_core::time::format_seconds_utc(old),
            ),
        )
        .unwrap();
        let mut agents = FakeAgents { panes: Vec::new() };
        let tasks = mv_state::tasks::sweep_tasks(&project_root);
        assert_eq!(
            reviewer_over_timeout_from_tasks(&tasks, 30, mv_core::time::now_utc(), &mut agents),
            None
        );

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn reviewer_over_timeout_line_is_none_under_the_timeout() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let recent = mv_core::time::now_utc() - time::Duration::minutes(5);
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            format!(
                "{}\tevent=dispatch\ttask=20250101-reviewer-aaaaaa\tbranch=main\trole=reviewer\n\
                 {}\tevent=started\ttask=20250101-reviewer-aaaaaa\tpane=w1:p1\n",
                mv_core::time::format_seconds_utc(recent),
                mv_core::time::format_seconds_utc(recent),
            ),
        )
        .unwrap();
        let mut agents = FakeAgents {
            panes: vec!["w1:p1".to_string()],
        };
        let tasks = mv_state::tasks::sweep_tasks(&project_root);
        assert_eq!(
            reviewer_over_timeout_from_tasks(&tasks, 30, mv_core::time::now_utc(), &mut agents),
            None
        );

        let _ = std::fs::remove_dir_all(&project_root);
    }

    // -- `close_merged_backlog_links` (card `20261102-b-0d8b17`) ---------

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    #[test]
    fn close_merged_backlog_links_closes_an_item_whose_linked_writer_task_landed() {
        let root = sandbox_project_root();
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

        let tasks = mv_state::tasks::sweep_tasks(&root);
        let closed = close_merged_backlog_links(&root, &tasks, true);
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].0, item.item);
        assert!(closed[0].2, "apply=true must record the close");

        let items = mv_state::backlog::read_items(&root, false);
        let reread = items.iter().find(|i| i.item == item.item).unwrap();
        assert_eq!(reread.state, "done");
        assert_eq!(reread.evidence, closed[0].1);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn close_merged_backlog_links_never_closes_an_auto_supersede_rewrite() {
        // A task `auto_supersede` turned `done` (its branch landed) is not
        // this backlog item's own outcome — only `outcome_state` still
        // distinguishes the two once `status` reads `done` either way.
        let root = sandbox_project_root();
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-bbbbbb\tbranch=multiverse/20260101-fixer-bbbbbb\trole=fixer\n\
             2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-bbbbbb\tstatus=done\tstate=superseded-merged\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();
        let item = mv_state::backlog::add(
            &root,
            "stale link",
            "nexus",
            "tracked",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        mv_state::backlog::link_task(&root, &item.item, "20260101-fixer-bbbbbb", None).unwrap();

        let tasks = mv_state::tasks::sweep_tasks(&root);
        let closed = close_merged_backlog_links(&root, &tasks, true);
        assert!(
            closed.is_empty(),
            "a superseded rewrite is not this item's own outcome"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    // -- Card `20261103-fixer-2faef1` (critic-later.md) ----------------------

    #[test]
    fn close_still_open_unlinked_replies_does_not_duplicate_a_close_already_on_disk() {
        // Finding 1: `matched` reflects `mail_index`, loaded once before
        // this pass's per-project loop — so it can still call an ask open
        // that a concurrent `multiverse reply`/`close_ask` has, since that
        // load, already closed on `holder_log`. `close_ask` itself never
        // re-checks, so writing straight from `matched` would append a
        // second, spurious close.
        let root = sandbox_project_root();
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        let holder_log = mv_state::activity::ProjectLog::open("holder", &root);
        mv_state::activity::send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "please rotate the staging creds before 9am",
            true,
            None,
            true,
            &|s: &str| s.to_string(),
        )
        .unwrap();
        // Simulates the race: by the time the write below runs, the ask
        // `matched` still names as open has already been closed.
        mv_state::activity::close_ask(&holder_log, 1, "answered", &|s: &str| s.to_string())
            .unwrap();

        let ask = mv_state::activity::MessageRecord {
            seq: 1,
            when: "2026-01-01T00:00:00+00:00".to_string(),
            id: "ask-1".to_string(),
            from_project: "asker".to_string(),
            to_project: "holder".to_string(),
            body: "please rotate the staging creds before 9am".to_string(),
            ask: true,
            reply_to: None,
        };
        let matched = vec![mv_state::activity::UnlinkedReply {
            answer: ask.clone(),
            ask,
            references_ask: true,
        }];

        let closed = close_still_open_unlinked_replies(&holder_log, matched);
        assert!(
            closed.is_empty(),
            "an ask already closed since the stale mail index was loaded must not be reported as newly closed"
        );
        let log_text = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log_text.matches("event=close").count(),
            1,
            "re-validating against stale data must not append a second close record"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tasks_snapshot_needs_reread_counts_a_stuck_resume_escalations_write() {
        // Finding 3: a stuck-resume escalation's `record_stranded_closure`
        // can land between the `tasks` snapshot and this check while all
        // four of `report`'s own counters stay zero.
        let report = mv_state::tasks::SweepReport::default();
        let mut nudges = mv_engine::sweep_nudges::NudgeOutcome::default();
        assert!(
            !tasks_snapshot_needs_reread(&report, &nudges),
            "a pass with no writes anywhere must reuse the in-memory snapshot"
        );

        nudges.resume.push(mv_engine::sweep_nudges::StuckResume {
            task_id: "20260101-fixer-aaaaaa".to_string(),
            pane: "w1:p1".to_string(),
            sent: false,
            failed: true,
        });
        assert!(
            tasks_snapshot_needs_reread(&report, &nudges),
            "a stuck-resume escalation's own write must force a fresh read even though report's four counters are all zero"
        );
    }

    #[test]
    fn a_sweep_pass_closes_a_backlog_item_linked_to_a_genuinely_done_landed_task() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-auto-supersede-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let project_root = dir.join("sandbox");
        std::fs::create_dir_all(&project_root).unwrap();
        git(&project_root, &["init", "-q", "-b", "main"]);
        git(
            &project_root,
            &["config", "user.email", "main@example.invalid"],
        );
        git(&project_root, &["config", "user.name", "Main Test"]);
        std::fs::write(project_root.join("README"), "seed\n").unwrap();
        git(&project_root, &["add", "README"]);
        git(&project_root, &["commit", "-q", "-m", "seed"]);
        let base = std::process::Command::new("git")
            .current_dir(&project_root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let base = String::from_utf8_lossy(&base.stdout).trim().to_string();
        git(
            &project_root,
            &["branch", "multiverse/20260101-fixer-aaaaaa", &base],
        );
        git(
            &project_root,
            &[
                "update-ref",
                "refs/multiverse/base/20260101-fixer-aaaaaa",
                &base,
            ],
        );
        git(
            &project_root,
            &["checkout", "-q", "multiverse/20260101-fixer-aaaaaa"],
        );
        std::fs::write(project_root.join("work.txt"), "a\n").unwrap();
        git(&project_root, &["add", "work.txt"]);
        git(&project_root, &["commit", "-q", "-m", "work"]);
        git(&project_root, &["checkout", "-q", "main"]);
        git(
            &project_root,
            &[
                "merge",
                "--no-ff",
                "-q",
                "-m",
                "merge lane",
                "multiverse/20260101-fixer-aaaaaa",
            ],
        );
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=multiverse/20260101-fixer-aaaaaa\trole=fixer\n\
             2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();
        let item = mv_state::backlog::add(
            &project_root,
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
        mv_state::backlog::link_task(&project_root, &item.item, "20260101-fixer-aaaaaa", None)
            .unwrap();
        let config_path = write_sandbox_config(&dir.join("config"), &project_root);

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let items = mv_state::backlog::read_items(&project_root, false);
        let reread = items.iter().find(|i| i.item == item.item).unwrap();
        assert_eq!(
            reread.state, "done",
            "the linked backlog item must close once its task's branch lands"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sweep_pass_closes_an_open_ask_a_later_unlinked_reply_already_answered() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-sweep-test-unlinked-reply-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let holder_root = dir.join("holder");
        let asker_root = dir.join("asker");
        std::fs::create_dir_all(holder_root.join(".multiverse")).unwrap();
        std::fs::create_dir_all(asker_root.join(".multiverse")).unwrap();

        mv_state::activity::send_message(
            &mv_state::activity::ProjectLog::open("holder", &holder_root),
            "ask-1",
            "asker",
            "holder",
            "please rotate the staging creds before 9am",
            true,
            None,
            true,
            &|s: &str| s.to_string(),
        )
        .unwrap();
        mv_state::activity::send_message(
            &mv_state::activity::ProjectLog::open("asker", &asker_root),
            "reply-1",
            "holder",
            "asker",
            "done, see #1 - rotated already",
            false,
            None,
            true,
            &|s: &str| s.to_string(),
        )
        .unwrap();

        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.holder]\npath = \"{}\"\n\n[projects.asker]\npath = \"{}\"\n",
                holder_root.display(),
                asker_root.display(),
            ),
        )
        .unwrap();

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        let holder_log = mv_state::activity::ProjectLog::open("holder", &holder_root);
        assert!(
            mv_state::activity::is_closed(&holder_log, 1).unwrap(),
            "a reply that references the ask by `#1` must close it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- mv_engine::sweep_nudges wiring (card 20261102-b-b4ede0) ------------

    /// Answers `pane read` with scripted text and records every `agent
    /// prompt` call as `(target, text)` — the two Herdr calls
    /// [`mv_engine::sweep_nudges::run`] can make once a candidate survives
    /// its own file-based prefilter.
    struct NudgeFake {
        pane_text: &'static str,
        prompts: Vec<(String, String)>,
    }

    impl Herdr for NudgeFake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            match (command.argv[1].as_str(), command.argv[2].as_str()) {
                ("pane", "read") => Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: self.pane_text.to_string(),
                    stderr: String::new(),
                    payload: None,
                    protocol_error: None,
                }),
                ("agent", "prompt") => {
                    self.prompts
                        .push((command.argv[3].clone(), command.argv[4].clone()));
                    Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: Some(json!({"result": {}})),
                        protocol_error: None,
                    })
                }
                other => panic!("unexpected call {other:?}"),
            }
        }
    }

    fn write_sandbox_config_with_capped_seat(
        dir: &std::path::Path,
        project_root: &std::path::Path,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[projects.sandbox]\npath = \"{}\"\n\n\
                 [accounts.codex-cap]\n\
                 kind = \"codex\"\n\
                 config_dir_var = \"CODEX_HOME\"\n\
                 config_dir = \"<<PLACEHOLDER: x>>\"\n\
                 max_runtime_minutes = 30\n",
                project_root.display()
            ),
        )
        .unwrap();
        config_path
    }

    #[test]
    fn a_lane_past_its_runtime_cap_gets_capped_through_run_with() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let dispatched = mv_core::time::now_utc() - time::Duration::minutes(90);
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            format!(
                "{d}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tbranch=main\trole=fixer\t\
                 account=codex-cap\tagent=t1-agent\n\
                 {d}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
                d = mv_core::time::format_seconds_utc(dispatched),
            ),
        )
        .unwrap();
        let config_path =
            write_sandbox_config_with_capped_seat(&project_root.join("config"), &project_root);

        let mut agents = NamedFakeAgents {
            panes: vec![("w1:p1".to_string(), "t1-agent".to_string())],
        };
        let mut herdr = NudgeFake {
            pane_text: "still working on it",
            prompts: Vec::new(),
        };
        run_with(
            &matches(&["sweep", "--config", &config_path.to_string_lossy()]),
            &mut agents,
            &mut herdr,
        )
        .unwrap();

        assert_eq!(
            herdr.prompts,
            vec![(
                "w1:p1".to_string(),
                mv_engine::sweep_nudges::RUNTIME_CAP_PROMPT.to_string()
            )]
        );
        assert!(mv_state::nudge_memory::runtime_cap_already_sent(
            &project_root,
            "20250101-fixer-aaaaaa"
        ));

        let _ = std::fs::remove_dir_all(&project_root);
    }

    #[test]
    fn dry_run_previews_the_runtime_cap_without_sending_or_claiming() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let dispatched = mv_core::time::now_utc() - time::Duration::minutes(90);
        std::fs::write(
            project_root.join(".multiverse/routing.log"),
            format!(
                "{d}\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tbranch=main\trole=fixer\t\
                 account=codex-cap\tagent=t1-agent\n\
                 {d}\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
                d = mv_core::time::format_seconds_utc(dispatched),
            ),
        )
        .unwrap();
        let config_path =
            write_sandbox_config_with_capped_seat(&project_root.join("config"), &project_root);

        let mut agents = FakeAgents {
            panes: vec!["w1:p1".to_string()],
        };
        let mut herdr = NudgeFake {
            pane_text: "still working on it",
            prompts: Vec::new(),
        };
        run_with(
            &matches(&[
                "sweep",
                "--dry-run",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut herdr,
        )
        .unwrap();

        assert!(herdr.prompts.is_empty(), "dry-run must never send");
        assert!(!mv_state::nudge_memory::runtime_cap_already_sent(
            &project_root,
            "20250101-fixer-aaaaaa"
        ));

        let _ = std::fs::remove_dir_all(&project_root);
    }

    // --- codex-pool recovery probe wiring (leg 3 of this module's own
    // "not ported" list) ------------------------------------------------

    /// `cargo test` runs on multiple threads by default, and
    /// `MULTIVERSE_CODEX_BIN` is read from the real process environment —
    /// same crate-wide lock as [`PmsetBinGuard`] above.
    struct CodexBinGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl CodexBinGuard {
        fn set(value: &std::path::Path) -> Self {
            let lock = crate::commands::worktrees::herdr_env::lock();
            let previous = std::env::var("MULTIVERSE_CODEX_BIN").ok();
            std::env::set_var("MULTIVERSE_CODEX_BIN", value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for CodexBinGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("MULTIVERSE_CODEX_BIN", value),
                None => std::env::remove_var("MULTIVERSE_CODEX_BIN"),
            }
        }
    }

    /// A fake `codex` that marks itself invoked by appending one line to
    /// `<config_dir>/probe-calls`, standing in for the real CLI that
    /// `mv_engine::limits::sweep_tick_codex_probe` execs (never through
    /// Herdr — AGENTS.md's "never exercise the real fleet during tests"
    /// covers Herdr/the real `codex` binary, both sidestepped here).
    fn write_fake_codex(dir: &std::path::Path) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let script = dir.join("fake-codex.sh");
        std::fs::write(
            &script,
            "#!/bin/sh\necho called >> \"$CODEX_HOME/probe-calls\"\nexit 0\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        script
    }

    fn write_sandbox_config_with_chatgpt_seat(
        dir: &std::path::Path,
        project_root: &std::path::Path,
        codex_home: &std::path::Path,
    ) -> std::path::PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "dispatch_min_quota_percent = 5\n\n\
                 [projects.sandbox]\npath = \"{}\"\n\n\
                 [accounts.codex-main]\n\
                 kind = \"codex\"\n\
                 config_dir_var = \"CODEX_HOME\"\n\
                 config_dir = \"{}\"\n\
                 pool = \"chatgpt\"\n",
                project_root.display(),
                codex_home.display(),
            ),
        )
        .unwrap();
        config_path
    }

    /// d96fea6 dropped `mv_engine::limits`'s env-touching
    /// `the_public_wrapper_honors_the_codex_binary_env_override` when it
    /// moved `codex_binary` onto a parameter seam. The seam still covers
    /// the override *logic*; this restores the public wrapper
    /// (`sweep_tick_codex_probe` → `codex_binary` → `$MULTIVERSE_CODEX_BIN`)
    /// under the crate-wide `commands::worktrees::herdr_env` lock every
    /// remaining writer of that var already holds.
    #[cfg(unix)]
    #[test]
    fn the_public_wrapper_honors_the_codex_binary_env_override() {
        let project_root = sandbox_project_root();
        let codex_home = project_root.parent().unwrap().join(format!(
            "{}-codex-home",
            project_root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&codex_home).unwrap();
        let sessions = codex_home.join("sessions/2026/09/27");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            "{\"type\":\"token_count\",\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":20}}}}\n",
        )
        .unwrap();
        let config_path = write_sandbox_config_with_chatgpt_seat(
            &project_root.join("config"),
            &project_root,
            &codex_home,
        );
        let state_dir = state_dir_from_config(&config_path);
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut gauges = mv_state::gauge::GaugeMap::new();
        gauges.insert(
            "chatgpt".to_string(),
            mv_state::gauge::GaugeEntry {
                vendor: Some("codex".to_string()),
                percent_left: Some(3.0),
                ..Default::default()
            },
        );
        mv_state::gauge::save(&state_dir, &gauges).unwrap();
        let _codex_bin = CodexBinGuard::set(&write_fake_codex(&project_root.join("bin")));
        let config = load_sandbox(&config_path);
        let account = &config.accounts["codex-main"];

        assert!(mv_engine::limits::sweep_tick_codex_probe(
            account,
            5,
            &state_dir,
            mv_core::time::now_utc()
        ));
        assert_eq!(
            std::fs::read_to_string(codex_home.join("probe-calls"))
                .unwrap()
                .lines()
                .count(),
            1,
            "the public wrapper must exec MULTIVERSE_CODEX_BIN, not a PATH lookup of `codex`"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&codex_home);
    }

    #[cfg(unix)]
    #[test]
    fn a_tick_pass_probes_a_drained_codex_pool_once_and_refreshes_its_gauge() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(&project_root).unwrap();
        let codex_home = project_root.parent().unwrap().join(format!(
            "{}-codex-home",
            project_root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&codex_home).unwrap();
        let sessions = codex_home.join("sessions/2026/09/27");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(
            sessions.join("rollout.jsonl"),
            "{\"type\":\"token_count\",\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":20}}}}\n",
        )
        .unwrap();
        let config_path = write_sandbox_config_with_chatgpt_seat(
            &project_root.join("config"),
            &project_root,
            &codex_home,
        );
        let state_dir = state_dir_from_config(&config_path);
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut gauges = mv_state::gauge::GaugeMap::new();
        gauges.insert(
            "chatgpt".to_string(),
            mv_state::gauge::GaugeEntry {
                vendor: Some("codex".to_string()),
                percent_left: Some(3.0),
                ..Default::default()
            },
        );
        mv_state::gauge::save(&state_dir, &gauges).unwrap();
        let _codex_bin = CodexBinGuard::set(&write_fake_codex(&project_root.join("bin")));

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&[
                "sweep",
                "--tick",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(codex_home.join("probe-calls"))
                .unwrap()
                .lines()
                .count(),
            1,
            "the drained pool must be probed exactly once per pass"
        );
        let gauges = mv_state::gauge::load(&state_dir);
        assert_eq!(
            gauges["chatgpt"].percent_left,
            Some(80.0),
            "the probe's own read_and_record_codex_usage must refresh the gauge"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&codex_home);
    }

    #[cfg(unix)]
    #[test]
    fn a_dry_run_never_spends_a_codex_recovery_probe() {
        let project_root = sandbox_project_root();
        std::fs::create_dir_all(&project_root).unwrap();
        let codex_home = project_root.parent().unwrap().join(format!(
            "{}-codex-home",
            project_root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&codex_home).unwrap();
        let config_path = write_sandbox_config_with_chatgpt_seat(
            &project_root.join("config"),
            &project_root,
            &codex_home,
        );
        let state_dir = state_dir_from_config(&config_path);
        std::fs::create_dir_all(&state_dir).unwrap();
        let mut gauges = mv_state::gauge::GaugeMap::new();
        gauges.insert(
            "chatgpt".to_string(),
            mv_state::gauge::GaugeEntry {
                percent_left: Some(3.0),
                ..Default::default()
            },
        );
        mv_state::gauge::save(&state_dir, &gauges).unwrap();
        let _codex_bin = CodexBinGuard::set(&write_fake_codex(&project_root.join("bin")));

        let mut agents = FakeAgents { panes: Vec::new() };
        run_with(
            &matches(&[
                "sweep",
                "--tick",
                "--dry-run",
                "--config",
                &config_path.to_string_lossy(),
            ]),
            &mut agents,
            &mut NullHerdr,
        )
        .unwrap();

        assert!(
            !codex_home.join("probe-calls").exists(),
            "a dry run must never exec the real recovery probe"
        );

        let _ = std::fs::remove_dir_all(&project_root);
        let _ = std::fs::remove_dir_all(&codex_home);
    }

    #[test]
    fn host_reporting_runs_every_pass_including_halt_and_never_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let state_dir = state_dir_from_config(&config_path);
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(
            &config_path,
            format!(
                "[projects.first]\npath = {:?}\n[projects.second]\npath = {:?}\n",
                first.to_string_lossy(),
                second.to_string_lossy(),
            ),
        )
        .unwrap();
        let mut calls = Vec::new();
        let mut host_report = |config: &Config, apply, _| {
            assert_eq!(config.projects.len(), 2);
            calls.push(apply);
            vec!["disk: 1 GB free (1%) CRITICAL".to_string()]
        };
        for flags in [vec![], vec!["--tick"], vec!["--tick", "--dry-run"]] {
            let mut args = vec!["sweep", "--config", config_path.to_str().unwrap()];
            args.extend(flags);
            super::run_with(
                &matches(&args),
                &mut FakeAgents { panes: Vec::new() },
                &mut NullHerdr,
                &mut host_report,
                ProcessProbe::Snapshot(Some(Vec::new())),
            )
            .unwrap();
        }
        mv_state::halt::write_halt_state(
            &state_dir,
            &mv_state::halt::HaltState {
                halted: true,
                reason: Some("test".into()),
                at: Some(mv_core::time::format_seconds_utc(mv_core::time::now_utc())),
            },
        )
        .unwrap();
        super::run_with(
            &matches(&["sweep", "--tick", "--config", config_path.to_str().unwrap()]),
            &mut FakeAgents { panes: Vec::new() },
            &mut NullHerdr,
            &mut host_report,
            ProcessProbe::Snapshot(Some(Vec::new())),
        )
        .unwrap();
        // Plain sweep, --tick, --tick --dry-run, then --tick while halted.
        // apply is true whenever the pass is not a dry-run.
        assert_eq!(calls, [true, true, false, true]);
    }
}

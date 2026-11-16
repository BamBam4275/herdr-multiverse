//! Port of Python's `tasks.nudge_idle_lanes_to_finish`, `tasks.
//! resume_stuck_lanes`, and `tasks.cap_long_running_lanes` (`multiverse/
//! tasks.py`) — one canned, claim-then-act prompt for three distinct
//! RUNNING-lane shapes `multiverse sweep` already has the task/pane
//! evidence for. Tracked as backlog `20261102-b-b4ede0`.
//!
//! [`run`] is the one entry point `commands::sweep` calls per project: it
//! computes every candidate from [`mv_state::tasks::SweepTask`] and
//! `mv_config::load::Config` alone (no Herdr call at all) and returns
//! immediately, paying for neither an `agent list` nor a pane read, when
//! none exist — the same "candidates first, Herdr second" discipline
//! `commands::sweep::reviewer_over_timeout_line` already uses, required
//! here for the same reason: every pinned `sweep-*` conformance transcript
//! scripts an exact Herdr call count, and the common case (nothing
//! RUNNING/STALLED with a recorded pane and agent) must add none.
//!
//! `apply=false` (`sweep --dry-run`'s own posture) computes and returns
//! every candidate without claiming a slot or sending anything, the same
//! contract every claim-then-act helper in this fleet already uses
//! (`mv_state::tasks::close_stranded`/`expire_awaiting`). `apply=true`
//! claims each candidate's one-time slot in [`mv_state::nudge_memory`]
//! before sending; a task whose slot is already claimed is skipped
//! outright, never retried — "never sent twice" holds even across two
//! sweeps started moments apart. A send that fails still keeps the claim:
//! the one nudge/resume/cap this task gets is spent either way, the same
//! trade Python's own `_claim_finish_nudge` docstring explains.
//!
//! Reuses, rather than re-deriving: `crate::runner::visible` (newly
//! `pub(crate)`, card `20261102-b-b4ede0`) for the identical dialog-or-
//! unreadable guard `crate::runner::deliver_prompt_to`/`deliver_heartbeat`
//! already pay before any send into a pane this process did not just
//! dispatch into — never a second copy of [`crate::runner::
//! attention_reason`]'s trust/approval vocabulary; and
//! [`mv_state::tasks::has_usable_result`] (newly `pub`) for the identical
//! "did this task leave a result behind" question Python's own
//! `result_written` answers for `finish_nudge_eligible`.
//!
//! Narrowed from Python, named here rather than silently dropped (see this
//! task's `result.md` for the backlog cards filed for each):
//! - No `TaskState::STALLED` exists in this crate's `SweepTask` model.
//!   "RUNNING or STALLED" is read as `base_running_candidate`: not yet
//!   recorded, no status file written yet, with a recorded pane and agent
//!   — the same narrowing `reviewer_over_timeout_line` already uses for
//!   the identical Python state pair, one door down in this same crate.
//! - `dialog_or_menu_reason`'s vendor money/credits/model-switch dialog
//!   classification (`limits.classify_prompt`) is now ported, as
//!   [`crate::blocking_prompt::classify_prompt`] (cards `20261102-b-bdf745`/
//!   `20261102-b-5b1325`, task `20261102-fixer-39a24c`), and every
//!   `safe_pane_text` read below refuses a pane it flags, the same as
//!   Python's `fail_closed=True`. That port is itself narrowed to the
//!   detectors this checkout's `tests/fixtures/rate-limit/` actually backs
//!   (codex's model-switch offer, codex/grok's billing dialogs) — no
//!   `sandbox-dialog`/`stall-cause` fixtures exist here, and no full
//!   `limits.classify`/`Verdict` port exists either — see that module's own
//!   doc comment. [`crate::runner::attention_reason`]'s trust/approval
//!   vocabulary, plus `menu_reason`'s `AskUserQuestion` menu-tail-line
//!   scan (card `20261102-b-5b1325`, task `20261102-fixer-c628da`), still
//!   guard every send alongside it; a vendor-specific dialog none of the
//!   three recognize can still receive a nudge.
//! - `find_stuck_signal`'s own pane read and `resume_stuck_lanes`'s later,
//!   separate dialog-guard read are collapsed into one `crate::runner::visible`
//!   call apiece: a dialog is detected (and the candidate
//!   skipped) at whichever of the two reads sees it first, rather than
//!   always at the second. The board-facing `"stuck: <id> (<account>)"`
//!   line Python prints on every confirmed-stuck pass (`cli.py:12730-12740`)
//!   is now [`stuck_notes`], called from [`run`] independently of whether
//!   the one-shot resume still has a claim left.
//! - `resume_stuck_lanes`'s "already resumed once, connection loss
//!   persisted" escalation (card `20261102-b-943ed3`, task
//!   `20261102-fixer-c628da`) now records the task `failed` via
//!   [`mv_state::tasks::record_stranded_closure`] — the same "append a
//!   terminal result line" primitive `close_stranded`/`expire_awaiting`
//!   already use, reused here in place of a second copy, standing in for
//!   Python's `accounts.append_outcome_if_absent` (no accounts.rs port of
//!   that exists, and this task could not take on one). The breaker
//!   evidence file is written best-effort through [`crate::breaker::
//!   write_evidence`], using the account's pool and the complete guarded
//!   pane text. The project-local evidence path is linked from the
//!   outcome's existing `state=` text; a failed evidence write still
//!   records the failed outcome without a link. Dry runs write neither.
//! - `sleep_minutes` (the host-sleep subtraction `find_stuck_signal`/
//!   `cap_long_running_lanes` both take) is not threaded through from
//!   `commands::sweep`, which has no `hostsleep.report()` call of its own
//!   yet; every elapsed-time comparison below behaves as every Python
//!   caller with no sleep total already does (`sleep_minutes=0`).
//!
//! Python has no analogue for `submit_unsubmitted_composers` (card
//! `20261114-b-4d8baf`, design `20261115-seer-c2c967`): a lane idle with
//! its own reconstructed dispatch brief still sitting, unsubmitted, in its
//! composer gets exactly one `send-keys … Enter`, run from [`run`] before
//! the three senders above so none of them types a competing canned prompt
//! over it. Its own one-shot claim lives in a dedicated
//! `composer-enter-memory.json`, kept local to this module rather than
//! folded into [`mv_state::nudge_memory`] — this task's own write scope
//! permits `mv-engine/src/sweep_nudges.rs` and a visibility-only change to
//! `mv-engine/src/runner.rs`, not a new `mv-state` claim file or the
//! `commands::sweep` print wiring a full board-facing rollout would also
//! need; both are filed as follow-ups rather than attempted here.
//!
//! Hardened past Python, not narrowed from it: every `send()` below — the
//! finish nudge, the stuck resume, and the runtime cap — is now gated on
//! `pane_still_hosts_task_agent`, a fresh `agent list` taken right before
//! that send, never the one `live` snapshot [`run`] took at the top of its
//! own pass. Without it, a pane whose original agent finished or closed
//! between the snapshot and the send could be recycled onto an unrelated,
//! actively-working agent, and Herdr resolves a prompt target by pane id
//! alone with no identity check of its own — the canned prompt would type
//! directly into that live agent's pane. Mirrors `mv_state::tasks::
//! sweep_project`'s own same-agent-name reclaim guard; Python carries the
//! identical gap (`herdr.agent_prompt(task.pane, ...)` with no re-check),
//! so there is no Python targeting to match here (card `20261103-b-695405`).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration as StdDuration;

use mv_config::load::{Account, Config};
use mv_herdr::argv;
use mv_herdr::envelope::AgentState;
use mv_state::tasks::{AgentPanes, LiveAgentStatus, SweepTask, TaskStatus};
use regex::Regex;
use time::OffsetDateTime;

use crate::runner::{composer_marker, prompt_still_in_composer, visible, Herdr};

/// The one canned text every finish nudge sends. Python's
/// `FINISH_NUDGE_PROMPT` (`tasks.py`), verbatim.
pub const FINISH_NUDGE_PROMPT: &str = "finish: write result.md and status";
/// Python's `STUCK_RESUME_PROMPT`, verbatim.
pub const STUCK_RESUME_PROMPT: &str = "resume: the connection dropped mid-turn — continue";
/// Python's `RUNTIME_CAP_PROMPT`, verbatim.
pub const RUNTIME_CAP_PROMPT: &str = "wrap up now: write result.md with what you have and status";

/// Python's `STUCK_AFTER_MINUTES`.
const STUCK_AFTER_MINUTES: i64 = 30;

const SEND_TIMEOUT: StdDuration = StdDuration::from_secs(120);

/// Python's `_STUCK_PANE_RE`: the broader vocabulary, matched against a
/// pane Herdr does not report `Working`.
fn stuck_pane_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)reconnecting|idle timeout|connection lost|ENOTFOUND|stopped arriving\
             |network error|transport error|api error",
        )
        .expect("static pattern")
    })
}

/// Python's `_CONNECTION_LOSS_RE`: the narrower vocabulary required while
/// Herdr still reports the pane `Working` (a reconnect spinner can keep it
/// there; only explicit connection loss is evidence against that state).
fn connection_loss_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)reconnecting|idle timeout|connection lost|ENOTFOUND|stopped arriving\
             |stream disconnected|network error|transport error",
        )
        .expect("static pattern")
    })
}

/// Python's `_DIALOG_MENU_MARKERS`, verbatim: the fixed vocabulary an open
/// `AskUserQuestion` picker's own footer shows, however Herdr classifies the
/// pane's `agent_status` (card `20261102-b-5b1325` — such a pane reads
/// `done`, not `blocked`, so [`crate::runner::attention_reason`] alone
/// never catches it).
const DIALOG_MENU_MARKERS: &[&str] = &[
    "enter to select",
    "esc to cancel",
    "enter confirm",
    "esc not now",
    "↑/↓ choose",
];

/// Python's `_DIALOG_MENU_TAIL_LINES`: only the pane's last few non-blank
/// lines are scanned, so a pane that merely scrolled past this vocabulary
/// earlier is not mistaken for one still parked on it.
const DIALOG_MENU_TAIL_LINES: usize = 6;

/// Whether `text`'s last few non-blank lines show an open question-menu
/// footer — Python's `dialog_or_menu_reason`'s own menu-tail-line scan,
/// ported standalone (its vendor-dialog half, `limits.classify_prompt`, has
/// no `mv-engine` port to call here — see the module doc comment).
fn menu_reason(text: &str) -> bool {
    let tail: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = tail.len().saturating_sub(DIALOG_MENU_TAIL_LINES);
    let joined = tail[start..].join("\n").to_lowercase();
    DIALOG_MENU_MARKERS
        .iter()
        .any(|marker| joined.contains(marker))
}

/// One task's outcome from [`nudge_idle_lanes_to_finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishNudge {
    pub task_id: String,
    pub pane: String,
    pub sent: bool,
}

/// One task's outcome from [`resume_stuck_lanes`]. `failed` (card
/// `20261102-b-943ed3`) is `true` only for the escalation leg: a lane still
/// showing the identical connection-loss banner, on the same pane, after
/// its one resume nudge was already sent — recorded `failed` instead of
/// nudged again, `sent` staying `false` on that outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StuckResume {
    pub task_id: String,
    pub pane: String,
    pub sent: bool,
    pub failed: bool,
}

/// One task's outcome from [`cap_long_running_lanes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeCap {
    pub task_id: String,
    pub pane: String,
    pub sent: bool,
}

/// One task's outcome from `submit_unsubmitted_composers`: `sent` is
/// `true` only once the one Enter for a positively matched, reconstructed
/// dispatch brief has actually gone out (card `20261114-b-4d8baf`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerHold {
    pub task_id: String,
    pub pane: String,
    pub sent: bool,
}

/// Every nudge/resume/cap outcome from one [`run`] pass, plus the
/// report-only `"stuck: …"` notes Python prints on every confirmed-stuck
/// pass even after the one-shot resume has already fired.
#[derive(Debug, Clone, Default)]
pub struct NudgeOutcome {
    pub finish: Vec<FinishNudge>,
    pub resume: Vec<StuckResume>,
    pub cap: Vec<RuntimeCap>,
    pub stuck: Vec<String>,
    /// `submit_unsubmitted_composers`'s own Enter-or-would-Enter
    /// outcomes, one per task whose composer positively matched its own
    /// reconstructed dispatch brief.
    pub composer: Vec<ComposerHold>,
    /// `"unsubmitted composer: <id>"`, one per task whose composer held
    /// non-empty text that matched none of this task's own reconstructed
    /// dispatch prompts — reported, never acted on.
    pub unsubmitted_composer: Vec<String>,
}

/// The "RUNNING or STALLED" proxy every function below starts from: not
/// yet recorded in the routing log, no status file written yet (a written
/// terminal status means `reconcile`'s own territory, not a live nudge),
/// and a recorded pane/agent to address. See the module doc comment for
/// why this, not a ported `TaskState::STALLED`, stands in for Python's
/// `task.state in (RUNNING, STALLED)`.
fn base_running_candidate(task: &SweepTask) -> bool {
    !task.recorded && task.file_status.is_none() && task.pane.is_some() && task.agent.is_some()
}

fn pane_status(live: &[LiveAgentStatus], pane: &str) -> Option<AgentState> {
    live.iter().find(|agent| agent.pane_id == pane)?.status
}

fn is_live(live: &[LiveAgentStatus], pane: &str) -> bool {
    live.iter().any(|agent| agent.pane_id == pane)
}

/// Re-verifies, from a listing taken right now — never the `live` snapshot
/// [`run`] took at the top of its own pass — that `pane` still hosts
/// `task`'s own dispatched agent (`task.agent`) before a nudge/resume/cap
/// prompt actually reaches it. Mirrors `mv_state::tasks::sweep_project`'s
/// own same-agent-name reclaim guard: a pane whose original agent finished
/// or closed between the snapshot and this send can be recycled onto an
/// unrelated, actively-working agent, and Herdr targets a prompt by pane id
/// alone with no identity check of its own (card `20261103-b-695405`). A
/// failed re-list, or no live entry left at `pane`, refuses the send the
/// same as a name mismatch — "not provably still this task's agent" either
/// way.
fn pane_still_hosts_task_agent(agents: &mut impl AgentPanes, task: &SweepTask, pane: &str) -> bool {
    let Some(fresh) = agents.list_checked() else {
        return false;
    };
    fresh
        .iter()
        .find(|agent| agent.pane_id == pane)
        .is_some_and(|agent| agent.name.as_deref() == task.agent.as_deref())
}

fn elapsed_since(task: &SweepTask, now: OffsetDateTime) -> Option<time::Duration> {
    let last_seen = task.started_at.or(task.dispatched_at)?;
    Some(now - last_seen)
}

/// Python's `_startup_grace_active`: whether a just-started task still
/// gets its account's configured missing-pane/not-yet-working grace.
fn startup_grace_active(task: &SweepTask, account: &Account, now: OffsetDateTime) -> bool {
    let Some(grace) = account.startup_grace_seconds else {
        return false;
    };
    if grace == 0 {
        return false;
    }
    let Some(elapsed) = elapsed_since(task, now) else {
        return false;
    };
    elapsed < time::Duration::seconds(i64::from(grace))
}

fn account_for<'a>(task: &SweepTask, config: &'a Config) -> Option<&'a Account> {
    config.accounts.get(task.account.as_deref()?)
}

/// Read `pane`'s visible text and classify it the same way every send in
/// this module does: `Some(text)` only when the pane is safe to act on
/// (readable, non-empty, no trust/approval dialog per [`crate::runner::
/// attention_reason`], no vendor dialog per [`crate::blocking_prompt::
/// classify_prompt`], no open question-menu footer per [`menu_reason`]);
/// `None` for a dialog, a parked menu, an unreadable pane, or any Herdr
/// error — "not safe to act on" either way, the same fail-closed posture
/// Python's own `fail_closed=True` reads use for this exact decision.
/// `kind` is the account's vendor kind (`"codex"`, `"grok"`, ...), the same
/// string Python's own `classify_prompt(text, kind)` callers pass.
fn safe_pane_text(
    herdr: &mut impl Herdr,
    task: &SweepTask,
    pane: &str,
    kind: &str,
) -> Option<String> {
    let name = task.agent.as_deref().unwrap_or(pane);
    let text = visible(herdr, name, pane).ok()?;
    if crate::blocking_prompt::classify_prompt(&text, kind).is_some() {
        return None;
    }
    if menu_reason(&text) {
        return None;
    }
    Some(text)
}

fn send(herdr: &mut impl Herdr, pane: &str, prompt: &str) -> bool {
    herdr
        .call(&argv::agent_prompt(pane, prompt, false, None), SEND_TIMEOUT)
        .map(|result| result.ok())
        .unwrap_or(false)
}

/// Whether `task` is a live local/codex/claude-kind lane, idle or done at
/// its own prompt, with no status file and no `result.md` — Python's
/// `finish_nudge_eligible`, split here into the part answerable with no
/// Herdr call ([`base_running_candidate`], account kind/vendor, startup
/// grace, `has_usable_result`) so [`run`]'s own fast path can skip the
/// listing call entirely when nothing qualifies. Card `20261109-b-946582`:
/// claude-kind accounts are eligible under exactly the same conditions as
/// codex; every other kind (grok, ...) stays excluded.
fn finish_nudge_base_eligible(
    task: &SweepTask,
    config: &Config,
    project_root: &Path,
    now: OffsetDateTime,
) -> bool {
    if !base_running_candidate(task) {
        return false;
    }
    let Some(account) = account_for(task, config) else {
        return false;
    };
    if account.vendor != "local" && account.kind != "codex" && account.kind != "claude" {
        return false;
    }
    if startup_grace_active(task, account, now) {
        return false;
    }
    if mv_state::tasks::has_usable_result(project_root, &task.task_id) {
        return false;
    }
    true
}

/// Send [`FINISH_NUDGE_PROMPT`] to every eligible task in `tasks`. See the
/// module doc comment for the claim-then-act/`apply` contract.
#[allow(clippy::too_many_arguments)]
pub fn nudge_idle_lanes_to_finish(
    project_root: &Path,
    tasks: &[SweepTask],
    live: &[LiveAgentStatus],
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
) -> Vec<FinishNudge> {
    let mut results = Vec::new();
    for task in tasks {
        if !finish_nudge_base_eligible(task, config, project_root, now) {
            continue;
        }
        let pane = task.pane.as_deref().expect("base_eligible checked pane");
        if !pane_status(live, pane).is_some_and(AgentState::is_ready_for_input) {
            continue;
        }
        if mv_state::nudge_memory::finish_nudge_already_sent(project_root, &task.task_id) {
            continue;
        }
        let Some(account) = account_for(task, config) else {
            continue;
        };
        let Some(text) = safe_pane_text(herdr, task, pane, &account.kind) else {
            continue;
        };
        // A pane showing the connection-drop vocabulary gets
        // `resume_stuck_lanes`'s own resume prompt instead, never this
        // generic finish prompt (Python card `20261022-b-ad011c`).
        if stuck_pane_re().is_match(&text) {
            continue;
        }
        if !apply {
            results.push(FinishNudge {
                task_id: task.task_id.clone(),
                pane: pane.to_string(),
                sent: false,
            });
            continue;
        }
        if !mv_state::nudge_memory::claim_finish_nudge(project_root, &task.task_id, now) {
            continue;
        }
        let sent = pane_still_hosts_task_agent(agents, task, pane)
            && send(herdr, pane, FINISH_NUDGE_PROMPT);
        results.push(FinishNudge {
            task_id: task.task_id.clone(),
            pane: pane.to_string(),
            sent,
        });
    }
    results
}

/// Python's `find_stuck_signal`, narrowed to exactly what [`resume_stuck_lanes`]
/// needs (see the module doc comment for the merged-read narrowing): `Some`
/// once a confirmed-stuck pane/task is found, `None` otherwise.
fn find_stuck_signal(
    project_root: &Path,
    task: &SweepTask,
    account: &Account,
    live: &[LiveAgentStatus],
    now: OffsetDateTime,
    herdr: &mut impl Herdr,
) -> bool {
    if !base_running_candidate(task) {
        return false;
    }
    if startup_grace_active(task, account, now) {
        return false;
    }
    let Some(elapsed) = elapsed_since(task, now) else {
        return false;
    };
    if elapsed <= time::Duration::minutes(STUCK_AFTER_MINUTES) {
        return false;
    }
    let pane = task
        .pane
        .as_deref()
        .expect("base_running_candidate checked pane");
    if !is_live(live, pane) {
        return false;
    }
    let working = pane_status(live, pane) == Some(AgentState::Working);
    let Some(text) = safe_pane_text(herdr, task, pane, &account.kind) else {
        return false;
    };
    let matched = if working {
        connection_loss_re().is_match(&text)
    } else {
        stuck_pane_re().is_match(&text)
    };
    if !matched {
        return false;
    }
    let live_panes: Vec<String> = live.iter().map(|agent| agent.pane_id.clone()).collect();
    mv_state::nudge_memory::stuck_signal_confirmed(project_root, pane, &text, &live_panes, now)
}

/// Python's standalone stuck visibility line (`cli.py:12730-12740`):
/// `"stuck: <task_id> (<account>)"` for every task `find_stuck_signal`
/// confirms this pass, whether or not [`resume_stuck_lanes`] still has a
/// claim left to spend. Sorted by task id, matching Python's `sorted(found)`.
pub fn stuck_notes(
    project_root: &Path,
    tasks: &[SweepTask],
    live: &[LiveAgentStatus],
    config: &Config,
    now: OffsetDateTime,
    herdr: &mut impl Herdr,
) -> Vec<String> {
    let mut tasks: Vec<&SweepTask> = tasks.iter().collect();
    tasks.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    let mut notes = Vec::new();
    for task in tasks {
        let Some(account) = account_for(task, config) else {
            continue;
        };
        if find_stuck_signal(project_root, task, account, live, now, herdr) {
            notes.push(format!(
                "stuck: {} ({})",
                task.task_id,
                task.account.as_deref().unwrap_or("-")
            ));
        }
    }
    notes
}

/// Send [`STUCK_RESUME_PROMPT`] to every task `find_stuck_signal`
/// confirms is a dead-mid-response lane — or, for one already sent its one
/// resume nudge and still showing the identical connection-loss banner on
/// the same pane, escalate to a recorded `failed` outcome instead of a
/// second send (card `20261102-b-943ed3`; see the module doc comment for
/// the claim-then-act/`apply` contract and what this escalation narrows
/// from Python's own).
#[allow(clippy::too_many_arguments)]
pub fn resume_stuck_lanes(
    project_root: &Path,
    tasks: &[SweepTask],
    live: &[LiveAgentStatus],
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
) -> Vec<StuckResume> {
    let mut results = Vec::new();
    for task in tasks {
        let Some(account) = account_for(task, config) else {
            continue;
        };
        if !find_stuck_signal(project_root, task, account, live, now, herdr) {
            continue;
        }
        let pane = task
            .pane
            .as_deref()
            .expect("find_stuck_signal confirmed a pane");
        let working = pane_status(live, pane) == Some(AgentState::Working);
        let Some(text) = safe_pane_text(herdr, task, pane, &account.kind) else {
            continue;
        };

        if let Some(previous) =
            mv_state::nudge_memory::stuck_resume_claim(project_root, &task.task_id)
        {
            let Some(signal) = connection_loss_re().find(&text) else {
                continue;
            };
            let fingerprint = mv_state::nudge_memory::fingerprint(&text);
            if previous.pane.as_deref() != Some(pane)
                || previous.fingerprint.as_deref() != Some(fingerprint.as_str())
            {
                continue;
            }
            let Ok(sent_at) = mv_core::time::parse_flexible(&previous.sent_at) else {
                continue;
            };
            if now <= sent_at {
                continue;
            }
            let mut failed = false;
            if apply {
                let mut state = format!(
                    "connection lost: {} persisted after resume",
                    mv_core::redact::redact(signal.as_str()),
                );
                if let Some(path) =
                    crate::breaker::write_evidence(project_root, &account.pool, now, &text)
                {
                    state.push_str(&format!(" (evidence: {})", path.display()));
                }
                failed = mv_state::tasks::record_stranded_closure(
                    project_root,
                    &task.task_id,
                    TaskStatus::Failed,
                    &state,
                    Some(pane),
                );
            }
            results.push(StuckResume {
                task_id: task.task_id.clone(),
                pane: pane.to_string(),
                sent: false,
                failed,
            });
            continue;
        }

        if !apply {
            results.push(StuckResume {
                task_id: task.task_id.clone(),
                pane: pane.to_string(),
                sent: false,
                failed: false,
            });
            continue;
        }
        if !mv_state::nudge_memory::claim_stuck_resume(project_root, &task.task_id, now) {
            continue;
        }
        let sent = pane_still_hosts_task_agent(agents, task, pane)
            && send(herdr, pane, STUCK_RESUME_PROMPT);
        if sent && working {
            mv_state::nudge_memory::remember_stuck_resume_signal(
                project_root,
                &task.task_id,
                pane,
                &text,
            );
        }
        results.push(StuckResume {
            task_id: task.task_id.clone(),
            pane: pane.to_string(),
            sent,
            failed: false,
        });
    }
    results
}

/// Send [`RUNTIME_CAP_PROMPT`] to every RUNNING/STALLED task whose
/// account's `max_runtime_minutes` it has exceeded. See the module doc
/// comment for the claim-then-act/`apply` contract.
#[allow(clippy::too_many_arguments)]
pub fn cap_long_running_lanes(
    project_root: &Path,
    tasks: &[SweepTask],
    live: &[LiveAgentStatus],
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
) -> Vec<RuntimeCap> {
    let mut results = Vec::new();
    for task in tasks {
        if !base_running_candidate(task) {
            continue;
        }
        let pane = task
            .pane
            .as_deref()
            .expect("base_running_candidate checked pane");
        if !is_live(live, pane) {
            continue;
        }
        let Some(account) = account_for(task, config) else {
            continue;
        };
        let Some(cap_minutes) = account.max_runtime_minutes else {
            continue;
        };
        let Some(elapsed) = elapsed_since(task, now) else {
            continue;
        };
        if elapsed < time::Duration::minutes(i64::from(cap_minutes)) {
            continue;
        }
        if startup_grace_active(task, account, now) {
            continue;
        }
        if mv_state::nudge_memory::runtime_cap_already_sent(project_root, &task.task_id) {
            continue;
        }
        if safe_pane_text(herdr, task, pane, &account.kind).is_none() {
            continue;
        }
        if !apply {
            results.push(RuntimeCap {
                task_id: task.task_id.clone(),
                pane: pane.to_string(),
                sent: false,
            });
            continue;
        }
        if !mv_state::nudge_memory::claim_runtime_cap(project_root, &task.task_id, now) {
            continue;
        }
        let sent = pane_still_hosts_task_agent(agents, task, pane)
            && send(herdr, pane, RUNTIME_CAP_PROMPT);
        results.push(RuntimeCap {
            task_id: task.task_id.clone(),
            pane: pane.to_string(),
            sent,
        });
    }
    results
}

/// Exactly [`crate::runner::deliver_prompt`]'s own format string
/// (`runner.rs:1336-1337`), duplicated here rather than exposed from
/// `runner.rs`: this task's own write scope (card `20261114-b-4d8baf`)
/// permits a visibility-only change there, not a new function.
fn dispatch_brief_prompt(brief: &Path) -> String {
    format!(
        "Read `{}` and execute it. Follow its write scope exactly. Write your findings to `{}` and then write `done`, `blocked`, or `failed` to `{}`.",
        brief.display(),
        brief.with_file_name("result.md").display(),
        brief.with_file_name("status").display(),
    )
}

/// Both brief roots a writing lane can actually hold its own reconstructed
/// dispatch prompt under: the project's own task directory
/// ([`mv_state::paths::TaskPaths`]), and the worktree lane's own copy of
/// the same path — a worktree-only lane (design `20261115-seer-c2c967`'s
/// own example, `20261115-builder-7abd49`) keeps its brief solely under
/// the worktree path, so a project-only reconstruction would miss it.
/// Neither path needs to exist on disk: only the string this task's own
/// dispatch would have typed matters for the composer-text match below.
fn reconstructed_dispatch_prompts(project_root: &Path, task_id: &str) -> Vec<String> {
    let mut prompts = Vec::new();
    if let Ok(paths) = mv_state::paths::TaskPaths::for_task(project_root, task_id) {
        prompts.push(dispatch_brief_prompt(&paths.brief()));
    }
    let worktree_brief = project_root
        .join(".multiverse")
        .join("worktrees")
        .join(task_id)
        .join(".multiverse")
        .join("tasks")
        .join(task_id)
        .join("brief.md");
    prompts.push(dispatch_brief_prompt(&worktree_brief));
    prompts
}

/// The composer's own inner line — `kind`'s own marker up to the next of
/// `│`, `╰`, or a newline — trimmed, and read as empty when what remains
/// is only a `─` rule. Deliberately narrower than
/// [`crate::runner::prompt_still_in_composer`]'s own whitespace-and-
/// border-stripped region: that region still reads as non-empty once a
/// *known* composer's own footer chrome is in it (the grok empty-composer
/// fixture, `tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt`,
/// keeps "Weekly limit left …" there), and this function exists
/// specifically to tell that apart from genuinely unknown typed text.
/// `None` when no marker is found at all — the same "not a recognisable
/// composer" refusal [`crate::runner::prompt_still_in_composer`] gives.
fn composer_inner_text(text: &str, kind: &str) -> Option<String> {
    let marker = composer_marker(kind);
    let start = text.rfind(marker)?;
    let after = &text[start + marker.len()..];
    let mut inner = String::new();
    for ch in after.chars() {
        if matches!(ch, '│' | '╰' | '\n') {
            break;
        }
        inner.push(ch);
    }
    let trimmed = inner.trim();
    if trimmed.chars().all(|c| c == '─') {
        return Some(String::new());
    }
    Some(trimmed.to_string())
}

const COMPOSER_ENTER_MEMORY_FILENAME: &str = "composer-enter-memory.json";

/// This module's own one-shot claim file for [`submit_unsubmitted_composers`]
/// — not [`mv_state::nudge_memory`] (see the module doc comment for why).
fn composer_enter_memory_path(project_root: &Path) -> PathBuf {
    project_root
        .join(".multiverse")
        .join(COMPOSER_ENTER_MEMORY_FILENAME)
}

fn composer_enter_lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

fn load_composer_enter_map(path: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn composer_enter_already_sent(project_root: &Path, task_id: &str) -> bool {
    load_composer_enter_map(&composer_enter_memory_path(project_root)).contains_key(task_id)
}

/// Atomically reserve `task_id`'s one composer Enter, the same
/// claim-then-act contract every map in this module's companion
/// [`mv_state::nudge_memory`] already uses: `true` only for the call that
/// wins the reservation.
fn claim_composer_enter(project_root: &Path, task_id: &str, now: OffsetDateTime) -> bool {
    let path = composer_enter_memory_path(project_root);
    let Ok(_guard) = mv_core::lock::acquire_default(
        &composer_enter_lock_path(&path),
        COMPOSER_ENTER_MEMORY_FILENAME,
    ) else {
        return false;
    };
    let mut map = load_composer_enter_map(&path);
    if map.contains_key(task_id) {
        return false;
    }
    map.insert(task_id.to_string(), mv_core::time::format_seconds_utc(now));
    let bytes = serde_json::to_vec(&map).expect("claim map always serializes");
    mv_core::atomic::write_atomic(&path, &bytes).is_ok()
}

fn send_enter(herdr: &mut impl Herdr, name: &str) -> bool {
    herdr
        .call(&argv::agent_send_keys(name, &["Enter"]), SEND_TIMEOUT)
        .map(|result| result.ok())
        .unwrap_or(false)
}

/// [`submit_unsubmitted_composers`]'s own result: the Enter-or-would-Enter
/// outcomes, the report-only unsubmitted-composer notes, and the set of
/// task ids held out of the three canned-prompt senders this pass —
/// whether because their own Enter already claimed this task, or because
/// their composer holds unknown text no sender may type over.
#[derive(Debug, Default)]
struct ComposerCheck {
    holds: Vec<ComposerHold>,
    unsubmitted: Vec<String>,
    held: HashSet<String>,
}

/// Card `20261114-b-4d8baf`'s safety net, design `20261115-seer-c2c967`:
/// press the one Enter a lane's own composer needs when it still holds
/// this task's own reconstructed dispatch brief, unsubmitted — before any
/// of the three canned-prompt senders below can type a competing prompt
/// over it. Reuses [`finish_nudge_base_eligible`]'s own candidate set
/// (not yet recorded, no status file, a recognised local/codex/claude
/// account past its own startup grace, no result on disk) and
/// [`safe_pane_text`]'s own dialog/menu/vendor-dialog refusal, so this
/// never acts on any of the same five dialog shapes those already screen.
///
/// Three outcomes per eligible task:
/// - the composer positively matches one of [`reconstructed_dispatch_prompts`]:
///   one `send-keys … Enter`, [`claim_composer_enter`] spent either way, no
///   second Enter once [`composer_enter_already_sent`] is already true.
/// - the composer's own [`composer_inner_text`] is non-empty but matches
///   neither reconstruction: no Enter, reported as `"unsubmitted composer:
///   <id>"`, held out of the three senders below.
/// - the composer's own inner text is empty (today's shape): no hold at
///   all — the three senders below see this task exactly as before.
#[allow(clippy::too_many_arguments)]
fn submit_unsubmitted_composers(
    project_root: &Path,
    tasks: &[SweepTask],
    live: &[LiveAgentStatus],
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
) -> ComposerCheck {
    let mut check = ComposerCheck::default();
    for task in tasks {
        if !finish_nudge_base_eligible(task, config, project_root, now) {
            continue;
        }
        let pane = task.pane.as_deref().expect("base_eligible checked pane");
        if !pane_status(live, pane).is_some_and(AgentState::is_ready_for_input) {
            continue;
        }
        let Some(account) = account_for(task, config) else {
            continue;
        };
        let Some(text) = safe_pane_text(herdr, task, pane, &account.kind) else {
            continue;
        };
        let matched = reconstructed_dispatch_prompts(project_root, &task.task_id)
            .iter()
            .any(|prompt| prompt_still_in_composer(&text, prompt, &account.kind));
        if matched {
            // Held out of the three senders below regardless of claim
            // state: a brief still positively matched in the composer
            // must never receive a competing canned prompt on top of it,
            // whether this is the first pass to see it or a later one
            // whose own Enter already claimed this task's one-time slot.
            check.held.insert(task.task_id.clone());
            if composer_enter_already_sent(project_root, &task.task_id) {
                continue;
            }
            if !apply {
                check.holds.push(ComposerHold {
                    task_id: task.task_id.clone(),
                    pane: pane.to_string(),
                    sent: false,
                });
                continue;
            }
            if !claim_composer_enter(project_root, &task.task_id, now) {
                continue;
            }
            let name = task.agent.as_deref().expect("base_eligible checked agent");
            let sent = pane_still_hosts_task_agent(agents, task, pane) && send_enter(herdr, name);
            check.holds.push(ComposerHold {
                task_id: task.task_id.clone(),
                pane: pane.to_string(),
                sent,
            });
            continue;
        }
        if let Some(inner) = composer_inner_text(&text, &account.kind) {
            if !inner.is_empty() {
                check.held.insert(task.task_id.clone());
                check
                    .unsubmitted
                    .push(format!("unsubmitted composer: {}", task.task_id));
            }
        }
    }
    check
}

/// Run all three nudges for one project's already-fetched `tasks`. Calls
/// [`mv_state::tasks::AgentPanes::list_checked`] — the one Herdr `agent
/// list` round trip every function above shares — only when at least one
/// task could plausibly qualify for any of them
/// (`base_running_candidate`); a failed listing (`None`) is read the
/// same way, "nothing live enough to act on," so a Herdr outage never
/// looks like zero candidates found by exhaustive search.
pub fn run(
    project_root: &Path,
    tasks: &[SweepTask],
    config: &Config,
    apply: bool,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
    herdr: &mut impl Herdr,
) -> NudgeOutcome {
    if !tasks.iter().any(base_running_candidate) {
        return NudgeOutcome::default();
    }
    let Some(live) = agents.list_checked() else {
        return NudgeOutcome::default();
    };
    let composer_check = submit_unsubmitted_composers(
        project_root,
        tasks,
        &live,
        config,
        apply,
        now,
        agents,
        herdr,
    );
    let remaining: Vec<SweepTask> = tasks
        .iter()
        .filter(|task| !composer_check.held.contains(&task.task_id))
        .cloned()
        .collect();
    NudgeOutcome {
        stuck: stuck_notes(project_root, tasks, &live, config, now, herdr),
        finish: nudge_idle_lanes_to_finish(
            project_root,
            &remaining,
            &live,
            config,
            apply,
            now,
            agents,
            herdr,
        ),
        resume: resume_stuck_lanes(
            project_root,
            &remaining,
            &live,
            config,
            apply,
            now,
            agents,
            herdr,
        ),
        composer: composer_check.holds,
        unsubmitted_composer: composer_check.unsubmitted,
        cap: cap_long_running_lanes(
            project_root,
            &remaining,
            &live,
            config,
            apply,
            now,
            agents,
            herdr,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::load::Config;
    use mv_herdr::argv::HerdrCommand;
    use mv_herdr::envelope::CommandResult;
    use mv_state::tasks::TaskStatus;
    use serde_json::json;
    use std::time::Duration;

    /// A config with one named codex-kind account, the shape every
    /// `finish_nudge`/`cap`-eligible fixture below starts from, plus
    /// whatever extra `[accounts.codex-main]` keys (`max_runtime_minutes`,
    /// `startup_grace_seconds`, ...) the test needs. Built from real TOML
    /// through [`mv_config::load::load_config_str`], the same fixture
    /// style `commands::sweep`'s own tests and `health.rs`'s already use,
    /// rather than a hand-built [`mv_config::load::Config`] literal —
    /// `Config` carries many fields unrelated to this module that a
    /// literal would have to fake.
    fn config_with_codex_account(extra: &str) -> Config {
        let content = format!(
            "[accounts.codex-main]\n\
             kind = \"codex\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"<<PLACEHOLDER: x>>\"\n\
             {extra}\n"
        );
        mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap()
    }

    /// A config with one named claude-kind account — never local vendor,
    /// but [`finish_nudge_base_eligible`] now treats claude kind the same
    /// as codex (card `20261109-b-946582`), so this account is eligible
    /// once its own startup grace has elapsed.
    fn config_with_claude_account(extra: &str) -> Config {
        let content = format!(
            "[accounts.claude-main]\n\
             kind = \"claude\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"<<PLACEHOLDER: x>>\"\n\
             command = \"claude\"\n\
             {extra}\n"
        );
        mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap()
    }

    /// A config with one named grok-kind account — never local vendor,
    /// never codex or claude kind, so [`finish_nudge_base_eligible`] always
    /// refuses it (card `20261109-b-946582`: grok and any other kind stay
    /// excluded from the finish nudge).
    fn config_with_grok_account(extra: &str) -> Config {
        let content = format!(
            "[accounts.grok-main]\n\
             kind = \"grok\"\n\
             config_dir_var = \"GROK_HOME\"\n\
             config_dir = \"<<PLACEHOLDER: x>>\"\n\
             command = \"grok\"\n\
             {extra}\n"
        );
        mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap()
    }

    fn task(task_id: &str, account: &str, pane: &str, age_minutes: i64) -> SweepTask {
        SweepTask {
            task_id: task_id.to_string(),
            branch: "main".to_string(),
            role: "fixer".to_string(),
            pane: Some(pane.to_string()),
            file_status: None,
            recorded: false,
            agent: Some(format!("{task_id}-agent")),
            account: Some(account.to_string()),
            dispatched_at: Some(mv_core::time::now_utc() - time::Duration::minutes(age_minutes)),
            started_at: Some(mv_core::time::now_utc() - time::Duration::minutes(age_minutes)),
            dispatch_ordinal: 0,
            status: None,
            outcome_state: None,
            outcome_ordinal: None,
            revisions: 0,
        }
    }

    /// Removed on drop, including on panic (card `20261115-b-3e0fde`: the
    /// plain `PathBuf` this replaced only cleaned up on each test's own
    /// trailing `remove_dir_all` call, so an earlier assertion failure left
    /// it behind in `TMPDIR`). `Deref`s to `Path` so every existing call
    /// site keeps reading `&root`/`root.join(...)` unchanged.
    struct TempProject(std::path::PathBuf);
    impl std::ops::Deref for TempProject {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempProject {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_project() -> TempProject {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let dir = std::env::temp_dir().join(format!(
                "mv-engine-sweep-nudges-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => {
                    std::fs::create_dir_all(dir.join(".multiverse")).unwrap();
                    return TempProject(dir);
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temporary project: {error}"),
            }
        }
    }

    struct FakeAgents {
        live: Vec<LiveAgentStatus>,
    }

    impl AgentPanes for FakeAgents {
        fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
            self.live
                .iter()
                .map(|agent| mv_state::tasks::LiveAgent {
                    pane_id: agent.pane_id.clone(),
                })
                .collect()
        }
        fn close(&mut self, _pane_id: &str) -> bool {
            true
        }
        fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
            Some(self.live.clone())
        }
    }

    struct FakeHerdr {
        pane_text: &'static str,
        prompts: Vec<String>,
    }

    impl Herdr for FakeHerdr {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, crate::runner::RunnerError> {
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
                    self.prompts.push(command.argv[4].clone());
                    Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: Some(json!({"result": {}})),
                        protocol_error: None,
                    })
                }
                ("agent", "send-keys") => {
                    // Recorded as `"Enter:<name>"` in the same `prompts`
                    // vec every canned-prompt assertion already reads —
                    // no existing test's own pane text matches a
                    // reconstructed dispatch brief, so no existing
                    // assertion sees this new arm fire.
                    self.prompts.push(format!("Enter:{}", command.argv[3]));
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

    #[test]
    fn zero_candidates_never_touches_agent_panes_or_herdr() {
        struct PanicAgents;
        impl AgentPanes for PanicAgents {
            fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
                panic!("list should never be called")
            }
            fn close(&mut self, _pane_id: &str) -> bool {
                panic!("close should never be called")
            }
            fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
                panic!("list_checked should never be called")
            }
        }
        struct PanicHerdr;
        impl Herdr for PanicHerdr {
            fn call(
                &mut self,
                _command: &HerdrCommand,
                _timeout: Duration,
            ) -> Result<CommandResult, crate::runner::RunnerError> {
                panic!("herdr should never be called")
            }
        }

        let root = temp_project();
        let config = config_with_codex_account("");
        let recorded_task = SweepTask {
            recorded: true,
            ..task("t1", "codex-main", "w1:p1", 5)
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&recorded_task),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut PanicAgents,
            &mut PanicHerdr,
        );
        assert!(outcome.finish.is_empty());
        assert!(outcome.resume.is_empty());
        assert!(outcome.cap.is_empty());
    }

    #[test]
    fn an_idle_codex_lane_with_no_result_gets_nudged_once() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].sent);
        assert_eq!(herdr.prompts, vec![FINISH_NUDGE_PROMPT.to_string()]);
        assert!(mv_state::nudge_memory::finish_nudge_already_sent(
            &root, "t1"
        ));

        // A second pass must not claim (or send) again.
        let again = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(again.is_empty());
        assert_eq!(herdr.prompts.len(), 1, "no second send");
    }

    /// Card `20261103-b-695405`: `t1`'s pane looked idle and unclaimed in
    /// the snapshot `live` represents, but by the time this send would
    /// fire, Herdr's own fresh listing shows that same pane id now hosts
    /// `other-agent` — `t1`'s own agent finished and the pane was recycled
    /// in between. The finish nudge must never reach that live agent's
    /// pane, and `t1`'s own one-time slot must not read as "sent".
    #[test]
    fn a_pane_recycled_between_snapshot_and_send_is_never_finish_nudged() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let mut recycled = FakeAgents {
            live: vec![LiveAgentStatus {
                pane_id: "w1:p1".to_string(),
                name: Some("other-agent".to_string()),
                status: Some(AgentState::Idle),
            }],
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut recycled,
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(!results[0].sent, "never send into the recycled pane");
        assert!(
            herdr.prompts.is_empty(),
            "the finish nudge must never reach other-agent's pane"
        );
    }

    #[test]
    fn a_working_pane_is_never_finish_nudged() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Working),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    #[test]
    fn a_trust_dialog_blocks_the_finish_nudge_without_claiming_the_slot() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "do you trust this folder?",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
        assert!(!mv_state::nudge_memory::finish_nudge_already_sent(
            &root, "t1"
        ));
    }

    #[test]
    fn a_codex_billing_dialog_blocks_the_finish_nudge_without_claiming_the_slot() {
        // Card `20261102-b-bdf745`/`20261102-b-5b1325`: a pane parked on
        // codex's own out-of-credits dialog reports `done`/`idle`, not
        // `blocked` — `classify_prompt` is what catches it here, same as
        // Python's `fail_closed=True` posture.
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: include_str!("../../../tests/fixtures/rate-limit/codex-out-of-credits.txt"),
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
        assert!(!mv_state::nudge_memory::finish_nudge_already_sent(
            &root, "t1"
        ));
    }

    #[test]
    fn a_pane_already_showing_the_connection_drop_banner_is_left_for_resume() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "Connection lost, reconnecting...",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    #[test]
    fn dry_run_previews_without_claiming_or_sending() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            false,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(!results[0].sent);
        assert!(herdr.prompts.is_empty());
        assert!(!mv_state::nudge_memory::finish_nudge_already_sent(
            &root, "t1"
        ));
    }

    #[test]
    fn a_grok_account_is_never_finish_nudged() {
        let root = temp_project();
        let config = config_with_grok_account("");
        let t = task("t1", "grok-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live },
            &mut FakeHerdr {
                pane_text: "ready",
                prompts: Vec::new(),
            },
        );
        assert!(outcome.finish.is_empty());
    }

    /// Card `20261109-b-946582`: a running claude-kind lane past its own
    /// startup grace, with no status file, is finish-nudge eligible under
    /// exactly the same conditions as codex.
    #[test]
    fn a_claude_lane_past_startup_grace_with_no_status_is_finish_nudge_eligible() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].sent);
        assert_eq!(herdr.prompts, vec![FINISH_NUDGE_PROMPT.to_string()]);
    }

    /// Same shape as the claude case above — past its own startup grace,
    /// no status file — but grok stays excluded (card `20261109-b-946582`:
    /// only claude joins codex; every other kind does not).
    #[test]
    fn a_grok_lane_in_the_same_state_is_not_finish_nudge_eligible() {
        let root = temp_project();
        let config = config_with_grok_account("startup_grace_seconds = 30");
        let t = task("t1", "grok-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    /// A claude lane still inside its own startup grace never gets the
    /// finish nudge, the same grace codex already honors.
    #[test]
    fn a_claude_lane_inside_startup_grace_is_not_finish_nudge_eligible() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 600");
        let t = task("t1", "claude-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    #[test]
    fn a_result_md_already_on_disk_skips_the_finish_nudge() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let task_dir = root.join(".multiverse/tasks/t1");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("result.md"), "some findings\n").unwrap();
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut FakeHerdr {
                pane_text: "ready",
                prompts: Vec::new(),
            },
        );
        assert!(results.is_empty());
    }

    #[test]
    fn a_fresh_connection_drop_is_reported_stuck_only_on_the_second_pass() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 45);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let now = mv_core::time::now_utc();
        let mut herdr = FakeHerdr {
            pane_text: "Connection lost, reconnecting...",
            prompts: Vec::new(),
        };
        let first = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now,
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(first.is_empty(), "a single read never confirms stuck");
        assert!(herdr.prompts.is_empty());

        let second = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now + time::Duration::minutes(5),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(second.len(), 1);
        assert!(second[0].sent);
        assert!(!second[0].failed);
        assert_eq!(herdr.prompts, vec![STUCK_RESUME_PROMPT.to_string()]);
    }

    #[test]
    fn stuck_notes_keep_printing_after_the_one_shot_resume_has_fired() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 45);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let now = mv_core::time::now_utc();
        let mut herdr = FakeHerdr {
            pane_text: "Connection lost, reconnecting...",
            prompts: Vec::new(),
        };
        let first = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            now,
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(first.stuck.is_empty(), "a single read never confirms stuck");
        assert!(first.resume.is_empty());

        let second = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            now + time::Duration::minutes(5),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(second.stuck, ["stuck: t1 (codex-main)"]);
        assert_eq!(second.resume.len(), 1);
        assert!(second.resume[0].sent);

        let third = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            now + time::Duration::minutes(10),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(
            third.stuck,
            ["stuck: t1 (codex-main)"],
            "the visibility line is independent of the spent resume claim"
        );
        assert!(
            third.resume.is_empty(),
            "the one-shot resume must not fire again"
        );
        assert_eq!(herdr.prompts.len(), 1);
    }

    /// Card `20261103-b-695405`: the same confirmed-stuck pane as above, but
    /// by the time the resume prompt would actually send, a fresh `agent
    /// list` shows `w1:p1` now hosts `other-agent` — `t1`'s own agent
    /// dropped the connection, Herdr reclaimed it, and an unrelated agent
    /// was started in the same pane. The resume prompt must never reach it.
    #[test]
    fn a_pane_recycled_between_the_stuck_confirmation_and_the_resume_send_is_never_resumed() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 45);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let now = mv_core::time::now_utc();
        let mut herdr = FakeHerdr {
            pane_text: "Connection lost, reconnecting...",
            prompts: Vec::new(),
        };
        let first = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now,
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(first.is_empty(), "a single read never confirms stuck");

        let mut recycled = FakeAgents {
            live: vec![LiveAgentStatus {
                pane_id: "w1:p1".to_string(),
                name: Some("other-agent".to_string()),
                status: Some(AgentState::Idle),
            }],
        };
        let second = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now + time::Duration::minutes(5),
            &mut recycled,
            &mut herdr,
        );
        assert_eq!(second.len(), 1);
        assert!(!second[0].sent, "never send into the recycled pane");
        assert!(!second[0].failed);
        assert!(
            herdr.prompts.is_empty(),
            "the resume prompt must never reach other-agent's pane"
        );
    }

    #[test]
    fn menu_reason_only_matches_an_open_menu_in_the_pane_tail() {
        let menu = "earlier scrollback\n\n? Pick one\n> option a\n  option b\n\n\
                    Enter to select · Esc to cancel";
        assert!(menu_reason(menu));
        assert!(!menu_reason("just some ordinary working output"));
    }

    #[test]
    fn a_pane_parked_on_an_open_question_menu_is_never_finish_nudged() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = task("t1", "codex-main", "w1:p1", 1);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "? Which should I do\n> a\n  b\nEnter to select · Esc to cancel",
            prompts: Vec::new(),
        };
        let results = nudge_idle_lanes_to_finish(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(results.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    #[test]
    fn a_lane_still_showing_the_same_connection_loss_banner_after_its_resume_escalates_to_failed() {
        assert_stuck_escalation(true);
    }

    #[test]
    fn failed_evidence_write_does_not_prevent_stuck_escalation() {
        assert_stuck_escalation(false);
    }

    fn assert_stuck_escalation(evidence_writable: bool) {
        let root = temp_project();
        let config = config_with_codex_account("pool = \"shared/pool\"");
        let t = task("t1", "codex-main", "w1:p1", 45);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Working),
        }];
        let now = mv_core::time::now_utc();
        let mut herdr = FakeHerdr {
            pane_text: "Connection lost, reconnecting...\nBearer test-credential",
            prompts: Vec::new(),
        };

        let first = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now,
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert!(first.is_empty(), "a single read never confirms stuck");

        let second = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now + time::Duration::minutes(5),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(second.len(), 1);
        assert!(second[0].sent);
        assert!(!second[0].failed);

        let evidence_dir = root.join(".multiverse/evidence");
        let dry_run = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            false,
            now + time::Duration::minutes(10),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(dry_run.len(), 1);
        assert!(!dry_run[0].failed);
        assert!(!evidence_dir.exists(), "dry run must not write evidence");
        assert!(!root.join(".multiverse/routing.log").exists());
        if !evidence_writable {
            std::fs::write(&evidence_dir, "block directory creation").unwrap();
        }

        let third = resume_stuck_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            now + time::Duration::minutes(10),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(third.len(), 1);
        assert!(!third[0].sent);
        assert!(third[0].failed);
        // Only ever the one resume prompt — the escalation never resends.
        assert_eq!(herdr.prompts, vec![STUCK_RESUME_PROMPT.to_string()]);

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains("event=result\ttask=t1\tstatus=failed"));
        assert!(log.contains("connection lost: Connection lost persisted after resume"));
        assert!(!log.contains("test-credential"));
        if evidence_writable {
            let files: Vec<_> = std::fs::read_dir(&evidence_dir).unwrap().collect();
            assert_eq!(files.len(), 1);
            let path = files[0].as_ref().unwrap().path();
            assert!(path
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with("-shared_pool.txt"));
            assert!(log.contains(&format!("evidence: {}", path.display())));
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, mv_core::redact::redact(herdr.pane_text));
            assert!(!text.contains("test-credential"));
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&evidence_dir)
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        } else {
            assert!(!log.contains("evidence:"), "do not link missing evidence");
        }
    }

    #[test]
    fn a_task_under_the_runtime_cap_is_not_capped() {
        let root = temp_project();
        let config = config_with_codex_account("max_runtime_minutes = 60");
        let t = task("t1", "codex-main", "w1:p1", 10);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Working),
        }];
        let results = cap_long_running_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut FakeHerdr {
                pane_text: "still going",
                prompts: Vec::new(),
            },
        );
        assert!(results.is_empty());
    }

    #[test]
    fn a_task_past_its_runtime_cap_gets_capped_even_while_working() {
        let root = temp_project();
        let config = config_with_codex_account("max_runtime_minutes = 60");
        let t = task("t1", "codex-main", "w1:p1", 90);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Working),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "still going",
            prompts: Vec::new(),
        };
        let results = cap_long_running_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(results[0].sent);
        assert_eq!(herdr.prompts, vec![RUNTIME_CAP_PROMPT.to_string()]);
    }

    /// Card `20261103-b-695405`: `t1` is well past its runtime cap in the
    /// snapshot `live` represents, but Herdr's own fresh listing right
    /// before the send shows `w1:p1` now hosts `other-agent` — `t1`'s own
    /// agent finished and the pane was recycled. The cap prompt must never
    /// interrupt that live agent's own, unrelated work.
    #[test]
    fn a_pane_recycled_between_snapshot_and_send_is_never_capped() {
        let root = temp_project();
        let config = config_with_codex_account("max_runtime_minutes = 60");
        let t = task("t1", "codex-main", "w1:p1", 90);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Working),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "still going",
            prompts: Vec::new(),
        };
        let mut recycled = FakeAgents {
            live: vec![LiveAgentStatus {
                pane_id: "w1:p1".to_string(),
                name: Some("other-agent".to_string()),
                status: Some(AgentState::Working),
            }],
        };
        let results = cap_long_running_lanes(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut recycled,
            &mut herdr,
        );
        assert_eq!(results.len(), 1);
        assert!(!results[0].sent, "never send into the recycled pane");
        assert!(
            herdr.prompts.is_empty(),
            "the runtime cap prompt must never reach other-agent's pane"
        );
    }

    #[test]
    fn a_task_with_a_terminal_status_file_is_never_a_candidate() {
        let root = temp_project();
        let config = config_with_codex_account("");
        let t = SweepTask {
            file_status: Some(TaskStatus::Done),
            ..task("t1", "codex-main", "w1:p1", 90)
        };
        assert!(!base_running_candidate(&t));
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: Vec::new() },
            &mut FakeHerdr {
                pane_text: "",
                prompts: Vec::new(),
            },
        );
        assert!(outcome.finish.is_empty());
        assert!(outcome.resume.is_empty());
        assert!(outcome.cap.is_empty());
    }

    // --- `submit_unsubmitted_composers` (card `20261114-b-4d8baf`, design
    // `20261115-seer-c2c967`). ---

    /// Fixed-width character wrap, oblivious to word boundaries — the same
    /// bordered-composer shape `runner.rs`'s own `boxed_composer` test
    /// helper builds (card `20261109-b-620eeb`), duplicated here rather
    /// than exposed from `runner.rs`'s own test module: this task's write
    /// scope permits only a visibility change there.
    fn boxed_composer(prompt: &str, kind: &str, width: usize) -> String {
        let marked: Vec<char> = format!("{} {prompt}", crate::runner::composer_marker(kind))
            .chars()
            .collect();
        let border = "─".repeat(width + 2);
        let mut out = format!("╭{border}╮\n");
        let mut i = 0;
        while i < marked.len() {
            let end = (i + width).min(marked.len());
            let row: String = marked[i..end].iter().collect();
            out.push_str(&format!("│ {row:<width$} │\n"));
            i = end;
        }
        out.push_str(&format!("╰{border}╯"));
        out
    }

    #[test]
    fn a_claude_lane_holding_its_own_reconstructed_worktree_brief_gets_exactly_one_enter_and_no_finish_nudge(
    ) {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let worktree_brief = root.join(".multiverse/worktrees/t1/.multiverse/tasks/t1/brief.md");
        let prompt = dispatch_brief_prompt(&worktree_brief);
        let pane_text = boxed_composer(&prompt, "claude", 30);
        let mut herdr = FakeHerdr {
            pane_text: Box::leak(pane_text.into_boxed_str()),
            prompts: Vec::new(),
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live: live.clone() },
            &mut herdr,
        );
        assert_eq!(outcome.composer.len(), 1);
        assert!(outcome.composer[0].sent);
        assert!(outcome.finish.is_empty(), "no competing finish nudge");
        assert!(outcome.unsubmitted_composer.is_empty());
        assert_eq!(herdr.prompts, vec!["Enter:t1-agent".to_string()]);
        assert!(composer_enter_already_sent(&root, "t1"));

        // A second pass over the same still-matching pane text must not
        // press a second Enter, and must still refuse the competing
        // finish nudge (the composer is still, as far as this pass can
        // tell, holding the brief).
        let second = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc() + time::Duration::minutes(1),
            &mut FakeAgents { live },
            &mut herdr,
        );
        assert!(second.composer.is_empty());
        assert!(second.finish.is_empty(), "still no competing finish nudge");
        assert_eq!(herdr.prompts.len(), 1, "no second Enter");
    }

    #[test]
    fn a_composer_holding_unrecognised_text_gets_no_enter_and_no_competing_finish_nudge() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "❯ check the log",
            prompts: Vec::new(),
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live },
            &mut herdr,
        );
        assert!(outcome.composer.is_empty());
        assert!(outcome.finish.is_empty(), "no competing finish nudge");
        assert_eq!(outcome.unsubmitted_composer, ["unsubmitted composer: t1"]);
        assert!(herdr.prompts.is_empty(), "no Enter, no canned prompt");
        assert!(!composer_enter_already_sent(&root, "t1"));
    }

    #[test]
    fn an_empty_composer_keeps_todays_finish_nudge_behaviour() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "ready for next instruction",
            prompts: Vec::new(),
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live },
            &mut herdr,
        );
        assert!(outcome.composer.is_empty());
        assert!(outcome.unsubmitted_composer.is_empty());
        assert_eq!(outcome.finish.len(), 1);
        assert!(outcome.finish[0].sent);
        assert_eq!(herdr.prompts, vec![FINISH_NUDGE_PROMPT.to_string()]);
    }

    #[test]
    fn composer_inner_text_reads_the_grok_empty_box_as_empty() {
        // The grok fixture's own footer chrome ("Weekly limit left …")
        // sits in the same region once the box border is gone; only this
        // narrower, un-stripped line (ending at the first `│`) tells that
        // apart from genuinely typed text.
        let text =
            include_str!("../../../tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt");
        assert_eq!(composer_inner_text(text, "grok"), Some(String::new()));
    }

    #[test]
    fn composer_inner_text_reads_typed_text_as_non_empty() {
        assert_eq!(
            composer_inner_text("❯ check the log", "claude").as_deref(),
            Some("check the log")
        );
    }

    #[test]
    fn composer_inner_text_is_none_with_no_marker_at_all() {
        assert_eq!(composer_inner_text("no marker here", "claude"), None);
    }

    #[test]
    fn a_trust_dialog_never_gets_a_composer_enter_or_an_unsubmitted_report() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let mut herdr = FakeHerdr {
            pane_text: "do you trust this folder?",
            prompts: Vec::new(),
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            true,
            mv_core::time::now_utc(),
            &mut FakeAgents { live },
            &mut herdr,
        );
        assert!(outcome.composer.is_empty());
        assert!(outcome.unsubmitted_composer.is_empty());
        assert!(outcome.finish.is_empty());
        assert!(herdr.prompts.is_empty());
    }

    #[test]
    fn dry_run_previews_the_composer_hold_without_sending_or_claiming() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let worktree_brief = root.join(".multiverse/worktrees/t1/.multiverse/tasks/t1/brief.md");
        let prompt = dispatch_brief_prompt(&worktree_brief);
        let pane_text = boxed_composer(&prompt, "claude", 30);
        let mut herdr = FakeHerdr {
            pane_text: Box::leak(pane_text.into_boxed_str()),
            prompts: Vec::new(),
        };
        let outcome = run(
            &root,
            std::slice::from_ref(&t),
            &config,
            false,
            mv_core::time::now_utc(),
            &mut FakeAgents { live },
            &mut herdr,
        );
        assert_eq!(outcome.composer.len(), 1);
        assert!(!outcome.composer[0].sent);
        assert!(outcome.finish.is_empty(), "preview never nudges either");
        assert!(herdr.prompts.is_empty());
        assert!(!composer_enter_already_sent(&root, "t1"));
    }

    /// Card `20261103-b-695405`'s own reclaim guard, re-checked here for
    /// the composer Enter: `t1`'s own agent looked idle and unclaimed in
    /// the snapshot `live` represents, but a fresh `agent list` taken
    /// right before the send shows the same pane now hosts `other-agent`
    /// — the Enter must never reach that live agent's own pane.
    #[test]
    fn a_pane_recycled_between_snapshot_and_send_is_never_composer_entered() {
        let root = temp_project();
        let config = config_with_claude_account("startup_grace_seconds = 30");
        let t = task("t1", "claude-main", "w1:p1", 5);
        let live = vec![LiveAgentStatus {
            pane_id: "w1:p1".to_string(),
            name: Some("t1-agent".to_string()),
            status: Some(AgentState::Idle),
        }];
        let worktree_brief = root.join(".multiverse/worktrees/t1/.multiverse/tasks/t1/brief.md");
        let prompt = dispatch_brief_prompt(&worktree_brief);
        let pane_text = boxed_composer(&prompt, "claude", 30);
        let mut herdr = FakeHerdr {
            pane_text: Box::leak(pane_text.into_boxed_str()),
            prompts: Vec::new(),
        };
        let mut recycled = FakeAgents {
            live: vec![LiveAgentStatus {
                pane_id: "w1:p1".to_string(),
                name: Some("other-agent".to_string()),
                status: Some(AgentState::Idle),
            }],
        };
        let check = submit_unsubmitted_composers(
            &root,
            std::slice::from_ref(&t),
            &live,
            &config,
            true,
            mv_core::time::now_utc(),
            &mut recycled,
            &mut herdr,
        );
        assert_eq!(check.holds.len(), 1);
        assert!(!check.holds[0].sent, "never send into the recycled pane");
        assert!(
            herdr.prompts.is_empty(),
            "the composer Enter must never reach other-agent's pane"
        );
    }
}

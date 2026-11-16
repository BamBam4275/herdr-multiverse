//! Task directories: status is `done|blocked|failed` only, and
//! absence means still running.
//!
//! Mirrors [`TaskStatus`] (`brief.py:47-70`) and the read side of
//! `runner.read_status` (`runner.py:703-722`): a missing status file,
//! an unreadable one, or one holding anything outside these three
//! tokens all collapse to "still running" — a crashed agent never
//! writes anything, so absence must never be promoted to any of the
//! three terminal states on its own (spec §5/§3.1).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use mv_core::time::{format_seconds_utc, now_utc};
use mv_herdr::envelope::AgentState;
use mv_herdr::{argv, run};
use time::OffsetDateTime;

use crate::backlog::{self, json_encode_str, BacklogError};
use crate::index::{records as index_records, Selector};
use crate::log::{LogError, ProjectLog};

/// The three values a specialist may write to its status file.
///
/// All three are terminal — that is the point of the file (§3.1:
/// "never act on assumptions from a still-running task; read
/// `result.md` only when `status` is terminal"). A running task has no
/// status file at all; absence is the non-terminal case, represented
/// here by [`read_status`] returning `None` rather than a fourth
/// variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaskStatus {
    Done,
    Blocked,
    Failed,
}

impl TaskStatus {
    /// All three values, in the order offered to a specialist.
    pub const ALL: [TaskStatus; 3] = [TaskStatus::Done, TaskStatus::Blocked, TaskStatus::Failed];

    /// The exact token a specialist writes to the status file.
    pub fn as_str(self) -> &'static str {
        match self {
            TaskStatus::Done => "done",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Failed => "failed",
        }
    }

    /// Parse a status file's contents. Whitespace and case are
    /// forgiven (a trailing newline from `echo` is the common case);
    /// anything else is rejected rather than promoted to a fourth
    /// status — an agent that wrote `"complete"` or `"ok"` did not
    /// follow the protocol.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "done" => Some(TaskStatus::Done),
            "blocked" => Some(TaskStatus::Blocked),
            "failed" => Some(TaskStatus::Failed),
            _ => None,
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The three permitted status values, rendered as backticked prose
/// (`` `done`, `blocked`, `failed` ``) for the brief's "On completion"
/// section.
///
/// Derived from [`TaskStatus::ALL`] rather than written out a second
/// time, so a brief can never list a value this reader does not
/// accept, or omit one it does (mirrors `_status_values`,
/// `brief.py:73-80`).
pub fn status_values_markdown() -> String {
    TaskStatus::ALL
        .iter()
        .map(|status| format!("`{status}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Read a task's status file.
///
/// `None` means "still running" — the file is absent, unreadable, or
/// holds something other than one of [`TaskStatus`]'s three tokens. A
/// handful of retries absorb a truncate-then-write race a plain
/// overwrite can expose to a reader for one read (`runner.py:703-722`
/// notes the same race on the Python side).
pub fn read_status(status_path: &Path) -> Option<TaskStatus> {
    for _attempt in 0..3 {
        match std::fs::read_to_string(status_path) {
            Ok(text) => {
                if let Some(status) = TaskStatus::parse(&text) {
                    return Some(status);
                }
                return None;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => continue,
        }
    }
    None
}

/// Like [`read_status`], but a status file whose mtime predates `since`
/// reads as absent rather than terminal — the same rule Python's
/// `_status_predates_run` applies (`tasks.py:811-839`): a status file left
/// over from a previous occupant of this task id must never be read as
/// *this* run's own terminal state. A caller driving automatic
/// completion detection across a same-id redispatch (e.g.
/// `mv_engine::runner::wait_for_result`) should use this instead of
/// [`read_status`] (`critic-history-compat.md` finding 6).
pub fn read_status_since(status_path: &Path, since: std::time::SystemTime) -> Option<TaskStatus> {
    let modified = std::fs::metadata(status_path).ok()?.modified().ok()?;
    if modified < since {
        return None;
    }
    read_status(status_path)
}

fn offset_to_system_time(when: OffsetDateTime) -> Option<std::time::SystemTime> {
    let unix = when.unix_timestamp();
    if unix < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::new(unix as u64, when.nanosecond()))
}

/// `task`'s lane worktree directory after validating `lane_task_id` is a
/// single identifier component — Python's `_safe_lane_root`. A hostile
/// `branch=` tail must never join past `.multiverse/worktrees/`.
pub fn safe_lane_root(project_root: &Path, lane_task_id: &str) -> Option<PathBuf> {
    if !mv_core::id::is_valid_identifier(lane_task_id) {
        return None;
    }
    Some(
        project_root
            .join(".multiverse/worktrees")
            .join(lane_task_id),
    )
}

/// `None` when `task_id` is not a single identifier component — the same
/// guard [`safe_lane_root`] applies to `lane_task_id`, just above, before
/// either is joined into a path. A hostile `task_id` (e.g. `../x`) must
/// never be joined past `.multiverse/tasks/`.
fn status_path_under(parent: &Path, task_id: &str) -> Option<PathBuf> {
    if !mv_core::id::is_valid_identifier(task_id) {
        return None;
    }
    Some(
        parent
            .join(".multiverse/tasks")
            .join(task_id)
            .join("status"),
    )
}

fn read_status_maybe_since(
    path: &Path,
    since: Option<std::time::SystemTime>,
) -> Option<TaskStatus> {
    match since {
        Some(since) => read_status_since(path, since),
        None => read_status(path),
    }
}

/// Writer status is read from the lane worktree first, then the project
/// `.multiverse/tasks/` tree. A status whose mtime predates `since`
/// (this run's `event=started`, else `event=dispatch`) is ignored as an
/// earlier occupant's leftover — Python's `resolve` / `_status_predates_run`.
pub fn read_writer_status(
    root: &Path,
    task_id: &str,
    lane_task_id: &str,
    since: Option<OffsetDateTime>,
) -> Option<TaskStatus> {
    let since = since.and_then(offset_to_system_time);
    if let Some(lane) = safe_lane_root(root, lane_task_id) {
        if let Some(status) =
            status_path_under(&lane, task_id).and_then(|path| read_status_maybe_since(&path, since))
        {
            return Some(status);
        }
    }
    status_path_under(root, task_id).and_then(|path| read_status_maybe_since(&path, since))
}

fn lane_holds_current_status(root: &Path, task: &SweepTask) -> bool {
    let Some(lane) = safe_lane_root(root, lane_task_id(task)) else {
        return false;
    };
    let Some(path) = status_path_under(&lane, &task.task_id) else {
        return false;
    };
    read_status_maybe_since(
        &path,
        task.started_at
            .or(task.dispatched_at)
            .and_then(offset_to_system_time),
    )
    .is_some()
}

/// `result.md` for a `done` outcome: the lane copy when that is where
/// this run's status lives, else the main checkout — Python `reconcile`'s
/// `result_root` choice.
fn has_usable_result_for(root: &Path, task: &SweepTask) -> bool {
    if lane_holds_current_status(root, task) {
        if let Some(lane) = safe_lane_root(root, lane_task_id(task)) {
            return has_usable_result(&lane, &task.task_id);
        }
    }
    has_usable_result(root, &task.task_id)
}

/// What [`read_status`]'s `None` collapses away: a status file that was
/// never written at all, versus one that was written but holds something
/// outside the three terminal tokens (the live fleet has seen `donedone`,
/// `donedonedone`, `doneblocked` from partial writes). Listing reads
/// (`read_status`) rightly treat both as "still running"; `result`'s
/// refusal text must tell them apart — "no status file" is ambiguous
/// (could still be running), "unrecognized status" is a reader refusing
/// to guess at a value it has never seen before (spec-on-disk-contract.md's
/// `tasks/<task-id>/status` row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusRead {
    Absent,
    Unrecognized(String),
    Terminal(TaskStatus),
}

/// Like [`read_status`], but keeps the raw (trimmed) contents on failure
/// instead of discarding them, and keeps "never written" distinct from
/// "written, not one of the three tokens".
pub fn read_status_verbose(status_path: &Path) -> StatusRead {
    for _attempt in 0..3 {
        match std::fs::read_to_string(status_path) {
            Ok(text) => {
                return match TaskStatus::parse(&text) {
                    Some(status) => StatusRead::Terminal(status),
                    None => StatusRead::Unrecognized(text.trim().to_string()),
                };
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return StatusRead::Absent,
            Err(_) => continue,
        }
    }
    StatusRead::Absent
}

/// Fields an `event=result` line must carry before it counts as a *usable*
/// outcome (`accounts.py`'s `_USABLE_RESULT_FIELD_PREFIXES`).
const RESULT_FIELDS: &[&str] = &["status", "state", "revisions"];

/// Fields an `event=supersede` line must carry before it counts as a
/// *usable* correction (`accounts.py`'s `_USABLE_SUPERSEDE_FIELD_PREFIXES`).
const SUPERSEDE_FIELDS: &[&str] = &["supersedes", "status", "state", "reason"];

/// A reason/note body's limit, matching `activity.MAX_BODY_BYTES`
/// (`activity.py:305`) — the same bound `supersede_log_line` enforces via
/// `activity._validate_body`.
const MAX_REASON_BYTES: usize = 4096;

/// Every line ordinal one task's own outcome records could be named by
/// (`--supersedes`/`--acknowledges`), what (if anything) has already
/// superseded each one, and each one's own `when` for display.
///
/// Mirrors `accounts._outcome_targets`: ordinals are 1-based positions over
/// **every raw line** the routing log has, not just the ones recognized
/// here — [`crate::index`] already counts that way (`IndexEntry::ordinal`
/// is 0-based; this adds the `+1` the display convention elsewhere in this
/// crate already uses for `#<line>`).
#[derive(Debug, Default, Clone)]
pub struct OutcomeTargets {
    /// Ordinals of every `event=result`/`event=supersede` line that carries
    /// a complete set of outcome fields — the only lines `--supersedes`/
    /// `--acknowledges` may name.
    pub targetable: BTreeSet<u64>,
    /// Ordinal of an already-superseded target mapped to the
    /// `(status, state)` it was corrected to, so a repeat of the same
    /// correction can be recognized as a no-op instead of a conflict.
    pub applied: BTreeMap<u64, (String, String)>,
    /// Ordinal mapped to that line's own `when`, kept only so error text can
    /// list known outcome lines for a human to recognize.
    pub when_by_ordinal: BTreeMap<u64, String>,
}

/// Scan `root`'s routing log for every outcome record `task` has written —
/// `event=result` or `event=supersede`, each carrying a complete set of
/// outcome fields. Only meaningful as a snapshot: a concurrent writer can
/// invalidate it before a caller acts on it, the same caveat
/// `accounts._outcome_targets` states for itself.
pub fn outcome_targets(root: &Path, task: &str) -> OutcomeTargets {
    let mut out = OutcomeTargets::default();
    let log_path = ProjectLog::new(root).path().to_path_buf();
    let Ok(recs) = index_records(&log_path, Selector::Tasks) else {
        return out;
    };
    for rec in &recs {
        let Ok(parsed) = rec.parse() else { continue };
        if parsed.fields.get("task").map(String::as_str) != Some(task) {
            continue;
        }
        let ordinal = rec.entry.ordinal + 1;
        match parsed.event() {
            "result" if RESULT_FIELDS.iter().all(|f| parsed.fields.contains_key(*f)) => {
                out.targetable.insert(ordinal);
                out.when_by_ordinal.insert(ordinal, parsed.when.clone());
            }
            "supersede"
                if SUPERSEDE_FIELDS
                    .iter()
                    .all(|f| parsed.fields.contains_key(*f)) =>
            {
                out.targetable.insert(ordinal);
                out.when_by_ordinal.insert(ordinal, parsed.when.clone());
                if let Some(target) = parsed
                    .fields
                    .get("supersedes")
                    .and_then(|v| v.parse::<u64>().ok())
                {
                    let status = parsed.fields.get("status").cloned().unwrap_or_default();
                    let state = parsed.fields.get("state").cloned().unwrap_or_default();
                    out.applied.insert(target, (status, state));
                }
            }
            _ => {}
        }
    }
    out
}

/// Every ordinal `task` already has an `event=acknowledged` record for
/// (`accounts._acknowledged_targets`).
fn acknowledged_targets(root: &Path, task: &str) -> BTreeSet<u64> {
    let mut out = BTreeSet::new();
    let log_path = ProjectLog::new(root).path().to_path_buf();
    let Ok(recs) = index_records(&log_path, Selector::Tasks) else {
        return out;
    };
    for rec in &recs {
        let Ok(parsed) = rec.parse() else { continue };
        if parsed.fields.get("task").map(String::as_str) != Some(task) {
            continue;
        }
        if parsed.event() != "acknowledged" {
            continue;
        }
        if let Some(target) = parsed
            .fields
            .get("acknowledges")
            .and_then(|v| v.parse::<u64>().ok())
        {
            out.insert(target);
        }
    }
    out
}

/// `task`'s outcome record at line `ordinal`'s own `status`/`state` fields,
/// read straight off the routing log — `"?"` for either field a damaged or
/// unexpectedly short line is missing (`accounts._current_outcome_fields`).
pub fn current_outcome_fields(root: &Path, ordinal: u64) -> (String, String) {
    let log_path = ProjectLog::new(root).path().to_path_buf();
    let Ok(text) = std::fs::read_to_string(&log_path) else {
        return ("?".to_string(), "?".to_string());
    };
    let Some(line) = text.lines().nth((ordinal.saturating_sub(1)) as usize) else {
        return ("?".to_string(), "?".to_string());
    };
    let parts: Vec<&str> = line.split('\t').collect();
    let status = parts
        .iter()
        .find_map(|p| p.strip_prefix("status="))
        .unwrap_or("?")
        .to_string();
    let state = parts
        .iter()
        .find_map(|p| p.strip_prefix("state="))
        .unwrap_or("?")
        .to_string();
    (status, state)
}

/// The known-outcome-lines listing every `UnknownSupersedeTargetError`/
/// `UnknownAcknowledgeTargetError` message carries.
fn known_lines_listing(when_by_ordinal: &BTreeMap<u64, String>) -> String {
    if when_by_ordinal.is_empty() {
        return "  (no outcome records at all)".to_string();
    }
    when_by_ordinal
        .iter()
        .map(|(ordinal, when)| format!("  - line {ordinal} ({when})"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Rejects a `task`/`status`/`state` value that could smuggle a duplicate
/// field into the tab-separated routing-log line: a tab, CR, or LF breaks
/// the line's own field separator, and `=` lets the value be read back as
/// `key=value` once it lands inside a field that is itself `key=<this>`
/// (card `20261116-b-5f49bd` — a tab in `--state` let a duplicate `status=`
/// field through, after which `outcome_targets` and `current_outcome_fields`
/// disagreed about what the line said). Leading/trailing space is rejected
/// too, as a value that was never meant to look like this.
fn validate_log_field(flag: &str, value: &str) -> Result<(), String> {
    if value.contains(['\t', '\r', '\n', '=']) {
        return Err(format!(
            "{flag}: must not contain a tab, carriage return, line feed, or '=' — any of those \
             could smuggle an extra field into the tab-separated routing-log line (got {value:?})"
        ));
    }
    if value.starts_with(' ') || value.ends_with(' ') {
        return Err(format!(
            "{flag}: must not have a leading or trailing space (got {value:?})"
        ));
    }
    Ok(())
}

fn validate_reason(reason: &str) -> Result<(), SupersedeError> {
    let bytes = reason.as_bytes();
    if bytes.contains(&0) {
        return Err(SupersedeError::InvalidReason(
            "message body must not contain a NUL byte".to_string(),
        ));
    }
    if bytes.len() > MAX_REASON_BYTES {
        return Err(SupersedeError::InvalidReason(format!(
            "message body is {} bytes; the limit is {MAX_REASON_BYTES} (4 KiB)",
            bytes.len()
        )));
    }
    Ok(())
}

/// `SupersedeError::Conflicting`'s payload, boxed so the common,
/// cheap-to-construct variants of [`SupersedeError`] stay small
/// (`clippy::result_large_err`).
#[derive(Debug)]
pub struct ConflictingSupersede {
    /// The task whose outcome record is being superseded.
    pub task: String,
    /// Ordinal of the outcome record the new correction targets.
    pub supersedes: u64,
    /// `when` of the correction already on file for this target.
    pub when: String,
    /// `status` the existing correction recorded.
    pub existing_status: String,
    /// `state` the existing correction recorded.
    pub existing_state: String,
    /// `status` the new, refused correction tried to record.
    pub attempted_status: String,
    /// `state` the new, refused correction tried to record.
    pub attempted_state: String,
}

impl fmt::Display for ConflictingSupersede {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}@line {} ({}) was already superseded with status={} state={}; refusing to also \
             record status={} state={} for the same target — supersede the corrector instead, \
             not the same target twice",
            self.task,
            self.supersedes,
            self.when,
            self.existing_status,
            self.existing_state,
            self.attempted_status,
            self.attempted_state
        )
    }
}

/// Every reason a correction is refused — mirrors `accounts.SupersedeError`
/// and its subclasses.
#[derive(Debug, thiserror::Error)]
pub enum SupersedeError {
    #[error(
        "refusing to supersede {task}@line {supersedes} with no reason: a correction with no \
         evidence is the same unsupported assertion this mechanism exists to fix"
    )]
    MissingReason { task: String, supersedes: u64 },
    #[error("--reason: {0}")]
    InvalidReason(String),
    #[error("{0}")]
    InvalidField(String),
    #[error(
        "{task} has no outcome record at line {supersedes} to supersede.\nknown outcome record \
         lines for {task}:\n{listing}"
    )]
    UnknownTarget {
        task: String,
        supersedes: u64,
        listing: String,
    },
    #[error("{0}")]
    Conflicting(Box<ConflictingSupersede>),
    #[error(transparent)]
    Log(#[from] LogError),
}

/// Append a correction for `task`'s outcome record at line ordinal
/// `supersedes`, unless this exact correction is already there.
///
/// Mirrors `accounts.append_supersede_if_absent`: never a rewrite, never a
/// delete, and `reason` is mandatory. Returns whether it appended — `false`
/// means this exact correction (same target, status, state) was already
/// recorded.
#[allow(clippy::too_many_arguments)]
pub fn append_supersede_if_absent(
    root: &Path,
    task: &str,
    supersedes: u64,
    status: &str,
    state: &str,
    reason: &str,
    revisions: i64,
    when: Option<OffsetDateTime>,
) -> Result<bool, SupersedeError> {
    validate_log_field("--supersede", task).map_err(SupersedeError::InvalidField)?;
    validate_log_field("--status", status).map_err(SupersedeError::InvalidField)?;
    validate_log_field("--state", state).map_err(SupersedeError::InvalidField)?;
    if reason.is_empty() {
        return Err(SupersedeError::MissingReason {
            task: task.to_string(),
            supersedes,
        });
    }
    validate_reason(reason)?;

    let log = ProjectLog::new(root);
    let lock = log.lock()?;
    let targets = outcome_targets(root, task);
    if !targets.targetable.contains(&supersedes) {
        return Err(SupersedeError::UnknownTarget {
            task: task.to_string(),
            supersedes,
            listing: known_lines_listing(&targets.when_by_ordinal),
        });
    }
    let attempted = (status.to_string(), state.to_string());
    if let Some(existing) = targets.applied.get(&supersedes) {
        if existing == &attempted {
            return Ok(false);
        }
        return Err(SupersedeError::Conflicting(Box::new(
            ConflictingSupersede {
                task: task.to_string(),
                supersedes,
                when: targets
                    .when_by_ordinal
                    .get(&supersedes)
                    .cloned()
                    .unwrap_or_default(),
                existing_status: existing.0.clone(),
                existing_state: existing.1.clone(),
                attempted_status: attempted.0,
                attempted_state: attempted.1,
            },
        )));
    }

    let moment = when.unwrap_or_else(now_utc);
    let stamp = format_seconds_utc(moment);
    let supersedes_when = targets
        .when_by_ordinal
        .get(&supersedes)
        .cloned()
        .unwrap_or_default();
    let reason_json = json_encode_str(reason);
    let line = format!(
        "{stamp}\tevent=supersede\ttask={task}\tsupersedes={supersedes}\t\
         supersedes_when={supersedes_when}\tstatus={status}\tstate={state}\t\
         revisions={revisions}\treason={reason_json}"
    );
    lock.append(&line)?;
    Ok(true)
}

/// Every reason an acknowledgement is refused — mirrors
/// `accounts.AcknowledgeError`.
#[derive(Debug, thiserror::Error)]
pub enum AcknowledgeError {
    #[error(
        "{task} has no outcome record at line {acknowledges} to acknowledge.\nknown outcome \
         record lines for {task}:\n{listing}"
    )]
    UnknownTarget {
        task: String,
        acknowledges: u64,
        listing: String,
    },
    #[error("{0}")]
    InvalidField(String),
    #[error(transparent)]
    Log(#[from] LogError),
}

/// Append an acknowledgement of `task`'s outcome record at line ordinal
/// `acknowledges`, unless it is already there.
///
/// Mirrors `accounts.append_acknowledge_if_absent`: never touches `status`
/// or `state`. Returns whether it appended.
pub fn append_acknowledge_if_absent(
    root: &Path,
    task: &str,
    acknowledges: u64,
    when: Option<OffsetDateTime>,
) -> Result<bool, AcknowledgeError> {
    validate_log_field("--acknowledge", task).map_err(AcknowledgeError::InvalidField)?;
    let log = ProjectLog::new(root);
    let lock = log.lock()?;
    let targets = outcome_targets(root, task);
    if !targets.targetable.contains(&acknowledges) {
        return Err(AcknowledgeError::UnknownTarget {
            task: task.to_string(),
            acknowledges,
            listing: known_lines_listing(&targets.when_by_ordinal),
        });
    }
    if acknowledged_targets(root, task).contains(&acknowledges) {
        return Ok(false);
    }
    let moment = when.unwrap_or_else(now_utc);
    let stamp = format_seconds_utc(moment);
    let line = format!("{stamp}\tevent=acknowledged\ttask={task}\tacknowledges={acknowledges}");
    lock.append(&line)?;
    Ok(true)
}

// ---------------------------------------------------------------------
// `multiverse sweep` support: reconcile settled-but-unrecorded tasks,
// reclaim a clean recorded task's still-open pane.
// ---------------------------------------------------------------------

/// One pane Herdr currently reports as live, trimmed to the one field
/// sweep's reconcile/reclaim comparisons need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgent {
    /// The Herdr pane id this live agent occupies.
    pub pane_id: String,
}

/// One pane Herdr currently reports as live, carrying the agent name and
/// Herdr status token a reclaim confirmation needs (finding 3/7) that
/// [`LiveAgent`] deliberately does not — added as its own type, not new
/// fields on [`LiveAgent`], so every existing `AgentPanes::list` caller
/// and fake keeps compiling unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveAgentStatus {
    /// The Herdr pane id this live agent occupies.
    pub pane_id: String,
    /// The agent's dispatch-time name, when Herdr's listing includes one —
    /// used to confirm a pane's current occupant still matches who it was
    /// handed to, not just that the pane id is still live.
    pub name: Option<String>,
    /// The agent's current Herdr status token, when known — lets a reclaim
    /// confirmation check readiness without a second round trip.
    pub status: Option<AgentState>,
}

/// What [`AgentPanes::confirm_live`] found when asked, right now, whether
/// one specific name is still live in one specific pane.
///
/// `state_change_seq` is carried along only when the implementor can
/// actually read it (card `20261105-b-bfc75b`): a continuing Herdr session
/// never sees its own counter go backwards, so a caller that captured an
/// earlier value and sees a lower one here has proof this is a *different*
/// session now answering to the same name in the same pane — not a stale
/// read of the one it found before. `None` means "can't tell" and must
/// never be read as "no change" or "reset to zero".
///
/// `cwd` and `terminal_id` (card `20261106-b-9d70c7`) exist for the same
/// reason: a reissued deterministic name occupying a *recycled* pane id
/// (after a Herdr restart) still matches on name, pane id, and even
/// readiness — `unfinished_agent_names` is empty in this port, so
/// uniqueness at mint time is only the live `agent list`. `cwd` is the
/// one field here that is actually dispatch-unique for a task with its
/// own worktree (`occupant_cwd_confirms_lane`'s own caller requires it to
/// canonicalize to that lane). `terminal_id` is carried for the same
/// "can't tell" posture but is not itself a gate: this port has no
/// evidence it survives a Herdr restart, only that `fake-herdr` derives
/// it from the pane id (so a unit test against the fake could not prove
/// reuse with it either way).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveConfirmation {
    /// Whether the confirmed agent is currently ready for input.
    pub ready: bool,
    /// Herdr's state-change counter at confirmation time, when the
    /// implementor can read it. `None` means "can't tell"; a lower value
    /// than one captured earlier proves a different session now answers to
    /// this name in this pane (see the struct doc above).
    pub state_change_seq: Option<u64>,
    /// The confirmed agent's current working directory, when known — the
    /// one field here that is dispatch-unique for a task with its own
    /// worktree, so callers use it to confirm the occupant is actually
    /// this task's lane and not a reissued name elsewhere.
    pub cwd: Option<PathBuf>,
    /// The confirmed agent's terminal id, when known. Carried for the same
    /// "can't tell" posture as `state_change_seq`, but is not itself a gate
    /// — this port has no evidence it survives a Herdr restart.
    pub terminal_id: Option<String>,
}

/// Reads Herdr's live agent list and closes a settled pane.
/// [`HerdrAgents`] is the real implementation; a sweep's own tests inject
/// a scripted fake instead — the same seam `scratch::ScratchPanes`/
/// `HerdrPanes` already establishes, so `cargo test` never reaches a real
/// Herdr socket.
pub trait AgentPanes {
    /// Every pane Herdr currently reports as live. A failed probe must
    /// collapse to an empty list here — callers that need to tell that
    /// apart from a confirmed-empty fleet should use
    /// [`AgentPanes::list_checked`] instead.
    fn list(&mut self) -> Vec<LiveAgent>;
    /// Closes `pane_id`. Returns whether Herdr itself reported success.
    fn close(&mut self, pane_id: &str) -> bool;

    /// Same probe as [`AgentPanes::list`], but distinguishes "Herdr
    /// confirmed the fleet empty" from "the probe itself failed" (finding
    /// 2: a failed listing must never be read as "no panes live"), and
    /// carries each pane's agent name/status for the idle/done reclaim
    /// confirmation (finding 3/7). `None` means the probe failed; `Some`
    /// (possibly empty) means Herdr actually answered. The default
    /// forwards to [`AgentPanes::list`], folding a failure into "empty"
    /// and leaving name/status unknown — the same posture every existing
    /// implementor already has, left unchanged unless it opts in.
    fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
        Some(
            self.list()
                .into_iter()
                .map(|agent| LiveAgentStatus {
                    pane_id: agent.pane_id,
                    name: None,
                    status: None,
                })
                .collect(),
        )
    }

    /// Visible text of `pane_id`, or `None` if the read itself failed
    /// (could not ask). A completed-but-unsuccessful read still returns
    /// its stdout — possibly empty — matching `_read_parked_pane_text`.
    /// Default is "could not ask" so existing fakes keep compiling.
    fn read_visible(&mut self, pane_id: &str) -> Option<String> {
        let _ = pane_id;
        None
    }

    /// Confirms `name` is live in `pane_id` right now, asked directly
    /// rather than re-derived from a `pane_id` match against a separate
    /// `list`/`list_checked` snapshot: a pane id is reusable, and (same
    /// role/seat) an agent name is reissuable by a later, unrelated
    /// dispatch, so matching both against one listing already taken is not
    /// durable identity on its own (card `20261105-b-bfc75b`). `None`
    /// means `name` is not live in `pane_id` at all right now — gone,
    /// moved, or the probe itself failed — which a caller must treat
    /// exactly like "not live", never like "probably still fine".
    ///
    /// Default forwards to [`AgentPanes::list_checked`] and never reports
    /// [`LiveConfirmation::state_change_seq`] (`None`) — existing fakes
    /// keep compiling and behave exactly as before this method existed.
    /// [`HerdrAgents`] overrides with a single `agent get <name>`, the
    /// closest primitive available to asking Herdr "is this exact name
    /// still here" without a new command of its own.
    fn confirm_live(&mut self, pane_id: &str, name: &str) -> Option<LiveConfirmation> {
        let agents = self.list_checked()?;
        let agent = agents
            .into_iter()
            .find(|agent| agent.pane_id == pane_id && agent.name.as_deref() == Some(name))?;
        Some(LiveConfirmation {
            ready: agent.status.is_some_and(AgentState::is_ready_for_input),
            state_change_seq: None,
            cwd: None,
            terminal_id: None,
        })
    }

    /// Direct `pane get <pane_id>`'s own `label` right now — read
    /// separately from [`AgentPanes::confirm_live`]'s `agent get <name>`
    /// because the label lives on the *pane* record, not the agent's.
    /// `runner::pane_rename` always ends a dispatched pane's label with
    /// the dispatching task's own id, so this is the occupancy witness
    /// `supersede_collision_predecessor` and the reclaim paths use to
    /// catch a reissued name occupying a pane this codebase itself
    /// labelled for a different task (card `20261106-b-9d70c7`). `None`
    /// means "can't tell" (the read failed, or the reply had no label at
    /// all) — the caller must fail closed, exactly like every other probe
    /// failure here. Default forwards to nothing (`None`): a fake that
    /// never models labels keeps compiling but never confirms this gate.
    fn pane_label(&mut self, pane_id: &str) -> Option<String> {
        let _ = pane_id;
        None
    }
}

/// The real [`AgentPanes`] implementation, talking to Herdr over the
/// `run::run`/`argv` subprocess helpers. Unit-less marker type: all state
/// lives in the Herdr process itself, not here.
pub struct HerdrAgents;

impl AgentPanes for HerdrAgents {
    /// A spawn failure, or a reply with no JSON `result`, collapses to
    /// "nothing known live" — sweep must never promote a failed probe to
    /// "every pane is gone" with more confidence than the probe earns.
    fn list(&mut self) -> Vec<LiveAgent> {
        let Ok(result) = run::run(&argv::agent_list(), None) else {
            return Vec::new();
        };
        result
            .payload
            .as_ref()
            .and_then(|payload| payload["result"]["agents"].as_array())
            .map(|agents| {
                agents
                    .iter()
                    .filter_map(|agent| agent["pane_id"].as_str())
                    .map(|pane_id| LiveAgent {
                        pane_id: pane_id.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn close(&mut self, pane_id: &str) -> bool {
        run::run(&argv::pane_close(pane_id), None).is_ok_and(|result| result.ok())
    }

    /// A subprocess failure, or a zero-exit reply Herdr itself marked
    /// failed (`CommandResult::ok` false), is a probe failure (`None`) —
    /// never "zero agents". Only a reply Herdr itself calls OK, whose
    /// JSON genuinely lists zero (or N) agents, is "confirmed empty".
    fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
        let Ok(result) = run::run(&argv::agent_list(), None) else {
            return None;
        };
        if !result.ok() {
            return None;
        }
        let agents = result
            .payload
            .as_ref()
            .and_then(|payload| payload["result"]["agents"].as_array());
        Some(
            agents
                .map(|agents| {
                    agents
                        .iter()
                        .filter_map(|agent| {
                            let pane_id = agent["pane_id"].as_str()?.to_string();
                            let name = agent["name"].as_str().map(str::to_owned);
                            let status = agent["agent_status"]
                                .as_str()
                                .and_then(|s| s.parse::<AgentState>().ok());
                            Some(LiveAgentStatus {
                                pane_id,
                                name,
                                status,
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    fn read_visible(&mut self, pane_id: &str) -> Option<String> {
        match run::run(&argv::agent_read(pane_id, Some("visible"), None), None) {
            Ok(result) => Some(result.stdout),
            Err(_) => None,
        }
    }

    /// `agent get <name>` directly, rather than `agent list` plus a
    /// `pane_id` scan — the lookup is keyed by the identity that matters
    /// (`name`), with `pane_id` as confirmation rather than the other way
    /// around, and it is the one call that can actually report
    /// `state_change_seq` for [`LiveConfirmation`]'s same-session witness.
    fn confirm_live(&mut self, pane_id: &str, name: &str) -> Option<LiveConfirmation> {
        let result = run::run(&argv::agent_get(name), None).ok()?;
        if !result.ok() {
            return None;
        }
        let agent = result.payload.as_ref()?.get("result")?.get("agent")?;
        if agent["pane_id"].as_str() != Some(pane_id) {
            return None;
        }
        let ready = agent["agent_status"]
            .as_str()
            .and_then(|s| s.parse::<AgentState>().ok())
            .is_some_and(AgentState::is_ready_for_input);
        // Python `_cwd_of`: `foreground_cwd`, else `cwd`.
        let cwd = agent["foreground_cwd"]
            .as_str()
            .or_else(|| agent["cwd"].as_str())
            .map(PathBuf::from);
        let terminal_id = agent["terminal_id"].as_str().map(str::to_owned);
        Some(LiveConfirmation {
            ready,
            state_change_seq: agent["state_change_seq"].as_u64(),
            cwd,
            terminal_id,
        })
    }

    /// `pane get <pane_id>`, sanity-checking the reply actually names the
    /// pane asked about before trusting its `label` — the same posture
    /// `confirm_live` already takes with `agent get`'s own `pane_id`.
    fn pane_label(&mut self, pane_id: &str) -> Option<String> {
        let result = run::run(&argv::pane_get(pane_id), None).ok()?;
        if !result.ok() {
            return None;
        }
        let pane = result.payload.as_ref()?.get("result")?.get("pane")?;
        if pane["pane_id"].as_str() != Some(pane_id) {
            return None;
        }
        pane["label"].as_str().map(str::to_owned)
    }
}

/// One task's history, gathered from its directory and `routing.log`,
/// just far enough for sweep's and `tasks --reconcile`'s reconcile/reclaim
/// comparisons: the dispatch record's `branch` (`worktrees::
/// dispatched_lane_is_clean`, reclaim condition 3) and `role` (the
/// writing-role gate `lane_is_dirty` below needs, mirroring Python's
/// `roles.writes`), the most recently known pane (`result`'s own `pane`
/// field when a result was already written, else `started`'s), and
/// whether a `result`/`supersede` event already closed the loop.
#[derive(Debug, Clone)]
pub struct SweepTask {
    /// This task's own identifier, as used under `.multiverse/tasks/`.
    pub task_id: String,
    /// The branch this task's dispatch record named — `worktrees::
    /// dispatched_lane_is_clean`'s input for reclaim condition 3.
    pub branch: String,
    /// The role this task was dispatched to (e.g. Builder, Critic) — feeds
    /// [`is_writing_role`] so write-capable roles get the stricter
    /// dirty-lane handling non-writing roles don't need.
    pub role: String,
    /// The most recently known pane for this task: `result`'s own `pane`
    /// field when a result was already written, else `started`'s.
    pub pane: Option<String>,
    /// This task's status *file* contents, when readable — distinct from
    /// [`Self::status`] (the routing log's recorded outcome).
    pub file_status: Option<TaskStatus>,
    /// Whether an `event=result`/`event=supersede` record already closed
    /// this task's loop in the routing log.
    pub recorded: bool,
    /// The Herdr agent name this task was dispatched under (`agent=` on
    /// its `dispatch` line) — reclaim's same-agent-name confirmation
    /// (finding 3/7) needs this to tell "this task's own agent is still
    /// idle in this pane" from "a different agent now occupies the same
    /// pane id".
    pub agent: Option<String>,
    /// The seat name this task was dispatched under (`account=` on its
    /// `dispatch` line) — `expire_awaiting`'s per-account lease override
    /// (finding 4) is keyed by this, not by role.
    pub account: Option<String>,
    /// This run's own `event=dispatch` timestamp.
    pub dispatched_at: Option<OffsetDateTime>,
    /// This run's own `event=started` timestamp, when the prompt is known
    /// to have landed — `expire_awaiting`'s `_last_seen`: the closer of
    /// the two to "how long has nothing been heard from this run" when
    /// it is known at all.
    pub started_at: Option<OffsetDateTime>,
    /// This task's own `event=dispatch` line's append-order position in
    /// `routing.log` — `lane_reuse_index`'s ordering key, never
    /// `dispatched_at`: two `--into` dispatches landing in the same
    /// lane within the same second are otherwise indistinguishable
    /// (mirrors `tasks.Task.dispatch_ordinal`).
    pub dispatch_ordinal: u64,
    /// The routing log's own recorded outcome status
    /// (`event=result`/`event=supersede`'s `status=`), parsed — distinct
    /// from [`Self::file_status`] (the task's own status *file*, a
    /// different surface entirely). `None` until something records an
    /// outcome.
    pub status: Option<TaskStatus>,
    /// That outcome record's free-text `state=` field — e.g. `"done"`,
    /// `ABANDONED_UNMERGED`, or, once [`auto_supersede`] has run,
    /// [`SUPERSEDED_MERGED`]/[`SUPERSEDED_REDISPATCHED`].
    pub outcome_state: Option<String>,
    /// That outcome record's own 1-based line ordinal — the `supersedes`
    /// target [`append_supersede_if_absent`] needs to correct it,
    /// numbered the same way [`outcome_targets`] numbers one (mirrors
    /// `Task.outcome_ordinal`).
    pub outcome_ordinal: Option<u64>,
    /// That outcome record's own `revisions=` field, carried forward
    /// unchanged on a correction (mirrors `Task.revisions`). `0` until
    /// an outcome exists.
    pub revisions: i64,
}

#[derive(Default)]
struct TaskEvents {
    branch: String,
    role: String,
    agent: Option<String>,
    account: Option<String>,
    dispatched_at: Option<OffsetDateTime>,
    dispatch_ordinal: u64,
    started_pane: Option<String>,
    started_at: Option<OffsetDateTime>,
    result_pane: Option<String>,
    result_status: Option<String>,
    result_state: Option<String>,
    result_ordinal: Option<u64>,
    revisions: i64,
}

/// One pass over `routing.log`'s task-lifecycle events, mirroring Python's
/// `read_log`: the task universe is whatever the log itself names, never a
/// raw directory listing. `sweep_tasks` used to list `.multiverse/tasks`
/// directly and treat every entry matching the generic task-id pattern as
/// a task — which also matched a stray loose file left directly under
/// `tasks/` (e.g. a wayward `<id>-result.md`, never a directory) and a
/// task directory whose own dispatch predates the routing log's earliest
/// surviving entry. Python's `read_log` only ever knows about a task
/// through its own log events, so neither phantom ever reaches `resolve`/
/// `reconcile` on that side (card `20261102-b-1b2d80`: Rust counted both
/// as permanently "unrecorded" — 34 loose files on one real project, 4
/// pre-log directories on another — while Python counted zero either way).
///
/// A `dispatch` event resets every field below for its task id, matching
/// `read_log`'s own per-dispatch reset: a fix round that reuses a task id
/// must not inherit an earlier round's pane or outcome. An `event=result`
/// whose `status=none` is skipped entirely, matching `read_log`'s own
/// `continue` for that value — a runner bug once wrote it for a turn that
/// only looked settled, and it was never a real outcome. `supersede`
/// replaces the recorded status the same way a later `result` would,
/// without touching the pane (`read_log` never updates `task.pane` from a
/// `supersede` line either).
fn task_events(root: &Path) -> HashMap<String, TaskEvents> {
    let log_path = root.join(".multiverse/routing.log");
    let mut by_task: HashMap<String, TaskEvents> = HashMap::new();
    let Ok(records) = crate::index::records(&log_path, crate::index::Selector::Tasks) else {
        return by_task;
    };
    for record in &records {
        let Ok(parsed) = record.parse() else {
            continue;
        };
        let Some(task_id) = parsed.fields.get("task").cloned() else {
            continue;
        };
        // `outcome_targets`'s own numbering (`accounts._outcome_targets`):
        // a 1-based line ordinal, so [`append_supersede_if_absent`]'s
        // `supersedes` target always lines up with what that function
        // (and a human reading the raw log) would call "line N".
        let line_ordinal = record.entry.ordinal + 1;
        match parsed.event() {
            "dispatch" => {
                by_task.insert(
                    task_id,
                    TaskEvents {
                        branch: parsed.fields.get("branch").cloned().unwrap_or_default(),
                        role: parsed.fields.get("role").cloned().unwrap_or_default(),
                        agent: parsed.fields.get("agent").cloned(),
                        account: parsed.fields.get("account").cloned(),
                        dispatched_at: mv_core::time::parse_flexible(&parsed.when).ok(),
                        dispatch_ordinal: record.entry.ordinal,
                        started_pane: None,
                        started_at: None,
                        result_pane: None,
                        result_status: None,
                        result_state: None,
                        result_ordinal: None,
                        revisions: 0,
                    },
                );
            }
            "started" => {
                let entry = by_task.entry(task_id).or_default();
                entry.started_pane = parsed.fields.get("pane").cloned();
                entry.started_at = mv_core::time::parse_flexible(&parsed.when).ok();
            }
            "result" => {
                let status = parsed.fields.get("status").cloned().unwrap_or_default();
                if status != "none" {
                    let entry = by_task.entry(task_id).or_default();
                    entry.result_pane = parsed.fields.get("pane").cloned();
                    entry.result_status = Some(status);
                    entry.result_state = parsed.fields.get("state").cloned();
                    entry.result_ordinal = Some(line_ordinal);
                    entry.revisions = parsed
                        .fields
                        .get("revisions")
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0);
                }
            }
            "supersede" => {
                let entry = by_task.entry(task_id).or_default();
                entry.result_status =
                    Some(parsed.fields.get("status").cloned().unwrap_or_default());
                entry.result_state = parsed.fields.get("state").cloned();
                entry.result_ordinal = Some(line_ordinal);
                entry.revisions = parsed
                    .fields
                    .get("revisions")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
            }
            _ => {}
        }
    }
    by_task
}

/// Every task `root`'s routing log knows about, joined with each one's own
/// status file. A missing or unreadable log is "no history for any task",
/// the same fail-open posture [`read_status`] already takes for one file.
pub fn sweep_tasks(root: &Path) -> Vec<SweepTask> {
    let mut by_task = task_events(root);
    let mut entries: Vec<String> = by_task.keys().cloned().collect();
    entries.sort();

    entries
        .into_iter()
        .map(|task_id| {
            let events = by_task.remove(&task_id).unwrap_or_default();
            let recorded = events.result_status.is_some();
            // Every consumer of `file_status` (`is_reconcile_candidate`,
            // `is_tasks_reconcile_candidate`, `dirty_done_detail`) checks
            // `recorded` first and short-circuits before ever reading it for
            // an already-recorded task — so a recorded task's own two status
            // reads (lane worktree, then main checkout) are pure overhead.
            // At fleet scale this was the dominant cost of a sweep pass:
            // nearly every task in a project's history is long since
            // recorded, and each one paid for a status file neither read nor
            // needed (card `20261105-b-69a655`).
            let file_status = if recorded {
                None
            } else {
                let lane_id = events
                    .branch
                    .strip_prefix("multiverse/")
                    .unwrap_or(&task_id);
                read_writer_status(
                    root,
                    &task_id,
                    lane_id,
                    events.started_at.or(events.dispatched_at),
                )
            };
            let pane = events.result_pane.or(events.started_pane);
            let status = events.result_status.as_deref().and_then(TaskStatus::parse);
            SweepTask {
                task_id,
                branch: events.branch,
                role: events.role,
                pane,
                file_status,
                recorded,
                agent: events.agent,
                account: events.account,
                dispatched_at: events.dispatched_at,
                started_at: events.started_at,
                dispatch_ordinal: events.dispatch_ordinal,
                status,
                outcome_state: events.result_state,
                outcome_ordinal: events.result_ordinal,
                revisions: events.revisions,
            }
        })
        .collect()
}

/// The two roles Python's reference gives their own worktree and branch
/// (`roles.WRITING_ROLES`: Builder, Artisan — see
/// `AGENTS.md`'s new-name list). Every other role's dispatch never gets a
/// lane, so `lane_is_dirty` never asks git a question that role was
/// never given the means to answer honestly (`_dirty_lane_detail`'s own
/// reasoning). `pub`: `sweep.rs`'s own closing of a merged backlog link
/// needs the same writes-or-not gate Python's `_close_merged_backlog_links`
/// takes from `roles.writes`.
pub fn is_writing_role(role: &str) -> bool {
    matches!(role, "fixer" | "builder" | "designer" | "artisan")
}

/// The task id that actually names `task`'s worktree directory and branch
/// — ordinarily its own id, but a task dispatched `--into` another task's
/// lane records that owner's branch instead (`_lane_task_id`). `pub`:
/// `sweep.rs`'s own closing of a merged backlog link needs the same lane
/// name `auto_supersede`/`close_stranded` already resolve here.
pub fn lane_task_id(task: &SweepTask) -> &str {
    task.branch
        .strip_prefix("multiverse/")
        .unwrap_or(&task.task_id)
}

/// Whether a pane's own label (`AgentPanes::pane_label`) names `task_id`
/// as a contiguous substring — the occupancy witness
/// `supersede_collision_predecessor` and `reclaimable_panes` both require
/// in addition to the bare name match, since a reissued dispatch name can
/// still answer at a pane this codebase labelled for someone else's task
/// (card `20261106-b-9d70c7`). `None` (the read failed, or found no
/// label) never confirms — the caller fails closed exactly like every
/// other probe failure here.
pub fn label_names_task(label: Option<&str>, task_id: &str) -> bool {
    label.is_some_and(|label| label.contains(task_id))
}

/// Whether `cwd` canonicalizes to the same path as `lane_root` — the
/// second occupancy witness alongside [`label_names_task`], required only
/// when the task actually has its own worktree (card `20261106-b-9d70c7`).
/// `None`, or either path failing to canonicalize (gone, or never
/// existed), is "can't tell" and never reads as a match.
pub fn cwd_confirms_lane(cwd: Option<&Path>, lane_root: &Path) -> bool {
    let Some(cwd) = cwd else {
        return false;
    };
    let (Ok(cwd), Ok(lane_root)) = (std::fs::canonicalize(cwd), std::fs::canonicalize(lane_root))
    else {
        return false;
    };
    cwd == lane_root
}

/// Whether `task`'s own lane still holds uncommitted work — the one
/// contradiction `reconcile` refuses to paper over: a status file saying
/// `done` while the tree disagrees (`_dirty_done`/`_dirty_lane_detail`).
/// Reserved for exactly a writing role's `done` outcome, same as Python:
/// every other combination (a read-only role, a non-`done` status, or a
/// lane this build cannot find on disk — the common case in a rehearsal
/// sandbox, which never copies worktree checkouts) answers `false`,
/// mirroring `_dirty_lane_detail`'s own "nothing to show" `None`. A `git`
/// that fails to run, or reports anything other than a clean exit, is
/// likewise read as "nothing to show" rather than "dirty" — a failed
/// read is not evidence either way.
fn lane_is_dirty(root: &Path, task: &SweepTask, cache: &mut LaneProbeCache) -> bool {
    if task.file_status != Some(TaskStatus::Done) || !is_writing_role(&task.role) {
        return false;
    }
    cached_lane_porcelain(root, lane_task_id(task), cache).is_some_and(|text| !text.is_empty())
}

/// What's dirty in an unrecorded writing-role `done` lane, or `None` when
/// there is nothing to show. Same gate as `lane_is_dirty` plus the
/// narrowed `DirtyDetail.describe()` wording `board_data` already uses
/// (modified/untracked counts and up to three paths; no commits-ahead).
/// `cmd_sweep` prints this as Python's `needs a decision:` line
/// (`cli.py:12882-12899`).
///
/// The dirty check and the detail text share a single
/// `git status --porcelain` spawn — this function used to probe the same
/// worktree twice (card `20261115-b-ca59eb`).
pub fn dirty_done_detail(root: &Path, task: &SweepTask) -> Option<String> {
    let mut cache = LaneProbeCache::new();
    dirty_done_detail_cached(root, task, &mut cache)
}

/// Like [`dirty_done_detail`], but reuses a [`LaneProbeCache`] the caller
/// already has instead of spawning a fresh one — what
/// `collect_dirty_details` calls, once per task, off the exact cache
/// `sweep_project_with_live`'s own reconcile/reclaim gating already built
/// this pass, so a lane that check already probed is a cache hit here,
/// never a second `git status --porcelain` for the same worktree (card
/// `20261115-b-1ba758`).
pub fn dirty_done_detail_cached(
    root: &Path,
    task: &SweepTask,
    cache: &mut LaneProbeCache,
) -> Option<String> {
    if task.recorded || !lane_is_dirty(root, task, cache) {
        return None;
    }
    let porcelain = cached_lane_porcelain(root, lane_task_id(task), cache)?;
    if porcelain.is_empty() {
        return None;
    }
    Some(describe_dirty(&porcelain))
}

/// Narrowed `worktrees.DirtyDetail.describe()`: counts plus up to three
/// example paths. Drops the trailing "N commits ahead" clause, matching
/// `board_data::describe_dirty`.
fn describe_dirty(porcelain: &str) -> String {
    let mut lines: Vec<String> = porcelain.lines().map(str::to_string).collect();
    if let Some(first) = lines.first_mut() {
        if first.len() > 2 && first.as_bytes().get(2) != Some(&b' ') {
            first.insert(0, ' ');
        }
    }
    let untracked = lines.iter().filter(|line| line.starts_with("??")).count();
    let modified = lines.len() - untracked;
    let shown: Vec<&str> = lines
        .iter()
        .take(3)
        .map(|line| line.get(3..).unwrap_or(""))
        .collect();
    let more = lines.len().saturating_sub(shown.len());
    let counts = format!("{modified} modified + {untracked} untracked");
    if shown.is_empty() {
        return counts;
    }
    let mut named = shown.join(", ");
    if more > 0 {
        named.push_str(&format!(", and {more} more"));
    }
    format!("{counts}: {named}")
}

/// `git status --porcelain` for `lane_task_id`'s own worktree directory —
/// shared by [`lane_is_dirty`] (reconcile's "done but dirty" refusal) and
/// `close_stranded`'s own uncommitted-work check below. `None` when the
/// lane directory is simply gone (never dirty by omission) or git could
/// not be read; a caller that needs "unreadable" to mean something other
/// than "clean" (`--reclaimable`'s own gate) checks the directory itself
/// first rather than trusting this return alone.
///
/// Spawns `git` every time it is called; callers that may ask about the
/// same lane more than once in a single sweep/reconcile/close-stranded
/// pass (a lane revisited by a later `--into` dispatch sharing the same
/// branch, or [`dirty_done_detail`] following [`lane_is_dirty`]) should
/// go through [`cached_lane_porcelain`] instead so the second ask costs a
/// map lookup, not a second subprocess — git's own status plumbing has no
/// way to answer for more than one worktree in a single invocation, so a
/// repeat ask of the *same* worktree is the only spawn this file can avoid
/// (card `20261115-b-ca59eb`, mirroring `board_data`'s `LaneProbeCache`).
fn lane_porcelain(root: &Path, lane_task_id: &str) -> Option<String> {
    let lane_root = root.join(".multiverse/worktrees").join(lane_task_id);
    if !lane_root.is_dir() {
        return None;
    }
    #[cfg(test)]
    LANE_GIT_PROBES.with(|count| count.set(count.get() + 1));
    match std::process::Command::new("git")
        .arg("-C")
        .arg(&lane_root)
        .args(["status", "--porcelain"])
        .output()
    {
        Ok(output) if output.status.success() => {
            Some(String::from_utf8_lossy(&output.stdout).into_owned())
        }
        _ => None,
    }
}

// Counts actual `git` spawns from `lane_porcelain` — a thread-local so
// parallel `#[test]` functions (cargo's default) never see each other's
// probes, since each test runs on its own OS thread. Test-only: this is
// the "counting fake" a real git runner seam does not exist for here,
// proving a pass's [`LaneProbeCache`] actually elides a repeat spawn
// rather than merely returning the same string by coincidence. Mirrors
// `board_data`'s `LANE_GIT_PROBES`.
#[cfg(test)]
thread_local! {
    static LANE_GIT_PROBES: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_lane_git_probe_count() {
    LANE_GIT_PROBES.with(|count| count.set(0));
}

#[cfg(test)]
fn lane_git_probe_count() -> u32 {
    LANE_GIT_PROBES.with(std::cell::Cell::get)
}

/// One pass's worth of `lane_porcelain` results, keyed by
/// `lane_task_id`, scoped to a single [`sweep_project_with_live`] /
/// [`reconcile_project`] / [`close_stranded`] / [`reclaimable_panes`]
/// call — the unit of batching git's per-worktree status actually allows,
/// since a `git status` reads one worktree's own index and cannot answer
/// for several at once. Several historical task ids can name the same
/// lane (a task dispatched `--into` an existing lane keeps dispatching
/// more work into that same branch/worktree), and each one reaching the
/// UNRECORDED-done check used to re-spawn `git` for a directory this same
/// pass had already read.
pub type LaneProbeCache = HashMap<String, Option<String>>;

fn cached_lane_porcelain(
    root: &Path,
    lane_task_id: &str,
    cache: &mut LaneProbeCache,
) -> Option<String> {
    if let Some(hit) = cache.get(lane_task_id) {
        return hit.clone();
    }
    let result = lane_porcelain(root, lane_task_id);
    cache.insert(lane_task_id.to_string(), result.clone());
    result
}

/// A `done` outcome needs a readable, non-empty `result.md` in its own
/// task directory before `reconcile` will record it (`_usable_result`).
/// Unlike Python, this never falls back to a lane-root copy of the same
/// file — see this task's result.md for why that fallback was left out.
///
/// `pub` (card `20261102-b-b4ede0`) so `mv_engine::sweep_nudges` can reuse
/// it for the identical "did this task leave a result behind" question
/// Python's own `result_written` answers for `finish_nudge_eligible` — the
/// same narrowed (main-checkout-only) reading, not a second copy of it.
pub fn has_usable_result(root: &Path, task_id: &str) -> bool {
    let path = root
        .join(".multiverse/tasks")
        .join(task_id)
        .join("result.md");
    std::fs::read_to_string(path)
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false)
}

/// Whether sweep's own reconcile leg would record `task`'s outcome: not
/// already recorded, a terminal status file on disk, its pane not
/// currently WORKING, and — for a `done` writing role — a clean lane and a
/// usable `result.md`. Python's `tasks.reconcile` only ever skips a pane
/// whose live state is exactly `herdr.AgentState.WORKING` — "a `running`
/// task is left alone" — never a merely-live idle/done/blocked one, the
/// same distinction [`is_tasks_reconcile_candidate`] already draws for
/// `tasks --reconcile`; `is_working` must answer that question, not
/// `SweepTask`'s broader pane-liveness one, or a settled pane Herdr has not
/// yet dropped from its listing is left unrecorded a sweep pass too long.
fn is_reconcile_candidate(
    root: &Path,
    task: &SweepTask,
    is_working: &impl Fn(&str) -> bool,
    cache: &mut LaneProbeCache,
) -> bool {
    if task.recorded {
        return false;
    }
    if task.file_status.is_none() {
        return false;
    }
    if task.pane.as_deref().is_some_and(is_working) {
        return false;
    }
    if lane_is_dirty(root, task, cache) {
        return false;
    }
    if task.file_status == Some(TaskStatus::Done) && !has_usable_result_for(root, task) {
        return false;
    }
    true
}

/// Python `reconcile`'s candidate gate: skip a live WORKING pane (a fix
/// round still spending), but record an idle/done/blocked/unknown live
/// pane so `_close_settled_panes` can tidy it. Distinct from
/// [`is_reconcile_candidate`], which sweep uses and which treats any live
/// pane as still in flight.
fn is_tasks_reconcile_candidate(
    root: &Path,
    task: &SweepTask,
    live: &[LiveAgentStatus],
    cache: &mut LaneProbeCache,
) -> bool {
    if task.recorded || task.file_status.is_none() {
        return false;
    }
    if let Some(pane) = task.pane.as_deref() {
        if live
            .iter()
            .any(|agent| agent.pane_id == pane && agent.status == Some(AgentState::Working))
        {
            return false;
        }
    }
    if lane_is_dirty(root, task, cache) {
        return false;
    }
    if task.file_status == Some(TaskStatus::Done) && !has_usable_result_for(root, task) {
        return false;
    }
    true
}

/// Options for [`reconcile_project`] — the shared write path `tasks`
/// listing (auto and `--reconcile`) and, next, `board` both call.
pub struct ReconcileRequest<'a> {
    /// Restrict reconcile to one task id, or scan every candidate task when
    /// absent — `tasks --reconcile <id>` vs. a full sweep.
    pub only_task: Option<&'a str>,
    /// Close each just-recorded pane when a fresh listing shows it idle/done
    /// (never WORKING/BLOCKED). Python `reconcile(close_panes=...)`.
    pub close_panes: bool,
    /// Run `converge_stale` (auto-supersede then lease-expire AWAITING).
    pub expire_stale: bool,
    /// Default lease length, in minutes, before an AWAITING task is treated
    /// as stale by [`expire_awaiting`].
    pub lease_minutes: u32,
    /// Per-account overrides of `lease_minutes`, keyed by the seat name a
    /// task was dispatched under — [`expire_awaiting`]'s finding-4 gate.
    pub lease_overrides: &'a HashMap<String, u32>,
    /// The instant this reconcile pass treats as "now", for every elapsed-
    /// time comparison it makes — passed in rather than read live so a
    /// caller's whole pass is consistent and a test can fix it.
    pub now: OffsetDateTime,
    /// Automatic listing must not guess when Herdr is down. Explicit
    /// `--reconcile` is allowed to record without a live listing.
    pub require_live: bool,
}

/// What [`reconcile_project`] actually wrote.
#[derive(Debug, Clone, Default)]
pub struct ReconcileOutcome {
    /// Task ids whose settled-but-unrecorded outcome this pass just wrote
    /// to the routing log.
    pub recorded: Vec<String>,
    /// How many just-recorded panes this pass confirmed idle/done and
    /// closed.
    pub closed_panes: usize,
    /// How many AWAITING tasks this pass lease-expired.
    pub expired: usize,
    /// Every auto-supersede this pass applied while converging stale tasks.
    pub superseded: Vec<SupersedeResult>,
    /// Set when `require_live` was set and the live listing itself failed —
    /// every other field is then left at its default, since this pass
    /// refused to record anything without a live listing to check against.
    pub herdr_down: bool,
}

/// Shared reconcile: record UNRECORDED outcomes, optionally close settled
/// panes, optionally `converge_stale`. Mirrors `tasklog.reconcile` plus
/// `converge_stale` so board can call the same function next.
pub fn reconcile_project(
    root: &Path,
    req: ReconcileRequest<'_>,
    agents: &mut impl AgentPanes,
) -> ReconcileOutcome {
    let live = agents.list_checked();
    if req.require_live && live.is_none() {
        return ReconcileOutcome {
            herdr_down: true,
            ..ReconcileOutcome::default()
        };
    }
    let live_status = live.unwrap_or_default();
    // Feeds `auto_supersede`/`expire_awaiting`'s own `is_awaiting` gate
    // below, not a same-occupant check by itself: `None` when the pane id
    // is gone from the listing, `Some(name)` (possibly empty) when it is
    // still listed. Card 20261108-b-152b33 keeps the reused-pane half (a
    // confirmed *different* name reads as gone); card 20261106-b-2c3d2f
    // Decision 1 keeps a merely unconfirmed (missing/empty) name from
    // reading as gone too.
    let live_name = |pane: &str| {
        live_status
            .iter()
            .find(|current| current.pane_id == pane)
            .map(|current| current.name.clone().unwrap_or_default())
    };
    let tasks = sweep_tasks(root);
    let mut lane_probe_cache = LaneProbeCache::new();

    let mut recorded_ids = Vec::new();
    let mut recorded_panes: Vec<(String, Option<AgentState>, Option<String>)> = Vec::new();
    for task in &tasks {
        if let Some(only) = req.only_task {
            if task.task_id != only {
                continue;
            }
        }
        if !is_tasks_reconcile_candidate(root, task, &live_status, &mut lane_probe_cache) {
            continue;
        }
        let status = task
            .file_status
            .expect("is_reconcile_candidate already required a status file");
        if record_reconciled(root, &task.task_id, status, task.pane.as_deref()) {
            recorded_ids.push(task.task_id.clone());
            if let Some(pane) = task.pane.as_deref() {
                let first = live_status
                    .iter()
                    .find(|agent| agent.pane_id == pane)
                    .and_then(|agent| agent.status);
                recorded_panes.push((pane.to_string(), first, task.agent.clone()));
            }
        }
    }

    let mut closed_panes = 0;
    if req.close_panes && !recorded_panes.is_empty() {
        closed_panes = close_settled_panes(agents, &recorded_panes);
    }

    let mut superseded = Vec::new();
    let mut expired = 0;
    if req.expire_stale {
        let mut remaining = sweep_tasks(root);
        superseded = auto_supersede(root, &mut remaining, true, &live_name);
        let handled: HashSet<&str> = superseded.iter().map(|r| r.task_id.as_str()).collect();
        let expire_candidates: Vec<SweepTask> = remaining
            .iter()
            .filter(|t| !handled.contains(t.task_id.as_str()))
            .cloned()
            .collect();
        let expiries = expire_awaiting(
            root,
            &expire_candidates,
            req.lease_minutes,
            req.lease_overrides,
            req.now,
            // `tasks --reconcile`/`board`'s own stale-convergence pass has
            // no host-sleep tally of its own to offer — only `sweep`'s own
            // call below does (critic-codex-areas.md H1).
            0,
            true,
            &live_name,
        );
        expired = expiries.iter().filter(|expiry| expiry.recorded).count();
    }

    ReconcileOutcome {
        recorded: recorded_ids,
        closed_panes,
        expired,
        superseded,
        herdr_down: false,
    }
}

/// Close each just-recorded pane only when a *second* listing still shows
/// it idle/done — never WORKING/BLOCKED, never an unknown status — and
/// still hosts the same agent this task was dispatched under. Python
/// `_close_settled_panes`. The pane id a task was recorded under is
/// reusable: by the time this second listing lands, Herdr may have handed
/// that id to an unrelated agent, and closing on the id alone would
/// destroy that agent's session instead of merely freeing a slot nobody
/// needs anymore (card 20261108-b-37df85, mirroring `reclaim_pane_is_safe`/
/// `reclaim_stale_panes`'s own same-agent-name confirmation).
fn close_settled_panes(
    agents: &mut impl AgentPanes,
    recorded_panes: &[(String, Option<AgentState>, Option<String>)],
) -> usize {
    let eligible: Vec<(&str, Option<&str>)> = recorded_panes
        .iter()
        .filter(|(_, status, _)| {
            !matches!(
                status,
                Some(AgentState::Working) | Some(AgentState::Blocked)
            )
        })
        .map(|(pane, _, agent)| (pane.as_str(), agent.as_deref()))
        .collect();
    if eligible.is_empty() {
        return 0;
    }
    let Some(fresh) = agents.list_checked() else {
        return 0;
    };
    let mut closed = 0;
    for (pane, recorded_agent) in eligible {
        let Some(current) = fresh.iter().find(|agent| agent.pane_id == pane) else {
            continue;
        };
        if current.name.as_deref().unwrap_or("") != recorded_agent.unwrap_or("") {
            continue;
        }
        if !current.status.is_some_and(AgentState::is_ready_for_input) {
            continue;
        }
        if agents.close(pane) {
            closed += 1;
        }
    }
    closed
}

/// `tasks --reconcile`'s record-only write leg. Pane close and
/// `converge_stale` go through [`reconcile_project`].
pub fn reconcile_settled(
    root: &Path,
    only_task: Option<&str>,
    agents: &mut impl AgentPanes,
) -> Vec<String> {
    let empty = HashMap::new();
    reconcile_project(
        root,
        ReconcileRequest {
            only_task,
            close_panes: false,
            expire_stale: false,
            lease_minutes: 60,
            lease_overrides: &empty,
            now: now_utc(),
            require_live: false,
        },
        agents,
    )
    .recorded
}

/// Whether `root`'s routing log already has a *usable* `event=result` line
/// for `task_id` — Python's `accounts._has_recorded_result`. Only read
/// inside [`append_outcome_if_absent`]'s lock; a caller elsewhere sees a
/// snapshot a concurrent writer can invalidate before it acts on it, which
/// is the exact race that function exists to close (critic-codex-areas.md
/// M1).
///
/// Only outcomes after the latest `event=dispatch` for this id count, so a
/// same-id redispatch's previous outcome never vetoes a recovered result.
/// A line missing `status=`/`state=`/`revisions=`, or carrying
/// `status=none` (a past runner bug), is not usable and never vetoes a
/// real write — a false negative here only risks one harmless extra line,
/// folded away by the log's own last-result-wins read; a false positive
/// would lose an outcome permanently.
fn has_recorded_result(root: &Path, task_id: &str) -> bool {
    let log_path = root.join(".multiverse/routing.log");
    let Ok(records) = index_records(&log_path, Selector::Tasks) else {
        return false;
    };
    for record in records.iter().rev() {
        if record.entry.task != task_id {
            continue;
        }
        let text = String::from_utf8_lossy(&record.bytes);
        let line = text.strip_suffix('\n').unwrap_or(&text);
        let (_, fields) = crate::log::split_fields(line);
        match fields.get("event").map(String::as_str) {
            Some("dispatch") => return false,
            Some("result") => {}
            _ => continue,
        }
        if fields.get("status").map(String::as_str) == Some("none") {
            continue;
        }
        if fields.contains_key("status")
            && fields.contains_key("state")
            && fields.contains_key("revisions")
        {
            return true;
        }
    }
    false
}

/// Append a first-time terminal outcome for `task_id`, unless the routing
/// log already has a usable one — Python's `accounts.append_outcome_if_absent`.
/// Holds the project lock across the read, the check, and the append
/// (re-entering the lock a caller already holds rather than blocking on
/// it, the same reentrant `flock` `mv_core::lock` already gives every
/// nested [`ProjectLog`] user), so two reconcilers racing the same settled
/// task — two sweeps, or a sweep racing `tasks --reconcile`/`board` —
/// produce exactly one result line, never two (critic-codex-areas.md M1).
/// Returns whether it appended; `false` means a usable result was already
/// there, which a caller should treat as already recorded, not a failure.
fn append_outcome_if_absent(
    root: &Path,
    task_id: &str,
    status: TaskStatus,
    state: &str,
    pane: Option<&str>,
) -> bool {
    let log = ProjectLog::new(root);
    let Ok(lock) = log.lock() else {
        return false;
    };
    if has_recorded_result(root, task_id) {
        return false;
    }
    let when = mv_core::time::now_stamp();
    let pane = pane.unwrap_or("");
    let line = format!(
        "{when}\tevent=result\ttask={task_id}\tstatus={status}\tstate={state}\trevisions=0\tpane={pane}"
    );
    lock.append(&line).is_ok()
}

/// Appends the `event=result` line sweep's reconcile leg owes a settled,
/// unrecorded task: the same legacy (unversioned) shape `dispatch`/
/// `started` already use. `state=reconciled` marks this as sweep's own
/// write, distinct from a `result` event the dispatched agent would have
/// written itself. Idempotent (critic-codex-areas.md M1), via
/// [`append_outcome_if_absent`] — the same read-check-append
/// `record_stranded_closure` below uses — so two reconcilers over the
/// same project settle on exactly one line per task, mirroring Python's
/// `tasks.reconcile`.
fn record_reconciled(root: &Path, task_id: &str, status: TaskStatus, pane: Option<&str>) -> bool {
    append_outcome_if_absent(root, task_id, status, "reconciled", pane)
}

/// One project's `multiverse sweep` pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// How many tasks this pass found still unrecorded before it ran.
    pub in_progress_before: usize,
    /// How many of those remain unrecorded after this pass — the delta
    /// against [`Self::in_progress_before`] is what this pass settled.
    pub in_progress_after: usize,
    /// How many settled-but-unrecorded tasks this pass just wrote an
    /// outcome for.
    pub recorded: usize,
    /// How many already-recorded tasks' still-open panes this pass closed.
    pub reclaimed: usize,
    /// AWAITING tasks [`expire_awaiting`] recorded `failed` this pass for
    /// outliving their lease (finding 4) — previews the would-be count
    /// under a dry run, the same posture `recorded`/`reclaimed` already
    /// take.
    pub expired: usize,
    /// What [`auto_supersede`] did (or, under `--dry-run`, would have
    /// done) this pass — named per task, not just counted, mirroring
    /// `converge_stale`'s own `superseded` list (card `20261102-b-0d8b17`).
    pub superseded: Vec<SupersedeResult>,
    /// Every [`expire_awaiting`] determination this pass made, named per
    /// task with its own free-text reason — card `20261105-b-421801`
    /// gap 2. Mirrors Python's `converged.expired` list verbatim
    /// (`cli.py:12850-12851`): printed regardless of whether the write
    /// itself landed, same as Python's unfiltered `for e in
    /// converged.expired`. `Self::expired`'s own count keeps filtering on
    /// `recorded` for `in_progress_after` — this field is additive, not a
    /// replacement.
    pub expired_tasks: Vec<LeaseExpiry>,
    /// Still-AWAITING tasks with no `result.md` on disk — Python's
    /// `awaiting_gone` (`cli.py:12704-12710`), card `20261105-b-421801`
    /// gap 2. A task [`expire_awaiting`] just recorded `failed` this same
    /// pass is excluded (it is named under `expired_tasks` instead, never
    /// both), matching Python's in-place `found` mutation taking it out of
    /// `TaskState.AWAITING` the moment `apply` actually writes it.
    pub awaiting_gone: Vec<AwaitingNamed>,
    /// Still-AWAITING tasks whose `result.md` was already written before
    /// the pane vanished — Python's `awaiting_partial`, same exclusion
    /// rule as [`Self::awaiting_gone`].
    pub awaiting_partial: Vec<AwaitingNamed>,
    /// Every unrecorded, writing-role, `done`-status task whose lane still
    /// holds uncommitted work, keyed by `task_id` — [`dirty_done_detail`]'s
    /// own text, computed once per task off this pass's own
    /// [`LaneProbeCache`] (the same one `is_reconcile_candidate`'s own
    /// `lane_is_dirty` check already built) rather than left for a caller
    /// to recompute with a fresh cache of its own. `commands::sweep`'s
    /// `dirty_done_notes` used to call [`dirty_done_detail`] again for
    /// exactly these tasks after this pass had already probed their lane,
    /// spawning `git status --porcelain` twice per lane each pass (card
    /// `20261115-b-1ba758`); it now reads this field instead.
    pub dirty_details: BTreeMap<String, String>,
}

/// One AWAITING task named in [`SweepReport::awaiting_gone`] or
/// [`SweepReport::awaiting_partial`] — `task_id` plus age since dispatch,
/// pre-formatted the way Python's own `_named` helper renders it
/// (`cli.py:12707-12712`: `format_elapsed` of `elapsed_since(dispatched_at,
/// now)`, or the literal `"unknown age"` when `dispatched_at` itself is
/// unreadable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwaitingNamed {
    /// The still-AWAITING task's own id.
    pub task_id: String,
    /// Elapsed time since dispatch, pre-formatted for display (see the
    /// struct doc above) — `"unknown age"` when `dispatched_at` itself
    /// could not be read.
    pub age: String,
}

/// Reconciles every settled-but-unrecorded task, then reclaims a clean
/// recorded task's still-open pane. Mirrors `cmd_sweep`/
/// `_reconcile_and_reclaim`: reclaim only ever considers tasks already
/// recorded *before* this pass ran — one just reconciled in the same
/// pass is left for the next sweep, never reclaimed in the same breath
/// it was recorded in.
///
/// Finding 2 (critic-sweep-board.md): a failed probe is never "no panes
/// live" — [`AgentPanes::list_checked`] distinguishes the two, and a
/// failure here fails the *whole* pass open (no reconcile, no reclaim),
/// mirroring `_reconcile_and_reclaim`'s own `live is None` cascade.
///
/// Finding 3/7: reclaim's own clean-lane check now reuses
/// `lane_clean_for_reclaim` (already correct for a writer's own
/// `multiverse/<id>` branch) rather than the narrow
/// `worktrees::dispatched_lane_is_clean`, which only ever answered true
/// for a non-lane (`branch == "main"`) dispatch. A candidate closes only
/// once a *second*, fresh listing confirms the same pane still holds
/// *this task's own dispatched agent* (`SweepTask::agent`, never merely
/// whatever name the first listing happened to show), in the same
/// idle/done status the first listing saw — never a bare "pane id still
/// appears somewhere" — mirroring `_confirm_reclaim_candidates`'s own
/// two-listing, same-agent-name, same-status gate. A dry run previews the
/// reconcile count without
/// writing, and previews the reclaim *candidate* count (before the
/// idle/done confirmation, which only ever gates an actual close) without
/// closing anything — mirroring `_reconcile_and_reclaim`'s own
/// `apply=False` return of `reclaimable_panes`'s raw candidate list.
///
/// `agents.list_checked()` is always called once, even with zero tasks,
/// as a stale/pane-liveness probe; it is called again, only for a real
/// (non-dry-run) pass with at least one reclaim candidate, immediately
/// before closing — a pane closed here must be reconfirmed right before
/// the close, not merely at the start of the pass. That second probe
/// failing fails the reclaim leg closed (no partial reclaim), the same
/// posture `_confirm_reclaim_candidates` takes for an unreadable re-list.
///
/// Also runs [`expire_awaiting`] (finding 4) over the same task snapshot
/// and the same first listing — part of `_reconcile_and_reclaim`'s own
/// always-run per-project pass on the Python side, not gated by `--tick`
/// or fleet halt, matching `sweep.rs`'s own halt posture: halt only ever
/// skips `--tick`'s maintenance pass, never this one. `lease_minutes`/
/// `lease_overrides` are the caller's `Config.awaiting_lease_minutes`/
/// per-account `awaiting_lease_minutes_override`s; `now` is the one clock
/// reading the whole pass shares.
pub fn sweep_project(
    root: &Path,
    dry_run: bool,
    lease_minutes: u32,
    lease_overrides: &HashMap<String, u32>,
    now: OffsetDateTime,
    agents: &mut impl AgentPanes,
) -> SweepReport {
    // No host-sleep tally of its own to offer — only `sweep_project_with_tasks`'s
    // own caller in `commands::sweep` has one (critic-codex-areas.md H1).
    sweep_project_with_tasks(
        root,
        dry_run,
        lease_minutes,
        lease_overrides,
        now,
        0,
        agents,
    )
    .0
}

/// [`sweep_project`] plus the task snapshot it already built, so a
/// caller that also needs [`sweep_tasks`] (nudges, reviewer-timeout,
/// merged-backlog) does not re-parse `routing.log` two or three more
/// times in the same pass.
///
/// `sleep_minutes` (critic-codex-areas.md H1) is this pass's own
/// host-sleep total, forwarded to [`expire_awaiting`] so a host suspension
/// never by itself expires a lane still well within its lease.
///
/// Fetches its own first listing via [`AgentPanes::list_checked`] — the
/// shape every direct caller of this function (every unit test below,
/// plus [`sweep_project`]) already relies on for its own `agents.
/// list_calls` bookkeeping. `mv-cli`'s own `commands::sweep` multi-project
/// loop does not call this: card `20261105-b-421801` gap 1 (Python
/// fetches "one Herdr listing for the reconcile/reclaim pass, never one
/// per project", `cli.py:12661`) needs that single listing fetched once,
/// ahead of the loop, and threaded into every project — see
/// [`sweep_project_with_live`], which this now forwards to.
pub fn sweep_project_with_tasks(
    root: &Path,
    dry_run: bool,
    lease_minutes: u32,
    lease_overrides: &HashMap<String, u32>,
    now: OffsetDateTime,
    sleep_minutes: i64,
    agents: &mut impl AgentPanes,
) -> (SweepReport, Vec<SweepTask>) {
    let live = agents.list_checked();
    sweep_project_with_live(
        root,
        dry_run,
        lease_minutes,
        lease_overrides,
        now,
        sleep_minutes,
        live.as_deref(),
        agents,
    )
}

/// [`sweep_project_with_tasks`], given the pass's own first listing
/// already fetched by the caller — card `20261105-b-421801` gap 1. `None`
/// is a failed/unreachable probe, the same "fails every leg open" meaning
/// [`AgentPanes::list_checked`] already carries (finding 2): every project
/// sharing this one `live` value sees the identical Herdr-unreachable
/// cascade Python's own single `live` variable gives every project in its
/// per-project loop. `agents` is still threaded through for the *second*
/// listing [`AgentPanes::list_checked`] takes immediately before a
/// non-dry-run reclaim close, and for [`AgentPanes::close`] itself — both
/// per-project, matching Python's own `_confirm_reclaim_candidates` (one
/// second listing per project with real candidates, never shared across
/// projects, since a close can invalidate it for the next one).
#[allow(clippy::too_many_arguments)]
pub fn sweep_project_with_live(
    root: &Path,
    dry_run: bool,
    lease_minutes: u32,
    lease_overrides: &HashMap<String, u32>,
    now: OffsetDateTime,
    sleep_minutes: i64,
    live: Option<&[LiveAgentStatus]>,
    agents: &mut impl AgentPanes,
) -> (SweepReport, Vec<SweepTask>) {
    let mut tasks = sweep_tasks(root);
    let in_progress_before = tasks.iter().filter(|task| !task.recorded).count();
    // Snapshot of which ids `in_progress_before` actually counted, taken
    // before `auto_supersede` mutates `tasks` in place below — a task
    // `auto_supersede` corrects can already be `recorded` (see
    // `sweep_project_supersedes_a_landed_blocked_task_and_never_also_expires_it`:
    // an already-recorded BLOCKED outcome auto-supersede rewrites), and
    // that one was never part of this tally to begin with.
    let in_progress_ids: HashSet<String> = tasks
        .iter()
        .filter(|task| !task.recorded)
        .map(|task| task.task_id.clone())
        .collect();

    let Some(live) = live else {
        // Herdr unreachable or itself reported failure: fail every leg
        // open for this project this pass, never promoting a failed
        // probe into "every pane is gone" (finding 2). The dirty-done scan
        // is pure git, not Herdr, so it still runs here, same as every
        // other branch below.
        let mut lane_probe_cache = LaneProbeCache::new();
        let dirty_details = collect_dirty_details(root, &tasks, &mut lane_probe_cache);
        return (
            SweepReport {
                in_progress_before,
                in_progress_after: in_progress_before,
                recorded: 0,
                reclaimed: 0,
                expired: 0,
                superseded: Vec::new(),
                expired_tasks: Vec::new(),
                awaiting_gone: Vec::new(),
                awaiting_partial: Vec::new(),
                dirty_details,
            },
            tasks,
        );
    };
    let is_live = |pane: &str| live.iter().any(|agent| agent.pane_id == pane);
    let is_working = |pane: &str| {
        live.iter()
            .any(|agent| agent.pane_id == pane && agent.status == Some(AgentState::Working))
    };
    // Feeds `auto_supersede`/`expire_awaiting`/`is_awaiting`'s own gate
    // below, not a same-occupant check by itself: `None` when the pane id
    // is gone from the listing, `Some(name)` (possibly empty) when it is
    // still listed. Card 20261108-b-152b33 keeps the reused-pane half (a
    // confirmed *different* name reads as gone); card 20261106-b-2c3d2f
    // Decision 1 keeps a merely unconfirmed (missing/empty) name from
    // reading as gone too. Only `is_awaiting`'s own callers below need
    // this; `reclaim_lane_checks` keeps the pane-only `is_live` above,
    // matching its own existing occupant guard.
    let pane_live_name = |pane: &str| {
        live.iter()
            .find(|current| current.pane_id == pane)
            .map(|current| current.name.clone().unwrap_or_default())
    };

    // `is_reconcile_candidate` only shells out to `git status --porcelain`
    // for a "done but unrecorded" writing-role task (`lane_is_dirty`). A
    // thread per historical dispatch — the same pattern that dominated a
    // warm `board --all` before the task-event index — costs more than it
    // saves: at fleet scale that is thousands of spawns for checks that
    // return false on `task.recorded` without touching git. Filter
    // sequentially; git round-trips stay rare. `lane_probe_cache` keeps
    // those remaining round-trips to one spawn per distinct lane: a later
    // `--into` dispatch naming the same worktree, or reclaim's own
    // porcelain check after this pass just recorded the occupant, must
    // not re-ask git for a directory this pass already read (card
    // `20261115-b-ca59eb`, mirroring `board_data`'s `LaneProbeCache`).
    let mut lane_probe_cache = LaneProbeCache::new();
    let stale_indices: Vec<usize> = tasks
        .iter()
        .enumerate()
        .filter(|(_, task)| is_reconcile_candidate(root, task, &is_working, &mut lane_probe_cache))
        .map(|(i, _)| i)
        .collect();
    let recorded = stale_indices.len();

    // Mirrors Python's `_reconcile_and_reclaim`: `tasklog.reconcile` mutates
    // `found`'s own Task objects in place, so the reclaim pass right below,
    // which reads this same `tasks` list, already sees a task this pass
    // just recorded as RECORDED rather than waiting a whole extra sweep
    // pass to notice (card `20261106-b-6b0385`) — the same in-place update
    // `auto_supersede` below already performs for its own writes.
    if !dry_run {
        for i in stale_indices {
            let status = tasks[i].file_status.expect("filtered to Some above");
            if record_reconciled(root, &tasks[i].task_id, status, tasks[i].pane.as_deref()) {
                tasks[i].recorded = true;
                tasks[i].status = Some(status);
                tasks[i].outcome_state = Some("reconciled".to_string());
            }
        }
    }
    // Reuse the first pane listing and one status verdict per distinct
    // lane. A porcelain-clean lane still needs the live occupant guard:
    // adopted tasks can leave all their state under gitignored paths.
    let pane_matched: Vec<(&SweepTask, &LiveAgentStatus)> = tasks
        .iter()
        .filter(|task| task.recorded)
        .filter_map(|task| {
            let pane = task.pane.as_deref()?;
            let agent = live.iter().find(|agent| agent.pane_id == pane)?;
            Some((task, agent))
        })
        .collect();
    let lane_checks = reclaim_lane_checks(root, &tasks, &is_live, &mut lane_probe_cache);
    let candidates: Vec<(&SweepTask, &LiveAgentStatus)> = pane_matched
        .into_iter()
        .filter(|(task, _)| reclaim_lane_is_safe(task, &lane_checks))
        .collect();

    let mut reclaimed = candidates.len();
    if !dry_run {
        reclaimed = 0;
        // `_confirm_reclaim_candidates`: the first listing must already
        // show idle/done; no eligible candidate skips the second listing
        // entirely (no sleep, no re-probe, matching
        // `a_lane_this_helper_cannot_prove_clean_is_never_reclaimed`'s own
        // "no clean candidate means no re-probe").
        let eligible: Vec<(&SweepTask, &LiveAgentStatus)> = candidates
            .into_iter()
            .filter(|(_, agent)| agent.status.is_some_and(AgentState::is_ready_for_input))
            .collect();
        if !eligible.is_empty() {
            if let Some(fresh) = agents.list_checked() {
                for (task, first) in eligible {
                    let pane = task.pane.as_deref().expect("filtered to Some above");
                    let Some(current) = fresh.iter().find(|agent| agent.pane_id == pane) else {
                        continue;
                    };
                    // Same-agent-name check: the pane must still host
                    // *this task's own* dispatched agent, never merely
                    // "whatever the first listing happened to see" — a
                    // pane that already drifted before the first listing
                    // is not reclaimable either. Missing on either side
                    // reads as `""`, mirroring a `Task.agent` field that
                    // defaults to empty rather than `None`.
                    if current.name.as_deref().unwrap_or("") != task.agent.as_deref().unwrap_or("")
                    {
                        continue;
                    }
                    let Some(second) = current.status else {
                        continue;
                    };
                    if Some(second) != first.status || !second.is_ready_for_input() {
                        continue;
                    }
                    if agents.close(pane) {
                        reclaimed += 1;
                    }
                }
            }
            // An unreadable re-list fails every candidate closed this
            // pass, never a partial reclaim from the stale first listing.
        }
    }

    // Card `20260929-b-fbd213` part B / `20261102-b-0d8b17`: auto-supersede
    // a stale BLOCKED/AWAITING record a later `--into` dispatch or a
    // landed merge has already settled, ahead of `expire_awaiting` —
    // mirrors `converge_stale`'s own fixed order. Mutates `tasks` in
    // place for anything it actually records (`apply=true`); either way,
    // every task this returns is excluded from `expire_awaiting`'s own
    // candidate list below by id, not merely by the mutation, so a
    // `--dry-run` preview — which records nothing — still never lists the
    // same task under both `superseded` and `expired` (mirrors
    // `converge_stale`'s own `handled` filter, which holds "even with
    // `apply=False`, where nothing actually mutates").
    let superseded = auto_supersede(root, &mut tasks, !dry_run, &pane_live_name);
    let handled: HashSet<&str> = superseded.iter().map(|r| r.task_id.as_str()).collect();
    let expire_candidates: Vec<SweepTask> = tasks
        .iter()
        .filter(|t| !handled.contains(t.task_id.as_str()))
        .cloned()
        .collect();

    let expiries = expire_awaiting(
        root,
        &expire_candidates,
        lease_minutes,
        lease_overrides,
        now,
        sleep_minutes,
        !dry_run,
        &pane_live_name,
    );
    let expired = if dry_run {
        expiries.len()
    } else {
        expiries.iter().filter(|expiry| expiry.recorded).count()
    };

    // Card `20261105-b-421801` gap 2: every still-AWAITING task, split by
    // whether it left a `result.md` behind, named with its own age since
    // dispatch — Python's `awaiting_gone`/`awaiting_partial` `_named`
    // suffixes (`cli.py:12704-12716`). A task `expire_awaiting` just
    // recorded `failed` this same pass is excluded, mirroring Python's
    // in-place `found` mutation taking it out of `TaskState.AWAITING` the
    // instant `apply` actually writes it; a `--dry-run` preview, which
    // writes nothing, leaves every one of them visible here too, same as
    // Python's own unmutated `found` under `apply=False`.
    let just_expired: HashSet<&str> = expiries
        .iter()
        .filter(|expiry| expiry.recorded)
        .map(|expiry| expiry.task_id.as_str())
        .collect();
    let mut awaiting_gone = Vec::new();
    let mut awaiting_partial = Vec::new();
    for task in tasks
        .iter()
        .filter(|task| is_awaiting(task, &pane_live_name))
    {
        if just_expired.contains(task.task_id.as_str()) {
            continue;
        }
        let age = task
            .dispatched_at
            .map(|dispatched| format_elapsed(now - dispatched))
            .unwrap_or_else(|| "unknown age".to_string());
        let named = AwaitingNamed {
            task_id: task.task_id.clone(),
            age,
        };
        if awaiting_result_path(root, &task.task_id, lane_task_id(task)).is_some() {
            awaiting_partial.push(named);
        } else {
            awaiting_gone.push(named);
        }
    }
    awaiting_gone.sort_by(|a, b| a.task_id.cmp(&b.task_id));
    awaiting_partial.sort_by(|a, b| a.task_id.cmp(&b.task_id));

    // Python's `after = sum(1 for t in found if t.state in in_progress_states)`
    // is read once `_reconcile_and_reclaim` (reconcile, auto-supersede, and
    // lease expiry together) has actually run, so a task this same apply
    // pass just superseded or lease-expired no longer counts. A `--dry-run`
    // pass applies none of that — `superseded`/`expired` above are preview
    // counts, not mutations — so it stays pinned to `in_progress_before`,
    // matching Python's own preview, which never advances `found`'s state
    // either.
    let in_progress_after = if dry_run {
        in_progress_before
    } else {
        let superseded_in_progress = superseded
            .iter()
            .filter(|result| in_progress_ids.contains(&result.task_id))
            .count();
        in_progress_before - recorded - superseded_in_progress - expired
    };

    // Reuses `lane_probe_cache` as it stands after reconcile and reclaim
    // have both already read it — a lane either of those already probed
    // this pass is a cache hit here, never a second `git status
    // --porcelain` spawn for the same worktree (card `20261115-b-1ba758`).
    let dirty_details = collect_dirty_details(root, &tasks, &mut lane_probe_cache);

    (
        SweepReport {
            in_progress_before,
            in_progress_after,
            recorded,
            reclaimed,
            expired,
            superseded,
            expired_tasks: expiries,
            awaiting_gone,
            awaiting_partial,
            dirty_details,
        },
        tasks,
    )
}

/// Builds [`SweepReport::dirty_details`]: every task in `tasks` whose
/// [`dirty_done_detail_cached`] is `Some`, keyed by `task_id`. Shares one
/// [`LaneProbeCache`] across every task, so two historical task ids naming
/// the same lane (a task dispatched `--into` another's worktree) probe
/// that lane at most once between them, same as every other cache user in
/// this file.
fn collect_dirty_details(
    root: &Path,
    tasks: &[SweepTask],
    cache: &mut LaneProbeCache,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for task in tasks {
        if let Some(detail) = dirty_done_detail_cached(root, task, cache) {
            out.insert(task.task_id.clone(), detail);
        }
    }
    out
}

// ---------------------------------------------------------------------
// Fleet-wide sweep lock (finding 7, critic-sweep-board.md): mirrors
// `cli._sweep_lock` — a single non-blocking flock under
// `<state_dir>/sweep.lock`, so two overlapping hub ticks never both
// record/reclaim the same pass. `--dry-run` never acquires it, the same
// posture `cmd_sweep` takes (a preview needs no mutual exclusion);
// `sweep.rs` is responsible for only calling this for a real pass.
// ---------------------------------------------------------------------

/// Held for the lifetime of one real sweep pass; releases the flock on
/// drop so a crash never leaves the fleet permanently locked out.
pub struct SweepLockGuard {
    file: std::fs::File,
}

impl Drop for SweepLockGuard {
    fn drop(&mut self) {
        let _ = rustix::fs::flock(&self.file, rustix::fs::FlockOperation::Unlock);
    }
}

/// Whether [`try_acquire_sweep_lock`] got the lock, or found another
/// process already holding it. `pid`/`since` are read back from the
/// lock file's own JSON marker on a best-effort basis — unreadable or
/// malformed reads as `0`/`"unknown"` rather than failing the whole
/// probe, since the only thing this fact is for is one printed line.
pub enum SweepLock {
    Acquired(SweepLockGuard),
    Held { pid: u64, since: String },
}

/// Tries, without blocking, to take the fleet-wide sweep lock at
/// `<state_dir>/sweep.lock`. Returns [`SweepLock::Acquired`] with a guard
/// that releases the flock on drop, or [`SweepLock::Held`] (never an error)
/// when another process already holds it — only a genuine I/O failure
/// opening or locking the file is an `Err`.
pub fn try_acquire_sweep_lock(state_dir: &Path) -> std::io::Result<SweepLock> {
    let path = state_dir.join("sweep.lock");
    let mut file = crate::activity::support::open_lock_file(&path)?;
    // Non-blocking: a second sweep must find out immediately that another
    // one is running, never queue up and wait its turn.
    match rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            use std::io::{Seek, SeekFrom, Write};
            let payload = format!(
                "{{\"pid\": {}, \"since\": \"{}\"}}\n",
                std::process::id(),
                mv_core::time::now_stamp(),
            );
            // Stamp the lock file with who holds it and since when, purely
            // for a human reading `SweepLock::Held`'s message — truncate
            // first since a previous holder's marker may be longer.
            let _ = file.set_len(0);
            let _ = file.seek(SeekFrom::Start(0));
            let _ = file.write_all(payload.as_bytes());
            let _ = file.flush();
            Ok(SweepLock::Acquired(SweepLockGuard { file }))
        }
        Err(e) if e == rustix::io::Errno::WOULDBLOCK || e == rustix::io::Errno::AGAIN => {
            // Already held elsewhere. Read back the other holder's marker
            // on a best-effort basis (see `SweepLock::Held`'s own doc) —
            // this is purely informational, so a bad read never fails here.
            let (pid, since) = std::fs::read_to_string(&path)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .map(|value| {
                    (
                        value["pid"].as_u64().unwrap_or(0),
                        value["since"].as_str().unwrap_or("unknown").to_string(),
                    )
                })
                .unwrap_or((0, "unknown".to_string()));
            Ok(SweepLock::Held { pid, since })
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------
// `tasks --close-stranded`/`tasks --reclaimable`: the human-in-the-loop
// twin of `sweep`'s own reconcile/reclaim passes above. Mirrors
// `tasks.close_stranded`/`runner.reclaimable_panes` (Part A of
// `multiverse/tasks.py`'s stranded-task machinery). Part B is implemented
// by `auto_supersede` and `expire_awaiting`, called in that order by
// `sweep_project_with_tasks` above.
// ---------------------------------------------------------------------

/// `close_stranded` records `done` for a branch proven to have landed —
/// mirrors `tasks.RECONCILED_BY_MERGE`.
pub const RECONCILED_BY_MERGE: &str = "reconciled-by-merge";
/// A branch that exists, has its own commits, and none of them has landed
/// — mirrors `tasks.ABANDONED_UNMERGED`.
pub const ABANDONED_UNMERGED: &str = "abandoned-unmerged";
/// A branch that exists but has no commits of its own since its cut point
/// — mirrors `tasks.ABANDONED_EMPTY_LANE`.
pub const ABANDONED_EMPTY_LANE: &str = "abandoned-empty-lane";
/// An empty branch whose working tree is not — mirrors
/// `tasks.STRANDED_UNCOMMITTED_WORK`.
pub const STRANDED_UNCOMMITTED_WORK: &str = "stranded-uncommitted-work";
/// A read-only role's stranded dispatch, which never gets a branch at all
/// — mirrors `tasks.NO_BRANCH_EXPECTED`.
pub const NO_BRANCH_EXPECTED: &str = "no-branch-expected";
/// A writer's branch that is simply gone, with no durable main-history
/// evidence either way — mirrors `tasks.UNKNOWN_NEEDS_HUMAN`.
pub const UNKNOWN_NEEDS_HUMAN: &str = "unknown-needs-human";
/// A `result.md` left behind with no status file — mirrors
/// `tasks.ABANDONED_PARTIAL_RESULT`.
pub const ABANDONED_PARTIAL_RESULT: &str = "abandoned-partial-result";

/// One stranded task's determination, and whether `close_stranded` actually
/// recorded it. Mirrors `tasks.StrandedClosure`: returned whether or not
/// `apply` was true, so a dry run and a real run share one shape.
#[derive(Debug, Clone)]
pub struct StrandedClosure {
    /// The stranded task's own id.
    pub task_id: String,
    /// The terminal status `close_stranded` determined for this task —
    /// `Done` only for [`RECONCILED_BY_MERGE`], `Failed`/`Blocked` for
    /// every other outcome constant above.
    pub status: TaskStatus,
    /// Which of the outcome constants above (e.g. [`ABANDONED_UNMERGED`],
    /// [`UNKNOWN_NEEDS_HUMAN`]) this determination landed on.
    pub state: &'static str,
    /// Human-readable evidence backing this determination — what
    /// `close_stranded` actually found on the branch/lane.
    pub evidence: String,
    /// Whether this was actually written to the routing log, as opposed
    /// to only previewed under a dry run.
    pub recorded: bool,
}

/// Whether `task` is a genuine AWAITING candidate for `close_stranded`/
/// `expire_awaiting`/`auto_supersede`: not yet recorded, no status file,
/// and the pane it started in can be *confirmed* gone — not merely
/// unconfirmed. `live_name` answers "what does the live listing say
/// occupies this pane right now": `None` when the pane id is not listed
/// at all, `Some("")` when it is listed but no name could be read back,
/// `Some(name)` when it is listed under `name`. A missing pane id, or a
/// listed pane id under a confirmed *different*, non-empty name (card
/// `20261108-b-152b33`: a reused pane id now hosting a different agent),
/// both read as gone. A listed pane id with a missing/empty live name is
/// neither — it is unconfirmed, and must not read as gone either (card
/// `20261106-b-2c3d2f` Decision 1): recording `failed` for a still-live
/// agent just because its name could not be read back invents a terminal
/// outcome the agent may still be about to settle itself.
fn is_awaiting(task: &SweepTask, live_name: &impl Fn(&str) -> Option<String>) -> bool {
    !task.recorded
        && task.file_status.is_none()
        && match task.pane.as_deref() {
            None => true,
            Some(pane) => match live_name(pane) {
                None => true,
                Some(name) if name.is_empty() => false,
                Some(name) => name != task.agent.as_deref().unwrap_or(""),
            },
        }
}

/// Where `task_id` left its `result.md`, if anywhere — the lane's own copy
/// first (an `--into` writer's lane is named by `lane_task_id`, not
/// necessarily `task_id`), then the main checkout's. Mirrors
/// `tasks._result_path`.
fn awaiting_result_path(root: &Path, task_id: &str, lane_task_id: &str) -> Option<PathBuf> {
    let lane_result = root
        .join(".multiverse/worktrees")
        .join(lane_task_id)
        .join(".multiverse/tasks")
        .join(task_id)
        .join("result.md");
    if lane_result.is_file() {
        return Some(lane_result);
    }
    let main_result = root
        .join(".multiverse/tasks")
        .join(task_id)
        .join("result.md");
    if main_result.is_file() {
        return Some(main_result);
    }
    None
}

/// Whether `root` is a git checkout we should bother spawning `git`
/// against. A rehearsal copy of `.multiverse` has no `.git` (file or
/// directory); every probe below already fails closed there, so skipping
/// the spawn is equivalent and avoids tens of `git` processes per sweep.
fn has_git(root: &Path) -> bool {
    root.join(".git").exists()
}

/// One `git -C <root> <args>` call, succeeding stdout only on a clean
/// exit — the same fail-closed posture every other git read in this
/// module already takes for an unreadable repository.
fn git_stdout(root: &Path, args: &[&str]) -> Option<String> {
    if !has_git(root) {
        return None;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Whether a `git -C <root> <args>` call exits cleanly — used for the
/// existence/ancestry probes `branch_disposition` needs only a yes/no
/// answer from.
fn git_ok(root: &Path, args: &[&str]) -> bool {
    has_git(root)
        && std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .is_ok_and(|output| output.status.success())
}

/// Whether `text` contains `needle` as a whole token — delimited by
/// anything other than a word character, `.`, `/` or `-` (a branch name's
/// own alphabet). The `regex` crate cannot express Python's
/// `(?<!...)...(?!...)` lookaround directly, so this walks `find` matches
/// by hand instead of pulling in `fancy-regex` for one call site.
fn contains_branch_token(text: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_token_char = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'/' | b'-');
    let bytes = text.as_bytes();
    let mut start = 0usize;
    while let Some(pos) = text[start..].find(needle) {
        let idx = start + pos;
        let before_ok = idx == 0 || !is_token_char(bytes[idx - 1]);
        let after = idx + needle.len();
        let after_ok = after >= bytes.len() || !is_token_char(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
        start = idx + 1;
        if start > text.len() {
            break;
        }
    }
    false
}

/// Durable main-history evidence that a now-missing `branch` landed before
/// it was pruned: a merge on `main` naming it exactly, or a main-reachable
/// commit descended from its recorded cut point carrying an exact
/// `Task: <lane_task_id>` trailer. Mirrors `tasks._pruned_branch_merge_evidence`.
fn pruned_branch_merge_evidence(root: &Path, lane_task_id: &str, branch: &str) -> Option<String> {
    if let Some(merges) = git_stdout(
        root,
        &[
            "log",
            "main",
            "--merges",
            "--fixed-strings",
            &format!("--grep={branch}"),
            "--format=%H%x1f%B%x1e",
        ],
    ) {
        for record in merges.split('\u{1e}') {
            let record = record.trim_matches('\n');
            if record.is_empty() {
                continue;
            }
            let Some((sha, message)) = record.split_once('\u{1f}') else {
                continue;
            };
            if contains_branch_token(message, branch) {
                let short = &sha[..sha.len().min(12)];
                return Some(format!(
                    "branch '{branch}' was pruned after main merge {short} named it"
                ));
            }
        }
    }

    let cut_point = git_stdout(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/multiverse/base/{lane_task_id}"),
        ],
    )?;
    let cut_point = cut_point.trim();
    if cut_point.is_empty() {
        return None;
    }

    let trailer = format!("Task: {lane_task_id}");
    let commits = git_stdout(root, &["log", "main", "--format=%H%x1f%B%x1e"])?;
    for record in commits.split('\u{1e}') {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let Some((sha, message)) = record.split_once('\u{1f}') else {
            continue;
        };
        if !message.lines().any(|line| line == trailer) {
            continue;
        }
        if !git_ok(root, &["merge-base", "--is-ancestor", cut_point, sha]) {
            continue;
        }
        let short = &sha[..sha.len().min(12)];
        return Some(format!(
            "branch '{branch}' was pruned after main commit {short} carried '{trailer}'"
        ));
    }
    None
}

/// Whether `sha`'s tree equals its first parent's — nothing to land, the
/// trivial case `landed_by_patch_id` still has to call landed even though
/// `git cherry` prints it `+` (no patch-id to match). Mirrors
/// `worktrees._zero_diff_vs_first_parent`.
fn zero_diff_vs_first_parent(root: &Path, sha: &str) -> bool {
    let Some(output) = git_stdout(
        root,
        &[
            "rev-parse",
            &format!("{sha}^{{tree}}"),
            &format!("{sha}^^{{tree}}"),
        ],
    ) else {
        return false;
    };
    let pair: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    pair.len() == 2 && pair[0] == pair[1]
}

/// Whether `branch` has landed by patch-id equivalence rather than
/// ancestry — survives a rebase, squash or re-commit that ancestry alone
/// would call unmerged. Mirrors `worktrees.landed_by_patch_id`, called
/// here the same way `tasks.branch_disposition` calls it: against `HEAD`
/// in `root`'s own shared checkout, never a live worktree.
fn landed_by_patch_id(root: &Path, branch: &str) -> bool {
    let Some(output) = git_stdout(root, &["cherry", "HEAD", branch]) else {
        return false;
    };
    for line in output.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.starts_with('-') {
            continue;
        }
        let Some(rest) = line.strip_prefix('+') else {
            return false;
        };
        let sha = rest.split_whitespace().next().unwrap_or("");
        if sha.is_empty() || !zero_diff_vs_first_parent(root, sha) {
            return false;
        }
    }
    true
}

/// `branch_disposition`'s four verdicts — kept distinct rather than folded
/// into a bool, mirroring `tasks.branch_disposition`'s own reasoning for
/// why they cannot collapse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BranchVerdict {
    Landed,
    NotLanded,
    Empty,
    Gone,
}

/// Whether `lane_task_id`'s own branch shows its work landed, and the
/// evidence for it — the one fact left, for a stranded task, that is not
/// a guess. Mirrors `tasks.branch_disposition`; only ever called for a
/// writing role (`close_stranded` asks that first).
fn branch_disposition(root: &Path, lane_task_id: &str) -> (BranchVerdict, String) {
    let branch = format!("multiverse/{lane_task_id}");
    let branch_ref = format!("refs/heads/{branch}");
    if !git_ok(root, &["rev-parse", "--verify", "--quiet", &branch_ref]) {
        if let Some(evidence) = pruned_branch_merge_evidence(root, lane_task_id, &branch) {
            return (BranchVerdict::Landed, evidence);
        }
        return (
            BranchVerdict::Gone,
            format!(
                "no branch '{branch}' exists in this repository — indistinguishable from one \
                 that landed and was cleaned up afterward; needs a human"
            ),
        );
    }

    let base_ref = format!("refs/multiverse/base/{lane_task_id}");
    let cut_point = if git_ok(root, &["rev-parse", "--verify", "--quiet", &base_ref]) {
        base_ref
    } else {
        // No recorded cut point (a lane older than the ref this build
        // writes at `create` time) — falls back to HEAD, same as
        // `worktrees.has_commits`'s own documented fallback.
        "HEAD".to_string()
    };
    let has_commits = git_stdout(
        root,
        &["rev-list", "--count", &format!("{cut_point}..{branch}")],
    )
    .is_some_and(|count| count.trim() != "0" && !count.trim().is_empty());
    if !has_commits {
        return (
            BranchVerdict::Empty,
            format!(
                "branch '{branch}' has no commits of its own since its cut point — the lane was \
                 created and never written to"
            ),
        );
    }

    if landed_by_patch_id(root, &branch) {
        return (
            BranchVerdict::Landed,
            format!("branch '{branch}' has landed (patch-id equivalent to HEAD)"),
        );
    }
    let count = git_stdout(root, &["rev-list", "--count", &format!("HEAD..{branch}")])
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    (
        BranchVerdict::NotLanded,
        format!("branch '{branch}' has {count} commit(s) not in HEAD"),
    )
}

/// A short main-branch sha when `lane_task_id`'s own branch has landed —
/// the concrete commit evidence a linked backlog item can carry. Mirrors
/// `tasks.merged_evidence`: `branch_disposition` stays the one authority
/// for *whether* the branch landed; this only supplies `HEAD`'s own sha,
/// read by the same sweep that made that determination.
pub fn merged_evidence(root: &Path, lane_task_id: &str) -> Option<String> {
    let (verdict, _evidence) = branch_disposition(root, lane_task_id);
    if verdict != BranchVerdict::Landed {
        return None;
    }
    let tip = git_stdout(root, &["rev-parse", "--short", "HEAD"])?;
    let tip = tip.trim();
    if tip.is_empty() {
        None
    } else {
        Some(tip.to_string())
    }
}

/// Close backlog items whose linked writer is genuinely `done` and whose
/// branch has independently landed. Mirrors `cli._close_merged_backlog_links`
/// so `tasks` listing (and, next, board) share one implementation.
///
/// Returns `(item id, evidence sha, recorded)` for every item closed (or,
/// under `apply=false`, that would be).
pub fn close_merged_backlog_links(
    root: &Path,
    tasks: &[SweepTask],
    apply: bool,
) -> Vec<(String, String, bool)> {
    let linked: Vec<_> = backlog::read_items(root, false)
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
        if task.status != Some(TaskStatus::Done) {
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
            match backlog::done(root, &item.item, &evidence, None) {
                Ok(_) => recorded = true,
                Err(BacklogError::TerminalItem { .. }) => continue,
                Err(_) => continue,
            }
        }
        closed.push((item.item.clone(), evidence, recorded));
    }
    closed
}

/// Evidence text for `STRANDED_UNCOMMITTED_WORK`: how many paths are
/// dirty and the first few of them. Mirrors `tasks._stranded_uncommitted_evidence`.
fn stranded_uncommitted_evidence(porcelain: &str) -> String {
    let lines: Vec<&str> = porcelain.lines().collect();
    let total = lines.len();
    let mut shown: Vec<String> = lines.iter().take(3).map(|s| s.to_string()).collect();
    if let Some(first) = shown.first_mut() {
        if first.len() > 2 && first.as_bytes().get(2) != Some(&b' ') {
            first.insert(0, ' ');
        }
    }
    let named: Vec<&str> = shown
        .iter()
        .map(|line| line.get(3..).unwrap_or(""))
        .collect();
    let remaining = total.saturating_sub(shown.len());
    let more = if remaining > 0 {
        format!(", and {remaining} more")
    } else {
        String::new()
    };
    let plural = if total == 1 { "" } else { "s" };
    format!(
        "branch has no commits of its own, but its working tree holds {total} uncommitted \
         change{plural}: {} — a human must decide whether to resume it with --into or discard \
         it{more}",
        named.join(", ")
    )
}

/// Appends the stranded-task result record `close_stranded`/
/// `expire_awaiting`'s own `apply=true` legs write, and the escalation leg
/// `mv_engine::sweep_nudges::resume_stuck_lanes` reuses — the same legacy
/// (unversioned) shape `record_reconciled` above uses. Idempotent, not
/// merely serialized (critic-codex-areas.md M1): routed through this
/// module's own `append_outcome_if_absent`, which holds the project lock
/// across the read, the check, and the append, so two reconcilers racing
/// the same settled AWAITING task — two overlapping sweeps, or a sweep racing
/// `tasks --reconcile`/`board` — can never both append an outcome line for
/// it. Mirrors Python's `accounts.append_outcome_if_absent`, the one
/// primitive every first-time-AWAITING-outcome writer shares on that side.
///
/// `pub` (card `20261102-b-943ed3`) so `mv_engine::sweep_nudges::
/// resume_stuck_lanes` can reuse this exact primitive for its own
/// escalation leg rather than a second copy of this format string.
pub fn record_stranded_closure(
    root: &Path,
    task_id: &str,
    status: TaskStatus,
    state: &str,
    pane: Option<&str>,
) -> bool {
    append_outcome_if_absent(root, task_id, status, state, pane)
}

/// Determine, and optionally record, an honest outcome for every AWAITING
/// task in `root`. Mirrors `tasks.close_stranded`: `apply=false` computes
/// every determination and writes nothing (the dry run a caller should
/// offer before ever passing `true`); `apply=true` records through
/// [`record_stranded_closure`].
///
/// A failed live listing is a no-op — no task is examined, nothing is
/// recorded, and this returns empty — never a fallback to treating the
/// listing as empty. An empty listing reads as "every pane is confirmed
/// gone", which `is_awaiting` would then take as license to close every
/// AWAITING task; collapsing a *failed probe* into that same empty shape
/// would fail open exactly when Herdr itself cannot be asked. The sole
/// caller today, `close_stranded_and_report`
/// (`crates/mv-cli/src/commands/tasks.rs`), already pre-checks this before
/// calling in here; the guard now lives with the risk, not only with that
/// one caller, so a future second call site cannot reintroduce it by
/// omitting the same check.
pub fn close_stranded(
    root: &Path,
    apply: bool,
    agents: &mut impl AgentPanes,
) -> Vec<StrandedClosure> {
    // `list_checked`, not `list`: only it carries the agent name a
    // same-occupant check needs (`list`'s own `LiveAgent` has no name
    // field at all).
    let Some(live) = agents.list_checked() else {
        return Vec::new();
    };
    let tasks = sweep_tasks(root);
    // Feeds `is_awaiting`'s own gate below, not a same-occupant check by
    // itself: `None` when the pane id is gone from the listing, `Some(name)`
    // (possibly empty) when it is still listed. Card 20261108-b-152b33
    // keeps the reused-pane half (a confirmed *different* name reads as
    // gone); card 20261106-b-2c3d2f Decision 1 keeps a merely unconfirmed
    // (missing/empty) name from reading as gone too.
    let live_name = |pane: &str| {
        live.iter()
            .find(|current| current.pane_id == pane)
            .map(|current| current.name.clone().unwrap_or_default())
    };
    let mut lane_probe_cache = LaneProbeCache::new();

    let mut closures = Vec::new();
    for task in &tasks {
        if !is_awaiting(task, &live_name) {
            continue;
        }
        let lane = lane_task_id(task);
        let (status, state, evidence) =
            if let Some(path) = awaiting_result_path(root, &task.task_id, lane) {
                (
                    TaskStatus::Failed,
                    ABANDONED_PARTIAL_RESULT,
                    format!(
                        "result.md exists at {} with no status file; the pane is gone and nothing \
                     further is coming",
                        path.strip_prefix(root).unwrap_or(&path).display()
                    ),
                )
            } else if !is_writing_role(&task.role) {
                (
                    TaskStatus::Failed,
                    NO_BRANCH_EXPECTED,
                    format!(
                    "role '{}' never gets a worktree or a branch; its own result.md/status file \
                     would be reconcile's evidence, not git's, and this task has none",
                    task.role
                ),
                )
            } else {
                match branch_disposition(root, lane) {
                    (BranchVerdict::Gone, evidence) => {
                        (TaskStatus::Blocked, UNKNOWN_NEEDS_HUMAN, evidence)
                    }
                    (BranchVerdict::Empty, evidence) => {
                        match cached_lane_porcelain(root, lane, &mut lane_probe_cache) {
                            Some(dirty) if !dirty.is_empty() => (
                                TaskStatus::Blocked,
                                STRANDED_UNCOMMITTED_WORK,
                                stranded_uncommitted_evidence(&dirty),
                            ),
                            _ => (TaskStatus::Failed, ABANDONED_EMPTY_LANE, evidence),
                        }
                    }
                    (BranchVerdict::Landed, evidence) => {
                        (TaskStatus::Done, RECONCILED_BY_MERGE, evidence)
                    }
                    (BranchVerdict::NotLanded, evidence) => {
                        (TaskStatus::Failed, ABANDONED_UNMERGED, evidence)
                    }
                }
            };

        let recorded = apply
            && record_stranded_closure(root, &task.task_id, status, state, task.pane.as_deref());
        closures.push(StrandedClosure {
            task_id: task.task_id.clone(),
            status,
            state,
            evidence,
            recorded,
        });
    }
    closures
}

// ---------------------------------------------------------------------
// `converge_stale`'s auto-supersede pass (card `20260929-b-fbd213` part B,
// task `20261102-fixer-c628da`, card `20261102-b-0d8b17`): part 2 of
// Python's unattended stale-convergence pair, ported after part 1
// (`expire_awaiting`, below) already landed. The
// nudge/resume/cap machinery stays out of scope — tracked separately as
// `20261102-b-b4ede0`.
// ---------------------------------------------------------------------

/// A stale BLOCKED/AWAITING record's auto-supersede determination, and
/// whether it was actually recorded. Mirrors `tasks.SupersedeResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupersedeResult {
    /// The stale record's own task id.
    pub task_id: String,
    /// The terminal status this auto-supersede determined.
    pub status: TaskStatus,
    /// Which auto-supersede reason this landed on — e.g.
    /// [`SUPERSEDED_REDISPATCHED`] or [`SUPERSEDED_MERGED`].
    pub state: &'static str,
    /// Human-readable reason text accompanying the correction.
    pub reason: String,
    /// Whether this was actually appended to the routing log, as opposed
    /// to only previewed under a dry run.
    pub recorded: bool,
}

/// `tasks.SUPERSEDED_REDISPATCHED`: a stale record whose lane a later
/// `--into` dispatch has already reused.
pub const SUPERSEDED_REDISPATCHED: &str = "superseded-redispatched";

/// `tasks.SUPERSEDED_MERGED`: a stale record whose own branch has already
/// landed on main.
pub const SUPERSEDED_MERGED: &str = "superseded-merged";

/// Map each task id to the newest *later* dispatch that reused its lane —
/// `auto_supersede` asks this same lane-reuse question for every
/// candidate, so the answer is built once here from the log's own
/// append-order (`dispatch_ordinal`), rather than rescanning the full
/// task list per candidate. Mirrors `tasks._lane_reuse_index`.
///
/// Walking newest-to-oldest, the *first* task seen for a branch is the
/// newest one — recorded once per branch and never overwritten — so an
/// older occupant is attributed to the newest later occupant, not merely
/// the next one after it.
fn lane_reuse_index(tasks: &[SweepTask]) -> HashMap<String, usize> {
    let mut by_dispatch_order: Vec<usize> = (0..tasks.len()).collect();
    by_dispatch_order.sort_by_key(|&i| tasks[i].dispatch_ordinal);

    let mut newest_by_branch: HashMap<String, usize> = HashMap::new();
    let mut reused_by: HashMap<String, usize> = HashMap::new();
    for &i in by_dispatch_order.iter().rev() {
        let branch = format!("multiverse/{}", lane_task_id(&tasks[i]));
        if let Some(&newer) = newest_by_branch.get(&branch) {
            if tasks[newer].dispatch_ordinal > tasks[i].dispatch_ordinal {
                reused_by.insert(tasks[i].task_id.clone(), newer);
            }
        }
        newest_by_branch.entry(branch).or_insert(i);
    }
    reused_by
}

/// Converge a stale BLOCKED or AWAITING record that a later event has
/// already settled. Mirrors `tasks.auto_supersede`: card `20260929-b-fbd213`
/// part B.
///
/// Only two triggers, both read straight off the log and git, never
/// inferred: (a) a later dispatch reused this task's own lane (`--into`
/// the same worktree/branch — `lane_reuse_index`), or (b) this task's
/// own branch has already landed on main (`branch_disposition`). Either
/// one means whatever this record currently says is moot.
///
/// Every candidate is recorded `done`, tagged [`SUPERSEDED_REDISPATCHED`]
/// or [`SUPERSEDED_MERGED`] — never evidence of failure, only that the
/// outcome is now known and it is not this stale record.
///
/// Restricted to a task whose log already shows a `blocked` outcome
/// (`task.recorded && task.status == Some(TaskStatus::Blocked)`), or one
/// still genuinely AWAITING (`is_awaiting`, the same gate
/// `expire_awaiting`/`close_stranded` use) — never a task still RUNNING/
/// STALLED or one already `done`/`failed`, which is terminal and not this
/// function's to revisit. A read-only role (`is_writing_role` false) is
/// skipped outright: it has no branch for either trigger to read.
///
/// A task dispatched `--into` another task's lane is only ever superseded
/// by trigger (a) above — its *own* lane is the owner's, and the owner's
/// branch landing is the owner's own outcome to record, never this
/// occupant's.
///
/// A RECORDED-BLOCKED candidate is corrected via
/// [`append_supersede_if_absent`] (mandatory evidence, never a rewrite); a
/// still-AWAITING one has no prior outcome to correct, so it is recorded
/// for the first time instead, via [`record_stranded_closure`] — the same
/// unconditional, snapshot-scoped append [`close_stranded`]/
/// [`expire_awaiting`] already use for their own first-time AWAITING
/// records (that function's own doc comment: a second call never revisits
/// the same task because this one already mutated it out of AWAITING).
///
/// `apply=false` computes every determination and writes nothing — the
/// same dry-run shape [`close_stranded`]/[`expire_awaiting`] already
/// offer. Every candidate this returns with `recorded=true` has also had
/// `task.recorded`/`status`/`outcome_state` updated in place on the
/// caller's own slice, so a caller that goes on to run [`expire_awaiting`]
/// over the same tasks right after sees this one as recorded, not
/// AWAITING, and never double-records it — mirroring `converge_stale`'s
/// own ordering contract.
pub fn auto_supersede(
    root: &Path,
    tasks: &mut [SweepTask],
    apply: bool,
    live_name: &impl Fn(&str) -> Option<String>,
) -> Vec<SupersedeResult> {
    let reuse_index = lane_reuse_index(tasks);
    let mut results = Vec::new();

    for i in 0..tasks.len() {
        let is_candidate = if tasks[i].recorded {
            tasks[i].status == Some(TaskStatus::Blocked)
        } else {
            is_awaiting(&tasks[i], live_name)
        };
        if !is_candidate || !is_writing_role(&tasks[i].role) {
            continue;
        }

        let lane = lane_task_id(&tasks[i]).to_string();
        let (state, reason) = if let Some(&newer) = reuse_index.get(&tasks[i].task_id) {
            let reused_task_id = tasks[newer].task_id.clone();
            let reused_dispatched_at = tasks[newer]
                .dispatched_at
                .map(mv_core::time::format_seconds_utc)
                .unwrap_or_default();
            (
                SUPERSEDED_REDISPATCHED,
                format!(
                    "lane reused by task {reused_task_id:?}'s dispatch at \
                     {reused_dispatched_at} (--into the same worktree/branch)"
                ),
            )
        } else {
            // A `--into` occupant's lane is its owner's, not its own — only
            // the lane's actual owner can be superseded by its own branch
            // landing.
            if lane != tasks[i].task_id {
                continue;
            }
            match branch_disposition(root, &lane) {
                (BranchVerdict::Landed, evidence) => (SUPERSEDED_MERGED, evidence),
                _ => continue,
            }
        };

        let status = TaskStatus::Done;
        let mut recorded = false;
        if tasks[i].recorded {
            if apply {
                let Some(supersedes) = tasks[i].outcome_ordinal else {
                    // No targetable outcome line (a pre-`revisions=` or
                    // otherwise damaged record) — left exactly as it was;
                    // not a reason to abort every other task's
                    // convergence in the same pass.
                    continue;
                };
                match append_supersede_if_absent(
                    root,
                    &tasks[i].task_id,
                    supersedes,
                    status.as_str(),
                    state,
                    &reason,
                    tasks[i].revisions,
                    None,
                ) {
                    Ok(appended) => recorded = appended,
                    Err(_) => continue,
                }
            }
        } else if apply {
            recorded = record_stranded_closure(
                root,
                &tasks[i].task_id,
                status,
                state,
                tasks[i].pane.as_deref(),
            );
        }

        if recorded {
            tasks[i].recorded = true;
            tasks[i].status = Some(status);
            tasks[i].outcome_state = Some(state.to_string());
        }
        results.push(SupersedeResult {
            task_id: tasks[i].task_id.clone(),
            status,
            state,
            reason,
            recorded,
        });
    }
    results
}

// ---------------------------------------------------------------------
// `converge_stale`'s lease-expiry pass (finding 4, critic-sweep-board.md,
// task `20261102-fixer-40e851`): part 1 of Python's unattended
// stale-convergence pair.
// ---------------------------------------------------------------------

/// One AWAITING task's lease-expiry determination, and whether it was
/// actually recorded — the unattended twin of [`StrandedClosure`]. See
/// [`expire_awaiting`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeaseExpiry {
    /// The AWAITING task's own id.
    pub task_id: String,
    /// Free-text reason this lease was treated as expired, for display.
    pub state: String,
    /// Whether this was actually recorded `failed` in the routing log, as
    /// opposed to only previewed under a dry run.
    pub recorded: bool,
}

/// `(tip short sha, has_commits_beyond_head)` for `lane_task_id`'s own
/// branch — mirrors `tasks._branch_evidence`: the narrower two-way
/// question [`expire_awaiting`] needs (are there commits `HEAD` does not
/// have), not `branch_disposition`'s own four-way merged/unmerged/empty/
/// gone verdict for a human. An empty tip means the branch does not
/// exist at all (a read-only role, or a writer whose lane was never
/// created).
fn branch_evidence(root: &Path, lane_task_id: &str) -> (String, bool) {
    let branch = format!("multiverse/{lane_task_id}");
    let branch_ref = format!("refs/heads/{branch}");
    let Some(tip) = git_stdout(
        root,
        &["rev-parse", "--short", "--verify", "--quiet", &branch_ref],
    )
    .map(|text| text.trim().to_string())
    .filter(|text| !text.is_empty()) else {
        return (String::new(), false);
    };
    let has_unmerged = git_stdout(root, &["rev-list", "--count", &format!("HEAD..{branch}")])
        .and_then(|text| text.trim().parse::<u64>().ok())
        .is_some_and(|count| count > 0);
    (tip, has_unmerged)
}

/// Even an unparseable or partial status is unknown, not abandonment —
/// Python's `tasks._has_current_status_file`. Checks the lane worktree
/// first, then the project `.multiverse/tasks/` tree (the same lane-first
/// order `read_writer_status`/`resolve` already use), and is satisfied by
/// bare existence once the file postdates this run's own
/// `started_at`/`dispatched_at` — content never has to parse as one of
/// the three terminal tokens, unlike every other status reader here,
/// because this gate only ever asks "did something just get written,"
/// never "what does it say" (critic-codex-areas.md H2: a lane that
/// finished in its own worktree, or wrote a status mid-truncate, must
/// never be superseded by a lease-expiry pass that only looked at main).
fn has_current_status_file(root: &Path, task: &SweepTask) -> bool {
    let since = task
        .started_at
        .or(task.dispatched_at)
        .and_then(offset_to_system_time);
    let lane = safe_lane_root(root, lane_task_id(task));
    for parent in [lane.as_deref(), Some(root)].into_iter().flatten() {
        let Some(path) = status_path_under(parent, &task.task_id) else {
            continue;
        };
        match std::fs::metadata(&path).and_then(|meta| meta.modified()) {
            Ok(modified) => {
                let predates_run = since.is_some_and(|since| modified < since);
                if !predates_run {
                    return true;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return true,
        }
    }
    false
}

/// Record a genuinely stranded AWAITING task `failed`, once it has stayed
/// that way past its lease — mirrors `tasks.expire_awaiting`.
/// `lease_minutes` is the fleet-wide default
/// (`Config.awaiting_lease_minutes`); `lease_overrides` is `{account name:
/// minutes}` (`Account.awaiting_lease_minutes_override`), consulted per
/// task before falling back to the fleet default. A task with no readable
/// `started_at`/`dispatched_at` timestamp, or one not yet past its lease,
/// is left alone — there is nothing conditional about that half of the
/// gate. `apply=false` computes every determination and writes nothing,
/// the same dry-run shape [`close_stranded`] already offers.
///
/// `sleep_minutes` (critic-codex-areas.md H1) is this sweep pass's own
/// host-sleep total — the caller's own tally from
/// `mv_engine::hostsleep::report`, the same one it already prints —
/// subtracted from the raw wall-clock gap before it is ever compared
/// against the lease, floored at zero: a host that slept through most of
/// the gap between `last_seen` and `now` must never, by itself, turn
/// healthy unfinished work into a durable `failed` record. A caller with
/// no sleep total of its own to offer (reconcile's own stale-convergence
/// pass) passes `0`, unchanged from before this parameter existed.
///
/// A task whose `result.md` already exists is still recorded once its
/// lease has passed too (card `20260929-b-fbd213`/`20261013-b-623eb5`,
/// quoted in full on the Python side): recording `failed` over real
/// findings would otherwise leave the task AWAITING forever, since
/// nothing else ever revisits it. The reason names the file instead of
/// the generic lease sentence, and the file itself is left exactly where
/// it was found — this never touches the branch, worktree, or result file.
///
/// `live_name` is the same pane-liveness probe a caller already took for
/// its own reconcile/reclaim pass — this never takes a probe of its own,
/// so a caller folding this into one sweep never pays for (or risks
/// disagreeing across) a second listing. It answers with the live name at
/// a pane id, or `None` when the pane id is gone (see `is_awaiting`): a
/// reused pane id now hosting a different, non-empty name reads as gone,
/// but a listed pane id whose name could not be read back does not.
#[allow(clippy::too_many_arguments)]
pub fn expire_awaiting(
    root: &Path,
    tasks: &[SweepTask],
    lease_minutes: u32,
    lease_overrides: &HashMap<String, u32>,
    now: OffsetDateTime,
    sleep_minutes: i64,
    apply: bool,
    live_name: &impl Fn(&str) -> Option<String>,
) -> Vec<LeaseExpiry> {
    let mut expiries = Vec::new();
    for task in tasks {
        if !is_awaiting(task, live_name) {
            continue;
        }
        let effective_lease = task
            .account
            .as_deref()
            .and_then(|account| lease_overrides.get(account))
            .copied()
            .unwrap_or(lease_minutes);
        let Some(last_seen) = task.started_at.or(task.dispatched_at) else {
            continue;
        };
        let elapsed = now - last_seen;
        let effective_elapsed =
            (elapsed - time::Duration::minutes(sleep_minutes)).max(time::Duration::ZERO);
        if effective_elapsed < time::Duration::minutes(i64::from(effective_lease)) {
            continue;
        }

        let lane = lane_task_id(task);
        let state = if let Some(path) = awaiting_result_path(root, &task.task_id, lane) {
            format!(
                "abandoned: pane gone, partial result kept at {}",
                path.strip_prefix(root).unwrap_or(&path).display()
            )
        } else {
            let (tip, has_unmerged) = branch_evidence(root, lane);
            if has_unmerged {
                format!("has unmerged commits {tip}")
            } else {
                format!(
                    "abandoned: pane gone, no status file, lease expired after {effective_lease}m"
                )
            }
        };

        // The AWAITING classification and branch/result evidence can
        // predate a status write; a status file in the lane worktree or
        // the project tree is a reason to defer, never evidence of
        // failure, even when it does not parse as one of the three
        // terminal tokens (critic-codex-areas.md H2). Re-checked here,
        // together with the append itself, under the project lock when
        // `apply`: the same reentrant `flock` `append_outcome_if_absent`
        // (inside `record_stranded_closure` below) re-enters rather than
        // blocks on, so a status landing in the gap between this check
        // and that append can never race it (critic-codex-areas.md M1).
        let _lock = apply.then(|| ProjectLog::new(root).lock().ok()).flatten();
        if has_current_status_file(root, task) {
            continue;
        }

        let recorded = apply
            && record_stranded_closure(
                root,
                &task.task_id,
                TaskStatus::Failed,
                &state,
                task.pane.as_deref(),
            );
        expiries.push(LeaseExpiry {
            task_id: task.task_id.clone(),
            state,
            recorded,
        });
    }
    expiries
}

/// Render a non-negative elapsed duration the way an operator would say
/// it — Python's `tasks.format_elapsed` (`tasks.py:335-350`): seconds
/// under a minute, minutes under an hour, hours from there, each bucket
/// floored rather than rounded. Used by `sweep.rs`'s reviewer-over-timeout
/// line (critic-sweep-board.md finding 4) to match the Python board's own
/// phrasing.
pub fn format_elapsed(elapsed: time::Duration) -> String {
    let seconds = elapsed.whole_seconds().max(0);
    if seconds < 60 {
        return format!("{seconds} sec");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes} min");
    }
    let hours = minutes / 60;
    format!("{hours} hr")
}

/// One `--reclaimable` candidate: a RECORDED task with a still-open pane,
/// either genuinely reclaimable (its pane is still live) or merely stale
/// bookkeeping (the pane is already gone by some other route).
#[derive(Debug, Clone)]
pub struct ReclaimCandidate {
    /// The recorded task's own id.
    pub task_id: String,
    /// The role this task was dispatched to.
    pub role: String,
    /// The still-open pane this candidate's reclaim would close.
    pub pane: String,
    /// Dispatched agent name (`agent=`), for `--reclaimable --yes`'s
    /// same-occupant confirmation.
    pub agent: Option<String>,
}

/// One pass-local verdict per lane, shared by every recorded occupant.
/// Status is read once per distinct lane rather than once per historical
/// dispatch, matching `runner.reclaimable_panes`'s batched status cache.
fn reclaim_lane_checks(
    root: &Path,
    tasks: &[SweepTask],
    is_live: &impl Fn(&str) -> bool,
    cache: &mut LaneProbeCache,
) -> HashMap<String, Option<bool>> {
    let mut checks = HashMap::new();
    for task in tasks
        .iter()
        .filter(|task| task.recorded && task.pane.is_some())
    {
        if !is_writing_role(&task.role) {
            continue;
        }
        let lane = lane_task_id(task).to_string();
        if checks.contains_key(&lane) {
            continue;
        }
        let verdict = lane_clean_for_reclaim(root, task, tasks, is_live, cache);
        checks.insert(lane, verdict);
    }
    checks
}

/// A clean Git checkout can still contain a live `--into` occupant under
/// gitignored `.multiverse/tasks`. Protect that occupant using the same
/// task and pane snapshots as the caller, including a brief materialized
/// before its dispatch was logged. Unknown filesystem state never grants
/// permission to reclaim a pane.
fn lane_clean_for_reclaim(
    root: &Path,
    task: &SweepTask,
    tasks: &[SweepTask],
    is_live: &impl Fn(&str) -> bool,
    cache: &mut LaneProbeCache,
) -> Option<bool> {
    let lane = lane_task_id(task);
    let lane_root =
        crate::worktrees::guarded_path(root, &Path::new(".multiverse/worktrees").join(lane), true)
            .ok()?;
    if !lane_root.try_exists().ok()? {
        return Some(true);
    }
    if !lane_root.is_dir() || !cached_lane_porcelain(root, lane, cache)?.is_empty() {
        return Some(false);
    }
    let mut occupants = BTreeSet::from([lane.to_string()]);
    occupants.extend(
        tasks
            .iter()
            .filter(|task| lane_task_id(task) == lane)
            .map(|task| task.task_id.clone()),
    );
    let local_tasks =
        crate::worktrees::guarded_path(&lane_root, Path::new(".multiverse/tasks"), true).ok()?;
    if local_tasks.try_exists().ok()? {
        for entry in std::fs::read_dir(local_tasks).ok()? {
            let entry = entry.ok()?;
            if entry.file_type().ok()?.is_dir() {
                occupants.insert(entry.file_name().into_string().ok()?);
            }
        }
    }
    for occupant in occupants {
        let project_paths = crate::paths::TaskPaths::for_task(root, &occupant).ok()?;
        let local_paths = crate::paths::TaskPaths::for_task(&lane_root, &occupant).ok()?;
        if read_status(&project_paths.status())
            .or_else(|| read_status(&local_paths.status()))
            .is_some()
        {
            continue;
        }
        if let Some(occupant_task) = tasks.iter().find(|task| task.task_id == occupant) {
            if occupant_task.pane.as_deref().is_some_and(is_live) {
                return Some(false);
            }
        } else if local_paths.brief().try_exists().ok()? {
            return Some(false);
        }
    }
    Some(true)
}

/// Non-writers need no lane check, but an unrecognized historical role
/// cannot be assumed to have had no worktree.
fn reclaim_lane_is_safe(task: &SweepTask, checks: &HashMap<String, Option<bool>>) -> bool {
    if mv_config::roles::Role::from_any_config_key(&task.role).is_err() {
        return false;
    }
    !is_writing_role(&task.role) || checks.get(lane_task_id(task)) == Some(&Some(true))
}

/// Lanes whose task output has no identical rescued copy in the project.
/// Used by `stop` before destroying pane evidence, even with `--force`.
/// Lane discovery uses the worktree module; artifact comparison mirrors
/// `worktrees.unrescued_artifacts` without performing rescue writes.
/// Kept here with task-output safety so this bounded port does not change
/// the worktree lifecycle module's API.
pub fn lanes_holding_unrescued_output(
    root: &Path,
) -> Result<Vec<String>, crate::worktrees::WorktreeError> {
    use crate::worktrees::{guarded_path, Worktrees};
    let lanes_root = guarded_path(root, Path::new(".multiverse/worktrees"), true)?;
    if !lanes_root.try_exists()? {
        return Ok(Vec::new());
    }
    let mut held = Vec::new();
    for lane in Worktrees::new(root)?.lanes()? {
        let state = guarded_path(&lane.path, Path::new(".multiverse"), true)?;
        if !state.try_exists()? {
            continue;
        }
        let mut artifacts = Vec::new();
        collect_lane_artifacts(&lane.path, Path::new(".multiverse"), &mut artifacts)?;
        // The roster is intentionally committed. If present, read the
        // lane's tracked/modified sets once, never once per artifact.
        let mut clean = HashSet::new();
        if artifacts
            .iter()
            .any(|path| path == Path::new(".multiverse/roster.toml"))
        {
            if let (Some(tracked), Some(modified)) = (
                git_stdout(&lane.path, &["ls-files", "-z", "--", ".multiverse"]),
                git_stdout(
                    &lane.path,
                    &[
                        "diff",
                        "--no-renames",
                        "--name-only",
                        "-z",
                        "HEAD",
                        "--",
                        ".multiverse",
                    ],
                ),
            ) {
                let modified: HashSet<&str> = modified.split('\0').collect();
                clean.extend(
                    tracked
                        .split('\0')
                        .filter(|path| !path.is_empty() && !modified.contains(path))
                        .map(PathBuf::from),
                );
            }
        }
        for artifact in artifacts {
            if clean.contains(&artifact) {
                continue;
            }
            let source = lane_artifact_bytes(&lane.path, &artifact)?;
            match lane_artifact_bytes(root, &artifact) {
                Ok(destination) if source == destination => {}
                Ok(_) => {
                    held.push(lane.task.clone());
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    held.push(lane.task.clone());
                    break;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(held)
}

fn collect_lane_artifacts(
    root: &Path,
    relative: &Path,
    artifacts: &mut Vec<PathBuf>,
) -> std::io::Result<()> {
    let directory =
        crate::worktrees::guarded_path(root, relative, false).map_err(std::io::Error::other)?;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_str(), Some(".gitignore" | "worktrees" | "input")) {
            continue;
        }
        let path = relative.join(name);
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect_lane_artifacts(root, &path, artifacts)?;
        } else if kind.is_file() {
            artifacts.push(path);
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "cannot verify a non-regular lane artifact",
            ));
        }
    }
    Ok(())
}

fn lane_artifact_bytes(root: &Path, relative: &Path) -> std::io::Result<Vec<u8>> {
    let mut reader = mv_core::secure_open::open_regular(root, relative, "lane artifact")?;
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes)?;
    Ok(bytes)
}

/// Every task id `root`'s routing log already has an `event=stale-pane`
/// record for — mirrors `accounts.stale_pane_marked_tasks`.
fn stale_pane_marked_tasks(root: &Path) -> HashSet<String> {
    let mut marked = HashSet::new();
    let log_path = ProjectLog::new(root).path().to_path_buf();
    let Ok(recs) = index_records(&log_path, Selector::Tasks) else {
        return marked;
    };
    for rec in &recs {
        let Ok(parsed) = rec.parse() else { continue };
        if parsed.event() != "stale-pane" {
            continue;
        }
        if let Some(task) = parsed.fields.get("task") {
            marked.insert(task.clone());
        }
    }
    marked
}

/// Appends `task`'s stale-pane record, unless one is already there —
/// mirrors `accounts.append_stale_pane_if_absent`. Returns whether it
/// appended.
pub fn append_stale_pane_if_absent(root: &Path, task: &str, pane: &str) -> bool {
    if stale_pane_marked_tasks(root).contains(task) {
        return false;
    }
    let when = mv_core::time::now_stamp();
    let line = format!("{when}\tevent=stale-pane\ttask={task}\tpane={pane}");
    ProjectLog::new(root).append(&line).is_ok()
}

/// Which of `root`'s RECORDED tasks hold a pane nobody needs anymore
/// (`reclaimable`), and which merely look like they do (`stale` —
/// terminal, recorded, clean lane, but the pane it names is already gone
/// from `agents.list_checked()` by some other route, or now answers to a
/// different agent). Mirrors `runner.reclaimable_panes`'s four conditions,
/// with one status read per lane and a live-unsettled-occupant guard for
/// adopted lanes.
///
/// A failed live listing is a no-op — no task is examined, nothing is
/// reclaimable, and nothing is marked stale — never a fallback to treating
/// the listing as empty. An empty listing reads as "every pane is
/// confirmed gone", which `occupant_matches` would then take as license to
/// call every live-paned task `stale`; collapsing a *failed probe* into
/// that same empty shape would fail open exactly when Herdr itself cannot
/// be asked. Both callers today, `reclaimable_and_report`
/// (`crates/mv-cli/src/commands/tasks.rs`) and `reclaim_stale_panes`
/// (`crates/mv-cli/src/commands/dispatch.rs`), already guard around this
/// before or after calling in here; the guard now lives with the risk
/// too, not only with those call sites.
pub fn reclaimable_panes(
    root: &Path,
    agents: &mut impl AgentPanes,
) -> (Vec<ReclaimCandidate>, Vec<ReclaimCandidate>) {
    let Some(live) = agents.list_checked() else {
        return (Vec::new(), Vec::new());
    };
    let tasks = sweep_tasks(root);
    let is_live = |pane: &str| live.iter().any(|agent| agent.pane_id == pane);
    // A pane id is reusable — after a Herdr restart, a finished task's old
    // pane id can belong to an unrelated live agent. Matching by pane id
    // alone (the bug behind card 20261108-b-37df85) would hand that
    // agent's live pane to this task's reclaim; a pane only counts as this
    // task's own occupant when the live listing's agent name at that pane
    // id still matches what this task was dispatched under. A differing
    // name, or no name at all, is treated exactly like the pane being
    // gone — the same fallthrough to `stale` bookkeeping below.
    let occupant_matches = |pane: &str, agent: Option<&str>| {
        live.iter()
            .find(|current| current.pane_id == pane)
            .is_some_and(|current| current.name.as_deref().unwrap_or("") == agent.unwrap_or(""))
    };
    let already_marked = stale_pane_marked_tasks(root);
    let mut lane_probe_cache = LaneProbeCache::new();
    let lane_checks = reclaim_lane_checks(root, &tasks, &is_live, &mut lane_probe_cache);

    let mut reclaimable = Vec::new();
    let mut stale = Vec::new();
    for task in &tasks {
        if !task.recorded {
            continue;
        }
        let Some(pane) = task.pane.clone() else {
            continue;
        };
        if !reclaim_lane_is_safe(task, &lane_checks) {
            continue;
        }
        let candidate = ReclaimCandidate {
            task_id: task.task_id.clone(),
            role: task.role.clone(),
            pane: pane.clone(),
            agent: task.agent.clone(),
        };
        if !occupant_matches(&pane, task.agent.as_deref()) {
            if !already_marked.contains(&task.task_id) {
                stale.push(candidate);
            }
            continue;
        }
        // Card 20261106-b-9d70c7: same pane id + same dispatched agent
        // name, even both still live, is not durable identity across a
        // Herdr restart that reissues this exact deterministic name into
        // a recycled pane (`unfinished_agent_names` is empty in this
        // port; uniqueness at mint time is only the live `agent list`).
        // The pane's own label (always ending in the dispatching task's
        // id, `runner::pane_rename`) and, for a task with its own
        // worktree, the live cwd (`agent get`) are both dispatch-unique
        // in a way the bare name match above is not — require both
        // before trusting this occupant as the task's own, exactly like
        // `supersede_collision_predecessor`'s close-time gate. A task
        // with no recorded `agent=` at all (a pre-naming-era dispatch
        // line) has no name to reissue in the first place, so there is
        // nothing for this gate to add over the existing nameless match —
        // skip it rather than spend an extra Herdr round trip proving a
        // race that cannot exist for this occupant.
        let (label_ok, cwd_ok) = if task.agent.is_none() {
            (true, true)
        } else {
            let label_ok = label_names_task(agents.pane_label(&pane).as_deref(), &task.task_id);
            let lane_root = root.join(".multiverse/worktrees").join(lane_task_id(task));
            let cwd_ok = !lane_root.is_dir()
                || agents
                    .confirm_live(&pane, task.agent.as_deref().unwrap_or(""))
                    .is_some_and(|confirmed| {
                        cwd_confirms_lane(confirmed.cwd.as_deref(), &lane_root)
                    });
            (label_ok, cwd_ok)
        };
        if label_ok && cwd_ok {
            reclaimable.push(candidate);
        } else if !already_marked.contains(&task.task_id) {
            stale.push(candidate);
        }
    }
    (reclaimable, stale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Removed on drop, including on panic (card `20261115-b-3e0fde`: the
    /// plain `PathBuf` this replaced only cleaned up on each test's own
    /// trailing `remove_dir_all` call, so an earlier assertion failure left
    /// it behind in `TMPDIR`). `Deref`s to `Path` so every existing call
    /// site keeps reading `&dir`/`dir.join(...)` unchanged.
    struct TempDir(PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir(tag: &str) -> TempDir {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mv-state-tasks-test-{tag}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write_status(path: &Path, contents: &str) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
    }

    #[test]
    fn task_status_values_match_the_spec() {
        let values: std::collections::BTreeSet<&str> =
            TaskStatus::ALL.iter().map(|s| s.as_str()).collect();
        assert_eq!(
            values,
            std::collections::BTreeSet::from(["done", "blocked", "failed"])
        );
    }

    #[test]
    fn absence_reads_as_still_running() {
        let dir = temp_dir("absent");
        assert_eq!(read_status(&dir.join("status")), None);
    }

    #[test]
    fn reads_each_terminal_status_whitespace_and_case_insensitively() {
        let dir = temp_dir("terminal");
        let path = dir.join("status");

        write_status(&path, "Done\n");
        assert_eq!(read_status(&path), Some(TaskStatus::Done));

        write_status(&path, "blocked");
        assert_eq!(read_status(&path), Some(TaskStatus::Blocked));

        write_status(&path, "  FAILED  \n");
        assert_eq!(read_status(&path), Some(TaskStatus::Failed));
    }

    #[test]
    fn anything_outside_the_three_tokens_reads_as_absent() {
        let dir = temp_dir("outside");
        let path = dir.join("status");

        write_status(&path, "complete");
        assert_eq!(read_status(&path), None);

        write_status(&path, "");
        assert_eq!(read_status(&path), None);
    }

    #[test]
    fn status_path_under_rejects_a_traversal_shaped_task_id_with_nothing_written() {
        let dir = temp_dir("status-path-traversal");
        assert_eq!(status_path_under(&dir, "../x"), None);
        assert!(!dir.join("x").join("status").exists());
        assert!(status_path_under(&dir, "20261101-fixer-aaaaaa").is_some());
    }

    #[test]
    fn a_status_file_older_than_the_run_s_start_reads_as_absent_not_terminal() {
        let dir = temp_dir("since-stale");
        let path = dir.join("status");
        write_status(&path, "done\n");

        let run_started_at = SystemTime::now() + std::time::Duration::from_secs(10);
        assert_eq!(
            read_status_since(&path, run_started_at),
            None,
            "a same-id redispatch must not inherit the previous run's done"
        );

        let run_started_before_the_write = SystemTime::now() - std::time::Duration::from_secs(10);
        assert_eq!(
            read_status_since(&path, run_started_before_the_write),
            Some(TaskStatus::Done)
        );
    }

    #[test]
    fn status_values_markdown_lists_all_three_backticked_in_order() {
        assert_eq!(status_values_markdown(), "`done`, `blocked`, `failed`");
    }

    #[test]
    fn verbose_read_tells_absent_from_unrecognized_from_terminal() {
        let dir = temp_dir("verbose");
        let path = dir.join("status");

        assert_eq!(read_status_verbose(&path), StatusRead::Absent);

        write_status(&path, "donedone");
        assert_eq!(
            read_status_verbose(&path),
            StatusRead::Unrecognized("donedone".to_string())
        );

        write_status(&path, "  Done \n");
        assert_eq!(
            read_status_verbose(&path),
            StatusRead::Terminal(TaskStatus::Done)
        );
    }

    fn write_result(root: &Path, task: &str, status: &str, state: &str) {
        ProjectLog::new(root)
            .append(&format!(
                "2026-11-01T00:00:00+00:00\tevent=result\ttask={task}\tstatus={status}\t\
                 state={state}\trevisions=0"
            ))
            .unwrap();
    }

    #[test]
    fn outcome_targets_finds_a_result_line_by_its_raw_ordinal() {
        let dir = temp_dir("outcome-targets");
        write_result(&dir, "20261101-fixer-aaaaaa", "done", "merged");
        let targets = outcome_targets(&dir, "20261101-fixer-aaaaaa");
        assert_eq!(targets.targetable, BTreeSet::from([1]));
        assert!(targets.applied.is_empty());
        assert_eq!(
            targets.when_by_ordinal.get(&1).unwrap(),
            "2026-11-01T00:00:00+00:00"
        );
    }

    #[test]
    fn validate_log_field_rejects_every_injection_shape_and_nothing_else() {
        for bad in [
            "a\tstatus=X",
            "a\rstatus=X",
            "a\nstatus=X",
            "a=b",
            " leading",
            "trailing ",
        ] {
            assert!(
                validate_log_field("--state", bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        for ok in ["merged", "reconciled-by-merge", ""] {
            assert!(
                validate_log_field("--state", ok).is_ok(),
                "{ok:?} should be accepted"
            );
        }
    }

    #[test]
    fn supersede_rejects_a_tab_in_state_before_writing_anything() {
        let dir = temp_dir("supersede-tab-state");
        write_result(&dir, "20261101-fixer-tabstat", "done", "merged");
        let log_path = ProjectLog::new(&dir).path().to_path_buf();
        let before = std::fs::read(&log_path).unwrap();
        let err = append_supersede_if_absent(
            &dir,
            "20261101-fixer-tabstat",
            1,
            "blocked",
            "a\tstatus=X",
            "corrected after review",
            0,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SupersedeError::InvalidField(_)));
        let after = std::fs::read(&log_path).unwrap();
        assert_eq!(
            before, after,
            "a rejected field must leave the log byte-unchanged"
        );
    }

    #[test]
    fn acknowledge_rejects_a_tab_in_task_before_writing_anything() {
        let dir = temp_dir("acknowledge-tab-task");
        write_result(&dir, "20261101-fixer-tabtask", "done", "merged");
        let log_path = ProjectLog::new(&dir).path().to_path_buf();
        let before = std::fs::read(&log_path).unwrap();
        let err = append_acknowledge_if_absent(&dir, "20261101-fixer-tabtask\tstatus=X", 1, None)
            .unwrap_err();
        assert!(matches!(err, AcknowledgeError::InvalidField(_)));
        let after = std::fs::read(&log_path).unwrap();
        assert_eq!(
            before, after,
            "a rejected field must leave the log byte-unchanged"
        );
    }

    #[test]
    fn supersede_requires_a_nonempty_reason() {
        let dir = temp_dir("supersede-reason");
        write_result(&dir, "20261101-fixer-bbbbbb", "done", "merged");
        let err = append_supersede_if_absent(
            &dir,
            "20261101-fixer-bbbbbb",
            1,
            "blocked",
            "reconciled-by-merge",
            "",
            0,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SupersedeError::MissingReason { .. }));
    }

    #[test]
    fn supersede_refuses_an_ordinal_that_names_no_outcome_record() {
        let dir = temp_dir("supersede-unknown");
        write_result(&dir, "20261101-fixer-cccccc", "done", "merged");
        let err = append_supersede_if_absent(
            &dir,
            "20261101-fixer-cccccc",
            99,
            "blocked",
            "reconciled-by-merge",
            "corrected after review",
            0,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SupersedeError::UnknownTarget { .. }));
    }

    #[test]
    fn supersede_appends_once_and_is_idempotent_for_the_same_verdict() {
        let dir = temp_dir("supersede-append");
        write_result(&dir, "20261101-fixer-dddddd", "done", "merged");
        let appended = append_supersede_if_absent(
            &dir,
            "20261101-fixer-dddddd",
            1,
            "blocked",
            "reconciled-by-merge",
            "the merge never happened",
            2,
            None,
        )
        .unwrap();
        assert!(appended);
        let targets = outcome_targets(&dir, "20261101-fixer-dddddd");
        assert_eq!(
            targets.applied.get(&1).unwrap(),
            &("blocked".to_string(), "reconciled-by-merge".to_string())
        );

        let appended_again = append_supersede_if_absent(
            &dir,
            "20261101-fixer-dddddd",
            1,
            "blocked",
            "reconciled-by-merge",
            "the merge never happened",
            2,
            None,
        )
        .unwrap();
        assert!(!appended_again, "the same correction twice appends once");
    }

    #[test]
    fn supersede_refuses_a_conflicting_second_verdict_for_the_same_target() {
        let dir = temp_dir("supersede-conflict");
        write_result(&dir, "20261101-fixer-eeeeee", "done", "merged");
        append_supersede_if_absent(
            &dir,
            "20261101-fixer-eeeeee",
            1,
            "blocked",
            "reconciled-by-merge",
            "the merge never happened",
            0,
            None,
        )
        .unwrap();

        let err = append_supersede_if_absent(
            &dir,
            "20261101-fixer-eeeeee",
            1,
            "failed",
            "abandoned-empty-lane",
            "a different verdict",
            0,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, SupersedeError::Conflicting(_)));
    }

    #[test]
    fn acknowledge_refuses_an_unknown_ordinal_and_appends_once() {
        let dir = temp_dir("acknowledge");
        write_result(&dir, "20261101-fixer-ffffff", "done", "merged");

        let err =
            append_acknowledge_if_absent(&dir, "20261101-fixer-ffffff", 99, None).unwrap_err();
        assert!(matches!(err, AcknowledgeError::UnknownTarget { .. }));

        let appended =
            append_acknowledge_if_absent(&dir, "20261101-fixer-ffffff", 1, None).unwrap();
        assert!(appended);
        let appended_again =
            append_acknowledge_if_absent(&dir, "20261101-fixer-ffffff", 1, None).unwrap();
        assert!(!appended_again, "acknowledging twice appends once");
    }

    #[test]
    fn current_outcome_fields_reads_the_raw_line_back() {
        let dir = temp_dir("current-outcome");
        write_result(&dir, "20261101-fixer-012345", "done", "merged");
        assert_eq!(
            current_outcome_fields(&dir, 1),
            ("done".to_string(), "merged".to_string())
        );
        assert_eq!(
            current_outcome_fields(&dir, 99),
            ("?".to_string(), "?".to_string())
        );
    }

    /// Scripted in-memory stand-in for [`HerdrAgents`], with the same
    /// strict-call-count posture `conformance/fake_herdr.py` enforces on
    /// the Rust binary end to end — these tests assert `list_calls`
    /// directly so a regression that adds or drops a live-pane probe
    /// fails here, not only in the slower conformance run.
    #[derive(Clone)]
    struct FakeAgent {
        pane_id: String,
        name: Option<String>,
        status: Option<AgentState>,
    }

    struct FakeAgents {
        panes: Vec<FakeAgent>,
        list_calls: usize,
        closed: Vec<String>,
        /// Scripts a probe failure (finding 2): `list_checked` returns
        /// `None` the next `fail_next_probes` times it is called, each
        /// one still counted in `list_calls` (a failed probe is still a
        /// probe), then resumes answering from `panes`.
        fail_next_probes: usize,
        /// `pane get`'s own `label`, by pane id — card `20261106-b-
        /// 9d70c7`'s occupancy gate. Absent (the default) models a fake
        /// that was never told to answer this probe, same posture as the
        /// trait default: `pane_label` returns `None`, not an empty
        /// label.
        labels: HashMap<String, String>,
        /// `agent get`'s own `cwd`, by pane id — the gate's second
        /// witness, read through `confirm_live`.
        cwds: HashMap<String, PathBuf>,
    }

    impl FakeAgents {
        /// Plain pane ids with no known agent name/status — sufficient for
        /// every reconcile-only test, and for a reclaim candidate this
        /// pass means to exclude before idle/done ever matters (unknown
        /// status never confirms a reclaim, by construction).
        fn new(panes: &[&str]) -> Self {
            Self {
                panes: panes
                    .iter()
                    .map(|pane_id| FakeAgent {
                        pane_id: pane_id.to_string(),
                        name: None,
                        status: None,
                    })
                    .collect(),
                list_calls: 0,
                closed: Vec::new(),
                fail_next_probes: 0,
                labels: HashMap::new(),
                cwds: HashMap::new(),
            }
        }

        /// `(pane, agent name)` pairs, each reported idle — the shape a
        /// genuine reclaim confirmation needs from both listings.
        fn new_idle(entries: &[(&str, &str)]) -> Self {
            Self {
                panes: entries
                    .iter()
                    .map(|(pane_id, name)| FakeAgent {
                        pane_id: pane_id.to_string(),
                        name: Some(name.to_string()),
                        status: Some(AgentState::Idle),
                    })
                    .collect(),
                list_calls: 0,
                closed: Vec::new(),
                fail_next_probes: 0,
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
                self.panes.push(FakeAgent {
                    pane_id: pane_id.to_string(),
                    name: None,
                    status: Some(status),
                });
            }
            self
        }

        fn with_label(mut self, pane_id: &str, label: &str) -> Self {
            self.labels.insert(pane_id.to_string(), label.to_string());
            self
        }

        fn with_cwd(mut self, pane_id: &str, cwd: impl Into<PathBuf>) -> Self {
            self.cwds.insert(pane_id.to_string(), cwd.into());
            self
        }
    }

    impl AgentPanes for FakeAgents {
        fn list(&mut self) -> Vec<LiveAgent> {
            self.list_calls += 1;
            self.panes
                .iter()
                .map(|agent| LiveAgent {
                    pane_id: agent.pane_id.clone(),
                })
                .collect()
        }
        fn close(&mut self, pane_id: &str) -> bool {
            self.closed.push(pane_id.to_string());
            self.panes.retain(|agent| agent.pane_id != pane_id);
            true
        }
        fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
            self.list_calls += 1;
            if self.fail_next_probes > 0 {
                self.fail_next_probes -= 1;
                return None;
            }
            Some(
                self.panes
                    .iter()
                    .map(|agent| LiveAgentStatus {
                        pane_id: agent.pane_id.clone(),
                        name: agent.name.clone(),
                        status: agent.status,
                    })
                    .collect(),
            )
        }
        fn pane_label(&mut self, pane_id: &str) -> Option<String> {
            self.labels.get(pane_id).cloned()
        }
        fn confirm_live(&mut self, pane_id: &str, name: &str) -> Option<LiveConfirmation> {
            let agent = self.panes.iter().find(|agent| {
                agent.pane_id == pane_id && agent.name.as_deref().unwrap_or("") == name
            })?;
            Some(LiveConfirmation {
                ready: agent.status.is_some_and(AgentState::is_ready_for_input),
                state_change_seq: None,
                cwd: self.cwds.get(pane_id).cloned(),
                terminal_id: None,
            })
        }
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    const UNRECORDED_LOG: &str = "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tbranch=main\n\
         2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n";

    #[test]
    fn sweep_project_finds_one_unrecorded_task_among_many_already_recorded() {
        // Sequential last-write-wins filtering must still pick the one
        // settled-but-unrecorded task out of a crowd of already-recorded
        // ones, in log order — the same result a thread-per-task join
        // produced, without the spawn cost.
        let root = temp_dir("sweep-many-recorded");
        let mut log = String::new();
        for i in 0..64u32 {
            let id = format!("20250101-fixer-{i:06x}");
            log.push_str(&format!(
                "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask={id}\trole=fixer\taccount=seat\tbranch=main\n\
                 2020-01-01T00:01:00+00:00\tevent=result\ttask={id}\tstatus=done\tstate=ok\trevisions=0\n"
            ));
        }
        log.push_str(UNRECORDED_LOG);
        write(&root.join(".multiverse/routing.log"), &log);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, true, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.in_progress_before, 1);
        assert_eq!(report.recorded, 1);
        assert_eq!(report.reclaimed, 0);
    }

    #[test]
    fn sweep_project_records_a_done_task_whose_pane_is_still_live_but_idle() {
        // Python's `tasks.reconcile` skips a candidate only when its pane
        // reads exactly `herdr.AgentState.WORKING` — "a `running` task is
        // left alone" — never a merely-live idle/done one Herdr has not
        // yet dropped from its listing. `is_reconcile_candidate` used to
        // skip any live pane at all, so a dispatch whose agent finished
        // but whose pane Herdr still reports (not yet closed) was left
        // UNRECORDED every sweep pass, never reconciled until the pane
        // disappeared entirely.
        let root = temp_dir("sweep-done-but-still-live");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Done);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.recorded, 1);
    }

    #[test]
    fn sweep_project_reclaims_a_task_it_just_reconciled_in_the_same_pass() {
        // Card `20261106-b-6b0385`: Python's `_reconcile_and_reclaim` calls
        // `reclaimable_panes` on the exact same `found` list `tasklog.
        // reconcile` just mutated `task.state` on, so a task recorded this
        // pass is reclaimed this same pass, not the next one.
        // `sweep_project_with_live` used to build its candidates from the
        // pre-record snapshot, leaving a just-recorded task's pane open for
        // a whole extra sweep pass.
        let root = temp_dir("sweep-reclaims-same-pass");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );
        let mut agents = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Done);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.recorded, 1);
        assert_eq!(
            report.reclaimed, 1,
            "must reclaim the pane in this same pass"
        );
        assert_eq!(agents.closed, vec!["w1:p1".to_string()]);
    }

    #[test]
    fn dry_run_previews_a_stale_unrecorded_task_without_writing_or_reclaiming() {
        let root = temp_dir("sweep-dry-run");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, true, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report,
            SweepReport {
                in_progress_before: 1,
                in_progress_after: 1,
                recorded: 1,
                reclaimed: 0,
                expired: 0,
                superseded: Vec::new(),
                expired_tasks: Vec::new(),
                awaiting_gone: Vec::new(),
                awaiting_partial: Vec::new(),
                dirty_details: BTreeMap::new(),
            }
        );
        assert_eq!(agents.list_calls, 1);
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log.lines().count(),
            2,
            "a dry run must leave the routing log exactly as it was"
        );
    }

    #[test]
    fn reconcile_records_a_settled_unrecorded_tasks_outcome() {
        let root = temp_dir("sweep-reconcile");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report,
            SweepReport {
                in_progress_before: 1,
                in_progress_after: 0,
                recorded: 1,
                reclaimed: 0,
                expired: 0,
                superseded: Vec::new(),
                expired_tasks: Vec::new(),
                awaiting_gone: Vec::new(),
                awaiting_partial: Vec::new(),
                dirty_details: BTreeMap::new(),
            }
        );
        assert_eq!(
            agents.list_calls, 1,
            "nothing was recorded yet to reclaim, so reclaim must never re-probe"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        let last = log.lines().last().unwrap();
        assert!(last.ends_with(
            "event=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=reconciled\trevisions=0\tpane=w1:p1"
        ));
    }

    #[test]
    fn a_live_pane_still_running_is_left_alone_not_reconciled() {
        let root = temp_dir("sweep-live-pane");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );

        let mut agents = FakeAgents::new(&["w1:p1"]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.recorded, 0, "a pane still live has not settled yet");
        assert_eq!(report.in_progress_after, 1);
    }

    #[test]
    fn a_loose_file_directly_under_tasks_dir_is_never_counted_as_a_task() {
        // Card 20261102-b-1b2d80: a large real project
        // had 34 stray `<id>-result.md` files sitting directly under
        // `.multiverse/tasks/` rather than inside their own `<id>/`
        // directory. `sweep_tasks` used to list that directory verbatim and
        // treat any entry matching the generic task-id pattern as a task —
        // which a loose file's own name does, since dots and hyphens are
        // both legal — inflating `sweep --dry-run`'s "in progress" count by
        // one for every such file even though no dispatch for it ever
        // existed. The fix sources the task universe from the routing log
        // instead, exactly as Python's `read_log` does, so a loose file
        // with no log entry of its own is invisible to this reader too.
        let root = temp_dir("sweep-loose-file");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );
        // The stray file: same directory level as the real task dir above,
        // and its own name alone still matches the generic task-id pattern.
        write(
            &root.join(".multiverse/tasks/20260101-fixer-ffffff-result.md"),
            "leftover content from some earlier mistake\n",
        );

        let tasks = sweep_tasks(&root);
        assert_eq!(
            tasks.len(),
            1,
            "the loose file must never be read back as a second task"
        );
        assert_eq!(tasks[0].task_id, "20250101-fixer-aaaaaa");
    }

    #[test]
    fn a_task_directory_predating_the_routing_log_is_never_counted() {
        // Card 20261102-b-1b2d80's second shape (seen on another real project): a task
        // directory whose dispatch predates the routing log's earliest
        // surviving entry (the log was rotated or started fresh at some
        // point; the task directory outlived it). Python's `read_log`
        // only ever learns about a task from the log itself, so this task
        // simply never appears in its `tasks` list — it is not merely
        // excluded, it does not exist from that reader's point of view.
        // `sweep_tasks` must agree: a directory with no log footprint at
        // all is not a task this build can speak to either.
        let root = temp_dir("sweep-pre-log-dir");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-aaaaaa\tbranch=\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-explorer-aaaaaa\tstatus=done\tstate=done\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-explorer-999999/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-explorer-999999/result.md"),
            "# Result\nfrom before this log existed.\n",
        );

        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1, "only the logged task is a task at all");
        assert_eq!(tasks[0].task_id, "20260101-explorer-aaaaaa");
    }

    #[test]
    fn a_result_line_with_status_none_is_never_treated_as_recorded() {
        // `read_log` (tasks.py) skips an `event=result` whose `status=none`
        // entirely — a runner bug once wrote it for a turn that only
        // looked settled, and `TaskStatus` has no such value, so it was
        // never a real outcome. A task with only that line must still read
        // as unrecorded, not quietly marked done.
        let root = temp_dir("sweep-status-none");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-reviewer-aaaaaa\tbranch=\trole=reviewer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-reviewer-aaaaaa\tstatus=none\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1);
        assert!(
            !tasks[0].recorded,
            "a bogus status=none result must not count as a recorded outcome"
        );
    }

    #[test]
    fn a_redispatch_reusing_a_task_id_does_not_inherit_the_earlier_rounds_pane_or_outcome() {
        // `read_log`'s dispatch handling resets a reused task id's state —
        // a fix round sharing the same id must never read as already
        // RECORDED from the previous round's own result line, nor carry
        // that round's now-closed pane forward.
        let root = temp_dir("sweep-redispatch");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-fixer-aaaaaa\tpane=w1:p1\n\
             2026-01-01T00:02:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n\
             2026-01-02T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n\
             2026-01-02T00:01:00+00:00\tevent=started\ttask=20260101-fixer-aaaaaa\tpane=w2:p2\n",
        );

        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1);
        assert!(
            !tasks[0].recorded,
            "the new round has no result of its own yet"
        );
        assert_eq!(tasks[0].pane.as_deref(), Some("w2:p2"));
    }

    #[test]
    fn a_done_writing_role_with_no_usable_result_md_is_left_unrecorded() {
        // `_usable_result`: a `done` outcome needs a readable, non-empty
        // `result.md` before `reconcile` will record it.
        let root = temp_dir("sweep-no-result-md");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        // No result.md written at all.

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report.recorded, 0,
            "a done outcome with no usable result.md must not be recorded"
        );
        assert_eq!(report.in_progress_after, 1);
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
    fn a_done_outcome_from_a_dirty_writing_lane_is_left_unrecorded() {
        // `_dirty_done`/`_dirty_lane_detail`: a writing role's `done`
        // outcome is refused when its own worktree still holds uncommitted
        // work — the status file and the tree disagree, and only the tree
        // can be believed.
        let root = temp_dir("sweep-dirty-lane");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );
        let lane = root.join(".multiverse/worktrees/20250101-fixer-aaaaaa");
        std::fs::create_dir_all(&lane).unwrap();
        git(&lane, &["init", "-q", "-b", "main"]);
        git(&lane, &["config", "user.email", "lane@example.invalid"]);
        git(&lane, &["config", "user.name", "Lane Test"]);
        std::fs::write(lane.join("uncommitted.txt"), "dirty\n").unwrap();

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report.recorded, 0,
            "a dirty writing lane must never be recorded done"
        );
        assert_eq!(report.in_progress_after, 1);
        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1);
        let detail = dirty_done_detail(&root, &tasks[0]).expect("dirty-done detail");
        assert!(
            detail.contains("uncommitted.txt"),
            "detail should name the dirty path, got {detail}"
        );
    }

    fn init_git_lane(lane: &Path) {
        std::fs::create_dir_all(lane).unwrap();
        git(lane, &["init", "-q", "-b", "main"]);
        git(lane, &["config", "user.email", "lane@example.invalid"]);
        git(lane, &["config", "user.name", "Lane Test"]);
    }

    /// Card `20261115-b-ca59eb`: `dirty_done_detail` used to call
    /// `lane_is_dirty` (one `git status --porcelain`) and then
    /// `lane_porcelain` again for the same worktree, doubling the spawn
    /// for a single public call. The one-shot cache collapses that to one.
    #[test]
    fn dirty_done_detail_probes_a_lane_once() {
        let root = temp_dir("dirty-done-one-probe");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask=20261102-builder-dirtyaa\trole=builder\tbranch=multiverse/20261102-builder-dirtyaa\n",
        );
        write(
            &root.join(".multiverse/tasks/20261102-builder-dirtyaa/status"),
            "done",
        );
        let lane = root.join(".multiverse/worktrees/20261102-builder-dirtyaa");
        init_git_lane(&lane);
        std::fs::write(lane.join("uncommitted.txt"), "dirty\n").unwrap();

        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1);
        reset_lane_git_probe_count();
        let detail = dirty_done_detail(&root, &tasks[0]).expect("dirty-done detail");
        assert!(
            detail.contains("uncommitted.txt"),
            "detail should name the dirty path, got {detail}"
        );
        assert_eq!(
            lane_git_probe_count(),
            1,
            "dirty_done_detail must not re-probe the same lane for the detail text"
        );
    }

    /// Card `20261115-b-1ba758`: `is_reconcile_candidate`'s own `lane_is_dirty`
    /// check already spawns `git status --porcelain` once for a done-but-dirty
    /// writing lane; `commands::sweep::dirty_done_notes` used to call
    /// `dirty_done_detail` again for the exact same task afterward, with a
    /// fresh, empty cache of its own, spawning a second `git status
    /// --porcelain` for the same worktree every pass. `SweepReport::
    /// dirty_details` now carries that detail text straight off the pass's
    /// own `LaneProbeCache`, so the one `git` call above already covers
    /// both — two before this fix, one after.
    #[test]
    fn sweep_report_carries_dirty_detail_from_the_passs_own_probe_not_a_second_one() {
        let root = temp_dir("sweep-dirty-details-one-probe");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask=20261102-builder-dirtyaa\trole=builder\tbranch=multiverse/20261102-builder-dirtyaa\n",
        );
        write(
            &root.join(".multiverse/tasks/20261102-builder-dirtyaa/status"),
            "done",
        );
        let lane = root.join(".multiverse/worktrees/20261102-builder-dirtyaa");
        init_git_lane(&lane);
        std::fs::write(lane.join("uncommitted.txt"), "dirty\n").unwrap();

        reset_lane_git_probe_count();
        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);

        assert_eq!(
            report.recorded, 0,
            "a dirty writing lane must never be recorded done"
        );
        let detail = report
            .dirty_details
            .get("20261102-builder-dirtyaa")
            .expect("dirty_details must carry this task's own detail");
        assert!(
            detail.contains("uncommitted.txt"),
            "detail should name the dirty path, got {detail}"
        );
        assert_eq!(
            lane_git_probe_count(),
            1,
            "one sweep pass must spawn git for this lane once, not once for \
             reconcile's own dirty check and again for the report's detail text"
        );
    }

    /// Card `20261115-b-ca59eb`: a lane revisited by a later `--into`
    /// dispatch (two task ids, one branch) used to cost one
    /// `git status --porcelain` spawn per task id reaching
    /// `is_reconcile_candidate`, even though both ask about the identical
    /// worktree directory in the same sweep pass. `lane_probe_cache`
    /// collapses that to one spawn per pass, mirroring `board_data`.
    #[test]
    fn a_lane_shared_by_two_unrecorded_done_tasks_is_probed_once() {
        let root = temp_dir("shared-lane-one-probe");
        let lane_id = "20261102-builder-shared";
        let first = lane_id.to_string();
        let second = "20261103-builder-followup".to_string();
        init_git_lane(&root.join(".multiverse/worktrees").join(lane_id));
        for task_id in [&first, &second] {
            write(
                &root.join(".multiverse/tasks").join(task_id).join("status"),
                "done",
            );
            write(
                &root
                    .join(".multiverse/tasks")
                    .join(task_id)
                    .join("result.md"),
                "# Result\ndone.\n",
            );
        }
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={first}\trole=builder\tbranch=multiverse/{lane_id}\n\
                 2026-11-03T00:00:00+00:00\tevent=dispatch\ttask={second}\trole=builder\tbranch=multiverse/{lane_id}\n"
            ),
        );

        reset_lane_git_probe_count();
        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.recorded, 2, "both unrecorded done tasks settle");
        assert_eq!(
            lane_git_probe_count(),
            1,
            "both task ids name the same lane; it should be probed once per pass"
        );
    }

    /// Card `20261115-b-ca59eb`: a writing-role task this pass just
    /// recorded is then considered for reclaim in the same
    /// `sweep_project_with_live` call. Reclaim's porcelain check must
    /// reuse the spawn reconcile already made, not re-ask git for a
    /// directory this pass already read.
    #[test]
    fn a_just_reconciled_lane_is_not_reprobed_for_reclaim() {
        let root = temp_dir("reconcile-reclaim-one-probe");
        let task_id = "20261102-builder-aaaaaa";
        init_git_lane(&root.join(".multiverse/worktrees").join(task_id));
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\tagent=x-buil-cl\tbranch=multiverse/{task_id}\n\
                 2026-11-02T00:01:00+00:00\tevent=started\ttask={task_id}\tpane=w1:p1\n"
            ),
        );
        write(
            &root.join(".multiverse/tasks").join(task_id).join("status"),
            "done",
        );
        write(
            &root
                .join(".multiverse/tasks")
                .join(task_id)
                .join("result.md"),
            "# Result\ndone.\n",
        );

        reset_lane_git_probe_count();
        let mut agents = FakeAgents::new_idle(&[("w1:p1", "x-buil-cl")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.recorded, 1);
        assert_eq!(
            report.reclaimed, 1,
            "must reclaim the pane in this same pass"
        );
        assert_eq!(
            lane_git_probe_count(),
            1,
            "reclaim must reuse reconcile's porcelain, not spawn git again"
        );
    }

    #[test]
    fn reclaim_closes_a_clean_recorded_tasks_still_open_pane() {
        let root = temp_dir("sweep-reclaim");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-fixer-1\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );

        // Idle, same agent name on both listings — exactly what
        // `_confirm_reclaim_candidates` requires before a close.
        let mut agents = FakeAgents::new_idle(&[("w1:p1", "mv-fixer-1")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report,
            SweepReport {
                in_progress_before: 0,
                in_progress_after: 0,
                recorded: 0,
                reclaimed: 1,
                expired: 0,
                superseded: Vec::new(),
                expired_tasks: Vec::new(),
                awaiting_gone: Vec::new(),
                awaiting_partial: Vec::new(),
                dirty_details: BTreeMap::new(),
            }
        );
        assert_eq!(
            agents.list_calls, 2,
            "reclaim must re-probe liveness immediately before closing"
        );
        assert_eq!(agents.closed, vec!["w1:p1".to_string()]);
    }

    #[test]
    fn a_working_pane_is_never_reclaimed_even_though_its_lane_is_clean() {
        // Finding 3 (critic-sweep-board.md): a recorded reviewer/scout
        // dispatched to `main` used to close as soon as its pane id was
        // still listed at all, including WORKING — idle/done on the
        // *first* listing is now a separate, mandatory gate before a
        // second listing is ever taken.
        let root = temp_dir("sweep-working-pane");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-reviewer-aaaaaa\tproject=sandbox\trole=reviewer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-reviewer-1\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-reviewer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-reviewer-aaaaaa\tstatus=blocked\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents {
            panes: vec![FakeAgent {
                pane_id: "w1:p1".to_string(),
                name: Some("mv-reviewer-1".to_string()),
                status: Some(AgentState::Working),
            }],
            list_calls: 0,
            closed: Vec::new(),
            fail_next_probes: 0,
            labels: HashMap::new(),
            cwds: HashMap::new(),
        };
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.reclaimed, 0);
        assert_eq!(
            agents.list_calls, 1,
            "not idle/done on the first listing already excludes it; no re-probe"
        );
    }

    #[test]
    fn a_pane_reused_by_a_different_agent_by_the_second_listing_is_never_reclaimed() {
        // `_confirm_reclaim_candidates`'s same-agent-name check: the pane
        // id alone proves nothing once a second agent could have taken
        // it between listings.
        let root = temp_dir("sweep-reused-pane");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-fixer-1\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );

        struct Rotating {
            calls: usize,
        }
        impl AgentPanes for Rotating {
            fn list(&mut self) -> Vec<LiveAgent> {
                vec![LiveAgent {
                    pane_id: "w1:p1".to_string(),
                }]
            }
            fn close(&mut self, _pane_id: &str) -> bool {
                true
            }
            fn list_checked(&mut self) -> Option<Vec<LiveAgentStatus>> {
                self.calls += 1;
                // First listing: this task's own agent, idle. Second
                // listing: a different agent now occupies the pane id.
                let name = if self.calls == 1 {
                    "mv-fixer-1"
                } else {
                    "mv-fixer-2"
                };
                Some(vec![LiveAgentStatus {
                    pane_id: "w1:p1".to_string(),
                    name: Some(name.to_string()),
                    status: Some(AgentState::Idle),
                }])
            }
        }

        let mut agents = Rotating { calls: 0 };
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.reclaimed, 0, "a different agent now holds the pane");
        assert_eq!(agents.calls, 2);
    }

    #[test]
    fn a_pane_never_held_by_this_tasks_own_dispatched_agent_is_never_reclaimed() {
        // The same-agent-name check compares the live pane's name against
        // the *task's own* dispatched `agent=`, never against whatever
        // the first listing happened to show — a first listing that
        // already disagrees with the dispatch record is not evidence of
        // anything, and two listings merely agreeing with *each other*
        // must not be mistaken for agreeing with the dispatched agent.
        let root = temp_dir("sweep-wrong-agent-both-listings");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-fixer-1\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );

        // Both listings agree with each other, idle, but neither is
        // `mv-fixer-1` — the pane was already someone else's before the
        // first listing ever ran.
        let mut agents = FakeAgents::new_idle(&[("w1:p1", "someone-else")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report.reclaimed, 0,
            "both listings agreeing with each other is not the same as agreeing with the dispatch record"
        );
    }

    #[test]
    fn a_dry_run_previews_the_reclaim_candidate_count_without_closing_anything() {
        // Finding 3: Python's own `apply=False` still returns
        // `reclaimable_panes`'s raw candidate count (before the idle/done
        // confirmation, which only ever gates a real close) — a dry run
        // must preview it too, not report zero unconditionally.
        let root = temp_dir("sweep-dry-run-reclaim");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-fixer-1\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );

        let mut agents = FakeAgents::new_idle(&[("w1:p1", "mv-fixer-1")]);
        let report = sweep_project(&root, true, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.reclaimed, 1, "dry run still previews the candidate");
        assert!(agents.closed.is_empty(), "a dry run never closes a pane");
        assert_eq!(agents.list_calls, 1, "a preview never re-probes to confirm");
    }

    #[test]
    fn a_failed_probe_skips_reconcile_and_reclaim_for_the_whole_pass() {
        // Finding 2: a failed Herdr listing must never be read as "no
        // panes live" — it fails the whole pass open, recording and
        // reclaiming nothing, even though the task below would otherwise
        // be a clean reconcile candidate.
        let root = temp_dir("sweep-probe-failure");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]).fail_next(1);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report,
            SweepReport {
                in_progress_before: 1,
                in_progress_after: 1,
                recorded: 0,
                reclaimed: 0,
                expired: 0,
                superseded: Vec::new(),
                expired_tasks: Vec::new(),
                awaiting_gone: Vec::new(),
                awaiting_partial: Vec::new(),
                dirty_details: BTreeMap::new(),
            }
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log.lines().count(),
            2,
            "a failed probe must never let reconcile write anything this pass"
        );
    }

    // -- finding 4: `expire_awaiting` (lease expiry) ---------------------

    #[test]
    fn a_stranded_awaiting_task_past_its_lease_is_recorded_failed() {
        let root = temp_dir("sweep-expire-awaiting");
        write(
            &root.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-aaaaaa\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tbranch=main\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.expired, 1);
        assert_eq!(report.in_progress_before, 1);
        assert_eq!(
            report.in_progress_after, 0,
            "card 20261105-b-e66235: a task this same apply pass just lease-expired must \
             leave the in-progress tally, not linger in it until the next sweep"
        );

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        let last = log.lines().last().unwrap();
        assert!(last.contains("task=20000101-fixer-aaaaaa"));
        assert!(last.contains("status=failed"));
        assert!(
            last.contains("state=abandoned: pane gone, no status file, lease expired after 60m")
        );
    }

    #[test]
    fn an_awaiting_task_not_yet_past_its_lease_is_left_alone() {
        let root = temp_dir("sweep-expire-fresh");
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{now}\tevent=dispatch\ttask=20990101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
                now = mv_core::time::format_seconds_utc(now_utc()),
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.expired, 0);
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "a fresh AWAITING task is untouched");
    }

    #[test]
    fn a_per_account_lease_override_shortens_the_window() {
        let root = temp_dir("sweep-expire-override");
        // Only 10 minutes old — past a 5-minute override, not past the
        // 60-minute fleet default.
        let dispatched = now_utc() - time::Duration::minutes(10);
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{when}\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\taccount=tight-seat\n",
                when = mv_core::time::format_seconds_utc(dispatched),
            ),
        );

        let mut overrides = HashMap::new();
        overrides.insert("tight-seat".to_string(), 5u32);
        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &overrides, now_utc(), &mut agents);
        assert_eq!(
            report.expired, 1,
            "the account's own 5-minute override must apply, not the 60-minute fleet default"
        );
    }

    #[test]
    fn host_sleep_is_subtracted_from_elapsed_before_the_lease_gate_critic_h1() {
        // 65 minutes of raw wall clock past a 60-minute lease would expire
        // this task on its own; 10 minutes of host sleep brings the
        // *effective* elapsed down to 55 minutes, still under the lease.
        let root = temp_dir("sweep-expire-sleep-saved");
        let dispatched = now_utc() - time::Duration::minutes(65);
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{when}\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
                when = mv_core::time::format_seconds_utc(dispatched),
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let (report, _) = sweep_project_with_tasks(
            &root,
            false,
            60,
            &HashMap::new(),
            now_utc(),
            10,
            &mut agents,
        );
        assert_eq!(
            report.expired, 0,
            "10 minutes of host sleep must be subtracted from the 65-minute raw gap, \
             leaving 55 minutes — still under the 60-minute lease"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "nothing must be recorded this pass");
    }

    #[test]
    fn host_sleep_never_saves_a_task_once_truly_past_lease_critic_h1() {
        // 10 minutes of sleep is not enough to save a task 90 minutes past
        // dispatch against a 60-minute lease (80 minutes effective).
        let root = temp_dir("sweep-expire-sleep-not-enough");
        let dispatched = now_utc() - time::Duration::minutes(90);
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{when}\tevent=dispatch\ttask=20260101-fixer-bbbbbb\tbranch=main\trole=fixer\n",
                when = mv_core::time::format_seconds_utc(dispatched),
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let (report, _) = sweep_project_with_tasks(
            &root,
            false,
            60,
            &HashMap::new(),
            now_utc(),
            10,
            &mut agents,
        );
        assert_eq!(
            report.expired, 1,
            "80 effective minutes is still past the 60-minute lease"
        );
    }

    #[test]
    fn a_status_file_in_the_lane_worktree_defers_expiry_even_unparseable_critic_h2() {
        // The lane worktree (not main) holds a status file written after
        // dispatch — garbage content, the shape a truncating `echo` leaves
        // mid-write. Python's `_has_current_status_file` only asks whether
        // something current was written, never what it says; a lease-expiry
        // pass that only looked at main would supersede a lane that
        // actually finished.
        let root = temp_dir("sweep-expire-lane-status");
        let dispatched = now_utc() - time::Duration::minutes(90);
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{when}\tevent=dispatch\ttask=20260101-fixer-cccccc\tbranch=multiverse/20260101-fixer-cccccc\trole=fixer\n",
                when = mv_core::time::format_seconds_utc(dispatched),
            ),
        );
        write(
            &root
                .join(".multiverse/worktrees/20260101-fixer-cccccc/.multiverse/tasks/20260101-fixer-cccccc/status"),
            "don",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(
            report.expired, 0,
            "a current status file in the lane worktree must defer expiry, even one that \
             does not parse as done/blocked/failed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log.lines().count(),
            1,
            "nothing must be recorded while the lane's own status is current"
        );
    }

    #[test]
    fn expire_awaiting_is_idempotent_under_a_duplicate_manual_append_critic_m1() {
        // Simulates two reconcilers racing the same stranded task: by the
        // time `expire_awaiting` goes to record it, something else (another
        // process, in the real race) has already appended a usable result.
        // `append_outcome_if_absent`'s read-check-append must see that and
        // record nothing a second time.
        let root = temp_dir("sweep-expire-idempotent");
        write(
            &root.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-dddddd\tbranch=main\trole=fixer\n\
             2000-01-01T00:01:00+00:00\tevent=result\ttask=20000101-fixer-dddddd\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let tasks = sweep_tasks(&root);
        // `sweep_tasks` already folds the winning result, so synthesize the
        // stale AWAITING view `expire_awaiting` itself would have seen at
        // snapshot time, ahead of the race.
        let stale_task = SweepTask {
            recorded: false,
            file_status: None,
            ..tasks[0].clone()
        };
        let live_name = |_: &str| None;
        let expiries = expire_awaiting(
            &root,
            std::slice::from_ref(&stale_task),
            60,
            &HashMap::new(),
            now_utc(),
            0,
            true,
            &live_name,
        );
        assert_eq!(
            expiries.len(),
            1,
            "the stale snapshot still looks AWAITING and produces a determination"
        );
        assert!(
            !expiries[0].recorded,
            "a usable result already in the log must veto the write, not duplicate it"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log.lines()
                .filter(|line| line.contains("event=result"))
                .count(),
            1,
            "exactly one result line must survive the race, never two"
        );
    }

    #[test]
    fn a_dry_run_previews_expiry_without_writing_anything() {
        let root = temp_dir("sweep-expire-dry-run");
        write(
            &root.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, true, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.expired, 1, "a dry run still previews the count");
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(log.lines().count(), 1, "a dry run never writes anything");
    }

    #[test]
    fn an_awaiting_task_with_a_partial_result_names_the_file_instead_of_the_generic_lease_reason() {
        let root = temp_dir("sweep-expire-partial-result");
        write(
            &root.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
        );
        write(
            &root.join(".multiverse/tasks/20000101-fixer-aaaaaa/result.md"),
            "# Result\nfindings before the pane vanished.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.expired, 1);
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        let last = log.lines().last().unwrap();
        assert!(
            last.contains(
                "state=abandoned: pane gone, partial result kept at .multiverse/tasks/20000101-fixer-aaaaaa/result.md"
            ),
            "got: {last}"
        );
    }

    #[test]
    fn a_lane_this_helper_cannot_prove_clean_is_never_reclaimed() {
        // The lane directory exists but is not a git repository, so
        // `lane_porcelain` cannot answer either way — `None`, not
        // "clean", which must never be read as reclaimable (unlike a
        // lane directory that is simply absent, which trivially is).
        let root = temp_dir("sweep-owned-lane");
        write(
            &root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-bbbbbb\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=old:test\toutcome=chain-primary\treason=\tagent=mv-fixer-1\tbranch=multiverse/20250101-fixer-bbbbbb\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-bbbbbb\tpane=w1:p1\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-fixer-bbbbbb\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-bbbbbb/status"),
            "done",
        );
        std::fs::create_dir_all(root.join(".multiverse/worktrees/20250101-fixer-bbbbbb")).unwrap();

        let mut agents = FakeAgents::new_idle(&[("w1:p1", "mv-fixer-1")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.reclaimed, 0);
        assert_eq!(agents.list_calls, 1, "no clean candidate means no re-probe");
    }

    #[test]
    fn zero_tasks_still_probes_live_panes_once_but_changes_nothing() {
        let root = temp_dir("sweep-empty");
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report, SweepReport::default());
        assert_eq!(agents.list_calls, 1);
    }

    #[test]
    fn reconcile_settled_records_a_finished_but_unlogged_task() {
        let root = temp_dir("reconcile-settled");
        write(&root.join(".multiverse/routing.log"), UNRECORDED_LOG);
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20250101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let written = reconcile_settled(&root, None, &mut agents);
        assert_eq!(written, vec!["20250101-fixer-aaaaaa".to_string()]);

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log
            .lines()
            .last()
            .unwrap()
            .contains("event=result\ttask=20250101-fixer-aaaaaa\tstatus=done\tstate=reconciled"));
    }

    #[test]
    fn reconcile_settled_with_a_task_filter_ignores_every_other_candidate() {
        let root = temp_dir("reconcile-settled-filtered");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=dispatch\ttask=20260101-fixer-bbbbbb\tbranch=main\trole=fixer\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-bbbbbb/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-bbbbbb/result.md"),
            "# Result\ndone.\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let written = reconcile_settled(&root, Some("20260101-fixer-bbbbbb"), &mut agents);
        assert_eq!(written, vec!["20260101-fixer-bbbbbb".to_string()]);

        let tasks = sweep_tasks(&root);
        let a = tasks
            .iter()
            .find(|t| t.task_id == "20260101-fixer-aaaaaa")
            .unwrap();
        assert!(!a.recorded, "the unnamed candidate must be left alone");
    }

    // -- `close_stranded`/`reclaimable_panes` ----------------------------

    /// `root` itself as the git repo `branch_disposition`'s reads run
    /// against — the shared checkout, on `main`, never a live worktree.
    fn init_main_repo(root: &Path) {
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "user.email", "main@example.invalid"]);
        git(root, &["config", "user.name", "Main Test"]);
        write(&root.join("README"), "seed\n");
        git(root, &["add", "README"]);
        git(root, &["commit", "-q", "-m", "seed"]);
    }

    /// Branches `multiverse/<lane>` off `main`'s current tip and records
    /// its cut point under `refs/multiverse/base/<lane>`, the same ref
    /// `Worktrees::create` writes — leaves `main` checked out.
    fn create_lane_branch(root: &Path, lane: &str) {
        let base = git_stdout(root, &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_string();
        git(root, &["branch", &format!("multiverse/{lane}"), &base]);
        git(
            root,
            &["update-ref", &format!("refs/multiverse/base/{lane}"), &base],
        );
    }

    fn commit_on_branch(root: &Path, branch: &str, file: &str, content: &str) {
        git(root, &["checkout", "-q", branch]);
        write(&root.join(file), content);
        git(root, &["add", file]);
        git(root, &["commit", "-q", "-m", &format!("work on {branch}")]);
        git(root, &["checkout", "-q", "main"]);
    }

    fn dispatch_line(when: &str, task: &str, role: &str, branch: &str) -> String {
        format!("{when}\tevent=dispatch\ttask={task}\tbranch={branch}\trole={role}\n")
    }

    #[test]
    fn close_stranded_records_a_landed_branch_as_done() {
        let root = temp_dir("close-stranded-landed");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-aaaaaa");
        commit_on_branch(&root, "multiverse/20260101-fixer-aaaaaa", "work.txt", "a\n");
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
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-aaaaaa",
                "fixer",
                "multiverse/20260101-fixer-aaaaaa",
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Done);
        assert_eq!(closures[0].state, RECONCILED_BY_MERGE);
        assert!(closures[0].recorded);

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains("event=result\ttask=20260101-fixer-aaaaaa\tstatus=done"));

        // A second pass sees the outcome it just wrote and leaves it alone.
        let again = close_stranded(&root, true, &mut agents);
        assert!(again.is_empty(), "a recorded task is no longer AWAITING");
    }

    #[test]
    fn close_stranded_records_an_unmerged_branch_as_abandoned_unmerged() {
        let root = temp_dir("close-stranded-unmerged");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-bbbbbb");
        commit_on_branch(&root, "multiverse/20260101-fixer-bbbbbb", "work.txt", "b\n");
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-bbbbbb",
                "fixer",
                "multiverse/20260101-fixer-bbbbbb",
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, false, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Failed);
        assert_eq!(closures[0].state, ABANDONED_UNMERGED);
        assert!(!closures[0].recorded, "a dry run must never write");

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("event=result"), "a dry run must never write");
    }

    #[test]
    fn close_stranded_records_an_empty_clean_branch_as_abandoned_empty_lane() {
        let root = temp_dir("close-stranded-empty-clean");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-cccccc");
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-cccccc",
                "fixer",
                "multiverse/20260101-fixer-cccccc",
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Failed);
        assert_eq!(closures[0].state, ABANDONED_EMPTY_LANE);
    }

    #[test]
    fn close_stranded_records_an_empty_dirty_branch_as_stranded_uncommitted_work() {
        let root = temp_dir("close-stranded-empty-dirty");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-dddddd");
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-dddddd",
                "fixer",
                "multiverse/20260101-fixer-dddddd",
            ),
        );
        let lane = root.join(".multiverse/worktrees/20260101-fixer-dddddd");
        std::fs::create_dir_all(&lane).unwrap();
        git(&lane, &["init", "-q", "-b", "main"]);
        git(&lane, &["config", "user.email", "lane@example.invalid"]);
        git(&lane, &["config", "user.name", "Lane Test"]);
        std::fs::write(lane.join("uncommitted.txt"), "dirty\n").unwrap();

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Blocked);
        assert_eq!(closures[0].state, STRANDED_UNCOMMITTED_WORK);
        assert!(closures[0].evidence.contains("uncommitted.txt"));
    }

    #[test]
    fn close_stranded_records_a_missing_branch_as_unknown_needs_human() {
        let root = temp_dir("close-stranded-gone");
        init_main_repo(&root);
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-eeeeee",
                "fixer",
                "multiverse/20260101-fixer-eeeeee",
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Blocked);
        assert_eq!(closures[0].state, UNKNOWN_NEEDS_HUMAN);
    }

    #[test]
    fn close_stranded_records_a_non_writing_role_as_no_branch_expected() {
        let root = temp_dir("close-stranded-read-only-role");
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-explorer-ffffff",
                "explorer",
                "",
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Failed);
        assert_eq!(closures[0].state, NO_BRANCH_EXPECTED);
    }

    #[test]
    fn close_stranded_records_a_leftover_result_md_as_abandoned_partial_result() {
        let root = temp_dir("close-stranded-partial-result");
        write(
            &root.join(".multiverse/routing.log"),
            &dispatch_line(
                "2026-01-01T00:00:00+00:00",
                "20260101-fixer-aabbcc",
                "fixer",
                "multiverse/20260101-fixer-aabbcc",
            ),
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-aabbcc/result.md"),
            "# Findings\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].status, TaskStatus::Failed);
        assert_eq!(closures[0].state, ABANDONED_PARTIAL_RESULT);
        assert!(closures[0].evidence.contains("result.md"));
    }

    #[test]
    fn close_stranded_never_touches_a_running_or_recorded_task() {
        let root = temp_dir("close-stranded-leaves-others-alone");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=live\tbranch=multiverse/live\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=live\tpane=w1:p1\n\
             2026-01-01T00:02:00+00:00\tevent=dispatch\ttask=done-already\tbranch=multiverse/done-already\trole=fixer\n\
             2026-01-01T00:03:00+00:00\tevent=result\ttask=done-already\tstatus=done\tstate=done\trevisions=0\tpane=w2:p2\n",
        );

        let mut agents = FakeAgents::new(&["w1:p1"]);
        let closures = close_stranded(&root, true, &mut agents);
        assert!(
            closures.is_empty(),
            "a live-paned task and an already-recorded one are never stranded: {closures:?}"
        );
    }

    #[test]
    fn close_stranded_records_nothing_when_the_live_listing_fails() {
        // A failed live probe must never collapse to "no panes are live" —
        // that would make `is_awaiting` read every pane as gone and close
        // every AWAITING task sight unseen. `w1:p1` below has no live
        // agent name on file (reused-pane probe never ran), so the
        // pre-fix `unwrap_or_default()` would have read it as gone too
        // and recorded it `no-branch-expected` (a non-writing role).
        let root = temp_dir("close-stranded-listing-failed");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-ffffff\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-ffffff\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new(&[]).fail_next(1);
        let closures = close_stranded(&root, true, &mut agents);
        assert!(
            closures.is_empty(),
            "a failed live listing must be a no-op, not a reason to close everything: {closures:?}"
        );

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=result"),
            "a failed live listing must record nothing"
        );

        // The very next call, with the probe succeeding, still finds and
        // closes the same task — the earlier failure didn't eat it.
        let closures = close_stranded(&root, true, &mut agents);
        assert_eq!(closures.len(), 1);
        assert_eq!(closures[0].state, NO_BRANCH_EXPECTED);
        assert!(closures[0].recorded);
    }

    #[test]
    fn close_stranded_treats_a_pane_reused_by_a_different_agent_as_stranded() {
        // Card 20261108-b-152b33: `w1:p1` is this task's own recorded pane
        // id, dispatched under agent `agent-a`, but the live listing now
        // shows a different agent, `agent-b`, answering there — the id was
        // handed to someone else's session after a Herdr restart. Matching
        // by pane id alone (pre-fix) reads this task as still running
        // forever; it must instead read as gone, same as
        // `close_stranded_never_touches_a_running_or_recorded_task`'s own
        // pane does when the *same* agent still occupies it.
        let root = temp_dir("close-stranded-reused-pane");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-aaaaaa\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-aaaaaa\tpane=w1:p1\n",
        );

        let mut reused = FakeAgents::new_idle(&[("w1:p1", "agent-b")]);
        let closures = close_stranded(&root, true, &mut reused);
        assert_eq!(
            closures.len(),
            1,
            "a pane now owned by a different agent must read as gone, not still running"
        );
        assert_eq!(closures[0].status, TaskStatus::Failed);
        assert_eq!(closures[0].state, NO_BRANCH_EXPECTED);

        let root2 = temp_dir("close-stranded-same-agent-still-live");
        write(
            &root2.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-bbbbbb\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-bbbbbb\tpane=w1:p1\n",
        );
        let mut same_agent = FakeAgents::new_idle(&[("w1:p1", "agent-a")]);
        let closures = close_stranded(&root2, true, &mut same_agent);
        assert!(
            closures.is_empty(),
            "the task's own dispatched agent still occupying the pane must still read as live: \
             {closures:?}"
        );
    }

    #[test]
    fn close_stranded_leaves_a_live_idle_same_name_task_with_no_status_file_unrecorded() {
        // Card 20261106-b-2c3d2f Decision 1, the baseline this decision
        // must not regress: a pane still listed, idle, under the task's
        // own dispatched name, with no status file, is not stranded.
        let root = temp_dir("close-stranded-live-idle-same-name");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-gggggg\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-gggggg\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new_idle(&[("w1:p1", "agent-a")]);
        let closures = close_stranded(&root, true, &mut agents);
        assert!(
            closures.is_empty(),
            "a live idle pane under the task's own agent name must not be recorded: {closures:?}"
        );
    }

    #[test]
    fn close_stranded_leaves_a_live_task_with_no_confirmed_name_unrecorded() {
        // Card 20261106-b-2c3d2f Decision 1: `w1:p1` is still listed as
        // live, but the listing carries no name for it — the gap this
        // decision exists to close. A missing/empty live name is not
        // confirmation the pane is gone the way a *different* name is
        // (`close_stranded_treats_a_pane_reused_by_a_different_agent_as_stranded`
        // above); recording `failed` here would invent a terminal outcome
        // for an agent that may still be about to write its own status.
        let root = temp_dir("close-stranded-live-no-name");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-hhhhhh\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-hhhhhh\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new(&["w1:p1"]);
        let closures = close_stranded(&root, true, &mut agents);
        assert!(
            closures.is_empty(),
            "a pane still listed with no confirmed name must not read as confirmed gone: \
             {closures:?}"
        );
    }

    #[test]
    fn reclaimable_panes_lists_a_live_recorded_clean_pane() {
        let root = temp_dir("reclaimable-live");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let mut agents =
            FakeAgents::new(&["w1:p1"]).with_label("w1:p1", "fixer · 20260101-fixer-aaaaaa");
        let (reclaimable, stale) = reclaimable_panes(&root, &mut agents);
        assert_eq!(reclaimable.len(), 1);
        assert_eq!(reclaimable[0].task_id, "20260101-fixer-aaaaaa");
        assert!(stale.is_empty());
    }

    #[test]
    fn reclaimable_panes_treats_a_pane_reused_by_a_different_agent_as_gone() {
        // Card 20261108-b-37df85: pane ids are reused after a Herdr
        // restart, so a recorded task's old pane id can now belong to a
        // different, unrelated live agent. That must never be offered as
        // this task's own reclaimable pane — it is treated exactly like
        // the pane being gone, falling to `stale` instead.
        let root = temp_dir("reclaimable-reused-agent");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-111111\tbranch=main\t\
             role=fixer\tagent=x-fix-cl\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-111111\tstatus=done\t\
             state=done\trevisions=0\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new_idle(&[("w1:p1", "y-buil-cl")]);
        let (reclaimable, stale) = reclaimable_panes(&root, &mut agents);
        assert!(
            reclaimable.is_empty(),
            "a pane now owned by a different agent must never be reclaimable"
        );
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].task_id, "20260101-fixer-111111");

        // The agent matching the task's own recorded dispatch still
        // reclaims normally, once its pane label also names this task.
        let mut same_agent = FakeAgents::new_idle(&[("w1:p1", "x-fix-cl")])
            .with_label("w1:p1", "builder · 20260101-fixer-111111");
        let (reclaimable, stale) = reclaimable_panes(&root, &mut same_agent);
        assert_eq!(reclaimable.len(), 1);
        assert!(stale.is_empty());

        // Card 20261106-b-9d70c7's own static race: the same name, in the
        // same pane, both idle — but the pane's own label now names a
        // *different* task, the shape a Herdr restart that reissues this
        // deterministic name into a recycled pane would produce. The
        // label gate must still refuse it, falling to `stale` exactly
        // like the differently-named case above.
        let mut same_name_wrong_label = FakeAgents::new_idle(&[("w1:p1", "x-fix-cl")])
            .with_label("w1:p1", "builder · 20260101-builder-999999");
        let (reclaimable, stale) = reclaimable_panes(&root, &mut same_name_wrong_label);
        assert!(
            reclaimable.is_empty(),
            "a pane label naming a different task must never confirm reclaim"
        );
        assert_eq!(stale.len(), 1);
    }

    #[test]
    fn reclaimable_panes_marks_a_dead_pane_stale_once() {
        let root = temp_dir("reclaimable-stale");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-bbbbbb\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-bbbbbb\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new(&[]);
        let (reclaimable, stale) = reclaimable_panes(&root, &mut agents);
        assert!(reclaimable.is_empty());
        assert_eq!(stale.len(), 1);
        assert!(append_stale_pane_if_absent(
            &root,
            &stale[0].task_id,
            &stale[0].pane
        ));

        // A second pass sees the bookkeeping record it just wrote and
        // leaves the task off `stale` entirely — no longer unreported
        // noise every call.
        let (reclaimable_again, stale_again) = reclaimable_panes(&root, &mut agents);
        assert!(reclaimable_again.is_empty());
        assert!(stale_again.is_empty());
    }

    #[test]
    fn reclaimable_panes_reports_nothing_when_the_live_listing_fails() {
        // A failed live probe must never collapse to "no panes are live" —
        // that would make `occupant_matches` read this task's pane as gone
        // and call it `stale` sight unseen, same risk `close_stranded`
        // guards against. The pre-fix `unwrap_or_default()` would have
        // read `w1:p1` below as gone and recorded it stale.
        let root = temp_dir("reclaimable-listing-failed");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_label("w1:p1", "fixer · 20260101-fixer-aaaaaa")
            .fail_next(1);
        let (reclaimable, stale) = reclaimable_panes(&root, &mut agents);
        assert!(
            reclaimable.is_empty(),
            "a failed live listing must never report a pane reclaimable: {reclaimable:?}"
        );
        assert!(
            stale.is_empty(),
            "a failed live listing must never report a pane stale either: {stale:?}"
        );

        // The very next call, with the probe succeeding, still finds and
        // reclaims the same pane — the earlier failure didn't eat it or
        // mark it stale.
        let (reclaimable, stale) = reclaimable_panes(&root, &mut agents);
        assert_eq!(reclaimable.len(), 1);
        assert_eq!(reclaimable[0].task_id, "20260101-fixer-aaaaaa");
        assert!(stale.is_empty());
    }

    fn reclaim_lane_fixture(root: &Path) -> PathBuf {
        init_main_repo(root);
        write(&root.join(".git/info/exclude"), ".multiverse/\n");
        let owner = "20260101-builder-aaaaaa";
        create_lane_branch(root, owner);
        let lane = root.join(".multiverse/worktrees").join(owner);
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                lane.to_str().unwrap(),
                &format!("multiverse/{owner}"),
            ],
        );
        write(&root.join(".multiverse/routing.log"), &format!(
            "2026-01-01T00:00:00Z\tevent=dispatch\ttask={owner}\trole=builder\tbranch=multiverse/{owner}\n\
             2026-01-01T00:01:00Z\tevent=result\ttask={owner}\tstatus=done\tpane=w1:p1\n\
             2026-01-01T00:02:00Z\tevent=dispatch\ttask=20260101-builder-bbbbbb\trole=builder\tbranch=multiverse/{owner}\n\
             2026-01-01T00:03:00Z\tevent=started\ttask=20260101-builder-bbbbbb\tpane=w1:p2\n"));
        write(
            &lane.join(".multiverse/tasks/20260101-builder-bbbbbb/brief.md"),
            "adopted\n",
        );
        write(
            &root.join(".multiverse/tasks").join(owner).join("status"),
            "done",
        );
        lane
    }

    #[test]
    fn reclaim_protects_a_clean_lane_with_a_live_unsettled_occupant() {
        let root = temp_dir("reclaim-live-occupant");
        let lane = reclaim_lane_fixture(&root);
        assert_eq!(
            lane_porcelain(&root, "20260101-builder-aaaaaa"),
            Some(String::new())
        );
        for panes in [&["w1:p1", "w1:p2"][..], &["w1:p2"][..]] {
            let (reclaimable, stale) = reclaimable_panes(&root, &mut FakeAgents::new(panes));
            assert!(reclaimable.is_empty(), "must protect the owner's live pane");
            assert!(stale.is_empty(), "must protect stale bookkeeping too");
        }
        let mut agents = FakeAgents::new_idle(&[("w1:p1", ""), ("w1:p2", "")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.reclaimed, 0);
        assert!(agents.closed.is_empty());

        write(
            &lane.join(".multiverse/tasks/20260101-builder-bbbbbb/status"),
            "done",
        );
        let mut agents = FakeAgents::new(&["w1:p1", "w1:p2"])
            .with_label("w1:p1", "builder · 20260101-builder-aaaaaa")
            .with_cwd("w1:p1", lane.clone());
        let (reclaimable, _) = reclaimable_panes(&root, &mut agents);
        assert_eq!(
            reclaimable.len(),
            1,
            "terminal local status releases the guard"
        );
    }

    #[test]
    fn reclaim_protects_an_occupant_brief_before_its_dispatch_was_logged() {
        let root = temp_dir("reclaim-orphan-brief");
        let lane = reclaim_lane_fixture(&root);
        let log = root.join(".multiverse/routing.log");
        let text = std::fs::read_to_string(&log).unwrap();
        write(&log, &text.lines().take(2).collect::<Vec<_>>().join("\n"));
        let (reclaimable, stale) = reclaimable_panes(&root, &mut FakeAgents::new(&["w1:p1"]));
        assert!(reclaimable.is_empty());
        assert!(stale.is_empty());
        write(
            &root.join(".multiverse/tasks/20260101-builder-bbbbbb/status"),
            "failed",
        );
        let mut agents = FakeAgents::new(&["w1:p1"])
            .with_label("w1:p1", "builder · 20260101-builder-aaaaaa")
            .with_cwd("w1:p1", lane.clone());
        let (reclaimable, _) = reclaimable_panes(&root, &mut agents);
        assert_eq!(
            reclaimable.len(),
            1,
            "rescued terminal status releases the guard"
        );
    }

    fn artifact_lane_fixture(root: &Path) -> PathBuf {
        init_main_repo(root);
        write(
            &root.join(".git/info/exclude"),
            ".multiverse/*\n!.multiverse/roster.toml\n",
        );
        let owner = "20260101-builder-aaaaaa";
        create_lane_branch(root, owner);
        let lane = root.join(".multiverse/worktrees").join(owner);
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                lane.to_str().unwrap(),
                &format!("multiverse/{owner}"),
            ],
        );
        lane
    }

    #[test]
    fn stop_artifact_gate_requires_identical_rescued_bytes() {
        let root = temp_dir("stop-artifacts");
        let lane = artifact_lane_fixture(&root);
        let relative = Path::new(".multiverse/tasks/20260101-builder-bbbbbb/result.md");
        write(&lane.join(relative), "unique output\n");
        assert_eq!(
            lanes_holding_unrescued_output(&root).unwrap(),
            ["20260101-builder-aaaaaa"]
        );
        write(&root.join(relative), "different output\n");
        assert_eq!(lanes_holding_unrescued_output(&root).unwrap().len(), 1);
        write(&root.join(relative), "unique output\n");
        assert!(lanes_holding_unrescued_output(&root).unwrap().is_empty());
        // Inputs and nested lanes are copies/scaffolding, not this lane's output.
        write(&lane.join(".multiverse/input/result.md"), "input\n");
        write(
            &lane.join(".multiverse/worktrees/nested/result.md"),
            "nested\n",
        );
        write(&lane.join(".multiverse/.gitignore"), "*\n");
        assert!(lanes_holding_unrescued_output(&root).unwrap().is_empty());
    }

    #[test]
    fn stop_artifact_gate_exempts_the_committed_clean_roster_only_while_clean() {
        let root = temp_dir("stop-roster");
        let lane = artifact_lane_fixture(&root);
        let roster = ".multiverse/roster.toml";
        write(&lane.join(roster), "# committed lane roster\n");
        git(&lane, &["add", roster]);
        git(&lane, &["commit", "-q", "-m", "lane roster"]);
        assert!(lanes_holding_unrescued_output(&root).unwrap().is_empty());
        write(&lane.join(roster), "# uncommitted lane roster\n");
        assert_eq!(lanes_holding_unrescued_output(&root).unwrap().len(), 1);
    }

    #[test]
    fn stop_artifact_gate_refuses_unsafe_source_and_rescued_paths() {
        let root = temp_dir("stop-artifact-links");
        let lane = artifact_lane_fixture(&root);
        let relative = Path::new(".multiverse/tasks/20260101-builder-bbbbbb/result.md");
        write(&lane.join(relative), "output\n");
        std::fs::create_dir_all(root.join(relative).parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(lane.join(relative), root.join(relative)).unwrap();
        assert!(lanes_holding_unrescued_output(&root).is_err());
        std::fs::remove_file(root.join(relative)).unwrap();
        std::fs::remove_file(lane.join(relative)).unwrap();
        std::os::unix::fs::symlink(root.join("README"), lane.join(relative)).unwrap();
        assert!(lanes_holding_unrescued_output(&root).is_err());
    }

    // -- `auto_supersede` / `lane_reuse_index` (card `20261102-b-0d8b17`) --

    fn bare_task(task_id: &str, branch: &str, dispatch_ordinal: u64) -> SweepTask {
        SweepTask {
            task_id: task_id.to_string(),
            branch: branch.to_string(),
            role: "fixer".to_string(),
            pane: None,
            file_status: None,
            recorded: false,
            agent: None,
            account: None,
            dispatched_at: None,
            started_at: None,
            dispatch_ordinal,
            status: None,
            outcome_state: None,
            outcome_ordinal: None,
            revisions: 0,
        }
    }

    #[test]
    fn lane_reuse_index_attributes_an_older_occupant_to_the_newest_later_occupant() {
        // Three dispatches into the same lane, oldest first: the owner's
        // own first dispatch, a `--into` redispatch, and a second
        // `--into` redispatch newer still. The owner (and the first
        // redispatch) must both resolve to the *newest* occupant, not
        // merely the next-later one — the bug Python's own
        // `_lane_reuse_index` docstring calls out by name.
        let tasks = vec![
            bare_task("owner", "multiverse/owner", 0),
            bare_task("second", "multiverse/owner", 1),
            bare_task("newest", "multiverse/owner", 2),
        ];
        let index = lane_reuse_index(&tasks);
        assert_eq!(index.get("owner"), Some(&2));
        assert_eq!(index.get("second"), Some(&2));
        assert!(!index.contains_key("newest"));
    }

    #[test]
    fn auto_supersede_corrects_a_recorded_blocked_task_whose_branch_has_landed() {
        let root = temp_dir("auto-supersede-merged");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-aaaaaa");
        commit_on_branch(&root, "multiverse/20260101-fixer-aaaaaa", "work.txt", "a\n");
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
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{dispatch}2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=blocked\tstate=needs-human\trevisions=0\tpane=w1:p1\n",
                dispatch = dispatch_line(
                    "2026-01-01T00:00:00+00:00",
                    "20260101-fixer-aaaaaa",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
            ),
        );

        let mut tasks = sweep_tasks(&root);
        let results = auto_supersede(&root, &mut tasks, true, &|_: &str| None);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task_id, "20260101-fixer-aaaaaa");
        assert_eq!(results[0].status, TaskStatus::Done);
        assert_eq!(results[0].state, SUPERSEDED_MERGED);
        assert!(results[0].recorded);

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        let last = log.lines().last().unwrap();
        assert!(last.contains("event=supersede\ttask=20260101-fixer-aaaaaa"));
        assert!(last.contains("supersedes=2"));
        assert!(last.contains("status=done"));
        assert!(last.contains("state=superseded-merged"));

        // The in-memory snapshot is updated too, so a caller folding this
        // into `expire_awaiting` over the same tasks never double-records
        // it.
        let task = tasks
            .iter()
            .find(|t| t.task_id == "20260101-fixer-aaaaaa")
            .unwrap();
        assert!(task.recorded);
        assert_eq!(task.status, Some(TaskStatus::Done));
    }

    #[test]
    fn auto_supersede_records_an_awaiting_tasks_reused_lane_as_superseded_redispatched() {
        let root = temp_dir("auto-supersede-redispatched");
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{owner}{occupant}",
                owner = dispatch_line(
                    "2026-01-01T00:00:00+00:00",
                    "20260101-fixer-aaaaaa",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
                occupant = dispatch_line(
                    "2026-01-02T00:00:00+00:00",
                    "20260102-fixer-bbbbbb",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
            ),
        );

        let mut tasks = sweep_tasks(&root);
        let results = auto_supersede(&root, &mut tasks, true, &|_: &str| None);
        // Only the superseded owner is reported — the later occupant
        // holding the lane now is not itself a candidate: its own lane
        // name resolves to the owner's id, not its own, so it is never
        // superseded by its own (nonexistent) branch landing either.
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].task_id, "20260101-fixer-aaaaaa");
        assert_eq!(results[0].status, TaskStatus::Done);
        assert_eq!(results[0].state, SUPERSEDED_REDISPATCHED);
        assert!(results[0].reason.contains("20260102-fixer-bbbbbb"));
        assert!(results[0].recorded);

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        let last = log.lines().last().unwrap();
        assert!(last.contains("event=result\ttask=20260101-fixer-aaaaaa"));
        assert!(last.contains("status=done"));
        assert!(last.contains("state=superseded-redispatched"));
    }

    #[test]
    fn auto_supersede_dry_run_previews_without_writing_or_mutating() {
        let root = temp_dir("auto-supersede-dry-run");
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{owner}{occupant}",
                owner = dispatch_line(
                    "2026-01-01T00:00:00+00:00",
                    "20260101-fixer-aaaaaa",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
                occupant = dispatch_line(
                    "2026-01-02T00:00:00+00:00",
                    "20260102-fixer-bbbbbb",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
            ),
        );

        let mut tasks = sweep_tasks(&root);
        let results = auto_supersede(&root, &mut tasks, false, &|_: &str| None);
        assert_eq!(results.len(), 1);
        assert!(!results[0].recorded, "a dry run must never write");

        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert_eq!(log.lines().count(), 2, "a dry run must never write");
        let task = tasks
            .iter()
            .find(|t| t.task_id == "20260101-fixer-aaaaaa")
            .unwrap();
        assert!(!task.recorded, "a dry run must never mutate the snapshot");
    }

    #[test]
    fn auto_supersede_skips_a_non_writing_role_even_with_a_reused_lane() {
        let root = temp_dir("auto-supersede-non-writing-role");
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{owner}{occupant}",
                owner = dispatch_line(
                    "2026-01-01T00:00:00+00:00",
                    "20260101-explorer-aaaaaa",
                    "explorer",
                    "multiverse/20260101-explorer-aaaaaa",
                ),
                occupant = dispatch_line(
                    "2026-01-02T00:00:00+00:00",
                    "20260102-fixer-bbbbbb",
                    "fixer",
                    "multiverse/20260101-explorer-aaaaaa",
                ),
            ),
        );

        let mut tasks = sweep_tasks(&root);
        let results = auto_supersede(&root, &mut tasks, true, &|_: &str| None);
        assert!(
            results.is_empty(),
            "a read-only role is skipped outright, even though its lane was reused"
        );
    }

    #[test]
    fn sweep_project_supersedes_a_landed_blocked_task_and_never_also_expires_it() {
        let root = temp_dir("auto-supersede-sweep-integration");
        init_main_repo(&root);
        create_lane_branch(&root, "20260101-fixer-aaaaaa");
        commit_on_branch(&root, "multiverse/20260101-fixer-aaaaaa", "work.txt", "a\n");
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
        write(
            &root.join(".multiverse/routing.log"),
            &format!(
                "{dispatch}2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=blocked\tstate=needs-human\trevisions=0\tpane=w1:p1\n",
                dispatch = dispatch_line(
                    "2026-01-01T00:00:00+00:00",
                    "20260101-fixer-aaaaaa",
                    "fixer",
                    "multiverse/20260101-fixer-aaaaaa",
                ),
            ),
        );

        let mut agents = FakeAgents::new(&[]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut agents);
        assert_eq!(report.superseded.len(), 1);
        assert_eq!(report.superseded[0].task_id, "20260101-fixer-aaaaaa");
        assert_eq!(report.superseded[0].state, SUPERSEDED_MERGED);
        assert_eq!(
            report.expired, 0,
            "a task `auto_supersede` just claimed must never also be lease-expired in the same pass"
        );
    }

    #[test]
    fn sweep_project_expires_an_awaiting_task_whose_pane_was_reused_by_a_different_agent() {
        // Card 20261108-b-152b33: `w1:p1` is this task's own recorded pane
        // id, dispatched under agent `agent-a`, long past any reasonable
        // lease. The live listing now shows a different agent, `agent-b`,
        // answering there. Matching by pane id alone (pre-fix) reads this
        // task as still running forever, so `expire_awaiting` never gets a
        // chance to lease-expire it; it must instead read as gone, same as
        // `close_stranded_treats_a_pane_reused_by_a_different_agent_as_stranded`.
        let root = temp_dir("sweep-expire-reused-pane");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-aaaaaa\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-aaaaaa\tpane=w1:p1\n",
        );

        let mut reused = FakeAgents::new_idle(&[("w1:p1", "agent-b")]);
        let report = sweep_project(&root, false, 60, &HashMap::new(), now_utc(), &mut reused);
        assert_eq!(
            report.expired, 1,
            "a pane now owned by a different agent must not keep this task AWAITING forever"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(log.contains("event=result\ttask=20260101-explorer-aaaaaa\tstatus=failed"));

        let root2 = temp_dir("sweep-expire-same-agent-still-live");
        write(
            &root2.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-bbbbbb\t\
             branch=\trole=explorer\tagent=agent-a\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-bbbbbb\tpane=w1:p1\n",
        );
        let mut same_agent = FakeAgents::new_idle(&[("w1:p1", "agent-a")]);
        let report = sweep_project(
            &root2,
            false,
            60,
            &HashMap::new(),
            now_utc(),
            &mut same_agent,
        );
        assert_eq!(
            report.expired, 0,
            "the task's own dispatched agent still occupying the pane must still read as live"
        );
    }

    #[test]
    fn writer_status_prefers_the_lane_worktree_over_the_main_checkout() {
        let root = temp_dir("writer-status-lane-wins");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\t\
             branch=multiverse/20260101-fixer-aaaaaa\trole=fixer\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-fixer-aaaaaa/status"),
            "blocked",
        );
        write(
            &root
                .join(".multiverse/worktrees/20260101-fixer-aaaaaa")
                .join(".multiverse/tasks/20260101-fixer-aaaaaa/status"),
            "done",
        );
        let tasks = sweep_tasks(&root);
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].file_status,
            Some(TaskStatus::Done),
            "the worktree status must win over a leftover main-checkout copy"
        );
    }

    #[test]
    fn reconcile_project_closes_an_idle_settled_pane_and_leaves_a_working_one_open() {
        let root = temp_dir("reconcile-close-idle");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-aaaaaa\t\
             branch=\trole=explorer\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-aaaaaa\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-aaaaaa/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        );

        let empty = HashMap::new();
        let mut idle = FakeAgents::new(&["w1:p1"]).with_status("w1:p1", AgentState::Idle);
        let outcome = reconcile_project(
            &root,
            ReconcileRequest {
                only_task: None,
                close_panes: true,
                expire_stale: false,
                lease_minutes: 60,
                lease_overrides: &empty,
                now: now_utc(),
                require_live: true,
            },
            &mut idle,
        );
        assert_eq!(
            outcome.recorded,
            vec!["20260101-explorer-aaaaaa".to_string()]
        );
        assert_eq!(outcome.closed_panes, 1);
        assert!(idle.closed.contains(&"w1:p1".to_string()));

        let root2 = temp_dir("reconcile-leave-working");
        write(
            &root2.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-bbbbbb\t\
             branch=\trole=explorer\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-bbbbbb\tpane=w1:p2\n",
        );
        write(
            &root2.join(".multiverse/tasks/20260101-explorer-bbbbbb/status"),
            "done",
        );
        write(
            &root2.join(".multiverse/tasks/20260101-explorer-bbbbbb/result.md"),
            "# Result\ndone.\n",
        );
        let mut working = FakeAgents::new(&["w1:p2"]).with_status("w1:p2", AgentState::Working);
        let outcome = reconcile_project(
            &root2,
            ReconcileRequest {
                only_task: None,
                close_panes: true,
                expire_stale: false,
                lease_minutes: 60,
                lease_overrides: &empty,
                now: now_utc(),
                require_live: true,
            },
            &mut working,
        );
        assert!(
            outcome.recorded.is_empty(),
            "a WORKING pane is a live fix round, not a lingering finish"
        );
        assert_eq!(outcome.closed_panes, 0);
        assert!(working.closed.is_empty());
    }

    #[test]
    fn reconcile_project_never_closes_a_pane_reused_by_a_different_agent() {
        // Card 20261108-b-37df85: `w1:p1` is this task's own recorded
        // pane id, but Herdr now reports a different agent answering
        // there — the id was handed to someone else's session. Reconcile
        // still records this task's own terminal status (that read is
        // off the local status file, not the live listing), but must
        // never close a pane it does not actually own.
        let root = temp_dir("reconcile-reused-pane");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-dddddd\t\
             branch=\trole=explorer\tagent=x-fix-cl\n\
             2026-01-01T00:01:00+00:00\tevent=started\ttask=20260101-explorer-dddddd\tpane=w1:p1\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-dddddd/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-dddddd/result.md"),
            "# Result\ndone.\n",
        );

        let empty = HashMap::new();
        let mut reused = FakeAgents::new_idle(&[("w1:p1", "y-buil-cl")]);
        let outcome = reconcile_project(
            &root,
            ReconcileRequest {
                only_task: None,
                close_panes: true,
                expire_stale: false,
                lease_minutes: 60,
                lease_overrides: &empty,
                now: now_utc(),
                require_live: true,
            },
            &mut reused,
        );
        assert_eq!(
            outcome.recorded,
            vec!["20260101-explorer-dddddd".to_string()]
        );
        assert_eq!(
            outcome.closed_panes, 0,
            "a pane id reused by a different live agent must never be closed"
        );
        assert!(reused.closed.is_empty());
    }

    #[test]
    fn reconcile_project_skips_when_herdr_is_down_and_live_is_required() {
        let root = temp_dir("reconcile-herdr-down");
        write(
            &root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-explorer-cccccc\t\
             branch=\trole=explorer\n",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-cccccc/status"),
            "done",
        );
        write(
            &root.join(".multiverse/tasks/20260101-explorer-cccccc/result.md"),
            "# Result\ndone.\n",
        );
        let empty = HashMap::new();
        let mut agents = FakeAgents::new(&[]).fail_next(1);
        let outcome = reconcile_project(
            &root,
            ReconcileRequest {
                only_task: None,
                close_panes: true,
                expire_stale: true,
                lease_minutes: 60,
                lease_overrides: &empty,
                now: now_utc(),
                require_live: true,
            },
            &mut agents,
        );
        assert!(outcome.herdr_down);
        assert!(outcome.recorded.is_empty());
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("state=reconciled"));
    }
}

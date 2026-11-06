//! Notify outbox and the sweep that drains it, rate-limited at the
//! sender rather than at each call site.
//!
//! Ports the **state** half of `multiverse/notify.py`'s rate limiter and
//! outbox, documented in `.multiverse/input/spec-mail.md` §5 and
//! `.multiverse/input/spec-on-disk-contract.md` §A (`notify-state.json`,
//! `notify-outbox.jsonl`). This file owns the *decisions* a send makes
//! against persisted rate/dedup state and the outbox's queue/claim/collapse
//! bookkeeping. [`send`] and [`flush`] accept an injected transport: a
//! push that cannot reach the network still queues durably, but the
//! dedup/rate credit for that attempt is recorded at decide time
//! regardless and never reverted, matching Python's own `notify.send`
//! (`notify.py:871-918`) rather than the prior Rust behavior of restoring
//! the pre-decision state on an unreachable push (hub decision
//! 2026-11-03 09:5xZ, card `20261103-b-50a389`). [`flush`] replays a
//! queued record straight through the transport without re-running
//! [`decide_and_record`] on it, since that credit was already spent when
//! it first queued.
//! This module does not own the `curl -K -` push itself or `sweep`'s
//! five-plus-three alert conditions (`.multiverse/input/
//! audit-mail-backlog-board-disk.md`'s P0 finding — those need `board`/
//! `hostsleep`/`seatstate` equivalents this crate's other files own, not
//! this one). See this task's `result.md` for exactly where that boundary
//! was drawn.
//!
//! Reuses `crate::activity::support` for timestamp formatting/parsing and
//! the atomic-write primitive rather than duplicating them.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::activity::support;
use mv_core::redact::redact_text;

/// `notify.py`'s `TITLE_MAX_CHARS` — ntfy's own title limit.
pub const TITLE_MAX_CHARS: usize = 250;

/// `notify.py`'s `BODY_MAX_BYTES` — ntfy's own body limit, counted in UTF-8
/// bytes rather than characters.
pub const BODY_MAX_BYTES: usize = 4096;

const TRUNCATION_MARKER: &str = "…[truncated]";

/// `_truncate(value, limit)` (`notify.py:331-343`): cap a `char`-counted
/// field, appending `"…[truncated]"` in place of whatever was cut.
pub fn truncate_title(value: &str) -> String {
    if value.chars().count() <= TITLE_MAX_CHARS {
        return value.to_string();
    }
    let marker_chars = TRUNCATION_MARKER.chars().count();
    let keep = TITLE_MAX_CHARS.saturating_sub(marker_chars);
    let prefix: String = value.chars().take(keep).collect();
    format!("{prefix}{TRUNCATION_MARKER}")
}

/// `_truncate(value, limit, byte_limit=True)` (`notify.py:331-343`): cap a
/// field by UTF-8 byte length, dropping a trailing byte sequence left
/// incomplete by the cut rather than emitting invalid UTF-8 (Python's own
/// `.decode("utf-8", "ignore")`).
pub fn truncate_body(value: &str) -> String {
    if value.len() <= BODY_MAX_BYTES {
        return value.to_string();
    }
    let marker_bytes = TRUNCATION_MARKER.len();
    let keep = BODY_MAX_BYTES.saturating_sub(marker_bytes).min(value.len());
    let mut end = keep;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &value[..end])
}

/// `notify.py`'s own `send` (`notify.py:832-838`): `redact_text` on
/// project/title/message/tags/dedup_key, before any disk write or push —
/// applied once, here, so every caller of [`send`] gets it for free rather
/// than redacting at each call site.
fn redact_record(record: &QueuedNotification) -> QueuedNotification {
    QueuedNotification {
        project: redact_text(&record.project),
        title: redact_text(&record.title),
        message: redact_text(&record.message),
        dedup_key: record.dedup_key.as_deref().map(redact_text),
        tags: record.tags.iter().map(|tag| redact_text(tag)).collect(),
        ..record.clone()
    }
}

/// `notify.py` — a repeat of the same `(project, dedup_key)` inside this
/// window is suppressed rather than resent.
pub const DEDUP_WINDOW: Duration = Duration::from_secs(30 * 60);

/// `PER_PROJECT_HOURLY_CAP` — bypassed by `Priority::Urgent`.
pub const PER_PROJECT_HOURLY_CAP: usize = 6;

/// `FLEET_HOURLY_CAP` — never bypassed, urgent or not.
pub const FLEET_HOURLY_CAP: usize = 20;

/// `OUTBOX_MAX_LINES` — oldest dropped first once the outbox is this long.
pub const OUTBOX_MAX_LINES: usize = 200;

/// `STALE_CLAIM_AFTER` — a `flush` claim file older than this is assumed
/// to belong to a crashed flush and is reclaimed rather than left to leak
/// forever (`a4ca6fa`, `cca0c1e`).
pub const STALE_CLAIM_AFTER: Duration = Duration::from_secs(10 * 60);

const HOURLY_WINDOW: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("notify state file is not valid JSON: {0}")]
    MalformedState(#[source] serde_json::Error),
}

/// `notify.py`'s `LOCK_FILENAME` (`notify.py:197`) — the `.lock` sibling
/// `_rate_lock`/`_sweep_lock` serialize on.
const RATE_LOCK_FILENAME: &str = "notify-state.json.lock";

fn rate_lock_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RATE_LOCK_FILENAME)
}

/// `mv_core::lock`'s own timeout carries no `io::Error`, so it is folded
/// into [`NotifyError::Io`] here rather than growing `NotifyError` a
/// variant for a failure mode this module otherwise never produces.
fn lock_err(error: mv_core::lock::LockError) -> NotifyError {
    match error {
        mv_core::lock::LockError::Timeout(timeout) => {
            NotifyError::Io(io::Error::other(timeout.to_string()))
        }
        mv_core::lock::LockError::Io(io) => NotifyError::Io(io),
    }
}

/// `notify.py`'s `_rate_lock` (`notify.py:481-516`): one bounded, reentrant
/// exclusive lock on `notify-state.json`'s `.lock` sibling, held across the
/// whole decide -> save -> push -> outbox-mutation span. Reentrant per
/// thread (`mv_core::lock::acquire`), so a caller already holding this lock
/// (e.g. [`send`] calling [`enqueue`]) can acquire it again without
/// self-deadlocking. Fixes `critic-engine-state.md` finding 2: `send` and
/// `enqueue` previously ran their read-decide-write spans with no lock at
/// all, unlike Python which holds this lock across the entire decision and
/// outbox mutation.
fn acquire_rate_lock(state_dir: &Path) -> Result<mv_core::lock::LockGuard, NotifyError> {
    mv_core::lock::acquire(
        &rate_lock_path(state_dir),
        "notify-rate",
        mv_core::lock::LOCK_ACQUIRE_TIMEOUT,
    )
    .map_err(lock_err)
}

/// Whether a priority bypasses the per-project cap. The fleet-wide cap is
/// never bypassed by either value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Normal,
    Urgent,
}

/// The outcomes a replayed send can report. `Sent`/`Suppressed`/`Queued`
/// come from `send` itself (the rate-limiter decision, not the push); `Stale`
/// is `flush`'s own — an episode-backed record whose episode has since
/// closed, dropped rather than delivered as if current (`notify.py`'s own
/// `"stale"` outcome, `notify.py:1089-1094`). `flush` also returns `Queued`
/// without calling `send` when an episode-backed record's table is
/// unreadable (`notify.py:1108-1122`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    Suppressed,
    Queued,
    Stale,
}

/// `suppressed_count`/`suppressed_since` as a pair: Python's own
/// `project_entry` dict never gains either key until `_record_suppressed`
/// first runs (`notify.py:574-578`) — before that, a project that has only
/// ever sent successfully has neither key at all, not `0`/`null`. Flattened
/// into [`ProjectState`] so the two fields still sit at that struct's own
/// JSON level, present together or absent together.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Suppression {
    #[serde(default)]
    suppressed_count: u64,
    #[serde(default)]
    suppressed_since: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProjectState {
    #[serde(default)]
    sends: Vec<String>,
    #[serde(default)]
    titles: BTreeMap<String, String>,
    #[serde(flatten, default)]
    suppression: Option<Suppression>,
}

impl ProjectState {
    fn suppressed_count(&self) -> u64 {
        self.suppression.as_ref().map_or(0, |s| s.suppressed_count)
    }

    fn suppressed_since(&self) -> Option<&str> {
        self.suppression
            .as_ref()
            .and_then(|s| s.suppressed_since.as_deref())
    }

    /// `_record_suppressed` (`notify.py:574-578`): count one held-back
    /// send, stamping `suppressed_since` on the first of a streak.
    fn record_suppressed(&mut self, when: String) {
        let suppression = self.suppression.get_or_insert_with(Suppression::default);
        suppression.suppressed_count += 1;
        if suppression.suppressed_since.is_none() {
            suppression.suppressed_since = Some(when);
        }
    }

    /// Only called once a push has actually reached the network
    /// (`notify.py:915-916`, under `if not pushed: ... return "queued"`'s
    /// early return — the reset two lines below it is never reached on a
    /// queued send) — not at decide time, so a send that merely decides
    /// `Sent` but then queues on an unreachable push leaves a prior
    /// suppression streak untouched, same as Python.
    fn reset_suppression(&mut self) {
        self.suppression = Some(Suppression::default());
    }
}

/// `notify-state.json` (`.multiverse/input/spec-on-disk-contract.md` §A):
/// `{"fleet_sends":[iso,...], "projects":{name:{"sends":[iso],
/// "suppressed_count":int,"suppressed_since":iso|null,
/// "titles":{dedup_key: iso}}}}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifyState {
    #[serde(default)]
    fleet_sends: Vec<String>,
    #[serde(default)]
    projects: BTreeMap<String, ProjectState>,
}

impl NotifyState {
    /// Missing file reads as a fresh, empty state — the same posture every
    /// other derived/first-run state file in this fleet takes.
    pub fn load(path: &Path) -> Result<Self, NotifyError> {
        match fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(NotifyError::MalformedState),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(NotifyError::from(e)),
        }
    }

    /// `tmp+replace, sort_keys=True, indent=2`
    /// (`.multiverse/input/spec-on-disk-contract.md` §A). Converting
    /// through [`serde_json::Value`] rather than serializing the struct
    /// directly is what gets the sorted-keys behavior `sort_keys=True`
    /// promises: this crate does not enable serde_json's `preserve_order`
    /// feature, so `Value::Object` is backed by a `BTreeMap` and always
    /// serializes in key order.
    pub fn save(&self, path: &Path) -> Result<(), NotifyError> {
        let value = serde_json::to_value(self).expect("NotifyState always serializes");
        let mut text = serde_json::to_string_pretty(&value).expect("Value always serializes");
        text.push('\n');
        support::atomic_write(path, text.as_bytes(), 0o600)?;
        Ok(())
    }

    fn project_mut(&mut self, project: &str) -> &mut ProjectState {
        self.projects.entry(project.to_string()).or_default()
    }

    /// `_trim_rate_state` (`cca0c1e`): drop every send/title timestamp past
    /// its own window so both grow bounded instead of forever. Titles use
    /// [`DEDUP_WINDOW`] (the only window a dedup check needs); sends use
    /// the one-hour cap window, since nothing in this module reads a send
    /// older than that.
    fn trim(&mut self, now: OffsetDateTime) {
        self.fleet_sends.retain(|ts| within(ts, now, HOURLY_WINDOW));
        for project in self.projects.values_mut() {
            project.sends.retain(|ts| within(ts, now, HOURLY_WINDOW));
            project.titles.retain(|_, ts| within(ts, now, DEDUP_WINDOW));
        }
    }
}

fn within(timestamp: &str, now: OffsetDateTime, window: Duration) -> bool {
    match support::parse_flexible(timestamp) {
        Some(parsed) => now - parsed < window,
        // A record this module cannot parse is treated as expired rather
        // than kept forever or rejected outright — tolerant-reader posture
        // consistent with the marker/last-ack files in `activity.rs`.
        None => false,
    }
}

/// `send`'s rate-limiter decision (`notify.py`, wired at `cli.cmd_notify`):
/// checked in order — (1) `(project, dedup_key)` dedup within
/// [`DEDUP_WINDOW`]; (2) [`PER_PROJECT_HOURLY_CAP`], bypassed by
/// `Priority::Urgent`; (3) [`FLEET_HOURLY_CAP`], never bypassed. A cap hit
/// is treated exactly like the dedup-window hit above it — suppressed and
/// counted, never queued to the outbox (`notify.py:852-863`: all three
/// checks call `_record_suppressed` and return `"suppressed"`; only an
/// unreachable push past this point ever returns `"queued"`). On
/// `Outcome::Sent`, the send is recorded in `state` **before** the caller
/// pushes anything (`a4ca6fa`: persisting after a push lost the record on
/// a crash in between and caused a duplicate re-delivery on retry) — the
/// caller must call [`NotifyState::save`] with the mutated state and only
/// then attempt the actual push.
pub fn decide_and_record(
    state: &mut NotifyState,
    project: &str,
    dedup_key: &str,
    priority: Priority,
    now: OffsetDateTime,
) -> Outcome {
    state.trim(now);
    let when = support::format_seconds_utc(now);

    let already_sent_recently = state
        .projects
        .get(project)
        .and_then(|p| p.titles.get(dedup_key))
        .is_some_and(|ts| within(ts, now, DEDUP_WINDOW));
    if already_sent_recently {
        state.project_mut(project).record_suppressed(when);
        return Outcome::Suppressed;
    }

    let project_sends = state
        .projects
        .get(project)
        .map(|p| p.sends.len())
        .unwrap_or(0);
    let project_capped = priority != Priority::Urgent && project_sends >= PER_PROJECT_HOURLY_CAP;
    let fleet_capped = state.fleet_sends.len() >= FLEET_HOURLY_CAP;
    if project_capped || fleet_capped {
        state.project_mut(project).record_suppressed(when);
        return Outcome::Suppressed;
    }

    state.fleet_sends.push(when.clone());
    let project_state = state.project_mut(project);
    project_state.sends.push(when.clone());
    project_state.titles.insert(dedup_key.to_string(), when);
    Outcome::Sent
}

/// One notification waiting in the outbox (`notify-outbox.jsonl`): a
/// capped-length, full-rewrite-on-write JSONL file — **not** true POSIX
/// append despite the `.jsonl` name
/// (`.multiverse/input/spec-on-disk-contract.md` §A). Field names/shape
/// match Python's own `_append_outbox` record (`notify.py:607-621`) —
/// `message`/`priority`/`project`/`queued_at`/`tags`/`title` always
/// present, the rest only when set — rather than the previously
/// Rust-only `body`/always-present `dedup_key` shape (hub decision
/// 2026-11-03 09:5xZ, card `20261103-b-50a389`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedNotification {
    pub project: String,
    pub title: String,
    pub message: String,
    /// The ntfy priority string this send was made with, e.g. `"default"`
    /// or `"urgent"` (`notify.py`'s own `args.priority`) — distinct from
    /// [`Priority`], which only ever carries whether the per-project cap
    /// is bypassed.
    pub priority: String,
    /// `notify.py`'s own `dedup_key` kwarg: `None` when the dedup identity
    /// is just `title` (the common case, which Python's `_append_outbox`
    /// omits from the record entirely, `notify.py:616-617`) — `Some` only
    /// when a caller's dedup key differs from its title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dedup_key: Option<String>,
    pub queued_at: String,
    /// The episode this notification belongs to, e.g. `"down"` /
    /// `"recovered"` — used only by [`collapse_down_recovered_pairs`].
    /// Absent for ordinary (non-episodic) queued sends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// `notify --tag`, replayed at `--flush` (`notify.py`'s outbox `tags`).
    /// Always serialized, even empty, matching Python's own
    /// `"tags": list(tags)` (`notify.py:612`).
    #[serde(default)]
    pub tags: Vec<String>,
    /// The key of the [`crate::notify_episodes`] episode this send is
    /// *for*, e.g. the picker-alert key `push_picker_ntfy` builds with
    /// [`crate::notify_episodes::picker_key`], or `notify --episode-key`.
    /// `episode_state` is the path to that episode table (`notify.py`'s own
    /// `episode_key`/`episode_state`, `notify.py:1601-1611`). Both `None`
    /// for an ordinary (non-episodic) send — `flush` never gates those on
    /// anything beyond the rate limiter. Distinct from `kind`/`subject`
    /// above: those name a down/recovered *pair* for collapsing two queued
    /// records into one; these name a single episode's own open/closed
    /// state for dropping a stale replay (card `20261103-b-a2215b`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_key: Option<String>,
    /// `notify --episode-state PATH` — JSON file `flush` re-reads to drop a
    /// queued alert whose episode has closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub episode_state: Option<String>,
}

/// Read every line of `path` as a [`QueuedNotification`], skipping (not
/// failing on) a line that does not parse — `cca0c1e`: an unparsable
/// outbox line must not corrupt or abort loading the rest.
pub fn load_outbox(path: &Path) -> io::Result<Vec<QueuedNotification>> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    Ok(text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<QueuedNotification>(l).ok())
        .collect())
}

/// `json.dumps(record, sort_keys=True)`'s own default separators when
/// `indent` is not given — `(", ", ": ")`, not serde_json's compact `(",",
/// ":")` — so a line this module writes round-trips byte-for-byte through
/// `conformance/diff_fuzz.py`'s text comparison against the same line
/// Python would have written.
#[derive(Default)]
struct PySeparatorFormatter;

impl serde_json::ser::Formatter for PySeparatorFormatter {
    fn begin_object_key<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + io::Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }

    fn begin_array_value<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }
}

fn to_py_json_line(value: &serde_json::Value) -> String {
    let mut buf = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buf, PySeparatorFormatter);
    serde::Serialize::serialize(value, &mut serializer).expect("Value always serializes");
    String::from_utf8(buf).expect("JSON output is always valid UTF-8")
}

/// Full rewrite, oldest-dropped-first past [`OUTBOX_MAX_LINES`],
/// `json.dumps(record, sort_keys=True)` per line (same `Value` round-trip
/// as [`NotifyState::save`], for the same sorted-keys reason).
pub fn save_outbox(path: &Path, records: &[QueuedNotification]) -> io::Result<()> {
    let start = records.len().saturating_sub(OUTBOX_MAX_LINES);
    let mut text = String::new();
    for record in &records[start..] {
        let value = serde_json::to_value(record).expect("QueuedNotification always serializes");
        text.push_str(&to_py_json_line(&value));
        text.push('\n');
    }
    support::atomic_write(path, text.as_bytes(), 0o600)
}

/// Load, append, cap, and rewrite in one call — the shape every caller
/// actually wants (`_append_outbox`). Held under the same rate lock as
/// [`send`]'s decision span (`critic-engine-state.md` finding 2): without
/// it, two concurrent appends — or a concurrent [`claim_outbox`] rename —
/// can race this load-then-save and silently drop or duplicate a queued
/// record.
pub fn enqueue(path: &Path, record: QueuedNotification) -> Result<(), NotifyError> {
    let state_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let _guard = acquire_rate_lock(state_dir)?;
    let mut records = load_outbox(path)?;
    records.push(record);
    save_outbox(path, &records)?;
    Ok(())
}

/// Atomically claim the outbox for a flush: rename it to
/// `<path>.<pid>.claimed` so a concurrent `flush` can't also claim it
/// (`cca0c1e`: unconditional unlink-after-read let a losing concurrent
/// flush crash on an already-removed file). Returns `None` if there was no
/// outbox to claim.
///
/// Held under the same rate lock as [`enqueue`] (`critic-engine-state.md`
/// finding 2): without it, this rename can land between a concurrent
/// `enqueue`'s load and save, so the save recreates the outbox with records
/// this claim already took, and they get replayed twice.
pub fn claim_outbox(path: &Path) -> Result<Option<PathBuf>, NotifyError> {
    let state_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let _guard = acquire_rate_lock(state_dir)?;
    if !path.exists() {
        return Ok(None);
    }
    let claimed = claim_path(path, std::process::id());
    fs::rename(path, &claimed)?;
    Ok(Some(claimed))
}

fn claim_path(outbox_path: &Path, pid: u32) -> PathBuf {
    let mut name = outbox_path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".{pid}.claimed"));
    outbox_path.with_file_name(name)
}

/// Find every `*.claimed` sibling of `outbox_path` whose mtime is older
/// than [`STALE_CLAIM_AFTER`] — left behind by a `flush` that crashed
/// mid-replay (`a4ca6fa`) — load their queued records back out, and remove
/// the stale claim file. The caller is responsible for re-enqueuing
/// whatever of the returned records it did not finish delivering.
///
/// Held under the same rate lock as [`enqueue`]/[`claim_outbox`]
/// (`rereview-engine.md`: "stale claims remain outside the shared lock"):
/// without it, two concurrent `flush` calls could both `load_outbox` the
/// same aged claim file before either removed it, delivering its records
/// twice. Locked, the loser finds the file already gone once it gets its
/// turn.
pub fn reclaim_stale_claims(
    outbox_path: &Path,
    now: std::time::SystemTime,
) -> Result<Vec<QueuedNotification>, NotifyError> {
    let dir = outbox_path.parent().unwrap_or_else(|| Path::new("."));
    let _guard = acquire_rate_lock(dir)?;
    let stem = outbox_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{stem}.");
    let mut reclaimed = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(reclaimed);
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&prefix) || !name.ends_with(".claimed") {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let Ok(age) = now.duration_since(modified) else {
            continue;
        };
        if age < STALE_CLAIM_AFTER {
            continue;
        }
        let claimed_path = entry.path();
        if let Ok(records) = load_outbox(&claimed_path) {
            reclaimed.extend(records);
        }
        let _ = fs::remove_file(&claimed_path);
    }
    Ok(reclaimed)
}

/// Collapse a queued `"down"` + `"recovered"` pair that share the same
/// `subject` into just the `"recovered"` record — a fleet that flapped
/// once before `flush` ever ran should replay one summary, not two
/// contradictory alerts. Records with no `subject`/`kind`, or whose
/// subject appears only once, pass through unchanged. Input order is
/// preserved for everything that is not collapsed.
pub fn collapse_down_recovered_pairs(records: Vec<QueuedNotification>) -> Vec<QueuedNotification> {
    let subjects_with_recovery: std::collections::HashSet<String> = records
        .iter()
        .filter(|r| r.kind.as_deref() == Some("recovered"))
        .filter_map(|r| r.subject.clone())
        .collect();
    records
        .into_iter()
        .filter(|r| {
            let is_down = r.kind.as_deref() == Some("down");
            let subject_recovered = r
                .subject
                .as_deref()
                .is_some_and(|s| subjects_with_recovery.contains(s));
            !(is_down && subject_recovered)
        })
        .collect()
}

/// A delivery attempt can queue because of a rate cap or an unreachable
/// transport. Only the latter needs the CLI's offline warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendResult {
    pub outcome: Outcome,
    pub unreachable: bool,
}

/// Persist rate decisions before calling the transport, and never revert
/// them on a failed push: a push that cannot reach the network still
/// spent its dedup/rate credit the moment it decided, same as Python's own
/// `notify.send` (`notify.py:871-880` saves the attempt's state before
/// `ntfy_push` and never undoes it when `ntfy_push` returns `False`) —
/// matching this means a retry does not get to decide twice. The original
/// record queues behind the unreachable push either way.
///
/// The whole decide -> save -> push -> outbox span runs under the rate
/// lock (`notify.py:840-918`'s `with _rate_lock():`), not just the state
/// read/write — two concurrent sends must not both decide against the same
/// rate snapshot, and a push's outcome must be recorded before either can
/// act on it again (`critic-engine-state.md` finding 2).
pub fn send(
    state_dir: &Path,
    record: &QueuedNotification,
    priority: Priority,
    push: &mut impl FnMut(&QueuedNotification) -> bool,
) -> Result<SendResult, NotifyError> {
    // `notify.py:832-838`: redact first, then bound title/message to ntfy's
    // own limits — both before this record is used as a dedup key, written
    // to disk, or pushed anywhere.
    let mut record = redact_record(record);
    record.title = truncate_title(&record.title);
    record.message = truncate_body(&record.message);
    let record = record;

    let _guard = acquire_rate_lock(state_dir)?;
    let state_path = state_dir.join("notify-state.json");
    let mut state = NotifyState::load(&state_path)?;
    let key = record.dedup_key.as_deref().unwrap_or(&record.title);
    let now = OffsetDateTime::now_utc();
    let outcome = decide_and_record(&mut state, &record.project, key, priority, now);
    if outcome != Outcome::Queued {
        state.save(&state_path)?;
    }
    // `notify.py:862-865`: the pushed body — not the one enqueued on
    // failure or stored anywhere else — gains a "+N suppressed since
    // HH:MMZ" suffix when a prior streak is ending.
    let push_record = if outcome == Outcome::Sent {
        suppressed_suffixed(&state, &record, now)
    } else {
        record.clone()
    };
    let unreachable = outcome == Outcome::Sent && !push(&push_record);
    if outcome == Outcome::Sent && !unreachable {
        // Confirmed delivery, not merely a decision: clears a prior
        // suppression streak only once the push actually reached the
        // network (`notify.py:915-918`), never on a decide that then
        // queues behind an unreachable push.
        state.project_mut(&record.project).reset_suppression();
        state.save(&state_path)?;
    }
    let outcome = if unreachable {
        Outcome::Queued
    } else {
        outcome
    };
    if outcome == Outcome::Queued {
        enqueue(&state_dir.join("notify-outbox.jsonl"), record.clone())?;
    }
    Ok(SendResult {
        outcome,
        unreachable,
    })
}

/// `notify.py:862-865`: once a held-back streak is ending, the delivered
/// body — not the outbox/disk copy — gains `" (+N suppressed since
/// HH:MMZ)"`. `since` falls back to `now` when the streak has no parseable
/// `suppressed_since` (Python's own `since = _parse_iso(...) or now`).
fn suppressed_suffixed(
    state: &NotifyState,
    record: &QueuedNotification,
    now: OffsetDateTime,
) -> QueuedNotification {
    let Some(project) = state.projects.get(&record.project) else {
        return record.clone();
    };
    let count = project.suppressed_count();
    if count == 0 {
        return record.clone();
    }
    let since = project
        .suppressed_since()
        .and_then(support::parse_flexible)
        .unwrap_or(now)
        .to_offset(time::UtcOffset::UTC);
    let mut suffixed = record.clone();
    suffixed.message = format!(
        "{} (+{count} suppressed since {:02}:{:02}Z)",
        record.message,
        since.hour(),
        since.minute(),
    );
    suffixed
}

/// Claim and replay pending records through the same transport. A record
/// that is not stale, not episode-unknown, and not blocked on the sweep's
/// own lock already spent its dedup/rate credit when it first queued
/// (`send`'s own doc comment above), so this does not call `decide_and_record`
/// on it again — it is delivered or re-queued on the transport's own
/// outcome alone (hub decision 2026-11-03 09:5xZ, card `20261103-b-50a389`:
/// "flush delivers queued outbox records without re-running dedup/rate
/// decide ... so a retried flush still delivers"). Failed pushes requeue
/// normally; a fresh claim is removed only after the whole replay. An error
/// retains the claim for stale-claim recovery. An episode-backed record
/// whose table is missing or unreadable is requeued without a send
/// attempt (`notify.py:1108-1122`, card `20261103-b-51b151`). A sweep that
/// already holds the episode-table lock is the same posture: requeue
/// rather than read the still-readable old table (`notify.py:1103-1108`,
/// card `20261103-b-906b57`).
pub fn flush(
    state_dir: &Path,
    push: &mut impl FnMut(&QueuedNotification) -> bool,
) -> Result<Vec<(QueuedNotification, SendResult)>, NotifyError> {
    let outbox = state_dir.join("notify-outbox.jsonl");
    let mut records = reclaim_stale_claims(&outbox, std::time::SystemTime::now())?;
    let claim = claim_outbox(&outbox)?;
    if let Some(path) = &claim {
        records.extend(load_outbox(path)?);
    }
    let mut results = Vec::new();
    for record in collapse_down_recovered_pairs(records) {
        // Critic finding critic-safety.md#6: redaction lives in `send`
        // (`notify.py:832-838`, ported above as `redact_record`), which
        // this loop deliberately does not call again for its dedup/rate
        // decision (see this function's doc comment) — but redaction is
        // not a dedup/rate decision, and dropping it here left a replayed
        // outbox record (one `send` already redacted before its first
        // enqueue, or an older on-disk record predating this function)
        // pushed to ntfy verbatim. Idempotent on already-redacted text, so
        // this costs nothing on the common path.
        let record = redact_record(&record);
        let (acquired, _replay_lock) = episode_replay_lock(record.episode_state.as_deref());
        if !acquired {
            // Sweep holds `notify_episodes::LOCK_FILENAME`. Keep the record
            // queued until a later flush can serialize with that close
            // (`notify.py:1103-1122`, card `20261103-b-906b57`).
            enqueue(&outbox, record.clone())?;
            results.push((
                record,
                SendResult {
                    outcome: Outcome::Queued,
                    unreachable: false,
                },
            ));
            continue;
        }
        if episode_has_closed(&record) {
            // The condition this alert was for has already cleared — drop
            // it rather than page for something no longer true (card
            // `20261103-b-a2215b`, `notify.py`'s own `"stale"` outcome,
            // `notify.py:1089-1094`). Not re-enqueued: an episode that
            // closes once does not reopen by itself, so there is nothing
            // later to replay this against.
            results.push((
                record,
                SendResult {
                    outcome: Outcome::Stale,
                    unreachable: false,
                },
            ));
            continue;
        }
        if episode_is_unknown(&record) {
            // The sweep may be rewriting this table right now. Keep the
            // record queued until a later flush can read a definite
            // open/closed answer (`notify.py:1108-1122`). Not a failed
            // push: the transport is not attempted, so `unreachable` stays
            // false and rate/dedup credit is not consumed.
            enqueue(&outbox, record.clone())?;
            results.push((
                record,
                SendResult {
                    outcome: Outcome::Queued,
                    unreachable: false,
                },
            ));
            continue;
        }
        let delivered = push(&record);
        if !delivered {
            enqueue(&outbox, record.clone())?;
        }
        results.push((
            record,
            SendResult {
                outcome: if delivered {
                    Outcome::Sent
                } else {
                    Outcome::Queued
                },
                unreachable: !delivered,
            },
        ));
    }
    if let Some(path) = claim {
        fs::remove_file(path)?;
    }
    Ok(results)
}

/// `notify.py`'s `_episode_replay_lock` (`notify.py:1004-1010`): a
/// non-blocking flock on the episode table's [`crate::notify_episodes::LOCK_FILENAME`]
/// sibling, held across this record's still-open check and send so a
/// concurrent sweep cannot be mid-close while flush delivers. `acquired`
/// is `true` when there is no episode table (nothing to serialize against)
/// or when this call got the lock. The guard is `None` in the no-table
/// case; it must stay alive until the record's send or drop finishes.
fn episode_replay_lock(episode_state: Option<&str>) -> (bool, Option<mv_core::lock::LockGuard>) {
    let Some(path) = episode_state.filter(|p| !p.is_empty()) else {
        return (true, None);
    };
    let lock_path = Path::new(path).with_file_name(crate::notify_episodes::LOCK_FILENAME);
    match mv_core::lock::acquire(lock_path.as_path(), "notify-episodes", Duration::ZERO) {
        Ok(guard) => (true, Some(guard)),
        Err(_) => (false, None),
    }
}

/// `notify.py`'s `_episode_still_open`: a missing key in a readable JSON
/// object means the episode closed (drop the queued alert). A missing or
/// unreadable file is unknown — [`flush`] keeps the record queued rather
/// than delivering it (`notify.py:1108-1122`, card `20261103-b-51b151`).
/// An object-shaped entry that is not a valid `{first_seen, alerted}`
/// episode is also unknown (`notify.py:998-1000`, card `20261103-b-5922a1`).
fn episode_has_closed(record: &QueuedNotification) -> bool {
    let (Some(path), Some(key)) = (&record.episode_state, &record.episode_key) else {
        return false;
    };
    episode_still_open(Path::new(path), key) == Some(false)
}

fn episode_is_unknown(record: &QueuedNotification) -> bool {
    let (Some(path), Some(key)) = (&record.episode_state, &record.episode_key) else {
        return false;
    };
    episode_still_open(Path::new(path), key).is_none()
}

fn episode_still_open(path: &Path, key: &str) -> Option<bool> {
    let text = fs::read_to_string(path).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&text).ok()?;
    let object = parsed.as_object()?;
    match object.get(key) {
        None => Some(false),
        Some(entry) if valid_state_entry(entry) => Some(
            !entry
                .get("stale")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        ),
        Some(_) => None,
    }
}

/// `notify.py`'s `_valid_state_entry` (`notify.py:1263-1270`): a dict with
/// a parseable `first_seen` and a bool `alerted`.
fn valid_state_entry(value: &serde_json::Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(first_seen) = object.get("first_seen").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if support::parse_flexible(first_seen).is_none() {
        return false;
    }
    object
        .get("alerted")
        .and_then(serde_json::Value::as_bool)
        .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use time::macros::datetime;

    fn t(minutes_from_epoch_start: i64) -> OffsetDateTime {
        datetime!(2026-11-02 00:00:00 UTC)
            + Duration::from_secs((minutes_from_epoch_start * 60) as u64)
    }

    /// `critic-engine-state.md` finding 2: `send` used to load, decide and
    /// save the rate state with no lock at all, so two concurrent senders
    /// could both read the same snapshot and each save their own update,
    /// the second silently discarding the first's (a lost update). Two
    /// threads sending distinct, always-successful notifications at once
    /// must both land in `fleet_sends`, matching Python's `_rate_lock`
    /// (`notify.py:840-918`) holding this whole span exclusive.
    #[test]
    fn concurrent_sends_do_not_lose_a_rate_state_update() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for i in 0..2 {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let record = sample("fleet", &format!("alert-{i}"));
                barrier.wait();
                send(&root, &record, Priority::Normal, &mut |_| true).unwrap()
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            results.iter().all(|r| r.outcome == Outcome::Sent),
            "both distinct, always-successful sends must report Sent: {results:?}"
        );

        let state = NotifyState::load(&root.join("notify-state.json")).unwrap();
        assert_eq!(
            state.fleet_sends.len(),
            2,
            "a lost update under concurrent sends would record only one"
        );
    }

    #[test]
    fn failed_delivery_survives_flush_then_delivers_once() {
        // Matches Python's `notify.send`/`notify.flush`, not the prior Rust
        // invariant this test used to assert: a queued (undelivered) send
        // still commits its dedup/rate credit at decide time
        // (`notify.py:871-880`) rather than reverting it on a failed push,
        // and a retried `flush` redelivers straight through the transport
        // without re-deciding — the credit was already spent when the
        // record first queued (hub decision 2026-11-03 09:5xZ, card
        // `20261103-b-50a389`).
        let dir = tempdir().unwrap();
        let record = sample("sandbox", "disk low");
        let queued = send(dir.path(), &record, Priority::Normal, &mut |_| false).unwrap();
        assert_eq!(queued.outcome, Outcome::Queued);
        assert!(queued.unreachable);
        let state = NotifyState::load(&dir.path().join("notify-state.json")).unwrap();
        assert_eq!(
            state.fleet_sends.len(),
            1,
            "a queued send's dedup/rate credit is recorded at decide time, not reverted"
        );
        let replay = flush(dir.path(), &mut |_| false).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Queued);
        assert!(replay[0].1.unreachable);
        let outbox = dir.path().join("notify-outbox.jsonl");
        assert_eq!(load_outbox(&outbox).unwrap(), vec![record.clone()]);
        // `notify-state.json`, `notify-outbox.jsonl`, and now the rate
        // lock's own `.lock` sibling (`critic-engine-state.md` finding 2;
        // `notify.py`'s own `_rate_lock` leaves the same file behind).
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);

        let mut pushes = 0;
        let delivered = flush(dir.path(), &mut |_| {
            pushes += 1;
            true
        })
        .unwrap();
        assert_eq!(pushes, 1);
        assert_eq!(delivered[0].1.outcome, Outcome::Sent);
        assert!(load_outbox(&outbox).unwrap().is_empty());
        let suppressed = send(dir.path(), &record, Priority::Normal, &mut |_| {
            panic!("a delivered duplicate must not reach the transport")
        })
        .unwrap();
        assert_eq!(suppressed.outcome, Outcome::Suppressed);
    }

    /// `rereview-engine.md` finding "`flush` bypasses rate caps for
    /// cap-queued records": before `e54bb31`, a cap hit returned
    /// `Outcome::Queued` and `flush` replayed every queued record straight
    /// through the transport without re-deciding, so a `flush` right after
    /// a cap-exceeded send delivered it anyway. `e54bb31` made a cap hit
    /// `Outcome::Suppressed` (never enqueued) instead, matching Python's
    /// `send` (`notify.py:852-863`) — this asserts that fix holds end to
    /// end: a cap-exceeded send never reaches the transport, never reaches
    /// the outbox, and a subsequent `flush` has nothing extra to deliver.
    #[test]
    fn flush_does_not_deliver_a_send_suppressed_by_the_per_project_cap() {
        let dir = tempdir().unwrap();
        let mut pushes = 0;
        for i in 0..PER_PROJECT_HOURLY_CAP {
            let record = sample("demo-app", &format!("key-{i}"));
            let result = send(dir.path(), &record, Priority::Normal, &mut |_| {
                pushes += 1;
                true
            })
            .unwrap();
            assert_eq!(result.outcome, Outcome::Sent);
        }
        assert_eq!(pushes, PER_PROJECT_HOURLY_CAP);

        let seventh = sample("demo-app", "key-7");
        let capped = send(dir.path(), &seventh, Priority::Normal, &mut |_| {
            panic!("a cap-exceeded send must not reach the transport")
        })
        .unwrap();
        assert_eq!(capped.outcome, Outcome::Suppressed);

        let outbox = dir.path().join("notify-outbox.jsonl");
        assert!(
            !outbox.exists() || load_outbox(&outbox).unwrap().is_empty(),
            "a cap-exceeded send must not reach the outbox"
        );

        let replayed = flush(dir.path(), &mut |_| {
            pushes += 1;
            true
        })
        .unwrap();
        assert!(
            replayed.is_empty(),
            "flush must not deliver a record that was never queued: {replayed:?}"
        );
        assert_eq!(
            pushes, PER_PROJECT_HOURLY_CAP,
            "flush must not bypass the cap by replaying a suppressed send"
        );
    }

    #[test]
    fn flush_drops_a_picker_alert_whose_episode_has_closed() {
        let dir = tempdir().unwrap();
        let episode_state = dir.path().join("notify-state.json");
        fs::write(&episode_state, "{}").unwrap(); // closed: deleted, not merely marked
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(episode_state.to_string_lossy().into_owned());
        enqueue(&dir.path().join("notify-outbox.jsonl"), record).unwrap();

        let mut pushed = false;
        let replay = flush(dir.path(), &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Stale);
        assert!(
            !pushed,
            "a closed episode's alert must never reach the transport"
        );
        assert!(load_outbox(&dir.path().join("notify-outbox.jsonl"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn flush_still_sends_a_picker_alert_whose_episode_is_still_open() {
        let dir = tempdir().unwrap();
        let episode_state = dir.path().join("notify-state.json");
        fs::write(
            &episode_state,
            r#"{"picker_blocked|sandbox|question":{"first_seen":"2026-11-02T00:00:00Z","alerted":false}}"#,
        )
        .unwrap();
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(episode_state.to_string_lossy().into_owned());
        enqueue(&dir.path().join("notify-outbox.jsonl"), record).unwrap();

        let mut pushed = false;
        let replay = flush(dir.path(), &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Sent);
        assert!(pushed);
    }

    #[test]
    fn flush_requeues_an_episode_backed_record_when_the_table_is_unreadable() {
        // Missing table: `episode_still_open` returns `None` (unknown).
        // Python's flush keeps the record queued (`notify.py:1108-1122`)
        // rather than delivering on a stale guess (card `20261103-b-51b151`).
        let dir = tempdir().unwrap();
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(
            dir.path()
                .join("no-such-file.json")
                .to_string_lossy()
                .into_owned(),
        );
        enqueue(&dir.path().join("notify-outbox.jsonl"), record.clone()).unwrap();

        let mut pushed = false;
        let replay = flush(dir.path(), &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Queued);
        assert!(
            !replay[0].1.unreachable,
            "an unknown episode must not be treated as a failed push"
        );
        assert!(
            !pushed,
            "an unknown episode's alert must never reach the transport"
        );
        assert_eq!(
            load_outbox(&dir.path().join("notify-outbox.jsonl")).unwrap(),
            vec![record]
        );
        assert!(
            !dir.path().join("notify-state.json").exists(),
            "an unknown episode must not consume rate/dedup credit"
        );
    }

    #[test]
    fn flush_requeues_an_episode_backed_record_when_the_table_is_corrupt() {
        let dir = tempdir().unwrap();
        let episode_state = dir.path().join("episodes.json");
        fs::write(&episode_state, "{").unwrap();
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(episode_state.to_string_lossy().into_owned());
        enqueue(&dir.path().join("notify-outbox.jsonl"), record.clone()).unwrap();

        let mut pushed = false;
        let replay = flush(dir.path(), &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Queued);
        assert!(!replay[0].1.unreachable);
        assert!(
            !pushed,
            "a corrupt episode table's alert must never reach the transport"
        );
        assert_eq!(
            load_outbox(&dir.path().join("notify-outbox.jsonl")).unwrap(),
            vec![record]
        );
        assert_eq!(fs::read_to_string(&episode_state).unwrap(), "{");
    }

    #[test]
    fn flush_requeues_an_object_shaped_entry_that_is_not_a_valid_state_entry() {
        // Python `_episode_still_open` returns None unless `_valid_state_entry`
        // (dict with parseable `first_seen` and bool `alerted`,
        // `notify.py:998-1000` / `notify.py:1263-1270`). An object-shaped
        // but otherwise invalid entry must not send (card `20261103-b-5922a1`).
        let dir = tempdir().unwrap();
        let episode_state = dir.path().join("episodes.json");
        fs::write(&episode_state, r#"{"picker_blocked|sandbox|question":{}}"#).unwrap();
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(episode_state.to_string_lossy().into_owned());
        enqueue(&dir.path().join("notify-outbox.jsonl"), record.clone()).unwrap();

        let mut pushed = false;
        let replay = flush(dir.path(), &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Queued);
        assert!(!replay[0].1.unreachable);
        assert!(
            !pushed,
            "an invalid episode entry must never reach the transport"
        );
        assert_eq!(
            load_outbox(&dir.path().join("notify-outbox.jsonl")).unwrap(),
            vec![record]
        );
        assert!(
            !dir.path().join("notify-state.json").exists(),
            "an invalid entry must not consume rate/dedup credit"
        );
        assert_eq!(
            fs::read_to_string(&episode_state).unwrap(),
            r#"{"picker_blocked|sandbox|question":{}}"#
        );
    }

    #[test]
    fn flush_requeues_when_sweep_holds_the_episode_table_lock() {
        // Python flush takes `_episode_replay_lock` (`notify.py:1103-1108`)
        // and requeues when that non-blocking flock is not acquired, so a
        // sweep mid-close cannot have flush deliver from the still-readable
        // old table (card `20261103-b-906b57`). `mv_core::lock::acquire` is
        // reentrant per-thread, so another thread must hold the lock.
        //
        // The episode table lives under its own project's `.multiverse/`,
        // a different directory from the fleet-wide rate-state dir `flush`
        // is given (`notify_episodes`'s own module doc) — a *different*
        // file from the rate lock despite sharing the same filename, so
        // this uses two separate directories rather than one, the way
        // production always does (critic-engine-state.md finding 2's rate
        // lock otherwise collides with this test's own episode lock purely
        // because both sat in the one temp dir).
        let dir = tempdir().unwrap();
        let fleet_dir = dir.path().join("fleet");
        let project_dir = dir.path().join("project");
        fs::create_dir_all(&fleet_dir).unwrap();
        fs::create_dir_all(&project_dir).unwrap();
        let episode_state = project_dir.join("notify-state.json");
        fs::write(
            &episode_state,
            r#"{"picker_blocked|sandbox|question":{"first_seen":"2026-11-02T00:00:00Z","alerted":false}}"#,
        )
        .unwrap();
        let mut record = sample("multiverse", "multiverse: sandbox picker waiting");
        record.episode_key = Some("picker_blocked|sandbox|question".to_string());
        record.episode_state = Some(episode_state.to_string_lossy().into_owned());
        enqueue(&fleet_dir.join("notify-outbox.jsonl"), record.clone()).unwrap();

        let lock_path = episode_state.with_file_name(crate::notify_episodes::LOCK_FILENAME);
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = mv_core::lock::acquire_default(&lock_path, "test").unwrap();
            tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        rx.recv().unwrap();

        let mut pushed = false;
        let replay = flush(&fleet_dir, &mut |_| {
            pushed = true;
            true
        })
        .unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Queued);
        assert!(
            !replay[0].1.unreachable,
            "a lock miss must not be treated as a failed push"
        );
        assert!(
            !pushed,
            "flush must not deliver while sweep holds the episode-table lock"
        );
        assert_eq!(
            load_outbox(&fleet_dir.join("notify-outbox.jsonl")).unwrap(),
            vec![record]
        );

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn a_first_send_succeeds_and_is_recorded() {
        let mut state = NotifyState::default();
        let outcome = decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(0));
        assert_eq!(outcome, Outcome::Sent);
        assert_eq!(state.fleet_sends.len(), 1);
        assert_eq!(state.projects["demo-app"].sends.len(), 1);
    }

    #[test]
    fn a_repeat_within_the_dedup_window_is_suppressed() {
        let mut state = NotifyState::default();
        decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(0));
        let outcome =
            decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(10));
        assert_eq!(outcome, Outcome::Suppressed);
        assert_eq!(state.projects["demo-app"].suppressed_count(), 1);
        assert!(state.projects["demo-app"].suppressed_since().is_some());
    }

    #[test]
    fn a_repeat_after_the_dedup_window_sends_again() {
        let mut state = NotifyState::default();
        decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(0));
        let outcome =
            decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(31));
        assert_eq!(outcome, Outcome::Sent);
    }

    #[test]
    fn the_per_project_cap_queues_a_seventh_send_within_an_hour() {
        let mut state = NotifyState::default();
        for i in 0..PER_PROJECT_HOURLY_CAP {
            let outcome = decide_and_record(
                &mut state,
                "demo-app",
                &format!("key-{i}"),
                Priority::Normal,
                t(i as i64),
            );
            assert_eq!(outcome, Outcome::Sent);
        }
        let seventh = decide_and_record(&mut state, "demo-app", "key-7", Priority::Normal, t(7));
        assert_eq!(seventh, Outcome::Suppressed);
        assert_eq!(state.projects["demo-app"].suppressed_count(), 1);
    }

    #[test]
    fn urgent_bypasses_the_per_project_cap_but_not_the_fleet_cap() {
        let mut state = NotifyState::default();
        for i in 0..PER_PROJECT_HOURLY_CAP {
            decide_and_record(
                &mut state,
                "demo-app",
                &format!("key-{i}"),
                Priority::Normal,
                t(i as i64),
            );
        }
        let urgent = decide_and_record(
            &mut state,
            "demo-app",
            "urgent-key",
            Priority::Urgent,
            t(10),
        );
        assert_eq!(urgent, Outcome::Sent, "urgent bypasses the per-project cap");

        // Now exhaust the fleet-wide cap, spread across enough other
        // projects that none of them individually hits its own
        // per-project cap (which would otherwise confound this check).
        let remaining = FLEET_HOURLY_CAP - PER_PROJECT_HOURLY_CAP - 1;
        for i in 0..remaining {
            let project = format!("other-{}", i / PER_PROJECT_HOURLY_CAP);
            decide_and_record(
                &mut state,
                &project,
                &format!("key-{i}"),
                Priority::Normal,
                t(20 + i as i64),
            );
        }
        assert_eq!(state.fleet_sends.len(), FLEET_HOURLY_CAP);
        let urgent_over_fleet_cap = decide_and_record(
            &mut state,
            "demo-app",
            "another-urgent",
            Priority::Urgent,
            t(40),
        );
        assert_eq!(
            urgent_over_fleet_cap,
            Outcome::Suppressed,
            "the fleet cap is never bypassed"
        );
    }

    #[test]
    fn state_round_trips_through_save_and_load_with_sorted_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-state.json");
        let mut state = NotifyState::default();
        decide_and_record(&mut state, "demo-app", "disk-low", Priority::Normal, t(0));
        state.save(&path).unwrap();

        let text = fs::read_to_string(&path).unwrap();
        // `projects` sorts before `fleet_sends`... no: alphabetically
        // "fleet_sends" < "projects", confirming key order is sorted, not
        // struct-declaration order (which declares fleet_sends first too,
        // so this also checks nested maps sort, e.g. "sends" < "titles").
        let sends_pos = text.find("\"sends\"").unwrap();
        let titles_pos = text.find("\"titles\"").unwrap();
        assert!(sends_pos < titles_pos);

        let loaded = NotifyState::load(&path).unwrap();
        assert_eq!(loaded.fleet_sends, state.fleet_sends);
    }

    #[test]
    fn missing_state_file_loads_as_empty() {
        let dir = tempdir().unwrap();
        let state = NotifyState::load(&dir.path().join("notify-state.json")).unwrap();
        assert!(state.fleet_sends.is_empty());
    }

    fn sample(project: &str, title: &str) -> QueuedNotification {
        QueuedNotification {
            project: project.to_string(),
            title: title.to_string(),
            message: "body".to_string(),
            priority: "default".to_string(),
            dedup_key: None,
            queued_at: support::now_stamp(),
            kind: None,
            subject: None,
            tags: Vec::new(),
            episode_key: None,
            episode_state: None,
        }
    }

    #[test]
    fn enqueue_appends_and_caps_at_oldest_dropped_first() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        for i in 0..(OUTBOX_MAX_LINES + 5) {
            enqueue(&path, sample("demo-app", &format!("n{i}"))).unwrap();
        }
        let records = load_outbox(&path).unwrap();
        assert_eq!(records.len(), OUTBOX_MAX_LINES);
        assert_eq!(records.first().unwrap().title, "n5");
        assert_eq!(
            records.last().unwrap().title,
            format!("n{}", OUTBOX_MAX_LINES + 4)
        );
    }

    #[test]
    fn load_outbox_skips_unparsable_lines_instead_of_failing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        let good = sample("demo-app", "ok");
        let good_line = serde_json::to_string(&good).unwrap();
        fs::write(&path, format!("not json at all\n{good_line}\n")).unwrap();
        let records = load_outbox(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].title, "ok");
    }

    #[test]
    fn missing_outbox_loads_as_empty() {
        let dir = tempdir().unwrap();
        let records = load_outbox(&dir.path().join("notify-outbox.jsonl")).unwrap();
        assert!(records.is_empty());
    }

    #[test]
    fn claim_outbox_renames_atomically_and_a_second_claim_finds_nothing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        enqueue(&path, sample("demo-app", "one")).unwrap();

        let claimed = claim_outbox(&path).unwrap().unwrap();
        assert!(claimed.to_string_lossy().contains(".claimed"));
        assert!(!path.exists());

        let second_claim = claim_outbox(&path).unwrap();
        assert!(second_claim.is_none());
    }

    #[test]
    fn a_fresh_claim_is_not_reclaimed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        enqueue(&path, sample("demo-app", "one")).unwrap();
        claim_outbox(&path).unwrap();

        let reclaimed = reclaim_stale_claims(&path, std::time::SystemTime::now()).unwrap();
        assert!(reclaimed.is_empty());
    }

    #[test]
    fn a_stale_claim_is_reclaimed_and_removed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        enqueue(&path, sample("demo-app", "one")).unwrap();
        let claimed = claim_outbox(&path).unwrap().unwrap();

        let far_future = std::time::SystemTime::now() + STALE_CLAIM_AFTER + Duration::from_secs(60);
        let reclaimed = reclaim_stale_claims(&path, far_future).unwrap();
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].title, "one");
        assert!(!claimed.exists(), "a reclaimed stale claim file is removed");
    }

    /// `rereview-engine.md` finding "stale claims remain outside the shared
    /// lock": `reclaim_stale_claims`'s read-then-remove ran with no lock at
    /// all, so two concurrent `flush` calls could both `load_outbox` the
    /// same aged `.claimed` file before either removed it, delivering the
    /// same records twice. Two threads racing this call through a barrier,
    /// both backdating the same stale candidate, must split the records
    /// exactly once between them, matching the same-file exclusivity
    /// [`claim_outbox`]/[`enqueue`] already get from this lock.
    #[test]
    fn concurrent_flushes_reclaim_a_stale_claim_exactly_once() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tempdir().unwrap();
        let path = dir.path().join("notify-outbox.jsonl");
        enqueue(&path, sample("demo-app", "one")).unwrap();
        claim_outbox(&path).unwrap();

        let far_future = std::time::SystemTime::now() + STALE_CLAIM_AFTER + Duration::from_secs(60);
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                reclaim_stale_claims(&path, far_future).unwrap()
            }));
        }
        let total: usize = handles.into_iter().map(|h| h.join().unwrap().len()).sum();
        assert_eq!(
            total, 1,
            "a stale claim must be reclaimed by exactly one of two racing flushes, not both"
        );
    }

    #[test]
    fn collapse_keeps_only_the_recovered_half_of_a_matching_pair() {
        let mut down = sample("demo-app", "demo-app is down");
        down.kind = Some("down".to_string());
        down.subject = Some("demo-app".to_string());
        let mut recovered = sample("demo-app", "demo-app recovered");
        recovered.kind = Some("recovered".to_string());
        recovered.subject = Some("demo-app".to_string());
        let mut unrelated = sample("other", "unrelated");
        unrelated.kind = Some("down".to_string());
        unrelated.subject = Some("other".to_string());

        let collapsed =
            collapse_down_recovered_pairs(vec![down, recovered.clone(), unrelated.clone()]);
        assert_eq!(collapsed, vec![recovered, unrelated]);
    }

    #[test]
    fn collapse_leaves_an_unpaired_down_alone() {
        let mut down = sample("demo-app", "demo-app is down");
        down.kind = Some("down".to_string());
        down.subject = Some("demo-app".to_string());
        let collapsed = collapse_down_recovered_pairs(vec![down.clone()]);
        assert_eq!(collapsed, vec![down]);
    }

    #[test]
    fn flush_drops_a_queued_alert_whose_episode_has_closed() {
        let dir = tempdir().unwrap();
        let episodes = dir.path().join("episodes.json");
        fs::write(
            &episodes,
            r#"{"other":{"first_seen":"2026-01-01T00:00:00Z"}}"#,
        )
        .unwrap();
        let mut record = sample("demo-app", "host asleep");
        record.episode_key = Some("host-sleep".into());
        record.episode_state = Some(episodes.display().to_string());
        enqueue(&dir.path().join("notify-outbox.jsonl"), record).unwrap();
        let replay = flush(dir.path(), &mut |_| panic!("closed episode must not push")).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Stale);
    }

    /// Scout gap #1: `notify.py:832-838` redacts `project`/`title`/
    /// `message`/`dedup_key` before any disk write or push; the Rust `send`
    /// used to write a secret straight through to `notify-outbox.jsonl`,
    /// `notify-state.json`, and the pushed record.
    #[test]
    fn a_bearer_token_in_the_message_never_reaches_disk_or_the_push() {
        let dir = tempdir().unwrap();
        let mut record = sample("demo-app", "leak-check");
        record.message = "see Bearer abcdefghijklmnop for access".to_string();
        let mut pushed_message = String::new();
        let result = send(dir.path(), &record, Priority::Normal, &mut |r| {
            pushed_message = r.message.clone();
            false // unreachable: forces the record into the outbox too
        })
        .unwrap();
        assert_eq!(result.outcome, Outcome::Queued);
        assert!(!pushed_message.contains("abcdefghijklmnop"));

        let outbox_text = fs::read_to_string(dir.path().join("notify-outbox.jsonl")).unwrap();
        assert!(!outbox_text.contains("abcdefghijklmnop"));

        let state_text = fs::read_to_string(dir.path().join("notify-state.json")).unwrap();
        assert!(!state_text.contains("abcdefghijklmnop"));
    }

    /// Critic finding critic-safety.md#6: redaction lives in `send`
    /// (`notify.py:832-838`), which only closes the forward path — `flush`
    /// replays `notify-outbox.jsonl` verbatim, so a secret already on disk
    /// (written by a pre-`a` binary, or any record this file predates)
    /// still reached ntfy unredacted. Writes a Bearer token straight to the
    /// outbox file, bypassing `send` entirely, the way an old on-disk
    /// record would already be sitting there.
    #[test]
    fn flush_redacts_a_replayed_outbox_record_before_pushing_it() {
        let dir = tempdir().unwrap();
        let mut record = sample("demo-app", "leak-check");
        record.message = "see Bearer abcdefghijklmnop for access".to_string();
        enqueue(&dir.path().join("notify-outbox.jsonl"), record).unwrap();

        let mut pushed_message = String::new();
        let replay = flush(dir.path(), &mut |r| {
            pushed_message = r.message.clone();
            true
        })
        .unwrap();

        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].1.outcome, Outcome::Sent);
        assert!(
            !pushed_message.contains("abcdefghijklmnop"),
            "{pushed_message}"
        );
    }

    /// Scout gap #19: `notify.py:862-865` appends `"(+N suppressed since
    /// HH:MMZ)"` to the first delivered body once a suppressed streak ends.
    #[test]
    fn a_delivered_send_reports_how_many_were_suppressed_before_it() {
        let dir = tempdir().unwrap();
        let record = sample("demo-app", "disk-low");
        // Exhaust the per-project cap with distinct keys so three more
        // sends of this exact (project, title) are suppressed by dedup,
        // not by the cap.
        send(dir.path(), &record, Priority::Normal, &mut |_| true).unwrap();
        for _ in 0..3 {
            let suppressed = send(dir.path(), &record, Priority::Normal, &mut |_| {
                panic!("a suppressed send must not reach the transport")
            })
            .unwrap();
            assert_eq!(suppressed.outcome, Outcome::Suppressed);
        }

        // A different dedup key in the same project is not itself
        // suppressed, but the project's streak still applies to it.
        let mut next_record = record.clone();
        next_record.title = "disk-low-2".to_string();
        let mut pushed_message = String::new();
        send(dir.path(), &next_record, Priority::Normal, &mut |r| {
            pushed_message = r.message.clone();
            true
        })
        .unwrap();
        assert!(
            pushed_message.contains("(+3 suppressed since") && pushed_message.ends_with("Z)"),
            "pushed message was {pushed_message:?}"
        );
    }

    #[test]
    fn truncate_title_bounds_to_title_max_chars_with_a_marker() {
        let long = "x".repeat(TITLE_MAX_CHARS + 50);
        let truncated = truncate_title(&long);
        assert_eq!(truncated.chars().count(), TITLE_MAX_CHARS);
        assert!(truncated.ends_with("…[truncated]"));
        assert_eq!(truncate_title("short"), "short");
    }

    #[test]
    fn truncate_body_bounds_to_body_max_bytes_with_a_marker() {
        let long = "y".repeat(BODY_MAX_BYTES + 500);
        let truncated = truncate_body(&long);
        assert!(truncated.len() <= BODY_MAX_BYTES);
        assert!(truncated.ends_with("…[truncated]"));
        assert_eq!(truncate_body("short"), "short");
    }
}

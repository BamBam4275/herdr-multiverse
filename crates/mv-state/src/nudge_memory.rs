//! One-shot claim memories backing the sweep nudge helpers
//! (`mv_engine::sweep_nudges`, card `20261102-b-b4ede0`): `finish-nudge-
//! memory.json`, `stuck-resume-memory.json`, `runtime-cap-memory.json`,
//! the two-pass `stuck-signal-memory.json` a confirmed-stuck verdict needs
//! before a resume nudge may ever claim its own slot,
//! `stranded-trip-memory.json` (`accounts reset`'s still-parked pane
//! snapshot, card `20261103-b-ffa660`), and `stuck-resume-send-
//! failures.json`/`runtime-cap-send-failures.json` (critic `3c0098`
//! finding 4, task `20261118-builder-6b9075`: consecutive-failed-send
//! counts bounding how long a send outage retries before the
//! resume/cap claim is left spent to arm its own escalation leg instead),
//! and the Rust-only stall-wake pair (card `20261122-b-3acaeb`):
//! `stall-wake-memory.json`, the one-shot claim for a Claude lane whose
//! background job finished without waking it, and `idle-pane-memory.json`,
//! the per-pane fingerprint-plus-first-seen record that measures how long
//! such a pane has sat unchanged. That clock is seeded only on an applying
//! sweep pass (card `20261122-b-7378a4`); `--dry-run`, a halt, and
//! `MULTIVERSE_READ_ONLY=1` read it and never write.
//!
//! One file per project under `.multiverse/`, mirroring Python's own
//! `_finish_nudge_memory_path` & co (`tasks.py`) byte-for-byte: same
//! filenames, same flat JSON shape, so a project swept by both a Python and
//! a Rust build shares one claim ledger rather than each nudging past the
//! other's memory.
//!
//! Every read fails open (`{}` on a missing or corrupt file — "no claims
//! yet"); every claim fails closed (no claim recorded, caller does not
//! send) on a lock or write error — the same posture reversal Python's own
//! `_claim_finish_nudge` docstring explains: a write error here recurs
//! every sweep pass, so failing open would resend unboundedly rather than
//! miss one nudge.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const FINISH_NUDGE_MEMORY_FILENAME: &str = "finish-nudge-memory.json";
const RUNTIME_CAP_MEMORY_FILENAME: &str = "runtime-cap-memory.json";
const STUCK_RESUME_MEMORY_FILENAME: &str = "stuck-resume-memory.json";
const STUCK_SIGNAL_MEMORY_FILENAME: &str = "stuck-signal-memory.json";
/// Rust-only (card `20261122-b-3acaeb`); Python has no stall wake.
const STALL_WAKE_MEMORY_FILENAME: &str = "stall-wake-memory.json";
/// Rust-only (card `20261122-b-3acaeb`): see [`idle_pane_since`].
const IDLE_PANE_MEMORY_FILENAME: &str = "idle-pane-memory.json";
/// Python `tasks._STRANDED_TRIP_MEMORY_FILENAME`.
pub const STRANDED_TRIP_MEMORY_FILENAME: &str = "stranded-trip-memory.json";
/// Same bound `mv_engine::breaker::ESCALATION_RESET_WINDOW` uses. Duplicated
/// here so `mv-state` does not depend on `mv-engine`.
const STRANDED_TRIP_RETENTION: time::Duration = time::Duration::minutes(30);
/// A one-shot claim in `finish-nudge-memory.json`/`runtime-cap-memory.json`
/// (both backed by [`load_claim_map`]) or `stuck-resume-memory.json` can no
/// longer matter once its task is done being nudged: either its task has a
/// recorded terminal outcome in the project's routing log — read once per
/// prune pass via [`mv_state::tasks::sweep_tasks`]'s own `SweepTask::
/// recorded`, never a second log scan per entry — or the claim has simply
/// sat unrecorded long enough that nothing could still be waiting on it.
/// Fourteen days is `worktrees::WORKTREE_STATE_RETENTION`, the retention
/// that module already gives an abandoned worktree-state copy it cannot
/// otherwise prove redundant; duplicated here (that constant is private to
/// its own module, and measured in `std::time::Duration` there against
/// `time::Duration` here) rather than imported, since an unrecorded claim
/// this old has just as thoroughly outlived anything that could still need
/// it. A first attempt at this pruning (task `20261118-builder-678fd1`)
/// used a flat 24h age cutoff with no recorded-outcome check at all; that
/// re-armed a still-RUNNING lane's nudge/resume/cap prompt a second time,
/// and dropped a stuck-resume claim's recorded pane/fingerprint, the
/// moment a lane lived past a day — which a host sleep alone can cause
/// (task `20261118-builder-bda758`).
const UNRECORDED_CLAIM_RETENTION: time::Duration = time::Duration::days(14);

fn memory_path(project_root: &Path, filename: &str) -> PathBuf {
    project_root.join(".multiverse").join(filename)
}

fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// SHA-256 of `text`, matching Python's `_stranded_banner_fingerprint`/
/// `_stuck_signal_confirmed`. Python hashes with `errors="surrogatepass"`
/// to tolerate a lone surrogate from a garbled pane dump; a Rust `&str` can
/// never hold one (it is always valid UTF-8), so hashing the UTF-8 bytes
/// directly is already the same operation for every text this ever sees.
pub fn fingerprint(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An entry is dropped on every read when its task is in `recorded` (it can
/// no longer be nudged — done being nudged, not merely done being waited
/// on) or when it is older than [`UNRECORDED_CLAIM_RETENTION`]; an
/// unrecorded entry younger than that is always kept, no matter how long
/// its task has been running. `recorded` is the caller's own routing-log
/// read, gathered once per prune pass and shared across every entry here —
/// never re-derived per entry.
fn load_claim_map(
    path: &Path,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> BTreeMap<String, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(raw) = serde_json::from_str::<BTreeMap<String, String>>(&text) else {
        return BTreeMap::new();
    };
    let cutoff = now - UNRECORDED_CLAIM_RETENTION;
    raw.into_iter()
        .filter(|(task_id, sent_at)| {
            if recorded.contains(task_id.as_str()) {
                return false;
            }
            mv_core::time::parse_flexible(sent_at).is_ok_and(|observed| observed >= cutoff)
        })
        .collect()
}

fn save_claim_map(path: &Path, map: &BTreeMap<String, String>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(map).expect("claim map always serializes");
    mv_core::atomic::write_atomic(path, &bytes)
}

fn already_claimed(
    project_root: &Path,
    filename: &str,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> bool {
    load_claim_map(
        &memory_path(project_root, filename),
        recorded,
        mv_core::time::now_utc(),
    )
    .contains_key(task_id)
}

/// Reserve `task_id`'s one claim in `filename`'s memory, atomically.
/// `true` only for the call that wins the reservation — two sweeps racing
/// the same candidate still send exactly one prompt between them.
fn claim_once(
    project_root: &Path,
    filename: &str,
    task_id: &str,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> bool {
    let path = memory_path(project_root, filename);
    let Ok(_guard) = mv_core::lock::acquire_default(&lock_path(&path), filename) else {
        return false;
    };
    let mut map = load_claim_map(&path, recorded, now);
    if map.contains_key(task_id) {
        return false;
    }
    map.insert(task_id.to_string(), mv_core::time::format_seconds_utc(now));
    save_claim_map(&path, &map).is_ok()
}

/// Whether `task_id` has already had its one finish nudge
/// (`FINISH_NUDGE_MEMORY_FILENAME`). Read-only, so a renderer can report
/// what already happened without sending anything itself. `recorded` is
/// the caller's own routing-log read (see `UNRECORDED_CLAIM_RETENTION`),
/// gathered once per prune pass.
pub fn finish_nudge_already_sent(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> bool {
    already_claimed(
        project_root,
        FINISH_NUDGE_MEMORY_FILENAME,
        task_id,
        recorded,
    )
}

/// Atomically reserve `task_id`'s one finish nudge.
pub fn claim_finish_nudge(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> bool {
    claim_once(
        project_root,
        FINISH_NUDGE_MEMORY_FILENAME,
        task_id,
        recorded,
        now,
    )
}

/// Whether `task_id` has already had its one runtime-cap nudge
/// (`RUNTIME_CAP_MEMORY_FILENAME`).
pub fn runtime_cap_already_sent(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> bool {
    already_claimed(project_root, RUNTIME_CAP_MEMORY_FILENAME, task_id, recorded)
}

/// Atomically reserve `task_id`'s one runtime-cap nudge.
pub fn claim_runtime_cap(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> bool {
    claim_once(
        project_root,
        RUNTIME_CAP_MEMORY_FILENAME,
        task_id,
        recorded,
        now,
    )
}

/// Undo a [`claim_runtime_cap`] reservation whose send never actually went
/// out, so the next qualifying pass can claim and retry it instead of
/// finding a permanently spent claim that got neither a cap prompt nor,
/// later, the cap escalation this memory also backs (Scout
/// `20261117-scout-342332` rank 6; mirrors [`release_stuck_resume_claim`]).
/// Best effort, matching every other write here: a lock or write failure
/// just leaves the claim spent, same as before this existed.
pub fn release_runtime_cap_claim(project_root: &Path, task_id: &str, recorded: &BTreeSet<&str>) {
    let path = memory_path(project_root, RUNTIME_CAP_MEMORY_FILENAME);
    let Ok(_guard) = mv_core::lock::acquire_default(&lock_path(&path), RUNTIME_CAP_MEMORY_FILENAME)
    else {
        return;
    };
    let mut map = load_claim_map(&path, recorded, mv_core::time::now_utc());
    map.remove(task_id);
    let _ = save_claim_map(&path, &map);
}

/// Whether `task_id` has already had its one stall wake
/// (`STALL_WAKE_MEMORY_FILENAME`), the same flat claim map, pruning and
/// read-only contract as [`finish_nudge_already_sent`].
pub fn stall_wake_already_sent(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> bool {
    already_claimed(project_root, STALL_WAKE_MEMORY_FILENAME, task_id, recorded)
}

/// Atomically reserve `task_id`'s one stall wake. Like
/// [`claim_finish_nudge`], the claim is kept even when the send that
/// follows fails: the one wake this task gets is spent either way.
pub fn claim_stall_wake(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> bool {
    claim_once(
        project_root,
        STALL_WAKE_MEMORY_FILENAME,
        task_id,
        recorded,
        now,
    )
}

const RUNTIME_CAP_FAILURE_MEMORY_FILENAME: &str = "runtime-cap-send-failures.json";
const STUCK_RESUME_FAILURE_MEMORY_FILENAME: &str = "stuck-resume-send-failures.json";

fn load_failure_map(path: &Path) -> BTreeMap<String, u32> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_failure_map(path: &Path, map: &BTreeMap<String, u32>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(map).expect("failure map always serializes");
    mv_core::atomic::write_atomic(path, &bytes)
}

/// Bump `task_id`'s consecutive-failed-send count in `filename`'s memory,
/// atomically, returning the count *after* this failure (`0` on a lock or
/// write error — the caller then treats this attempt as if it were the
/// first, matching every other best-effort write here: a storage failure
/// must not itself force the bounded-retry path closed). A send that
/// later succeeds never clears this count; the task's own one-shot claim
/// is spent or armed by then regardless, so the stray counter entry is
/// inert, the same already-accepted unbounded-task-id-map shape every
/// other claim memory in this module has.
fn record_send_failure(project_root: &Path, filename: &str, task_id: &str) -> u32 {
    let path = memory_path(project_root, filename);
    let Ok(_guard) = mv_core::lock::acquire_default(&lock_path(&path), filename) else {
        return 0;
    };
    let mut map = load_failure_map(&path);
    let count = map.entry(task_id.to_string()).or_insert(0);
    *count = count.saturating_add(1);
    let attempts = *count;
    let _ = save_failure_map(&path, &map);
    attempts
}

/// Finding 4 (critic `3c0098`, task `20261118-builder-6b9075`): how many
/// consecutive sends `mv_engine::sweep_nudges::resume_stuck_lanes` has
/// now failed for `task_id`'s one-shot resume claim — new, dedicated
/// memory rather than a field on [`StuckResumeClaim`], since that claim's
/// own `pane`/`fingerprint` already mean "a send went out," and an
/// attempt count must keep counting through every release-and-reclaim
/// cycle a failing send causes. The caller bounds retries against this
/// count; once the bound is spent it arms the escalation safety net
/// instead of releasing the claim again.
pub fn record_stuck_resume_send_failure(project_root: &Path, task_id: &str) -> u32 {
    record_send_failure(project_root, STUCK_RESUME_FAILURE_MEMORY_FILENAME, task_id)
}

/// Mirrors [`record_stuck_resume_send_failure`] for
/// `mv_engine::sweep_nudges::cap_long_running_lanes`'s own one-shot
/// runtime-cap claim.
pub fn record_runtime_cap_send_failure(project_root: &Path, task_id: &str) -> u32 {
    record_send_failure(project_root, RUNTIME_CAP_FAILURE_MEMORY_FILENAME, task_id)
}

/// One task's resume-nudge claim. `pane`/`fingerprint` are populated only
/// after a successfully sent WORKING-pane resume
/// ([`remember_stuck_resume_signal`]) — mirrors Python's own
/// `_load_stuck_resume_memory`, which also accepts a bare legacy
/// `{task_id: iso-string}` entry (pre-dating these fields) by reading it as
/// `{"sent_at": entry}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StuckResumeClaim {
    /// When this claim was made (or, for a legacy bare-string entry, the
    /// stored timestamp itself), formatted by `mv_core::time`.
    pub sent_at: String,
    /// The pane the resume nudge was actually sent to, recorded only once
    /// [`remember_stuck_resume_signal`] confirms the send; `None` for a
    /// freshly reserved claim or an imported legacy entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    /// Fingerprint of the exact pane text the nudge was sent for, set
    /// alongside `pane`; lets a later pass detect "still the same banner"
    /// versus a different pane now occupying this claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// Mirrors [`load_claim_map`]'s own recorded-outcome-or-[`UNRECORDED_CLAIM_
/// RETENTION`] cutoff: an entry is dropped on every read once its task is
/// in `recorded`, or once its `sent_at` is older than that retention,
/// whichever comes first; an unrecorded entry younger than that is kept
/// regardless of its task's own runtime.
fn load_stuck_resume_map(
    path: &Path,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> BTreeMap<String, StuckResumeClaim> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(serde_json::Value::Object(raw)) = serde_json::from_str(&text) else {
        return BTreeMap::new();
    };
    let cutoff = now - UNRECORDED_CLAIM_RETENTION;
    raw.into_iter()
        .filter_map(|(task_id, value)| {
            if recorded.contains(task_id.as_str()) {
                return None;
            }
            let claim = match value {
                serde_json::Value::String(sent_at) => StuckResumeClaim {
                    sent_at,
                    pane: None,
                    fingerprint: None,
                },
                serde_json::Value::Object(obj) => StuckResumeClaim {
                    sent_at: obj.get("sent_at")?.as_str()?.to_string(),
                    pane: obj.get("pane").and_then(|v| v.as_str()).map(str::to_owned),
                    fingerprint: obj
                        .get("fingerprint")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned),
                },
                _ => return None,
            };
            let observed = mv_core::time::parse_flexible(&claim.sent_at).ok()?;
            if observed < cutoff {
                return None;
            }
            Some((task_id, claim))
        })
        .collect()
}

fn save_stuck_resume_map(
    path: &Path,
    map: &BTreeMap<String, StuckResumeClaim>,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(map).expect("claim map always serializes");
    mv_core::atomic::write_atomic(path, &bytes)
}

/// `task_id`'s resume-nudge claim, if one has already been made. `recorded`
/// is the caller's own routing-log read (see `UNRECORDED_CLAIM_RETENTION`),
/// gathered once per prune pass.
pub fn stuck_resume_claim(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> Option<StuckResumeClaim> {
    load_stuck_resume_map(
        &memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME),
        recorded,
        mv_core::time::now_utc(),
    )
    .remove(task_id)
}

/// Whether `task_id` has already had its one resume nudge.
pub fn stuck_resume_already_sent(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
) -> bool {
    stuck_resume_claim(project_root, task_id, recorded).is_some()
}

/// Atomically reserve `task_id`'s one resume nudge. The claim is only
/// truly spent once [`remember_stuck_resume_signal`] confirms the send; a
/// caller whose send fails must release it with
/// [`release_stuck_resume_claim`] so a later pass can retry rather than
/// finding a claim with no recorded `pane`/`fingerprint` it can never match.
pub fn claim_stuck_resume(
    project_root: &Path,
    task_id: &str,
    recorded: &BTreeSet<&str>,
    now: OffsetDateTime,
) -> bool {
    let path = memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_RESUME_MEMORY_FILENAME)
    else {
        return false;
    };
    let mut map = load_stuck_resume_map(&path, recorded, now);
    if map.contains_key(task_id) {
        return false;
    }
    map.insert(
        task_id.to_string(),
        StuckResumeClaim {
            sent_at: mv_core::time::format_seconds_utc(now),
            pane: None,
            fingerprint: None,
        },
    );
    save_stuck_resume_map(&path, &map).is_ok()
}

/// Bind a successfully sent WORKING-pane resume nudge to its exact pane
/// text, so a later pass can tell "still showing the same connection-loss
/// banner" from "a different pane, or different text, now occupies this
/// claim." A failure to persist leaves the one-time claim intact.
pub fn remember_stuck_resume_signal(
    project_root: &Path,
    task_id: &str,
    pane: &str,
    text: &str,
    recorded: &BTreeSet<&str>,
) {
    let path = memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_RESUME_MEMORY_FILENAME)
    else {
        return;
    };
    let mut map = load_stuck_resume_map(&path, recorded, mv_core::time::now_utc());
    let Some(entry) = map.get_mut(task_id) else {
        return;
    };
    entry.pane = Some(pane.to_string());
    entry.fingerprint = Some(fingerprint(text));
    let _ = save_stuck_resume_map(&path, &map);
}

/// Undo a [`claim_stuck_resume`] reservation whose send never actually
/// went out, so the next qualifying pass can claim and retry it instead of
/// finding a permanently unmatchable `pane: None` entry
/// (`resume_stuck_lanes`'s escalation branch requires `previous.pane` to
/// equal the current pane, which a freshly reserved, never-confirmed claim
/// can never satisfy). A no-op if the claim was already confirmed (has a
/// `pane` recorded) or already removed — never undoes a real send. Best
/// effort, matching every other write here: a lock or write failure just
/// leaves the claim spent, same as before this existed.
pub fn release_stuck_resume_claim(project_root: &Path, task_id: &str, recorded: &BTreeSet<&str>) {
    let path = memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_RESUME_MEMORY_FILENAME)
    else {
        return;
    };
    let mut map = load_stuck_resume_map(&path, recorded, mv_core::time::now_utc());
    if map.get(task_id).is_some_and(|claim| claim.pane.is_none()) {
        map.remove(task_id);
        let _ = save_stuck_resume_map(&path, &map);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StuckSignalEntry {
    fingerprint: String,
    observed_at: String,
    #[serde(default)]
    confirmed: bool,
}

fn load_stuck_signal_map(
    path: &Path,
    live_panes: Option<&[String]>,
    now: OffsetDateTime,
) -> BTreeMap<String, StuckSignalEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(raw) = serde_json::from_str::<BTreeMap<String, StuckSignalEntry>>(&text) else {
        return BTreeMap::new();
    };
    let cutoff = now - time::Duration::hours(24);
    raw.into_iter()
        .filter(|(pane, entry)| {
            let Ok(observed) = mv_core::time::parse_flexible(&entry.observed_at) else {
                return false;
            };
            if observed < cutoff {
                return false;
            }
            if let Some(live) = live_panes {
                if !live.iter().any(|p| p == pane) {
                    return false;
                }
            }
            true
        })
        .collect()
}

/// Remember one pane read and require the same signal on a *prior sweep
/// pass's* read — never a second read made within this same pass.
///
/// `find_stuck_signal` can be asked to confirm the same pane/task twice in
/// one pass (its own "stuck:" report, then again inside a resume attempt),
/// both sharing the identical `now` the whole pass uses. Comparing
/// fingerprints alone would let the first of those two reads seed the
/// memory and the second "confirm" it before a second pass ever happened,
/// so a pane that merely flickered once would read as persistently stuck.
/// `now`'s own formatted stamp doubles as this pass's identity: a second
/// read whose `observed_at` would match the entry already on disk is this
/// pass replaying its own prior read, so it replays that read's verdict
/// unchanged rather than re-deriving one from a fingerprint this same pass
/// just wrote; only a read whose `observed_at` differs is a genuinely new
/// pass, and only that one may change the verdict. Mirrors Python's
/// `_stuck_signal_confirmed`.
pub fn stuck_signal_confirmed(
    project_root: &Path,
    pane: &str,
    text: &str,
    live_panes: &[String],
    now: OffsetDateTime,
) -> bool {
    let path = memory_path(project_root, STUCK_SIGNAL_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_SIGNAL_MEMORY_FILENAME)
    else {
        return false;
    };
    let mut memory = load_stuck_signal_map(&path, Some(live_panes), now);
    let fp = fingerprint(text);
    let observed_at = mv_core::time::format_seconds_utc(now);
    if let Some(previous) = memory.get(pane) {
        if previous.observed_at == observed_at {
            return previous.confirmed;
        }
    }
    let confirmed = memory
        .get(pane)
        .is_some_and(|previous| previous.fingerprint == fp);
    memory.insert(
        pane.to_string(),
        StuckSignalEntry {
            fingerprint: fp,
            observed_at,
            confirmed,
        },
    );
    let bytes = match serde_json::to_vec(&memory) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };
    if mv_core::atomic::write_atomic(&path, &bytes).is_err() {
        return false;
    }
    confirmed
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IdlePaneEntry {
    fingerprint: String,
    first_seen: String,
    observed_at: String,
}

/// Same 24h-since-last-read and live-listing pruning
/// [`load_stuck_signal_map`] applies: an entry is dropped once its pane is
/// no longer live or nothing has read it for a day. `first_seen` itself is
/// never a pruning input — a pane can legitimately sit unchanged for hours.
fn load_idle_pane_map(
    path: &Path,
    live_panes: &[String],
    now: OffsetDateTime,
) -> BTreeMap<String, IdlePaneEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(raw) = serde_json::from_str::<BTreeMap<String, IdlePaneEntry>>(&text) else {
        return BTreeMap::new();
    };
    let cutoff = now - time::Duration::hours(24);
    raw.into_iter()
        .filter(|(pane, entry)| {
            mv_core::time::parse_flexible(&entry.observed_at)
                .is_ok_and(|observed| observed >= cutoff)
                && mv_core::time::parse_flexible(&entry.first_seen).is_ok()
                && live_panes.iter().any(|p| p == pane)
        })
        .collect()
}

/// Remember one read of `pane`'s text and return when this exact text
/// (by [`fingerprint`]) was first seen on it: the stored `first_seen` when
/// the fingerprint matches the previous read, or `now` when the pane is new
/// to this memory or its text changed (which restarts the clock). `None`
/// only on a lock or write failure — fail closed, so a storage error can
/// never make a pane look idle for longer than it has been. Card
/// `20261122-b-3acaeb`: `mv_engine::sweep_nudges`'s stall wake measures
/// "unchanged for N minutes" from this; [`stuck_signal_confirmed`] records
/// no first-seen time, so it cannot answer that question itself.
///
/// `apply` is the real-sweep gate (card `20261122-b-7378a4`). An applying
/// pass persists the observation into `.multiverse/idle-pane-memory.json`.
/// Under `apply=false` (`sweep --dry-run` or a halt) or
/// `MULTIVERSE_READ_ONLY=1` the same first-seen is returned from the
/// existing file, and the file is neither created nor rewritten.
pub fn idle_pane_since(
    project_root: &Path,
    pane: &str,
    text: &str,
    live_panes: &[String],
    now: OffsetDateTime,
    apply: bool,
) -> Option<OffsetDateTime> {
    let path = memory_path(project_root, IDLE_PANE_MEMORY_FILENAME);
    let persist = apply && std::env::var("MULTIVERSE_READ_ONLY").as_deref() != Ok("1");
    let _guard = if persist {
        Some(mv_core::lock::acquire_default(&lock_path(&path), IDLE_PANE_MEMORY_FILENAME).ok()?)
    } else {
        None
    };
    let mut memory = load_idle_pane_map(&path, live_panes, now);
    let fp = fingerprint(text);
    let observed_at = mv_core::time::format_seconds_utc(now);
    let first_seen = match memory.get(pane) {
        Some(previous) if previous.fingerprint == fp => previous.first_seen.clone(),
        _ => observed_at.clone(),
    };
    if persist {
        memory.insert(
            pane.to_string(),
            IdlePaneEntry {
                fingerprint: fp,
                first_seen: first_seen.clone(),
                observed_at,
            },
        );
        let bytes = serde_json::to_vec(&memory).ok()?;
        mv_core::atomic::write_atomic(&path, &bytes).ok()?;
    }
    mv_core::time::parse_flexible(&first_seen).ok()
}

/// One pane's last-tripped banner, matching Python's
/// `stranded-trip-memory.json` entries (`fingerprint` + `tripped_at`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrandedTripEntry {
    /// Fingerprint of the banner text that was showing when this pane was
    /// recorded as stranded, used to detect whether it's still the same
    /// frozen text on a later poll.
    pub fingerprint: String,
    /// When this trip was recorded, formatted by `mv_core::time`; entries
    /// older than `STRANDED_TRIP_RETENTION` are pruned on read.
    pub tripped_at: String,
}

fn load_stranded_trip_map(
    path: &Path,
    live_panes: Option<&[String]>,
    now: OffsetDateTime,
) -> BTreeMap<String, StrandedTripEntry> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(serde_json::Value::Object(raw)) = serde_json::from_str(&text) else {
        return BTreeMap::new();
    };
    let cutoff = now - STRANDED_TRIP_RETENTION;
    raw.into_iter()
        .filter_map(|(pane, value)| {
            let serde_json::Value::Object(obj) = value else {
                return None;
            };
            let fingerprint = obj.get("fingerprint")?.as_str()?.to_owned();
            let tripped_at = obj.get("tripped_at")?.as_str()?.to_owned();
            let recorded = mv_core::time::parse_flexible(&tripped_at).ok()?;
            if recorded < cutoff {
                return None;
            }
            if let Some(live) = live_panes {
                if !live.iter().any(|p| p == &pane) {
                    return None;
                }
            }
            Some((
                pane,
                StrandedTripEntry {
                    fingerprint,
                    tripped_at,
                },
            ))
        })
        .collect()
}

/// Python `tasks.record_stranded_banner_at_reset` /
/// `_remember_stranded_banner_trip`: write `pane`'s current banner into
/// stranded-trip memory so the next poll of the same frozen text is a
/// no-op. Best effort — a lock or write failure here must not surface
/// out of a reset that already closed the breaker. `live_panes` `None`
/// records regardless of listing; `Some` skips a pane that is no longer
/// live, matching Python's `if live_panes is None or pane in live_panes`.
pub fn record_stranded_banner_at_reset(
    project_root: &Path,
    pane: &str,
    text: &str,
    live_panes: Option<&[String]>,
) {
    if let Some(live) = live_panes {
        if !live.iter().any(|p| p == pane) {
            return;
        }
    }
    let path = memory_path(project_root, STRANDED_TRIP_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STRANDED_TRIP_MEMORY_FILENAME)
    else {
        return;
    };
    let now = mv_core::time::now_utc();
    let mut memory = load_stranded_trip_map(&path, live_panes, now);
    memory.insert(
        pane.to_string(),
        StrandedTripEntry {
            fingerprint: fingerprint(text),
            tripped_at: mv_core::time::format_seconds_utc(now),
        },
    );
    let Ok(bytes) = serde_json::to_vec(&memory) else {
        return;
    };
    let _ = mv_core::atomic::write_atomic(&path, &bytes);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Removed on drop, including on panic (card `20261115-b-3e0fde`: the
    /// plain `PathBuf` this replaced only cleaned up on each test's own
    /// trailing `remove_dir_all` call, so an earlier assertion failure left
    /// it behind in `TMPDIR`). `Deref`s to `Path` so every existing call
    /// site keeps reading `&root`/`root.join(...)` unchanged.
    struct TempProject(PathBuf);
    impl std::ops::Deref for TempProject {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl AsRef<Path> for TempProject {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_project() -> TempProject {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let dir = std::env::temp_dir().join(format!(
                "mv-state-nudge-memory-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&dir) {
                Ok(()) => {
                    fs::create_dir_all(dir.join(".multiverse")).unwrap();
                    return TempProject(dir);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temporary project: {error}"),
            }
        }
    }

    fn now() -> OffsetDateTime {
        mv_core::time::now_utc()
    }

    /// No task is recorded — the common case, every candidate still RUNNING.
    fn none() -> BTreeSet<&'static str> {
        BTreeSet::new()
    }

    /// `ids` have a recorded terminal outcome in this pass's routing log.
    fn recorded_set(ids: &[&'static str]) -> BTreeSet<&'static str> {
        ids.iter().copied().collect()
    }

    #[test]
    fn a_claim_wins_exactly_once() {
        let root = temp_project();
        assert!(!finish_nudge_already_sent(&root, "t1", &none()));
        assert!(claim_finish_nudge(&root, "t1", &none(), now()));
        assert!(finish_nudge_already_sent(&root, "t1", &none()));
        assert!(!claim_finish_nudge(&root, "t1", &none(), now()));
    }

    #[test]
    fn an_unrecorded_finish_nudge_claim_3_days_old_is_kept() {
        let root = temp_project();
        let age = now() - time::Duration::days(3);
        assert!(claim_finish_nudge(&root, "t1", &none(), age));
        assert!(finish_nudge_already_sent(&root, "t1", &none()));
    }

    #[test]
    fn a_fresh_finish_nudge_claim_for_a_recorded_task_is_pruned() {
        let root = temp_project();
        assert!(claim_finish_nudge(&root, "t1", &none(), now()));
        let recorded = recorded_set(&["t1"]);
        assert!(!finish_nudge_already_sent(&root, "t1", &recorded));
        // Pruned, not merely invisible: the slot is free to claim again.
        assert!(claim_finish_nudge(&root, "t1", &recorded, now()));
    }

    #[test]
    fn an_unrecorded_finish_nudge_claim_15_days_old_is_pruned() {
        let root = temp_project();
        let stale = now() - time::Duration::days(15);
        assert!(claim_finish_nudge(&root, "t1", &none(), stale));
        assert!(!finish_nudge_already_sent(&root, "t1", &none()));
        assert!(claim_finish_nudge(&root, "t1", &none(), now()));
    }

    #[test]
    fn a_kept_finish_nudge_claim_is_not_resent_on_a_later_pass() {
        let root = temp_project();
        let age = now() - time::Duration::days(3);
        assert!(claim_finish_nudge(&root, "t1", &none(), age));
        // A second sweep pass, the task still unrecorded: the one-shot slot
        // stays spent rather than re-arming the prompt.
        assert!(!claim_finish_nudge(&root, "t1", &none(), now()));
        assert!(finish_nudge_already_sent(&root, "t1", &none()));
    }

    #[test]
    fn an_unrecorded_runtime_cap_claim_3_days_old_is_kept() {
        let root = temp_project();
        let age = now() - time::Duration::days(3);
        assert!(claim_runtime_cap(&root, "t1", &none(), age));
        assert!(runtime_cap_already_sent(&root, "t1", &none()));
    }

    #[test]
    fn a_fresh_runtime_cap_claim_for_a_recorded_task_is_pruned() {
        let root = temp_project();
        assert!(claim_runtime_cap(&root, "t1", &none(), now()));
        let recorded = recorded_set(&["t1"]);
        assert!(!runtime_cap_already_sent(&root, "t1", &recorded));
        assert!(claim_runtime_cap(&root, "t1", &recorded, now()));
    }

    #[test]
    fn an_unrecorded_runtime_cap_claim_15_days_old_is_pruned() {
        let root = temp_project();
        let stale = now() - time::Duration::days(15);
        assert!(claim_runtime_cap(&root, "t1", &none(), stale));
        assert!(!runtime_cap_already_sent(&root, "t1", &none()));
        assert!(claim_runtime_cap(&root, "t1", &none(), now()));
    }

    #[test]
    fn an_unrecorded_stuck_resume_claim_3_days_old_is_kept() {
        let root = temp_project();
        let age = now() - time::Duration::days(3);
        assert!(claim_stuck_resume(&root, "t1", &none(), age));
        assert!(stuck_resume_claim(&root, "t1", &none()).is_some());
    }

    #[test]
    fn a_fresh_stuck_resume_claim_for_a_recorded_task_is_pruned() {
        let root = temp_project();
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
        let recorded = recorded_set(&["t1"]);
        assert!(stuck_resume_claim(&root, "t1", &recorded).is_none());
        // Pruned, not merely invisible: the slot is free to claim again.
        assert!(claim_stuck_resume(&root, "t1", &recorded, now()));
    }

    #[test]
    fn an_unrecorded_stuck_resume_claim_15_days_old_is_pruned() {
        let root = temp_project();
        let stale = now() - time::Duration::days(15);
        assert!(claim_stuck_resume(&root, "t1", &none(), stale));
        assert!(stuck_resume_claim(&root, "t1", &none()).is_none());
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
    }

    #[test]
    fn send_failure_counts_increment_and_stay_independent_by_purpose() {
        let root = temp_project();
        assert_eq!(record_stuck_resume_send_failure(&root, "t1"), 1);
        assert_eq!(record_stuck_resume_send_failure(&root, "t1"), 2);
        // A different task's count starts fresh.
        assert_eq!(record_stuck_resume_send_failure(&root, "t2"), 1);
        // The cap counter is a separate memory, not shared with resume's.
        assert_eq!(record_runtime_cap_send_failure(&root, "t1"), 1);
    }

    #[test]
    fn finish_and_runtime_cap_memories_are_independent() {
        let root = temp_project();
        assert!(claim_finish_nudge(&root, "t1", &none(), now()));
        assert!(!runtime_cap_already_sent(&root, "t1", &none()));
        assert!(claim_runtime_cap(&root, "t1", &none(), now()));
        assert!(finish_nudge_already_sent(&root, "t1", &none()));
        assert!(runtime_cap_already_sent(&root, "t1", &none()));
    }

    #[test]
    fn releasing_a_runtime_cap_claim_makes_it_retryable() {
        let root = temp_project();
        assert!(claim_runtime_cap(&root, "t1", &none(), now()));
        assert!(!claim_runtime_cap(&root, "t1", &none(), now()));
        release_runtime_cap_claim(&root, "t1", &none());
        assert!(!runtime_cap_already_sent(&root, "t1", &none()));
        assert!(claim_runtime_cap(&root, "t1", &none(), now()));
    }

    #[test]
    fn releasing_an_unclaimed_runtime_cap_is_a_no_op() {
        let root = temp_project();
        release_runtime_cap_claim(&root, "t1", &none());
        assert!(!runtime_cap_already_sent(&root, "t1", &none()));
    }

    #[test]
    fn stuck_resume_claim_then_remember_round_trips_pane_and_fingerprint() {
        let root = temp_project();
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
        assert!(!claim_stuck_resume(&root, "t1", &none(), now()));
        let claim = stuck_resume_claim(&root, "t1", &none()).unwrap();
        assert_eq!(claim.pane, None);
        remember_stuck_resume_signal(&root, "t1", "w1:p1", "connection lost", &none());
        let claim = stuck_resume_claim(&root, "t1", &none()).unwrap();
        assert_eq!(claim.pane.as_deref(), Some("w1:p1"));
        assert_eq!(claim.fingerprint, Some(fingerprint("connection lost")));
    }

    #[test]
    fn releasing_a_never_confirmed_claim_makes_it_retryable() {
        let root = temp_project();
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
        assert!(!claim_stuck_resume(&root, "t1", &none(), now()));
        release_stuck_resume_claim(&root, "t1", &none());
        assert!(stuck_resume_claim(&root, "t1", &none()).is_none());
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
    }

    #[test]
    fn releasing_a_confirmed_claim_is_a_no_op() {
        let root = temp_project();
        assert!(claim_stuck_resume(&root, "t1", &none(), now()));
        remember_stuck_resume_signal(&root, "t1", "w1:p1", "connection lost", &none());
        release_stuck_resume_claim(&root, "t1", &none());
        let claim = stuck_resume_claim(&root, "t1", &none()).unwrap();
        assert_eq!(claim.pane.as_deref(), Some("w1:p1"));
    }

    #[test]
    fn stuck_resume_memory_reads_a_legacy_bare_string_entry() {
        let root = temp_project();
        let path = memory_path(&root, STUCK_RESUME_MEMORY_FILENAME);
        // Fresh, not a fixed historical date: a legacy entry old enough to
        // be pruned (which a date more than 14 days old still would be,
        // the same as under the previous 24h rule) would make this a
        // pruning test, not a format test.
        let sent_at = mv_core::time::format_seconds_utc(now());
        fs::write(&path, format!(r#"{{"t1": "{sent_at}"}}"#)).unwrap();
        let claim = stuck_resume_claim(&root, "t1", &none()).unwrap();
        assert_eq!(claim.sent_at, sent_at);
        assert_eq!(claim.pane, None);
    }

    #[test]
    fn a_fresh_read_is_never_confirmed_on_its_own() {
        let root = temp_project();
        let moment = now();
        let live = vec!["w1:p1".to_string()];
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            moment
        ));
    }

    #[test]
    fn the_same_signal_on_a_later_pass_confirms() {
        let root = temp_project();
        let first = now();
        let second = first + time::Duration::minutes(5);
        let live = vec!["w1:p1".to_string()];
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            first
        ));
        assert!(stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            second
        ));
    }

    #[test]
    fn a_second_read_within_the_same_pass_replays_the_prior_verdict() {
        let root = temp_project();
        let first = now();
        let second = first + time::Duration::minutes(5);
        let live = vec!["w1:p1".to_string()];
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            first
        ));
        assert!(stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            second
        ));
        // Re-reading inside the pass stamped `second` must replay `true`,
        // not re-derive a fresh "not confirmed yet" from a seeded entry.
        assert!(stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            second
        ));
    }

    #[test]
    fn a_changed_signal_resets_confirmation() {
        let root = temp_project();
        let first = now();
        let second = first + time::Duration::minutes(5);
        let live = vec!["w1:p1".to_string()];
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            first
        ));
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "a different banner",
            &live,
            second
        ));
    }

    #[test]
    fn a_pane_no_longer_live_is_pruned_and_cannot_confirm() {
        let root = temp_project();
        let first = now();
        let second = first + time::Duration::minutes(5);
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &["w1:p1".to_string()],
            first
        ));
        // Second pass: the pane dropped out of the live listing.
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &[],
            second
        ));
    }

    #[test]
    fn a_claim_older_than_a_day_is_pruned() {
        let root = temp_project();
        let first = now() - time::Duration::hours(25);
        let second = now();
        let live = vec!["w1:p1".to_string()];
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            first
        ));
        // A stale entry cannot confirm a fresh read, even with the same text.
        assert!(!stuck_signal_confirmed(
            &root,
            "w1:p1",
            "connection lost",
            &live,
            second
        ));
    }

    #[test]
    fn a_stall_wake_claim_wins_exactly_once_and_is_independent_of_finish() {
        let root = temp_project();
        assert!(claim_finish_nudge(&root, "t1", &none(), now()));
        assert!(!stall_wake_already_sent(&root, "t1", &none()));
        assert!(claim_stall_wake(&root, "t1", &none(), now()));
        assert!(stall_wake_already_sent(&root, "t1", &none()));
        assert!(!claim_stall_wake(&root, "t1", &none(), now()));
        // Pruned once the task is recorded, like every flat claim map.
        assert!(!stall_wake_already_sent(
            &root,
            "t1",
            &recorded_set(&["t1"])
        ));
    }

    #[test]
    fn idle_pane_since_keeps_first_seen_while_the_text_is_unchanged() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let later = first + time::Duration::minutes(16);
        let live = vec!["w1:p1".to_string()];
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, first, true),
            Some(first)
        );
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, later, true),
            Some(first)
        );
    }

    #[test]
    fn idle_pane_since_restarts_the_clock_when_the_text_changes() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let later = first + time::Duration::minutes(16);
        let live = vec!["w1:p1".to_string()];
        idle_pane_since(&root, "w1:p1", "idle", &live, first, true);
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "new output", &live, later, true),
            Some(later)
        );
    }

    #[test]
    fn idle_pane_since_forgets_a_pane_that_left_the_live_listing() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let later = first + time::Duration::minutes(16);
        idle_pane_since(&root, "w1:p1", "idle", &["w1:p1".to_string()], first, true);
        // A pass where the pane was gone prunes it; its return restarts.
        idle_pane_since(&root, "w2:p2", "other", &["w2:p2".to_string()], later, true);
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &["w1:p1".to_string()], later, true),
            Some(later)
        );
    }

    #[test]
    fn idle_pane_since_without_apply_does_not_create_the_file() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let live = vec!["w1:p1".to_string()];
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, first, false),
            Some(first)
        );
        assert!(!memory_path(&root, IDLE_PANE_MEMORY_FILENAME).exists());
    }

    #[test]
    fn idle_pane_since_without_apply_leaves_existing_bytes_identical() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let later = first + time::Duration::minutes(16);
        let live = vec!["w1:p1".to_string()];
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, first, true),
            Some(first)
        );
        let path = memory_path(&root, IDLE_PANE_MEMORY_FILENAME);
        let before = fs::read(&path).unwrap();
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, later, false),
            Some(first)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn idle_pane_since_without_apply_does_not_persist_a_restarted_clock() {
        let root = temp_project();
        let first = mv_core::time::parse_flexible("2026-11-21T10:00:00Z").unwrap();
        let later = first + time::Duration::minutes(16);
        let live = vec!["w1:p1".to_string()];
        idle_pane_since(&root, "w1:p1", "idle", &live, first, true);
        let path = memory_path(&root, IDLE_PANE_MEMORY_FILENAME);
        let before = fs::read(&path).unwrap();
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "new output", &live, later, false),
            Some(later)
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        // A later applying pass still sees the original fingerprint.
        assert_eq!(
            idle_pane_since(&root, "w1:p1", "idle", &live, later, true),
            Some(first)
        );
    }

    #[test]
    fn record_stranded_banner_at_reset_writes_fingerprint_for_a_live_pane() {
        let root = temp_project();
        let live = vec!["w1:p1".to_string()];
        record_stranded_banner_at_reset(&root, "w1:p1", "out of credits", Some(&live));
        let path = memory_path(&root, STRANDED_TRIP_MEMORY_FILENAME);
        let raw: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            raw["w1:p1"]["fingerprint"].as_str(),
            Some(fingerprint("out of credits").as_str())
        );
        assert!(raw["w1:p1"]["tripped_at"].as_str().is_some());
    }

    #[test]
    fn record_stranded_banner_at_reset_skips_a_pane_not_in_the_live_listing() {
        let root = temp_project();
        let live = vec!["w2:p2".to_string()];
        record_stranded_banner_at_reset(&root, "w1:p1", "out of credits", Some(&live));
        let path = memory_path(&root, STRANDED_TRIP_MEMORY_FILENAME);
        assert!(!path.exists());
    }
}

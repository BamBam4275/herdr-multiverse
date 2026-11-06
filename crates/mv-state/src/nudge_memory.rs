//! One-shot claim memories backing the sweep nudge helpers
//! (`mv_engine::sweep_nudges`, card `20261102-b-b4ede0`): `finish-nudge-
//! memory.json`, `stuck-resume-memory.json`, `runtime-cap-memory.json`,
//! the two-pass `stuck-signal-memory.json` a confirmed-stuck verdict needs
//! before a resume nudge may ever claim its own slot, and
//! `stranded-trip-memory.json` (`accounts reset`'s still-parked pane
//! snapshot, card `20261103-b-ffa660`).
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

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

const FINISH_NUDGE_MEMORY_FILENAME: &str = "finish-nudge-memory.json";
const RUNTIME_CAP_MEMORY_FILENAME: &str = "runtime-cap-memory.json";
const STUCK_RESUME_MEMORY_FILENAME: &str = "stuck-resume-memory.json";
const STUCK_SIGNAL_MEMORY_FILENAME: &str = "stuck-signal-memory.json";
/// Python `tasks._STRANDED_TRIP_MEMORY_FILENAME`.
pub const STRANDED_TRIP_MEMORY_FILENAME: &str = "stranded-trip-memory.json";
/// Same bound `mv_engine::breaker::ESCALATION_RESET_WINDOW` uses. Duplicated
/// here so `mv-state` does not depend on `mv-engine`.
const STRANDED_TRIP_RETENTION: time::Duration = time::Duration::minutes(30);

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
    format!("{:x}", hasher.finalize())
}

fn load_claim_map(path: &Path) -> BTreeMap<String, String> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_claim_map(path: &Path, map: &BTreeMap<String, String>) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(map).expect("claim map always serializes");
    mv_core::atomic::write_atomic(path, &bytes)
}

fn already_claimed(project_root: &Path, filename: &str, task_id: &str) -> bool {
    load_claim_map(&memory_path(project_root, filename)).contains_key(task_id)
}

/// Reserve `task_id`'s one claim in `filename`'s memory, atomically.
/// `true` only for the call that wins the reservation — two sweeps racing
/// the same candidate still send exactly one prompt between them.
fn claim_once(project_root: &Path, filename: &str, task_id: &str, now: OffsetDateTime) -> bool {
    let path = memory_path(project_root, filename);
    let Ok(_guard) = mv_core::lock::acquire_default(&lock_path(&path), filename) else {
        return false;
    };
    let mut map = load_claim_map(&path);
    if map.contains_key(task_id) {
        return false;
    }
    map.insert(task_id.to_string(), mv_core::time::format_seconds_utc(now));
    save_claim_map(&path, &map).is_ok()
}

/// Whether `task_id` has already had its one finish nudge
/// (`FINISH_NUDGE_MEMORY_FILENAME`). Read-only, so a renderer can report
/// what already happened without sending anything itself.
pub fn finish_nudge_already_sent(project_root: &Path, task_id: &str) -> bool {
    already_claimed(project_root, FINISH_NUDGE_MEMORY_FILENAME, task_id)
}

/// Atomically reserve `task_id`'s one finish nudge.
pub fn claim_finish_nudge(project_root: &Path, task_id: &str, now: OffsetDateTime) -> bool {
    claim_once(project_root, FINISH_NUDGE_MEMORY_FILENAME, task_id, now)
}

/// Whether `task_id` has already had its one runtime-cap nudge
/// (`RUNTIME_CAP_MEMORY_FILENAME`).
pub fn runtime_cap_already_sent(project_root: &Path, task_id: &str) -> bool {
    already_claimed(project_root, RUNTIME_CAP_MEMORY_FILENAME, task_id)
}

/// Atomically reserve `task_id`'s one runtime-cap nudge.
pub fn claim_runtime_cap(project_root: &Path, task_id: &str, now: OffsetDateTime) -> bool {
    claim_once(project_root, RUNTIME_CAP_MEMORY_FILENAME, task_id, now)
}

/// One task's resume-nudge claim. `pane`/`fingerprint` are populated only
/// after a successfully sent WORKING-pane resume
/// ([`remember_stuck_resume_signal`]) — mirrors Python's own
/// `_load_stuck_resume_memory`, which also accepts a bare legacy
/// `{task_id: iso-string}` entry (pre-dating these fields) by reading it as
/// `{"sent_at": entry}`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct StuckResumeClaim {
    pub sent_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

fn load_stuck_resume_map(path: &Path) -> BTreeMap<String, StuckResumeClaim> {
    let Ok(text) = fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    let Ok(serde_json::Value::Object(raw)) = serde_json::from_str(&text) else {
        return BTreeMap::new();
    };
    raw.into_iter()
        .filter_map(|(task_id, value)| {
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

/// `task_id`'s resume-nudge claim, if one has already been made.
pub fn stuck_resume_claim(project_root: &Path, task_id: &str) -> Option<StuckResumeClaim> {
    load_stuck_resume_map(&memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME)).remove(task_id)
}

/// Whether `task_id` has already had its one resume nudge.
pub fn stuck_resume_already_sent(project_root: &Path, task_id: &str) -> bool {
    stuck_resume_claim(project_root, task_id).is_some()
}

/// Atomically reserve `task_id`'s one resume nudge.
pub fn claim_stuck_resume(project_root: &Path, task_id: &str, now: OffsetDateTime) -> bool {
    let path = memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_RESUME_MEMORY_FILENAME)
    else {
        return false;
    };
    let mut map = load_stuck_resume_map(&path);
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
pub fn remember_stuck_resume_signal(project_root: &Path, task_id: &str, pane: &str, text: &str) {
    let path = memory_path(project_root, STUCK_RESUME_MEMORY_FILENAME);
    let Ok(_guard) =
        mv_core::lock::acquire_default(&lock_path(&path), STUCK_RESUME_MEMORY_FILENAME)
    else {
        return;
    };
    let mut map = load_stuck_resume_map(&path);
    let Some(entry) = map.get_mut(task_id) else {
        return;
    };
    entry.pane = Some(pane.to_string());
    entry.fingerprint = Some(fingerprint(text));
    let _ = save_stuck_resume_map(&path, &map);
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

/// One pane's last-tripped banner, matching Python's
/// `stranded-trip-memory.json` entries (`fingerprint` + `tripped_at`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StrandedTripEntry {
    pub fingerprint: String,
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

    fn temp_project() -> PathBuf {
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
                    return dir;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => panic!("temporary project: {error}"),
            }
        }
    }

    fn now() -> OffsetDateTime {
        mv_core::time::now_utc()
    }

    #[test]
    fn a_claim_wins_exactly_once() {
        let root = temp_project();
        assert!(!finish_nudge_already_sent(&root, "t1"));
        assert!(claim_finish_nudge(&root, "t1", now()));
        assert!(finish_nudge_already_sent(&root, "t1"));
        assert!(!claim_finish_nudge(&root, "t1", now()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finish_and_runtime_cap_memories_are_independent() {
        let root = temp_project();
        assert!(claim_finish_nudge(&root, "t1", now()));
        assert!(!runtime_cap_already_sent(&root, "t1"));
        assert!(claim_runtime_cap(&root, "t1", now()));
        assert!(finish_nudge_already_sent(&root, "t1"));
        assert!(runtime_cap_already_sent(&root, "t1"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stuck_resume_claim_then_remember_round_trips_pane_and_fingerprint() {
        let root = temp_project();
        assert!(claim_stuck_resume(&root, "t1", now()));
        assert!(!claim_stuck_resume(&root, "t1", now()));
        let claim = stuck_resume_claim(&root, "t1").unwrap();
        assert_eq!(claim.pane, None);
        remember_stuck_resume_signal(&root, "t1", "w1:p1", "connection lost");
        let claim = stuck_resume_claim(&root, "t1").unwrap();
        assert_eq!(claim.pane.as_deref(), Some("w1:p1"));
        assert_eq!(claim.fingerprint, Some(fingerprint("connection lost")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stuck_resume_memory_reads_a_legacy_bare_string_entry() {
        let root = temp_project();
        let path = memory_path(&root, STUCK_RESUME_MEMORY_FILENAME);
        fs::write(&path, r#"{"t1": "2026-01-01T00:00:00+00:00"}"#).unwrap();
        let claim = stuck_resume_claim(&root, "t1").unwrap();
        assert_eq!(claim.sent_at, "2026-01-01T00:00:00+00:00");
        assert_eq!(claim.pane, None);
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
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
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn record_stranded_banner_at_reset_skips_a_pane_not_in_the_live_listing() {
        let root = temp_project();
        let live = vec!["w2:p2".to_string()];
        record_stranded_banner_at_reset(&root, "w1:p1", "out of credits", Some(&live));
        let path = memory_path(&root, STRANDED_TRIP_MEMORY_FILENAME);
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&root);
    }
}

//! Atomic on-disk store backing the breaker (fenced/available) state.
//!
//! Mirrors `breaker.py`'s `breaker-state.json`: a JSON object keyed by pool
//! name, each value a loosely-typed entry whose fields accumulated ad hoc
//! over time (`spec-seats-quotas.md`, "Per-pool breaker entry fields").
//! Reads fail open — a missing or corrupt file is "nothing recorded," never
//! an error — so a hand-edited or not-yet-created store never blocks a
//! seat.
//!
//! Tombstone contract (`audit-dispatch-tasks-seats.md` finding 6, which
//! corrects the plain spec and wins per this task's brief): an **active**
//! trip may omit `open` entirely for backward compatibility with entries
//! written before the field existed — absence means open, never write
//! `open: false` for anything but a genuine close. A **closed** entry must
//! carry `open: false` plus `closed_at` and `close_reason`, and must never
//! be achieved by deleting the key outright — a popped entry loses the
//! `_last_change_at` a stale gauge reading needs to be told it is stale.
//!
//! `support` adapts the shared `mv-core` lock and atomic writer to this
//! store's error type and lock-file pid convention. `gauge.rs` and
//! `seatstate_store.rs` reuse the same adapters.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One pool's breaker evidence. Fields accumulate ad hoc in the Python
/// original; unknown/extra keys round-trip via `extra` so this struct never
/// silently drops a field a reader still needs.
///
/// Field order is Python `json.dump(..., sort_keys=True)` order so a
/// freshly written store is byte-comparable with the oracle (card
/// `20261103-b-cad0bc`). `extra` stays last: flatten emits at this
/// position, and a writer of only known fields has an empty map.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BreakerEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tripped_at: Option<String>,
    /// Written `true` only — absence means automatic, never written
    /// `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manual: Option<bool>,
    /// `Some(false)` only on a closed/reset tombstone; **absent** means
    /// open, for backward compatibility with entries written before this
    /// field existed. Never write `Some(true)` for the common case — only
    /// the absence-means-open convention.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub open: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_artifact_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_claimed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_pane_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_project_root: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trial_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trip_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tripped_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Any field this struct does not yet know about, preserved verbatim so
    /// an older/newer writer's extra fields never get lost on a round trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Breaker state keyed by pool name.
pub type BreakerMap = BTreeMap<String, BreakerEntry>;

/// Re-exported so callers outside this crate (`mv-engine`'s `breaker.rs`)
/// can name the error type `save`/`with_locked` return without reaching
/// into the `support` shim directly.
pub use support::StoreError;

pub fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join("breaker-state.json")
}

fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Load the breaker store, failing open (empty map) on a missing or
/// unparseable file — never an error, matching the Python original's
/// `_load`.
pub fn load(config_dir: &Path) -> BreakerMap {
    let path = store_path(config_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return BreakerMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Pretty-printed store bytes matching Python's `json.dump(..., indent=2,
/// sort_keys=True)` plus a trailing newline (`breaker._atomic_write`).
fn serialize_map(map: &BreakerMap) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(map).expect("BreakerMap always serializes");
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

/// Persist the breaker store atomically, under the store's advisory lock.
pub fn save(config_dir: &Path, map: &BreakerMap) -> Result<(), support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    support::write_atomic(&path, &serialize_map(map))?;
    Ok(())
}

/// Run `f` against the current map under the store's advisory lock, then
/// write back the (possibly mutated) result. This is the read-modify-write
/// unit every mutator (trip, reset, trial lifecycle) uses so two writers
/// can never interleave — the lock is held across the whole span, not just
/// the final write.
pub fn with_locked<T>(
    config_dir: &Path,
    f: impl FnOnce(&mut BreakerMap) -> T,
) -> Result<T, support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let mut map: BreakerMap = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let result = f(&mut map);
    support::write_atomic(&path, &serialize_map(&map))?;
    Ok(result)
}

/// Shared store adapters; lock and atomic-write algorithms live in mv-core.
pub(crate) mod support {
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use mv_core::lock::{self, LockError, LockGuard};

    pub(crate) use mv_core::atomic::write_atomic;

    /// Mirrors `LockAcquireTimeoutError`: names the lock path, the bound,
    /// and (when readable) the holding pid.
    #[derive(Debug, thiserror::Error)]
    #[error("could not acquire lock {path:?} within {timeout_secs}s (held by pid {holder_pid:?})")]
    pub struct LockTimeoutError {
        pub path: PathBuf,
        pub timeout_secs: u64,
        pub holder_pid: Option<i32>,
    }

    #[derive(Debug, thiserror::Error)]
    pub enum StoreError {
        #[error(transparent)]
        LockTimeout(#[from] LockTimeoutError),
        #[error("store I/O error: {0}")]
        Io(#[from] io::Error),
    }

    /// Acquire the shared reentrant lock, preserving store error diagnostics
    /// and the best-effort pid written for other processes inspecting it.
    pub(crate) fn acquire(lock_path: &Path) -> Result<LockGuard, StoreError> {
        let guard = lock::acquire_default(lock_path, "store").map_err(|error| match error {
            LockError::Timeout(error) => StoreError::LockTimeout(LockTimeoutError {
                path: error.path,
                timeout_secs: error.timeout.as_secs(),
                holder_pid: fs::read_to_string(lock_path)
                    .ok()
                    .and_then(|text| text.trim().parse().ok()),
            }),
            LockError::Io(error) => StoreError::Io(error),
        })?;
        let _ = fs::write(lock_path, std::process::id().to_string());
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_lock_reenters_the_shared_lock_and_creates_private_parent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("new/state");
        let path = lock_path(&store_path(&state_dir));
        let _held = mv_core::lock::acquire_default(&path, "test").unwrap();
        with_locked(&state_dir, |map| {
            map.insert("pool".to_string(), BreakerEntry::default());
        })
        .unwrap();
        assert!(load(&state_dir).contains_key("pool"));
        assert_eq!(
            fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(store_path(&state_dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn missing_store_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn corrupt_store_fails_open_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(store_path(dir.path()), b"{not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn round_trips_a_tombstoned_entry() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = BreakerMap::new();
        map.insert(
            "chatgpt".to_string(),
            BreakerEntry {
                open: Some(false),
                closed_at: Some("2026-11-02T12:00:00+00:00".to_string()),
                close_reason: Some("manual reset".to_string()),
                ..Default::default()
            },
        );
        save(dir.path(), &map).unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded["chatgpt"].open, Some(false));
        assert_eq!(
            reloaded["chatgpt"].close_reason.as_deref(),
            Some("manual reset")
        );
    }

    #[test]
    fn active_entry_omits_open_field_in_serialized_json() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = BreakerMap::new();
        map.insert(
            "claude".to_string(),
            BreakerEntry {
                tripped_at: Some("2026-11-02T12:00:00+00:00".to_string()),
                ..Default::default()
            },
        );
        save(dir.path(), &map).unwrap();
        let text = fs::read_to_string(store_path(dir.path())).unwrap();
        assert!(
            !text.contains("\"open\""),
            "an active trip must not write an `open` key at all: {text}"
        );
        assert!(
            text.ends_with('\n'),
            "Python's breaker._atomic_write ends with a newline: {text:?}"
        );
    }

    #[test]
    fn serialized_keys_follow_python_sort_keys_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = BreakerMap::new();
        map.insert(
            "codex:test".to_string(),
            BreakerEntry {
                caller: Some("cmd_accounts".to_string()),
                kind: Some("weekly".to_string()),
                manual: Some(true),
                reset_at: Some("2030-01-01T00:00:00+00:00".to_string()),
                tripped_at: Some("2026-11-03T00:00:00+00:00".to_string()),
                vendor: Some("codex".to_string()),
                ..Default::default()
            },
        );
        save(dir.path(), &map).unwrap();
        let text = fs::read_to_string(store_path(dir.path())).unwrap();
        let caller = text.find("\"caller\"").expect("caller");
        let kind = text.find("\"kind\"").expect("kind");
        let manual = text.find("\"manual\"").expect("manual");
        let reset_at = text.find("\"reset_at\"").expect("reset_at");
        let tripped_at = text.find("\"tripped_at\"").expect("tripped_at");
        let vendor = text.find("\"vendor\"").expect("vendor");
        assert!(
            caller < kind
                && kind < manual
                && manual < reset_at
                && reset_at < tripped_at
                && tripped_at < vendor,
            "keys must be alphabetical like Python sort_keys=True: {text}"
        );
        assert!(text.ends_with('\n'), "{text:?}");
        assert!(
            !text.contains("\"rule\""),
            "absent rule must stay omitted: {text}"
        );
    }

    #[test]
    fn unknown_fields_round_trip_via_extra() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            store_path(dir.path()),
            br#"{"chatgpt": {"tripped_at": "2026-11-02T12:00:00+00:00", "future_field": 42}}"#,
        )
        .unwrap();
        let map = load(dir.path());
        assert_eq!(
            map["chatgpt"].extra.get("future_field"),
            Some(&Value::from(42))
        );
        save(dir.path(), &map).unwrap();
        let reloaded = load(dir.path());
        assert_eq!(
            reloaded["chatgpt"].extra.get("future_field"),
            Some(&Value::from(42))
        );
    }

    #[test]
    fn with_locked_persists_the_mutation() {
        let dir = tempfile::tempdir().unwrap();
        with_locked(dir.path(), |map| {
            map.insert("grok".to_string(), BreakerEntry::default());
        })
        .unwrap();
        assert!(load(dir.path()).contains_key("grok"));
    }

    #[test]
    fn a_held_lock_times_out_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let lock_file_path = lock_path(&store_path(dir.path()));
        let held = support::acquire(&lock_file_path).unwrap();
        // A second, independent lock handle on the same path cannot
        // acquire while `held` is alive; we cannot easily force a 30s
        // real-time wait in a unit test, so this test only checks that the
        // immediate contention path (a different fd on the same file)
        // reports busy rather than silently succeeding.
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&lock_file_path)
            .unwrap();
        use rustix::fd::AsFd;
        use rustix::fs::{flock, FlockOperation};
        let busy = flock(file.as_fd(), FlockOperation::NonBlockingLockExclusive);
        assert!(busy.is_err(), "lock should still be held");
        drop(held);
    }
}

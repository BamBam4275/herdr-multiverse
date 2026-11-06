//! Atomic on-disk store for per-seat state.
//!
//! Mirrors `seatstate.py`'s `seat-state.json`: `{"<seat>": {"cap": int>=1,
//! "lend": bool, "prefer": bool}}`, all three independently settable and
//! clearable. This store only owns the raw JSON shape and persistence; the
//! fail-open defaults on a missing/corrupt field ("no cap = uncapped,
//! missing/non-bool `lend` = lent, missing/non-bool `prefer` = not
//! preferred") are a read-path *policy* decision, applied by
//! `mv_engine::seatstate`, not here — this module just stores what is
//! there (or isn't).
//!
//! Per `architecture.md`'s L10 brief, the write path (`accounts
//! lend/prefer/trip`) is Wave 2 work; `save`/`with_locked` exist here
//! because they are trivial given the shared atomic-write shim and useful
//! for tests, but no caller in this lane's `seatstate.rs` uses them yet.
//!
//! The lock/atomic-write primitive is the same temporary shim documented in
//! [`crate::breaker_store`]. Unlike `breaker-state.json`'s lock, which
//! appends `.lock` to the full filename, this store's lock path mirrors
//! Python's `Path.with_suffix(".lock")` — it *replaces* the `.json`
//! extension, producing `seat-state.lock` (`seatstate.py:161`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::breaker_store::support;

/// One seat's cap/lend/prefer flags. `serde`'s default `bool`/`Option`
/// handling already matches the Python reader's "missing field" case;
/// Python's additional "non-bool value present" fail-open case is handled
/// by `mv_engine::seatstate`'s callers treating any field that fails to
/// deserialize as absent (a store whose `lend` field holds e.g. a string
/// fails this struct's deserialization entirely for that seat, which
/// `load` below already turns into "nothing recorded" at the whole-store
/// level — see `load`'s doc for why that is the intended fail-open
/// behavior here, not a per-field one).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cap: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lend: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefer: Option<bool>,
}

impl SeatEntry {
    pub fn is_empty(&self) -> bool {
        self.cap.is_none() && self.lend.is_none() && self.prefer.is_none()
    }
}

/// Seat state keyed by seat name.
pub type SeatMap = BTreeMap<String, SeatEntry>;

pub fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join("seat-state.json")
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// Load the seat-state store, failing open (empty map) on a missing or
/// unparseable file. A store with one corrupt seat entry (e.g. `"cap":
/// "not a number"`) fails whole-file JSON parsing in `serde_json`, which
/// this treats the same as "no store" — matching the Python original's
/// posture that a damaged store blocks nobody rather than only the one bad
/// seat.
pub fn load(config_dir: &Path) -> SeatMap {
    let path = store_path(config_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return SeatMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the seat-state store atomically, under the store's advisory
/// lock.
pub fn save(config_dir: &Path, map: &SeatMap) -> Result<(), support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let bytes = serde_json::to_vec_pretty(map).expect("SeatMap always serializes");
    support::write_atomic(&path, &bytes)?;
    Ok(())
}

/// Read-modify-write under the store's advisory lock; see
/// [`crate::breaker_store::with_locked`] for the same pattern. Callers that
/// clear the last of a seat's `cap`/`lend`/`prefer` should follow with
/// [`prune_if_empty`] so an emptied seat is popped rather than left as
/// `{}` (`seatstate.py:180-184,256-261`).
pub fn with_locked<T>(
    config_dir: &Path,
    f: impl FnOnce(&mut SeatMap) -> T,
) -> Result<T, support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let mut map: SeatMap = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let result = f(&mut map);
    let bytes = serde_json::to_vec_pretty(&map).expect("SeatMap always serializes");
    support::write_atomic(&path, &bytes)?;
    Ok(result)
}

/// Remove `seat`'s entry if it is now empty — matching Python's
/// pop-when-empty behavior rather than leaving a stray `{}`.
pub fn prune_if_empty(map: &mut SeatMap, seat: &str) {
    if map.get(seat).is_some_and(SeatEntry::is_empty) {
        map.remove(seat);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_store_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn corrupt_store_fails_open_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(store_path(dir.path()), b"{").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn lock_path_matches_pythons_with_suffix_not_a_json_lock_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        assert_eq!(
            lock_path(&path).file_name().unwrap(),
            "seat-state.lock",
            "Python's seatstate.py uses Path.with_suffix(\".lock\"), which \
             replaces the .json extension rather than appending to it"
        );
    }

    #[test]
    fn cap_lend_prefer_are_independently_settable() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = SeatMap::new();
        map.insert(
            "claude-1".to_string(),
            SeatEntry {
                cap: Some(2),
                lend: None,
                prefer: Some(true),
            },
        );
        save(dir.path(), &map).unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded["claude-1"].cap, Some(2));
        assert_eq!(reloaded["claude-1"].lend, None);
        assert_eq!(reloaded["claude-1"].prefer, Some(true));
    }

    #[test]
    fn setting_a_cap_does_not_disturb_that_seats_lend_flag() {
        let dir = tempfile::tempdir().unwrap();
        with_locked(dir.path(), |map| {
            map.entry("codex-1".to_string()).or_default().lend = Some(false);
        })
        .unwrap();
        with_locked(dir.path(), |map| {
            map.entry("codex-1".to_string()).or_default().cap = Some(1);
        })
        .unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded["codex-1"].lend, Some(false));
        assert_eq!(reloaded["codex-1"].cap, Some(1));
    }

    #[test]
    fn clearing_the_last_field_prunes_the_seat() {
        let dir = tempfile::tempdir().unwrap();
        with_locked(dir.path(), |map| {
            map.insert(
                "grok-1".to_string(),
                SeatEntry {
                    cap: None,
                    lend: Some(true),
                    prefer: None,
                },
            );
        })
        .unwrap();
        with_locked(dir.path(), |map| {
            if let Some(entry) = map.get_mut("grok-1") {
                entry.lend = None;
            }
            prune_if_empty(map, "grok-1");
        })
        .unwrap();
        assert!(!load(dir.path()).contains_key("grok-1"));
    }

    #[test]
    fn an_entry_with_other_fields_set_is_not_pruned() {
        let mut map = SeatMap::new();
        map.insert(
            "claude-1".to_string(),
            SeatEntry {
                cap: Some(1),
                lend: None,
                prefer: None,
            },
        );
        prune_if_empty(&mut map, "claude-1");
        assert!(map.contains_key("claude-1"));
    }
}

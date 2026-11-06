//! Atomic on-disk store for usage/quota gauges.
//!
//! Mirrors `limits.py`'s `gauge-state.json`. Per
//! `audit-dispatch-tasks-seats.md` finding 1 — which corrects
//! `spec-seats-quotas.md`'s "keyed by kind+config_dir" claim and wins per
//! this task's brief — the store is a JSON object **keyed by pool**, each
//! value carrying the full primary/secondary gauge fields plus an optional
//! probe timestamp. The on-disk field names for the secondary window are
//! pinned to Python's own spelling — `secondary_percent_left`,
//! `secondary_label`, `secondary_resets_at`, `secondary_window_minutes`
//! (`limits.py:1274-1288`, cited in `critic-history-compat.md` finding 3) —
//! via `#[serde(rename = ...)]` on this struct's Rust-named fields, so a
//! Python-written entry's secondary window is not silently dropped into
//! `extra`.
//!
//! The lock/atomic-write primitive is the same temporary shim documented in
//! [`crate::breaker_store`] — delete the `support` reuse here once
//! `mv-core::lock`/`mv-core::atomic` land. Unlike `breaker-state.json`'s
//! lock, which appends `.lock` to the full filename, this store's lock path
//! mirrors Python's `Path.with_suffix(".lock")` — it *replaces* the `.json`
//! extension, producing `gauge-state.lock` (`limits.py:1262`).
//!
//! `percent_left` serializes as a JSON integer, not a float, when it holds a
//! whole number: Python's `limits.py:1367-1373` `read_last_gauge` returns
//! `None` unless `isinstance(percent_left, int)`, so a Rust-written `42.0`
//! would read back on the Python side as "never probed"
//! (`critic-history-compat.md` finding 5).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize, Serializer};
use serde_json::Value;

use crate::breaker_store::support;

/// Writes a whole-number `f64` as a bare JSON integer (`42`, not `42.0`) so
/// Python's `isinstance(percent_left, int)` check still holds after a Rust
/// write; a genuinely fractional value still serializes as a float.
fn serialize_percent_left<S>(value: &Option<f64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match value {
        None => serializer.serialize_none(),
        Some(v) if v.is_finite() && v.fract() == 0.0 => serializer.serialize_i64(*v as i64),
        Some(v) => serializer.serialize_f64(*v),
    }
}

/// One pool's most recently observed quota gauge.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GaugeEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    /// Percent of the primary window left (0-100), already range-checked by
    /// the caller — this store does not re-validate the range.
    #[serde(
        serialize_with = "serialize_percent_left",
        skip_serializing_if = "Option::is_none"
    )]
    pub percent_left: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
    #[serde(
        rename = "secondary_percent_left",
        skip_serializing_if = "Option::is_none"
    )]
    pub percent_left_secondary: Option<f64>,
    #[serde(rename = "secondary_label", skip_serializing_if = "Option::is_none")]
    pub label_secondary: Option<String>,
    #[serde(
        rename = "secondary_resets_at",
        skip_serializing_if = "Option::is_none"
    )]
    pub resets_at_secondary: Option<String>,
    #[serde(
        rename = "secondary_window_minutes",
        skip_serializing_if = "Option::is_none"
    )]
    pub window_minutes_secondary: Option<u32>,
    /// A recovery-probe timestamp (`limits.py`'s `last_probe_at`) — never
    /// written by invoking a vendor CLI, just a marker of when a passive
    /// re-check last happened.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_probe_at: Option<String>,
    /// Any field this struct does not yet know about, preserved verbatim.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Gauge state keyed by pool name.
pub type GaugeMap = BTreeMap<String, GaugeEntry>;

pub fn store_path(config_dir: &Path) -> PathBuf {
    config_dir.join("gauge-state.json")
}

fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// Load the gauge store, failing open (empty map) on a missing or
/// unparseable file.
pub fn load(config_dir: &Path) -> GaugeMap {
    let path = store_path(config_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return GaugeMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist the gauge store atomically, under the store's advisory lock.
pub fn save(config_dir: &Path, map: &GaugeMap) -> Result<(), support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let bytes = serde_json::to_vec_pretty(map).expect("GaugeMap always serializes");
    support::write_atomic(&path, &bytes)?;
    Ok(())
}

/// Read-modify-write under the store's advisory lock; see
/// [`crate::breaker_store::with_locked`] for the same pattern.
pub fn with_locked<T>(
    config_dir: &Path,
    f: impl FnOnce(&mut GaugeMap) -> T,
) -> Result<T, support::StoreError> {
    let path = store_path(config_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let mut map: GaugeMap = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    let result = f(&mut map);
    let bytes = serde_json::to_vec_pretty(&map).expect("GaugeMap always serializes");
    support::write_atomic(&path, &bytes)?;
    Ok(result)
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
        fs::write(store_path(dir.path()), b"not json at all").unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn round_trips_primary_and_secondary_windows() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = GaugeMap::new();
        map.insert(
            "claude".to_string(),
            GaugeEntry {
                vendor: Some("claude".to_string()),
                percent_left: Some(42.0),
                observed_at: Some("2026-11-02T12:00:00+00:00".to_string()),
                percent_left_secondary: Some(80.0),
                ..Default::default()
            },
        );
        save(dir.path(), &map).unwrap();
        let reloaded = load(dir.path());
        assert_eq!(reloaded["claude"].percent_left, Some(42.0));
        assert_eq!(reloaded["claude"].percent_left_secondary, Some(80.0));
    }

    #[test]
    fn keyed_by_pool_not_kind_plus_config_dir() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = GaugeMap::new();
        map.insert("chatgpt".to_string(), GaugeEntry::default());
        save(dir.path(), &map).unwrap();
        let text = fs::read_to_string(store_path(dir.path())).unwrap();
        let value: Value = serde_json::from_str(&text).unwrap();
        assert!(value.as_object().unwrap().contains_key("chatgpt"));
    }

    #[test]
    fn a_whole_number_percent_left_serializes_as_a_json_integer_not_a_float() {
        let dir = tempfile::tempdir().unwrap();
        let mut map = GaugeMap::new();
        map.insert(
            "claude".to_string(),
            GaugeEntry {
                percent_left: Some(42.0),
                ..Default::default()
            },
        );
        save(dir.path(), &map).unwrap();
        let text = fs::read_to_string(store_path(dir.path())).unwrap();
        assert!(
            text.contains("\"percent_left\": 42") && !text.contains("42.0"),
            "Python's read_last_gauge requires isinstance(percent_left, int); got: {text}"
        );
    }

    #[test]
    fn lock_path_matches_pythons_with_suffix_not_a_json_lock_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = store_path(dir.path());
        assert_eq!(
            lock_path(&path).file_name().unwrap(),
            "gauge-state.lock",
            "Python's limits.py uses Path.with_suffix(\".lock\"), which \
             replaces the .json extension rather than appending to it"
        );
    }

    #[test]
    fn a_python_written_entry_s_secondary_window_loads_into_the_typed_fields() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            store_path(dir.path()),
            br#"{"chatgpt": {"percent_left": 40, "secondary_percent_left": 3,
                "secondary_label": "weekly", "secondary_resets_at": "2026-11-09T00:00:00+00:00",
                "secondary_window_minutes": 10080, "observed_at": "2026-11-02T00:00:00+00:00",
                "vendor": "chatgpt"}}"#,
        )
        .unwrap();
        let map = load(dir.path());
        let entry = &map["chatgpt"];
        assert_eq!(entry.percent_left_secondary, Some(3.0));
        assert_eq!(entry.label_secondary, Some("weekly".to_string()));
        assert_eq!(
            entry.resets_at_secondary,
            Some("2026-11-09T00:00:00+00:00".to_string())
        );
        assert_eq!(entry.window_minutes_secondary, Some(10080));
        assert!(
            entry.extra.is_empty(),
            "Python's own field names must bind to the typed struct, not fall through to extra"
        );
    }

    #[test]
    fn unrecognized_field_names_round_trip_via_extra() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            store_path(dir.path()),
            br#"{"chatgpt": {"percent_left": 10, "secondary_window_pct": 55}}"#,
        )
        .unwrap();
        let map = load(dir.path());
        assert_eq!(
            map["chatgpt"].extra.get("secondary_window_pct"),
            Some(&Value::from(55))
        );
    }
}

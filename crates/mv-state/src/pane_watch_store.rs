//! Atomic on-disk store for per-pane last-change observations, backing
//! `multiverse workers`' stalled-pane detection (card `20261115-b-59e017`).
//!
//! Herdr's own `agent_status` cannot tell a lane quietly waiting on its own
//! background job from a dead one — both can sit at the same reported
//! status indefinitely. Herdr's `revision`/`state_change_seq` cannot
//! either: they track Herdr's own bookkeeping, not the pane's content, and
//! sit still for the entire length of a long-running command even though
//! the agent is plainly busy — flagging that as stalled is exactly the
//! false positive card `20261115-b-59e017` found, and Mike's rule closes
//! stalled lanes, so a false positive there is dangerous, not just noisy.
//! What this store watches instead is a hash of the pane's own content
//! (the caller's `herdr pane read <pane> --lines N`), which only moves
//! when something actually printed to the screen. This store remembers,
//! per pane, the last content hash observed and the instant it was first
//! seen that way — so a caller comparing today's hash against the stored
//! one can tell "unchanged since 40 minutes ago" from "changed a moment
//! ago" across separate `multiverse` invocations, which a single read can
//! never do on its own. This module only owns that persistence; deciding
//! what counts as "stalled" (the threshold, and what to render) is the
//! caller's policy, not this store's. Herdr's own status and revision
//! never feed this store at all, so neither can reset or advance a pane's
//! age on its own.
//!
//! A pane Herdr no longer lists is dropped from the store the next time
//! [`observe`] is called with the full current live set, so this never
//! grows unbounded over a fleet's lifetime.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::breaker_store::support;

/// One pane's last-known content hash and when it was first seen that way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneObservation {
    /// The hash last observed for this pane, or `None` if the cycle that
    /// set `last_changed_at` had no usable read (a failed or skipped
    /// `herdr pane read`) rather than the pane being genuinely empty.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// When this hash was first seen, formatted with
    /// `mv_core::time::format_seconds_utc` and read back with
    /// `mv_core::time::parse_flexible`.
    pub last_changed_at: String,
}

/// Pane-watch state keyed by Herdr `pane_id`.
pub type PaneMap = BTreeMap<String, PaneObservation>;

/// This invocation's signal for one live pane: a hash of its content, from
/// the caller's own `herdr pane read <pane> --lines N`. `None` means the
/// caller could not read the pane this cycle (a failed or skipped read),
/// never "the pane is empty" — an empty pane still hashes to a concrete
/// value.
pub struct PaneSignal<'a> {
    /// Herdr's own identifier for the pane, used as this store's map key.
    pub pane_id: &'a str,
    /// This cycle's content hash for the pane; see the struct doc above
    /// for why `None` must never be read as "empty pane".
    pub content_hash: Option<String>,
}

/// Path to this project's pane-watch store, inside its state dir.
pub fn store_path(state_dir: &Path) -> PathBuf {
    state_dir.join("pane-watch.json")
}

fn lock_path(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".lock");
    PathBuf::from(p)
}

/// Load the pane-watch store, failing open (empty map) on a missing or
/// unparseable file — a damaged store must never block `workers` from
/// rendering, only cost it the staleness signal for this one cycle.
pub fn load(state_dir: &Path) -> PaneMap {
    let path = store_path(state_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return PaneMap::new();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Record `now`'s observation of every pane in `panes`, pruning any pane
/// not present in this call, and return how long each pane's content hash
/// has gone unchanged, keyed by `pane_id`. A pane observed for the first
/// time, or whose hash has moved since the stored one, reports `now`
/// itself — "just changed" — rather than any inherited age. A pane
/// supplying no hash (`None`, a failed or skipped read) carries no change
/// signal at all, so it is always reported as "just changed" instead of
/// silently latching onto a false staleness.
pub fn observe(
    state_dir: &Path,
    panes: &[PaneSignal<'_>],
    now: OffsetDateTime,
) -> Result<BTreeMap<String, OffsetDateTime>, support::StoreError> {
    let path = store_path(state_dir);
    let _lock = support::acquire(&lock_path(&path))?;
    let mut stored: PaneMap = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();

    let mut last_changed = BTreeMap::new();
    let mut kept = PaneMap::new();
    // Each live pane's entry is moved out of `stored` and into `kept`
    // below. Whatever is left in `stored` once the loop ends belongs to
    // panes absent from this call's `panes` list and is simply never
    // copied into `kept`, which is how a pane Herdr no longer lists gets
    // dropped from the file written at the end of this function.
    for signal in panes {
        let has_signal = signal.content_hash.is_some();
        let previous = stored.remove(signal.pane_id);
        let unchanged = has_signal
            && previous
                .as_ref()
                .is_some_and(|previous| previous.content_hash == signal.content_hash);
        let last_changed_at = if unchanged {
            previous
                .as_ref()
                .and_then(|previous| mv_core::time::parse_flexible(&previous.last_changed_at).ok())
                .unwrap_or(now)
        } else {
            now
        };
        kept.insert(
            signal.pane_id.to_string(),
            PaneObservation {
                content_hash: signal.content_hash.clone(),
                last_changed_at: mv_core::time::format_seconds_utc(last_changed_at),
            },
        );
        last_changed.insert(signal.pane_id.to_string(), last_changed_at);
    }

    let mut bytes = serde_json::to_vec_pretty(&kept).expect("PaneMap always serializes");
    bytes.push(b'\n');
    support::write_atomic(&path, &bytes)?;
    Ok(last_changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_first_observation_reports_itself_as_just_changed() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-15 12:00:00 UTC);
        let signals = [PaneSignal {
            pane_id: "w1:p1",
            content_hash: Some("hash-a".into()),
        }];
        let result = observe(dir.path(), &signals, now).unwrap();
        assert_eq!(result["w1:p1"], now);
    }

    #[test]
    fn an_unchanged_hash_keeps_the_originally_stored_instant() {
        let dir = tempfile::tempdir().unwrap();
        let first_seen = datetime!(2026-11-15 12:00:00 UTC);
        let later = first_seen + time::Duration::minutes(31);
        let signals = [PaneSignal {
            pane_id: "w1:p1",
            content_hash: Some("hash-a".into()),
        }];
        observe(dir.path(), &signals, first_seen).unwrap();
        let result = observe(dir.path(), &signals, later).unwrap();
        assert_eq!(result["w1:p1"], first_seen);
    }

    /// Card `20261115-b-59e017`'s revert check: the signal this store
    /// compares is the content hash alone. A caller that holds some other
    /// field (Herdr's `revision`/`state_change_seq`, say) constant while
    /// the hash itself moves still reports "just changed" here — proof
    /// that this store can no longer be fooled by a pane whose own
    /// bookkeeping sits still while its content does not, the exact shape
    /// of the false stall `4e8d178`'s revision-keyed detector was prone to
    /// (it never read content at all, so two different contents hashing
    /// differently here were indistinguishable to it as long as the
    /// revision pair it keyed on held).
    #[test]
    fn a_changed_hash_resets_the_last_changed_instant_even_with_other_fields_held_constant() {
        let dir = tempfile::tempdir().unwrap();
        let first_seen = datetime!(2026-11-15 12:00:00 UTC);
        let later = first_seen + time::Duration::minutes(31);
        observe(
            dir.path(),
            &[PaneSignal {
                pane_id: "w1:p1",
                content_hash: Some("hash-a".into()),
            }],
            first_seen,
        )
        .unwrap();
        let result = observe(
            dir.path(),
            &[PaneSignal {
                pane_id: "w1:p1",
                content_hash: Some("hash-b".into()),
            }],
            later,
        )
        .unwrap();
        assert_eq!(result["w1:p1"], later);
    }

    #[test]
    fn a_pane_with_no_change_signal_at_all_is_always_just_changed() {
        let dir = tempfile::tempdir().unwrap();
        let first_seen = datetime!(2026-11-15 12:00:00 UTC);
        let later = first_seen + time::Duration::minutes(31);
        let signals = [PaneSignal {
            pane_id: "w1:p1",
            content_hash: None,
        }];
        observe(dir.path(), &signals, first_seen).unwrap();
        let result = observe(dir.path(), &signals, later).unwrap();
        assert_eq!(result["w1:p1"], later);
    }

    #[test]
    fn a_pane_no_longer_present_is_pruned_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-15 12:00:00 UTC);
        observe(
            dir.path(),
            &[PaneSignal {
                pane_id: "w1:p1",
                content_hash: Some("hash-a".into()),
            }],
            now,
        )
        .unwrap();
        observe(dir.path(), &[], now).unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn independent_panes_track_independently() {
        let dir = tempfile::tempdir().unwrap();
        let first_seen = datetime!(2026-11-15 12:00:00 UTC);
        let later = first_seen + time::Duration::minutes(31);
        observe(
            dir.path(),
            &[
                PaneSignal {
                    pane_id: "w1:p1",
                    content_hash: Some("hash-a".into()),
                },
                PaneSignal {
                    pane_id: "w1:p2",
                    content_hash: Some("hash-c".into()),
                },
            ],
            first_seen,
        )
        .unwrap();
        let result = observe(
            dir.path(),
            &[
                PaneSignal {
                    pane_id: "w1:p1",
                    content_hash: Some("hash-a".into()),
                },
                PaneSignal {
                    pane_id: "w1:p2",
                    content_hash: Some("hash-d".into()),
                },
            ],
            later,
        )
        .unwrap();
        assert_eq!(result["w1:p1"], first_seen, "unchanged pane keeps its age");
        assert_eq!(result["w1:p2"], later, "changed pane resets");
    }

    #[test]
    fn a_missing_store_loads_as_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
    }

    #[test]
    fn a_corrupt_store_fails_open_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path()).unwrap();
        fs::write(store_path(dir.path()), b"not json").unwrap();
        assert!(load(dir.path()).is_empty());
    }
}

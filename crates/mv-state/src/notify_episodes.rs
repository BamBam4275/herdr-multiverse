//! The hub project's own per-episode state table for `multiverse sweep`'s
//! alert conditions — the *episode* half of `notify.py`'s own
//! `notify-state.json` (that module's doc comment, "sweep" bullet;
//! `_load_state`/`_save_state`/`_process_episode`, `notify.py:1191-1500`).
//! Deliberately a different module from [`crate::notify`], which ports the
//! *other* half of the same filename: the fleet-wide rate-limiter file kept
//! under the selected config directory. This one lives under a project's
//! own `.multiverse/`, one entry per `(condition, subject)` episode, so a
//! condition no longer observed closes its episode outright (deleted, not
//! merely marked) and the next occurrence opens a fresh one — `notify.py`'s
//! own `_state_path` doc comment draws the identical distinction between
//! the two same-named files.
//!
//! Card `20261103-b-c5d832`: only the picker/blocked-pane half this card
//! names is ported here — [`advance_picker_episode`]/[`close_picker_episode`]
//! mirror `notify.picker_alert`'s and `notify.close_picker_episode`'s own
//! state bookkeeping (`notify.py:1503-1633`), minus the ntfy push and hub
//! activity post those two functions also make: no `notify.send`/
//! `activity.send` wiring exists in `mv-cli`'s `sweep --tick` yet (see
//! `commands::sweep`'s own module doc and this task's `result.md`).
//! [`process_episode`] is the general `_process_episode` primitive
//! (`notify.py:1443-1500`) every other sweep condition (stale board,
//! battery, disk, open ask) will eventually need too; only the picker
//! caller exists in this checkout today.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::activity::support;

/// Sibling to [`crate::notify`]'s fleet-wide `notify-state.json` but a *different file* at a
/// different path: this one is `<hub project>/.multiverse/notify-state.json`,
/// not a file under the selected config directory.
pub const STATE_FILENAME: &str = "notify-state.json";

/// `.lock` sibling serializing one sweep's load -> act -> save span — the
/// same non-blocking `fcntl.flock` idiom `notify._sweep_lock` uses.
pub const LOCK_FILENAME: &str = "notify-state.json.lock";

/// `notify.PICKER_ALERT_AFTER` (== `BLOCKED_ALERT_AFTER`): how long a
/// picker episode must have stood open before it is alert-worthy.
pub const PICKER_ALERT_AFTER: Duration = Duration::minutes(30);

pub fn state_path(hub_root: &Path) -> PathBuf {
    hub_root.join(".multiverse").join(STATE_FILENAME)
}

fn lock_path(hub_root: &Path) -> PathBuf {
    hub_root.join(".multiverse").join(LOCK_FILENAME)
}

/// One `(condition, subject)` episode: `notify.py`'s own two required
/// fields, plus `picker_alert`'s own `activity_posted` flag.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Episode {
    pub first_seen: String,
    #[serde(default)]
    pub alerted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub activity_posted: bool,
}

pub type EpisodeTable = BTreeMap<String, Episode>;

/// The persisted table, and whether this load just recovered from a corrupt
/// file — mirrors `_load_state`'s own `(state, corrupt)` return.
///
/// A **missing** file is the ordinary first-run case: `({}, false)`. A file
/// that exists but cannot be trusted (bad JSON, or JSON that isn't an
/// object) is moved aside to `<name>.corrupt-<UTC>` (never deleted — it
/// might hold a clue) and read back as `({}, true)`, so a caller can
/// pre-mark this sweep's newly opened episodes `alerted` instead of paging
/// for every one of them at once, the same recovery `_load_state`'s own doc
/// comment describes.
///
/// Any key whose value does not parse as a valid `{first_seen, alerted}`
/// episode is dropped. `notify.py`'s own table also carries non-episode
/// entries (its disk-reading array, keyed `_host_disk_readings`); no
/// condition that writes one has a Rust port yet, so this function has
/// nothing to preserve them as — a future port of one of those conditions
/// will need to extend this loader the same way `_load_state` special-cases
/// `DISK_READINGS_KEY`, or such an entry will silently not round-trip.
pub fn load(hub_root: &Path) -> (EpisodeTable, bool) {
    let path = state_path(hub_root);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return (EpisodeTable::new(), false)
        }
        Err(_) => {
            move_aside(&path);
            return (EpisodeTable::new(), true);
        }
    };
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => {
            move_aside(&path);
            return (EpisodeTable::new(), true);
        }
    };
    let Some(object) = parsed.as_object() else {
        move_aside(&path);
        return (EpisodeTable::new(), true);
    };
    let mut table = EpisodeTable::new();
    for (key, value) in object {
        if let Ok(episode) = serde_json::from_value::<Episode>(value.clone()) {
            if support::parse_flexible(&episode.first_seen).is_some() {
                table.insert(key.clone(), episode);
            }
        }
    }
    (table, false)
}

fn move_aside(path: &Path) {
    let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return;
    };
    let now = OffsetDateTime::now_utc();
    let stamp = format!(
        "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
    );
    let corrupt_path = path.with_file_name(format!("{name}.corrupt-{stamp}"));
    let _ = fs::rename(path, &corrupt_path);
}

/// Persist the table atomically (temp file, then rename) — the same shape
/// `_save_state`'s own `os.replace` gets. Sorted through [`serde_json::Value`]
/// rather than the struct directly (this crate enables no `preserve_order`
/// feature, so `Value::Object` is backed by a `BTreeMap`) to match
/// `_save_state`'s `json.dumps(state, sort_keys=True, indent=2)` key order
/// at every level, not just the table's own top-level keys.
pub fn save(hub_root: &Path, table: &EpisodeTable) -> io::Result<()> {
    let path = state_path(hub_root);
    let value = serde_json::to_value(table).expect("EpisodeTable always serializes");
    let mut text = serde_json::to_string_pretty(&value).expect("Value always serializes");
    text.push('\n');
    support::atomic_write(&path, text.as_bytes(), 0o600)
}

/// Acquire this hub project's non-blocking episode-table lock — the same
/// `fcntl.flock(LOCK_EX | LOCK_NB)` posture `notify._sweep_lock` takes: a
/// caller that loses the race gets `None` back and must skip its own work
/// for this tick entirely, never wait on the winner.
fn try_lock(hub_root: &Path) -> Option<mv_core::lock::LockGuard> {
    mv_core::lock::acquire(
        &lock_path(hub_root),
        "notify-episodes",
        std::time::Duration::ZERO,
    )
    .ok()
}

/// Advance one `(condition, subject)` episode and report whether *this*
/// call should alert — `notify._process_episode`, verbatim.
///
/// `active = None` preserves an episode when its observation is unknown.
/// `active = Some(false)` closes an open episode outright (deleted, not
/// just marked) so the condition's next occurrence is a fresh episode.
/// `active = Some(true)` opens one if none exists yet; `min_age` gates the
/// alert on how long `first_seen` has stood, while `requires_prior_seen`
/// gates it on this not being the very first sighting at all.
///
/// `seed_alerted` is the one-sweep recovery from a corrupt state file
/// (this module's own [`load`] second return value): every episode this
/// call newly opens is recorded already `alerted: true` and this call
/// returns `false` — the state just lost can't say which of these were
/// already paged for, so the safe read is "yes", not an alert storm
/// re-paging all of them at once.
///
/// Does not otherwise set `alerted` itself — the caller persists
/// `alerted: true` only after an actual send reports success, so a queued
/// copy still counts as the episode's one page.
pub fn process_episode(
    table: &mut EpisodeTable,
    key: &str,
    active: Option<bool>,
    now: OffsetDateTime,
    min_age: Option<Duration>,
    requires_prior_seen: bool,
    seed_alerted: bool,
) -> bool {
    let Some(active) = active else {
        return false;
    };
    if !active {
        table.remove(key);
        return false;
    }

    if !table.contains_key(key) {
        table.insert(
            key.to_string(),
            Episode {
                first_seen: support::format_seconds_utc(now),
                alerted: seed_alerted,
                activity_posted: false,
            },
        );
        if seed_alerted || requires_prior_seen {
            return false;
        }
    }

    let entry = table.get(key).expect("inserted above or already present");
    if entry.alerted {
        return false;
    }
    if let Some(min_age) = min_age {
        match support::parse_flexible(&entry.first_seen) {
            Some(first_seen) if now - first_seen >= min_age => {}
            _ => return false,
        }
    }
    true
}

/// Card `20261103-b-15d3ca`: exposed (was crate-private) so a caller that
/// needs to post an activity message or ntfy push *about* a specific
/// episode — [`crate::activity::send_message`]'s own `id` has to be
/// stable per episode, the same way `notify.picker_alert`'s own
/// `hashlib.sha256(key.encode("utf-8")).hexdigest()` hashes this exact
/// string — can derive the same key this module uses internally, rather
/// than guessing at its format.
pub fn picker_key(project_name: &str, question: &str) -> String {
    format!("picker_blocked|{project_name}|{question}")
}

fn picker_prefix(project_name: &str) -> String {
    format!("picker_blocked|{project_name}|")
}

/// The state-table bookkeeping half of `notify.picker_alert`
/// (`notify.py:1503-1615`), minus the ntfy push and hub activity post — see
/// this module's own doc comment for why those are not wired here yet.
///
/// Keyed by the project and the picker's own (caller-normalized) question,
/// so a genuinely changed question opens a new episode while a repeated
/// read of the same picker — including a flap between `blocked` and `done`
/// the caller detects purely from pane text — remains the same episode and
/// keeps its `first_seen`. A previous picker episode for this project (if
/// the question text itself changed) carries its `first_seen` forward.
///
/// Returns [`process_episode`]'s own min-age-gated verdict. This port never
/// sets `alerted` itself (see [`process_episode`]'s own doc comment) — with
/// no sender wired into `sweep --tick` yet, that field stays `false`
/// forever until a future task adds one; `ready` is exposed for that
/// task's own alert-sending logic to consult, and must not be read as "an
/// alert was sent" by anything in this checkout today.
pub fn advance_picker_episode(
    table: &mut EpisodeTable,
    project_name: &str,
    question: &str,
    now: OffsetDateTime,
    seed_alerted: bool,
) -> bool {
    let key = picker_key(project_name, question);
    let prefix = picker_prefix(project_name);

    let mut previous_first_seen: Option<OffsetDateTime> = None;
    for (old_key, entry) in table.iter() {
        if old_key.starts_with(&prefix) {
            if let Some(first_seen) = support::parse_flexible(&entry.first_seen) {
                if previous_first_seen.is_none_or(|previous| first_seen < previous) {
                    previous_first_seen = Some(first_seen);
                }
            }
        }
    }
    let stale: Vec<String> = table
        .keys()
        .filter(|candidate| candidate.starts_with(&prefix) && **candidate != key)
        .cloned()
        .collect();
    for old_key in stale {
        table.remove(&old_key);
    }
    if !table.contains_key(&key) {
        if let Some(first_seen) = previous_first_seen {
            table.insert(
                key.clone(),
                Episode {
                    first_seen: support::format_seconds_utc(first_seen),
                    alerted: false,
                    activity_posted: false,
                },
            );
        }
    }

    process_episode(
        table,
        &key,
        Some(true),
        now,
        Some(PICKER_ALERT_AFTER),
        false,
        seed_alerted,
    )
}

/// `notify.close_picker_episode`'s own state mutation: remove any open
/// picker episode(s) for `project_name`. Returns whether anything changed,
/// so a caller only pays for a save when it did.
pub fn close_picker_episode(table: &mut EpisodeTable, project_name: &str) -> bool {
    let prefix = picker_prefix(project_name);
    let stale: Vec<String> = table
        .keys()
        .filter(|candidate| candidate.starts_with(&prefix))
        .cloned()
        .collect();
    let changed = !stale.is_empty();
    for key in stale {
        table.remove(&key);
    }
    changed
}

/// Load, advance, and persist one project's picker episode under the hub
/// project's own lock — the on-disk counterpart to
/// [`advance_picker_episode`]. `None` means either no hub project is
/// configured (the caller passes `hub_root`, so this never checks that
/// itself) or another sweep holds the lock this tick; either way the
/// caller's own episode bookkeeping is skipped entirely for this tick,
/// the same "skip, don't wait" posture `notify._sweep_lock`'s every caller
/// takes.
pub fn report_picker_episode(
    hub_root: &Path,
    project_name: &str,
    question: &str,
    now: OffsetDateTime,
) -> Option<bool> {
    let _guard = try_lock(hub_root)?;
    let (mut table, corrupt) = load(hub_root);
    let ready = advance_picker_episode(&mut table, project_name, question, now, corrupt);
    let _ = save(hub_root, &table);
    Some(ready)
}

/// The full counterpart to `notify.picker_alert` (`notify.py:1503-1615`):
/// [`report_picker_episode`]'s own load/advance/save, plus the hub activity
/// post and ntfy push that function also makes once a picker episode has
/// stood open at least [`PICKER_ALERT_AFTER`] *and* `mike_idle` says Mike
/// is away. `apply = false` mirrors `picker_alert`'s own `dry_run`
/// parameter: both sends are skipped and nothing is persisted at all — the
/// same no-op posture a `--dry-run` sweep takes everywhere else in this
/// fleet (Python's own `if not dry_run: _save_state(...)`, last line of
/// that function).
///
/// `post_activity` and `push_ntfy` run only while this call's own lock is
/// held — the same single critical section `picker_alert` itself wraps
/// both sends in — so a concurrent sweep for the same project can never
/// also decide to page while this one is mid-send. `post_activity` reports
/// whether the hub activity post landed; `push_ntfy` reports whether the
/// alert should count as delivered (Python's own `outcome in {"sent",
/// "queued"}` check — a queued copy still counts as the episode's one
/// page, `notify.send`'s own "first message allowed through" accounting).
/// Each is one-shot per episode, tracked by the entry's own
/// `activity_posted`/`alerted` flags, and a `false` return lets a later
/// sweep retry it.
///
/// Returns [`advance_picker_episode`]'s own `ready` verdict, or `None` if
/// another sweep holds the lock this tick (in which case neither closure
/// is ever called).
#[allow(clippy::too_many_arguments)]
pub fn report_and_alert_picker_episode(
    hub_root: &Path,
    project_name: &str,
    question: &str,
    now: OffsetDateTime,
    apply: bool,
    mike_idle: bool,
    mut post_activity: impl FnMut() -> bool,
    mut push_ntfy: impl FnMut() -> bool,
) -> Option<bool> {
    let _guard = try_lock(hub_root)?;
    let (mut table, corrupt) = load(hub_root);
    let ready = advance_picker_episode(&mut table, project_name, question, now, corrupt);
    let key = picker_key(project_name, question);

    let age_ready = table
        .get(&key)
        .and_then(|entry| support::parse_flexible(&entry.first_seen))
        .is_some_and(|first_seen| now - first_seen >= PICKER_ALERT_AFTER);

    if apply && age_ready && mike_idle {
        if !table.get(&key).is_some_and(|entry| entry.activity_posted) && post_activity() {
            if let Some(entry) = table.get_mut(&key) {
                entry.activity_posted = true;
            }
        }
        if ready && !table.get(&key).is_some_and(|entry| entry.alerted) && push_ntfy() {
            if let Some(entry) = table.get_mut(&key) {
                entry.alerted = true;
            }
        }
    }

    if apply {
        let _ = save(hub_root, &table);
    }
    Some(ready)
}

/// Load, close, and (if anything changed) persist — the on-disk counterpart
/// to [`close_picker_episode`]. `apply = false` mirrors
/// `notify.close_picker_episode`'s own `dry_run` parameter: the episode is
/// still computed as closed for this call's own return, but nothing is
/// written (Python's `if changed and not dry_run: _save_state(...)`).
pub fn close_picker_episode_on_disk(
    hub_root: &Path,
    project_name: &str,
    apply: bool,
) -> Option<()> {
    let _guard = try_lock(hub_root)?;
    let (mut table, _corrupt) = load(hub_root);
    if close_picker_episode(&mut table, project_name) && apply {
        let _ = save(hub_root, &table);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mv-state-notify-episodes-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- load/save -----------------------------------------------------

    #[test]
    fn a_missing_file_loads_as_an_empty_uncorrupted_table() {
        let root = temp_dir("missing");
        assert_eq!(load(&root), (EpisodeTable::new(), false));
    }

    #[test]
    fn a_saved_table_round_trips() {
        let root = temp_dir("roundtrip");
        let mut table = EpisodeTable::new();
        table.insert(
            "picker_blocked|proj|Q?".to_string(),
            Episode {
                first_seen: "2026-11-02T12:00:00+00:00".to_string(),
                alerted: false,
                activity_posted: true,
            },
        );
        save(&root, &table).unwrap();
        let (loaded, corrupt) = load(&root);
        assert!(!corrupt);
        assert_eq!(loaded, table);
    }

    #[test]
    fn a_malformed_episode_is_dropped_not_fatal() {
        let root = temp_dir("malformed-entry");
        fs::create_dir_all(root.join(".multiverse")).unwrap();
        fs::write(
            state_path(&root),
            r#"{"ok": {"first_seen": "2026-11-02T12:00:00+00:00", "alerted": false}, "bad": {"alerted": true}}"#,
        )
        .unwrap();
        let (table, corrupt) = load(&root);
        assert!(!corrupt);
        assert_eq!(table.len(), 1);
        assert!(table.contains_key("ok"));
    }

    #[test]
    fn unparsable_json_is_moved_aside_and_reads_as_corrupt() {
        let root = temp_dir("corrupt-json");
        fs::create_dir_all(root.join(".multiverse")).unwrap();
        fs::write(state_path(&root), "not json").unwrap();
        let (table, corrupt) = load(&root);
        assert!(corrupt);
        assert!(table.is_empty());
        assert!(!state_path(&root).exists(), "original path moved aside");
        let siblings: Vec<_> = fs::read_dir(root.join(".multiverse"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            siblings
                .iter()
                .any(|name| name.starts_with("notify-state.json.corrupt-")),
            "{siblings:?}"
        );
    }

    // --- process_episode (`_process_episode`) ---------------------------

    #[test]
    fn active_none_leaves_an_existing_episode_untouched_and_never_alerts() {
        let mut table = EpisodeTable::new();
        table.insert(
            "k".to_string(),
            Episode {
                first_seen: "2026-11-02T12:00:00+00:00".to_string(),
                alerted: false,
                activity_posted: false,
            },
        );
        let now = datetime!(2026-11-02 13:00:00 UTC);
        assert!(!process_episode(
            &mut table, "k", None, now, None, false, false
        ));
        assert!(table.contains_key("k"));
    }

    #[test]
    fn active_false_deletes_the_episode_outright() {
        let mut table = EpisodeTable::new();
        table.insert(
            "k".to_string(),
            Episode {
                first_seen: "2026-11-02T12:00:00+00:00".to_string(),
                alerted: true,
                activity_posted: false,
            },
        );
        let now = datetime!(2026-11-02 13:00:00 UTC);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(false),
            now,
            None,
            false,
            false
        ));
        assert!(!table.contains_key("k"));
    }

    #[test]
    fn a_new_episode_with_no_min_age_alerts_immediately() {
        let mut table = EpisodeTable::new();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            None,
            false,
            false
        ));
        assert_eq!(table["k"].first_seen, support::format_seconds_utc(now));
        assert!(!table["k"].alerted);
    }

    #[test]
    fn a_new_episode_under_min_age_does_not_alert_yet_but_still_opens() {
        let mut table = EpisodeTable::new();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            Some(Duration::minutes(30)),
            false,
            false
        ));
        assert!(table.contains_key("k"));

        let later = now + Duration::minutes(29);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            later,
            Some(Duration::minutes(30)),
            false,
            false
        ));

        let ready_at = now + Duration::minutes(30);
        assert!(process_episode(
            &mut table,
            "k",
            Some(true),
            ready_at,
            Some(Duration::minutes(30)),
            false,
            false
        ));
    }

    #[test]
    fn an_already_alerted_episode_never_alerts_again() {
        let mut table = EpisodeTable::new();
        table.insert(
            "k".to_string(),
            Episode {
                first_seen: "2026-11-02T12:00:00+00:00".to_string(),
                alerted: true,
                activity_posted: false,
            },
        );
        let now = datetime!(2026-11-02 14:00:00 UTC);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            None,
            false,
            false
        ));
    }

    #[test]
    fn requires_prior_seen_suppresses_only_the_opening_sighting() {
        let mut table = EpisodeTable::new();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            None,
            true,
            false
        ));
        assert!(process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            None,
            true,
            false
        ));
    }

    #[test]
    fn seed_alerted_pre_marks_a_newly_opened_episode_and_never_alerts_it() {
        let mut table = EpisodeTable::new();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            now,
            None,
            false,
            true
        ));
        assert!(table["k"].alerted, "seeded entry is pre-marked alerted");
        assert!(!process_episode(
            &mut table,
            "k",
            Some(true),
            now + Duration::hours(1),
            None,
            false,
            true
        ));
    }

    // --- advance_picker_episode / close_picker_episode ------------------

    #[test]
    fn a_picker_flap_keeps_the_same_first_seen_and_does_not_alert_early() {
        let mut table = EpisodeTable::new();
        let start = datetime!(2026-10-28 12:00:00 UTC);
        let question = "How should I close the download check?";

        assert!(!advance_picker_episode(
            &mut table, "proj", question, start, false
        ));
        let first_seen = table[&picker_key("proj", question)].first_seen.clone();

        // The pane flaps to `done` at minute 15 with the identical picker
        // text still on screen — the caller re-detects the same question,
        // so the same episode (and `first_seen`) must carry forward.
        let flapped = start + Duration::minutes(15);
        assert!(!advance_picker_episode(
            &mut table, "proj", question, flapped, false
        ));
        assert_eq!(table[&picker_key("proj", question)].first_seen, first_seen);

        // 30 minutes after the *original* sighting, not after the flap.
        let ready_at = start + Duration::minutes(31);
        assert!(advance_picker_episode(
            &mut table, "proj", question, ready_at, false
        ));
    }

    #[test]
    fn a_changed_question_opens_a_new_episode_but_carries_first_seen_forward() {
        let mut table = EpisodeTable::new();
        let start = datetime!(2026-11-02 12:00:00 UTC);
        advance_picker_episode(&mut table, "proj", "First question?", start, false);

        let changed_at = start + Duration::minutes(5);
        advance_picker_episode(&mut table, "proj", "Second question?", changed_at, false);

        assert_eq!(table.len(), 1, "the old question's episode is gone");
        let entry = &table[&picker_key("proj", "Second question?")];
        assert_eq!(entry.first_seen, support::format_seconds_utc(start));
    }

    #[test]
    fn two_projects_own_separate_picker_episodes() {
        let mut table = EpisodeTable::new();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        advance_picker_episode(&mut table, "a", "Q?", now, false);
        advance_picker_episode(&mut table, "b", "Q?", now, false);
        assert_eq!(table.len(), 2);
        assert!(close_picker_episode(&mut table, "a"));
        assert_eq!(table.len(), 1);
        assert!(table.contains_key(&picker_key("b", "Q?")));
    }

    #[test]
    fn closing_an_absent_episode_reports_no_change() {
        let mut table = EpisodeTable::new();
        assert!(!close_picker_episode(&mut table, "proj"));
    }

    // --- locked on-disk wrappers -----------------------------------------

    #[test]
    fn report_picker_episode_persists_across_calls() {
        let root = temp_dir("report-persist");
        let start = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(
            report_picker_episode(&root, "proj", "Q?", start),
            Some(false)
        );
        let (table, _) = load(&root);
        assert!(table.contains_key(&picker_key("proj", "Q?")));

        let ready_at = start + Duration::minutes(31);
        assert_eq!(
            report_picker_episode(&root, "proj", "Q?", ready_at),
            Some(true)
        );
    }

    #[test]
    fn close_picker_episode_on_disk_removes_a_persisted_episode() {
        let root = temp_dir("close-persist");
        let now = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", now);
        close_picker_episode_on_disk(&root, "proj", true);
        let (table, _) = load(&root);
        assert!(table.is_empty());
    }

    #[test]
    fn close_picker_episode_on_disk_with_apply_false_computes_but_does_not_persist() {
        let root = temp_dir("close-dry-run");
        let now = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", now);
        close_picker_episode_on_disk(&root, "proj", false);
        let (table, _) = load(&root);
        assert!(!table.is_empty(), "a dry run must not persist the close");
    }

    #[test]
    fn a_held_lock_skips_the_whole_report_rather_than_waiting() {
        // `mv_core::lock::acquire` is reentrant per-thread, so holding the
        // lock from this same thread would not reproduce contention at
        // all — a second process (here, another thread) must hold it.
        let root = temp_dir("locked");
        let path = lock_path(&root);
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = mv_core::lock::acquire_default(&path, "test").unwrap();
            tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        rx.recv().unwrap();

        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(report_picker_episode(&root, "proj", "Q?", now), None);
        assert_eq!(close_picker_episode_on_disk(&root, "proj", true), None);

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }

    // --- report_and_alert_picker_episode ---------------------------------

    #[test]
    fn under_min_age_neither_closure_runs_even_with_mike_idle_and_apply() {
        let root = temp_dir("alert-under-age");
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let mut activity_calls = 0;
        let mut ntfy_calls = 0;
        let ready = report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            now,
            true,
            true,
            || {
                activity_calls += 1;
                true
            },
            || {
                ntfy_calls += 1;
                true
            },
        );
        assert_eq!(ready, Some(false));
        assert_eq!(activity_calls, 0, "min age not yet reached");
        assert_eq!(ntfy_calls, 0, "min age not yet reached");
    }

    #[test]
    fn mike_present_suppresses_both_sends_past_min_age() {
        let root = temp_dir("alert-mike-present");
        let start = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", start);
        let ready_at = start + PICKER_ALERT_AFTER;

        let mut activity_calls = 0;
        let mut ntfy_calls = 0;
        report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at,
            true,
            false,
            || {
                activity_calls += 1;
                true
            },
            || {
                ntfy_calls += 1;
                true
            },
        );
        assert_eq!(activity_calls, 0, "mike is present, nothing should page");
        assert_eq!(ntfy_calls, 0, "mike is present, nothing should page");
    }

    #[test]
    fn apply_false_skips_both_sends_and_persists_nothing() {
        let root = temp_dir("alert-dry-run");
        let start = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", start);
        let ready_at = start + PICKER_ALERT_AFTER;

        let mut activity_calls = 0;
        let mut ntfy_calls = 0;
        let ready = report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at,
            false,
            true,
            || {
                activity_calls += 1;
                true
            },
            || {
                ntfy_calls += 1;
                true
            },
        );
        assert_eq!(ready, Some(true), "the verdict is still computed");
        assert_eq!(activity_calls, 0, "a dry run must never send");
        assert_eq!(ntfy_calls, 0, "a dry run must never send");
        let (table, _) = load(&root);
        assert_eq!(
            table[&picker_key("proj", "Q?")].first_seen,
            support::format_seconds_utc(start),
            "a dry run must not persist anything either"
        );
    }

    #[test]
    fn past_min_age_with_mike_idle_sends_both_and_marks_the_episode() {
        let root = temp_dir("alert-sends");
        let start = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", start);
        let ready_at = start + PICKER_ALERT_AFTER;

        let mut activity_calls = 0;
        let mut ntfy_calls = 0;
        let ready = report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at,
            true,
            true,
            || {
                activity_calls += 1;
                true
            },
            || {
                ntfy_calls += 1;
                true
            },
        );
        assert_eq!(ready, Some(true));
        assert_eq!(activity_calls, 1);
        assert_eq!(ntfy_calls, 1);
        let (table, _) = load(&root);
        let entry = &table[&picker_key("proj", "Q?")];
        assert!(entry.activity_posted);
        assert!(entry.alerted);

        // A later sweep of the same, now-alerted episode must not resend
        // either side effect — `activity_posted`/`alerted` are both
        // one-shot once they land.
        let mut activity_calls = 0;
        let mut ntfy_calls = 0;
        report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at + Duration::minutes(5),
            true,
            true,
            || {
                activity_calls += 1;
                true
            },
            || {
                ntfy_calls += 1;
                true
            },
        );
        assert_eq!(activity_calls, 0, "activity post is one-shot");
        assert_eq!(ntfy_calls, 0, "ntfy push is one-shot once alerted");
    }

    #[test]
    fn a_failed_activity_post_retries_on_a_later_sweep_independent_of_ntfy() {
        let root = temp_dir("alert-activity-retry");
        let start = datetime!(2026-11-02 12:00:00 UTC);
        report_picker_episode(&root, "proj", "Q?", start);
        let ready_at = start + PICKER_ALERT_AFTER;

        let mut ntfy_calls = 0;
        report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at,
            true,
            true,
            || false, // activity post fails
            || {
                ntfy_calls += 1;
                true
            },
        );
        let (table, _) = load(&root);
        let entry = &table[&picker_key("proj", "Q?")];
        assert!(!entry.activity_posted, "a failed post is not recorded");
        assert!(entry.alerted, "the ntfy push still succeeded independently");
        assert_eq!(ntfy_calls, 1);

        let mut activity_calls = 0;
        report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            ready_at + Duration::minutes(5),
            true,
            true,
            || {
                activity_calls += 1;
                true
            },
            || panic!("already alerted, must not push again"),
        );
        assert_eq!(activity_calls, 1, "a failed post retries later");
        let (table, _) = load(&root);
        assert!(table[&picker_key("proj", "Q?")].activity_posted);
    }

    #[test]
    fn a_closed_lock_reports_none_and_calls_neither_closure() {
        let root = temp_dir("alert-locked");
        let path = lock_path(&root);
        let (tx, rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = mv_core::lock::acquire_default(&path, "test").unwrap();
            tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        rx.recv().unwrap();

        let now = datetime!(2026-11-02 12:00:00 UTC);
        let result = report_and_alert_picker_episode(
            &root,
            "proj",
            "Q?",
            now,
            true,
            true,
            || panic!("locked out, must not call"),
            || panic!("locked out, must not call"),
        );
        assert_eq!(result, None);

        release_tx.send(()).unwrap();
        holder.join().unwrap();
    }
}

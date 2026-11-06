//! On-disk markers and pending-report queue for `mv_engine::hostsleep`'s
//! sweep gate (card `20261102-b-32b6bb`): two small timestamp files plus a
//! JSON queue, all under a project's `.multiverse/` state dir. Mirrors
//! `hostsleep.py`'s `STATE_FILENAME`/`PMSET_STATE_FILENAME`/
//! `PENDING_FILENAME`/`LOCK_FILENAME` and their atomic-write shape
//! (`_write_timestamp`/`_queue_report`, both temp-file-then-rename).
//!
//! `mv_engine::hostsleep` owns the gate, the `pmset` probe, and the log
//! parser; this module owns only reading and writing the state these need
//! to persist between sweeps. It lives in `mv-state` rather than
//! `mv-engine` because it is pure filesystem I/O under `.multiverse/`, the
//! same boundary every other store in this crate (`breaker_store`,
//! `gauge`, `notify`) already draws.

use std::io;
use std::path::{Path, PathBuf};

use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// Sibling to `activity.py`'s own `activity.lastseen` marker: the previous
/// sweep's own `now`, advanced only when a real `pmset` call happens and
/// its caller applies the result.
pub const STATE_FILENAME: &str = "hostsleep-last-sweep";

/// Sibling to [`STATE_FILENAME`]: the last time `pmset -g log` was actually
/// invoked, read by the gate's staleness check (`hostsleep::should_run_pmset`'s
/// second condition).
pub const PMSET_STATE_FILENAME: &str = "hostsleep-last-pmset";

/// Guards the read-markers -> run-pmset -> write-markers span, the same way
/// `notify.LOCK_FILENAME` guards `notify.sweep`'s own span.
pub const LOCK_FILENAME: &str = "hostsleep-last-sweep.lock";

/// Completed scans wait here until a notification pass saves their
/// episodes, mirroring `hostsleep.py`'s `PENDING_FILENAME`.
pub const PENDING_FILENAME: &str = "hostsleep-pending.json";

fn state_path(project_root: &Path, filename: &str) -> PathBuf {
    project_root.join(".multiverse").join(filename)
}

/// This store's own lock path, for a caller to hand to
/// `mv_core::lock::acquire` with a zero timeout — one non-blocking
/// attempt, the same posture `hostsleep.py`'s `_sweep_lock` takes: a call
/// that loses the race returns empty and leaves every marker untouched
/// rather than waiting on the winner.
pub fn lock_path(project_root: &Path) -> PathBuf {
    state_path(project_root, LOCK_FILENAME)
}

/// Read a timestamp marker, or `None` if missing, empty, or unparsable —
/// missing and corrupt are folded together the same way
/// `hostsleep.py::_read_timestamp` folds them, since a bad marker here
/// must never fail a sweep.
pub fn read_timestamp(project_root: &Path, filename: &str) -> Option<OffsetDateTime> {
    let text = std::fs::read_to_string(state_path(project_root, filename)).ok()?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    OffsetDateTime::parse(trimmed, &Rfc3339).ok()
}

/// Record `when` to `filename`, atomically — temp file in the same
/// directory, then rename over the destination (`mv_core::atomic::
/// write_atomic`), the same durability `hostsleep.py::_write_timestamp`
/// gets from its own `os.replace`.
pub fn write_timestamp(
    project_root: &Path,
    filename: &str,
    when: OffsetDateTime,
) -> io::Result<()> {
    let path = state_path(project_root, filename);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = when
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    mv_core::atomic::write_atomic(&path, format!("{text}\n").as_bytes())
}

/// Every report queued since the last successful handoff — empty, not an
/// error, when the file has never been written.
pub fn read_pending(project_root: &Path) -> io::Result<Vec<Vec<String>>> {
    let path = state_path(project_root, PENDING_FILENAME);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    serde_json::from_str(&text).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Append `lines` as one more queued report, atomically rewriting the
/// whole file — mirrors `hostsleep.py::_queue_report`'s read-append-write.
pub fn queue_pending(project_root: &Path, lines: Vec<String>) -> io::Result<()> {
    let mut reports = read_pending(project_root)?;
    reports.push(lines);
    let path = state_path(project_root, PENDING_FILENAME);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(&reports)?;
    mv_core::atomic::write_atomic(&path, format!("{text}\n").as_bytes())
}

/// Clear the pending queue once a notification pass has durably saved it —
/// mirrors `hostsleep.py`'s `pending_reports` context manager removing the
/// file after a successful `apply`.
pub fn clear_pending(project_root: &Path) -> io::Result<()> {
    match std::fs::remove_file(state_path(project_root, PENDING_FILENAME)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mv-state-hostsleep-store-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_timestamp_marker_reads_as_none() {
        let root = temp_dir("missing-timestamp");
        assert_eq!(read_timestamp(&root, STATE_FILENAME), None);
    }

    #[test]
    fn a_written_timestamp_round_trips() {
        let root = temp_dir("roundtrip");
        let when = datetime!(2026-11-02 12:00:00 UTC);
        write_timestamp(&root, STATE_FILENAME, when).unwrap();
        assert_eq!(read_timestamp(&root, STATE_FILENAME), Some(when));
    }

    #[test]
    fn the_two_marker_files_do_not_collide() {
        let root = temp_dir("two-markers");
        let sweep = datetime!(2026-11-02 12:00:00 UTC);
        let pmset = datetime!(2026-11-02 11:00:00 UTC);
        write_timestamp(&root, STATE_FILENAME, sweep).unwrap();
        write_timestamp(&root, PMSET_STATE_FILENAME, pmset).unwrap();
        assert_eq!(read_timestamp(&root, STATE_FILENAME), Some(sweep));
        assert_eq!(read_timestamp(&root, PMSET_STATE_FILENAME), Some(pmset));
    }

    #[test]
    fn a_corrupt_timestamp_marker_reads_as_none_not_an_error() {
        let root = temp_dir("corrupt-timestamp");
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(root.join(".multiverse").join(STATE_FILENAME), "garbage").unwrap();
        assert_eq!(read_timestamp(&root, STATE_FILENAME), None);
    }

    #[test]
    fn pending_reports_queue_and_clear() {
        let root = temp_dir("pending");
        assert_eq!(read_pending(&root).unwrap(), Vec::<Vec<String>>::new());
        queue_pending(&root, vec!["a".to_string()]).unwrap();
        queue_pending(&root, vec!["b".to_string(), "c".to_string()]).unwrap();
        assert_eq!(
            read_pending(&root).unwrap(),
            vec![
                vec!["a".to_string()],
                vec!["b".to_string(), "c".to_string()],
            ]
        );
        clear_pending(&root).unwrap();
        assert_eq!(read_pending(&root).unwrap(), Vec::<Vec<String>>::new());
        // Clearing an already-absent queue is a no-op, not an error.
        clear_pending(&root).unwrap();
    }

    #[test]
    fn lock_path_is_a_sibling_of_the_sweep_marker() {
        let root = Path::new("/project");
        assert_eq!(
            lock_path(root),
            root.join(".multiverse").join("hostsleep-last-sweep.lock")
        );
    }
}

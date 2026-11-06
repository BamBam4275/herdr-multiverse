//! `routing.log`: tab-separated `key=value` records, `v=1` envelope,
//! crc32 trailer, fsync + 0600 + project lock on append.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use mv_core::atomic::fsync_dir;
use mv_core::crc32::crc32_hex;
use mv_core::lock::{self, LockError, LockGuard};
use mv_core::time::parse_flexible;

use crate::index::{self, Selector};

/// An invalid envelope is evidence to report, never a valid activity record.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("damaged routing record: {reason}")]
pub struct DamagedRecord {
    pub reason: String,
    pub seq: Option<u64>,
}

/// The storage codec deliberately leaves role names and event payloads opaque.
/// Activity, task and backlog modules apply their own semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub when: String,
    pub fields: BTreeMap<String, String>,
    pub seq: Option<u64>,
    pub versioned: bool,
}

impl LogRecord {
    pub fn event(&self) -> &str {
        self.fields.get("event").map(String::as_str).unwrap_or("")
    }
}

pub(crate) const VERSIONED_EVENTS: &[&str] =
    &["message", "update", "backlog", "annotation", "close"];

/// Split at the first `=` only: JSON strings and reasons may contain `=`.
/// Legacy lines may contain bare, unknown fields; ignore those as Python does.
pub(crate) fn split_fields(line: &str) -> (&str, BTreeMap<String, String>) {
    let mut parts = line.split('\t');
    let when = parts.next().unwrap_or("");
    let fields = parts
        .filter_map(|part| part.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
    (when, fields)
}

pub(crate) fn looks_versioned(fields: &BTreeMap<String, String>) -> bool {
    fields.contains_key("v")
        || fields.contains_key("seq")
        || fields.contains_key("crc32")
        || fields
            .get("event")
            .is_some_and(|event| VERSIONED_EVENTS.contains(&event.as_str()))
}

/// Decode a legacy record or validate the shared v=1 envelope.
/// CRC is checked over the original UTF-8 bytes, before any normalization.
pub fn parse(line: &str) -> Result<LogRecord, DamagedRecord> {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let (when, fields) = split_fields(line);
    let seq = fields.get("seq").and_then(|value| value.parse().ok());
    let versioned = looks_versioned(&fields);
    let damaged = |reason: &str| DamagedRecord {
        reason: reason.to_owned(),
        seq,
    };
    if versioned {
        if fields.get("v").map(String::as_str) != Some("1") {
            return Err(damaged("missing or unsupported version"));
        }
        if seq.is_none() {
            return Err(damaged("missing or invalid nonnegative sequence"));
        }
        if !valid_timestamp(when) {
            return Err(damaged("invalid timestamp"));
        }
        if fields.get("event").is_none_or(String::is_empty) {
            return Err(damaged("missing event"));
        }
        let mut keys = std::collections::HashSet::new();
        for part in line.split('\t').skip(1) {
            let Some((key, _)) = part.split_once('=') else {
                return Err(damaged("bare field in versioned envelope"));
            };
            if key.is_empty() || !keys.insert(key) {
                return Err(damaged("empty or duplicate field"));
            }
        }
        let Some((prefix, checksum)) = line.rsplit_once("\tcrc32=") else {
            return Err(damaged("missing checksum trailer"));
        };
        if checksum.len() != 8
            || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !checksum.eq_ignore_ascii_case(&crc32_hex(prefix.as_bytes()))
        {
            return Err(damaged("checksum mismatch or invalid trailer"));
        }
    }
    Ok(LogRecord {
        when: when.to_owned(),
        fields,
        seq,
        versioned,
    })
}

/// Encode fields in caller-supplied order. Payload JSON encoding and redaction
/// belong to the owning module; tabs/newlines here must already be escaped.
pub fn encode_versioned(
    when: &str,
    seq: u64,
    event: &str,
    fields: &[(&str, &str)],
) -> Result<String, DamagedRecord> {
    if !valid_timestamp(when) || event.is_empty() || event.contains(['=', '\t', '\r', '\n']) {
        return Err(DamagedRecord {
            reason: "invalid timestamp or unsafe event".into(),
            seq: Some(seq),
        });
    }
    let mut line = format!("{when}\tv=1\tseq={seq}\tevent={event}");
    for (key, value) in fields {
        if key.is_empty()
            || key.contains(['=', '\t', '\r', '\n'])
            || value.contains(['\t', '\r', '\n'])
        {
            return Err(DamagedRecord {
                reason: "unsafe field in routing record".into(),
                seq: Some(seq),
            });
        }
        line.push('\t');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    let checksum = crc32_hex(line.as_bytes());
    line.push_str("\tcrc32=");
    line.push_str(&checksum);
    parse(&line)?;
    Ok(line)
}

#[derive(Debug, thiserror::Error)]
pub enum LogError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Damaged(#[from] DamagedRecord),
    #[error("timed out acquiring project lock {0}")]
    LockTimeout(PathBuf),
    #[error("routing sequence exhausted")]
    SequenceExhausted,
}

/// The authoritative log lives in the project, never in fleet runtime state.
#[derive(Debug, Clone)]
pub struct ProjectLog {
    path: PathBuf,
}

impl ProjectLog {
    pub fn new(project_root: impl AsRef<Path>) -> Self {
        Self {
            path: project_root.as_ref().join(".multiverse/routing.log"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn lock(&self) -> Result<ProjectLock, LogError> {
        self.lock_timeout(lock::LOCK_ACQUIRE_TIMEOUT)
    }

    pub fn lock_timeout(&self, timeout: Duration) -> Result<ProjectLock, LogError> {
        let parent = self.path.parent().expect("project log has a parent");
        ensure_state_dir(parent)?;
        let path = fs::canonicalize(parent)?.join("routing.log");
        let lock_path = path.with_extension("log.lock");
        let existing = HELD_LOCKS.with(|held| held.borrow().get(&lock_path).cloned());
        let file = if let Some(file) = existing {
            file
        } else {
            // Keep the established 0600 lock-file mode; mv-core owns locking.
            private_file(&lock_path, false)?;
            let guard =
                lock::acquire(&lock_path, "project", timeout).map_err(|error| match error {
                    LockError::Timeout(_) => LogError::LockTimeout(lock_path.clone()),
                    LockError::Io(error) => LogError::Io(error),
                })?;
            Rc::new(guard)
        };
        HELD_LOCKS.with(|held| {
            held.borrow_mut().insert(lock_path.clone(), file.clone());
        });
        Ok(ProjectLock {
            path,
            lock_path,
            file,
        })
    }

    pub fn append(&self, line: &str) -> Result<u64, LogError> {
        self.lock()?.append(line)
    }

    pub fn append_versioned(
        &self,
        when: &str,
        event: &str,
        fields: &[(&str, &str)],
    ) -> Result<LogRecord, LogError> {
        self.lock()?.append_versioned(when, event, fields)
    }
}

// Lifetime adapter: retain the shared guard until the last project guard drops,
// including when callers drop the outer guard before a nested one.
thread_local! {
    static HELD_LOCKS: RefCell<HashMap<PathBuf, Rc<LockGuard>>> = RefCell::new(HashMap::new());
}

pub struct ProjectLock {
    path: PathBuf,
    lock_path: PathBuf,
    file: Rc<LockGuard>,
}

impl std::fmt::Debug for ProjectLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectLock")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        HELD_LOCKS.with(|held| {
            // Map + this guard are the final two owners.
            if Rc::strong_count(&self.file) == 2 {
                held.borrow_mut().remove(&self.lock_path);
            }
        });
    }
}

impl ProjectLock {
    /// Exposes read/check/append under one lock for idempotent higher layers.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append_versioned(
        &self,
        when: &str,
        event: &str,
        fields: &[(&str, &str)],
    ) -> Result<LogRecord, LogError> {
        let seq = index::records(&self.path, Selector::All)?
            .last()
            .map_or(0, |record| record.entry.high_water_seq)
            .checked_add(1)
            .ok_or(LogError::SequenceExhausted)?;
        let line = encode_versioned(when, seq, event, fields)?;
        self.append(&line)?;
        Ok(parse(&line)?)
    }

    /// Append exactly one record. A torn existing tail is an error rather than
    /// silently concatenating a new valid record onto damaged evidence.
    pub fn append(&self, line: &str) -> Result<u64, LogError> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if line.contains(['\n', '\r']) || line.is_empty() {
            return Err(DamagedRecord {
                reason: "append requires exactly one nonempty line".into(),
                seq: None,
            }
            .into());
        }
        parse(line)?;
        let existed = self.path.exists();
        let mut file = private_file(&self.path, true)?;
        let offset = file.metadata()?.len();
        if offset > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut tail = [0];
            file.read_exact(&mut tail)?;
            if tail != *b"\n" {
                return Err(DamagedRecord {
                    reason: "existing log has an unterminated tail".into(),
                    seq: None,
                }
                .into());
            }
        }
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        file.write_all(&bytes)?;
        file.sync_all()?;
        if !existed {
            fsync_dir(self.path.parent().expect("log parent"))?;
        }
        // The ledger is committed. Cache failures must never turn a successful
        // write into a retry that could duplicate the authoritative record.
        let _ = index::append(&self.path, offset);
        Ok(offset)
    }
}

fn ensure_state_dir(path: &Path) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            fsync_dir(path)?;
            fsync_dir(path.parent().expect("state parent"))
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn private_file(path: &Path, append: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .append(append)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

// The ledger historically rejects year zero and leap seconds, which RFC 3339
// otherwise permits. Calendar/offset parsing belongs to mv-core.
fn valid_timestamp(value: &str) -> bool {
    parse_flexible(value)
        .is_ok_and(|instant| instant.year() != 0 && value.as_bytes().get(17..19) != Some(b"60"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Instant;

    pub(crate) const WHEN: &str = "2026-11-02T12:00:00+00:00";

    pub(crate) struct TempProject(pub PathBuf);

    impl TempProject {
        pub(crate) fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            loop {
                let path = std::env::temp_dir().join(format!(
                    "mv-state-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("temporary project: {error}"),
                }
            }
        }

        pub(crate) fn log(&self) -> ProjectLog {
            ProjectLog::new(&self.0)
        }
    }

    impl Drop for TempProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn python_crc_golden_vectors() {
        assert_eq!(crc32_hex(b""), "00000000");
        assert_eq!(crc32_hex(b"123456789"), "cbf43926");
        assert_eq!(crc32_hex("é".as_bytes()), "0e048d3e");
        let line = encode_versioned(
            WHEN,
            42,
            "message",
            &[
                ("id", "20000101-fixer-000001"),
                ("from", "multiverse"),
                ("to", "example"),
                ("body", "\"hello world\""),
            ],
        )
        .unwrap();
        assert!(line.ends_with("\tcrc32=00fbbb8f"));
        assert_eq!(parse(&line).unwrap().seq, Some(42));
    }

    #[test]
    fn history_fixtures_keep_old_roles_and_free_text_backlog_roles() {
        for fixture in [
            include_str!("../../../conformance/scenarios/history-tasks-reads-old-role-names.json"),
            include_str!(
                "../../../conformance/scenarios/history-mixed-old-and-new-role-names.json"
            ),
        ] {
            let fixture: serde_json::Value = serde_json::from_str(fixture).unwrap();
            let log = fixture["projects"]["sandbox"]["files"][".multiverse/routing.log"]
                .as_str()
                .unwrap();
            for line in log.lines() {
                let record = parse(line).unwrap();
                if record.event() == "dispatch" {
                    assert!(
                        ["fixer", "oracle", "reviewer"].contains(&record.fields["role"].as_str())
                    );
                    assert!(!record.versioned);
                } else if record.event() == "backlog" {
                    assert_eq!(record.fields["role"], "builder");
                    assert!(record.versioned);
                }
            }
        }
        let record = parse("legacy\tevent=dispatch\treason=a=b\trole=fixer\tbare").unwrap();
        assert_eq!(record.fields["reason"], "a=b");
    }

    #[test]
    fn damaged_envelopes_never_decode_as_valid_records() {
        let line = encode_versioned(WHEN, 7, "message", &[("body", "\"hello\"")]).unwrap();
        for damaged in [
            line.replace("hello", "jello"),
            line.replace("v=1", "v=2"),
            line.replace("seq=7", "seq=-1"),
            line.replace("seq=7\t", ""),
            line.replace("2026-11-02", "2026-02-30"),
            line.split("\tcrc32=").next().unwrap().to_owned(),
            format!("{line}\textra=yes"),
            format!("{line}\tcrc32=00000000"),
            format!("{WHEN}\tevent=backlog\titem=legacy"),
        ] {
            assert!(parse(&damaged).is_err(), "accepted {damaged}");
        }
        assert!(encode_versioned(WHEN, 1, "message", &[("body", "two\nlines")]).is_err());
        assert!(encode_versioned(WHEN, 1, "message", &[("seq", "2")]).is_err());
        assert!(encode_versioned(WHEN, 1, "message\trole=injected", &[]).is_err());
        for stamp in ["2026-11-02T12:00:00Z", "2026-11-02T12:00:00.123-05:00"] {
            assert!(parse(&encode_versioned(stamp, 0, "future-event", &[]).unwrap()).is_ok());
        }
    }

    #[test]
    fn shared_parser_preserves_ledger_timestamp_constraints() {
        for stamp in [
            "2026-11-02 12:00:00+00:00",
            "2026-11-02t12:00:00z",
            "2026-11-02T12:00:00.123456789123-05:00",
        ] {
            assert!(
                encode_versioned(stamp, 1, "message", &[]).is_ok(),
                "{stamp}"
            );
        }
        for stamp in [
            "0000-01-01T00:00:00Z",
            "2026-11-02T12:00:60Z",
            "2026-11-02T12:00:00",
        ] {
            assert!(
                encode_versioned(stamp, 1, "message", &[]).is_err(),
                "{stamp}"
            );
        }
    }

    #[test]
    fn durable_append_modes_nested_locks_and_torn_tail() {
        let project = TempProject::new();
        let log = project.log();
        let outer = log.lock().unwrap();
        let inner = log.lock_timeout(Duration::ZERO).unwrap();
        drop(outer);
        inner.append_versioned(WHEN, "message", &[]).unwrap();
        let other_log = log.clone();
        let blocked = thread::spawn(move || {
            matches!(
                other_log.lock_timeout(Duration::from_millis(60)),
                Err(LogError::LockTimeout(_))
            )
        })
        .join()
        .unwrap();
        assert!(
            blocked,
            "nested guard must retain the lock after outer drop"
        );
        drop(inner);
        log.append_versioned(WHEN, "update", &[]).unwrap();
        for path in [
            log.path().to_owned(),
            log.path().with_extension("log.lock"),
            index::index_path(log.path()),
            index::fingerprint_path(log.path()),
        ] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(
            fs::metadata(project.0.join(".multiverse"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        fs::write(log.path(), b"torn").unwrap();
        assert!(matches!(
            log.append("legacy\tevent=dispatch"),
            Err(LogError::Damaged(_))
        ));
        assert_eq!(fs::read(log.path()).unwrap(), b"torn");
    }

    #[test]
    fn cache_failure_does_not_fail_a_committed_append() {
        let project = TempProject::new();
        let log = project.log();
        let guard = log.lock().unwrap();
        fs::create_dir(index::index_path(log.path())).unwrap();
        guard.append_versioned(WHEN, "message", &[]).unwrap();
        let records = index::records(log.path(), Selector::All).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].parse().unwrap().seq, Some(1));
    }

    #[test]
    fn append_process_worker() {
        let Some(root) = std::env::var_os("MV_STATE_APPEND_TEST_PROJECT") else {
            return;
        };
        let log = ProjectLog::new(PathBuf::from(root));
        let start = Instant::now();
        let barrier = log.path().parent().unwrap().parent().unwrap().join("start");
        while !barrier.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "barrier timed out"
            );
            thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..50 {
            log.append_versioned(WHEN, "message", &[("body", "\"concurrent\"")])
                .unwrap();
        }
    }

    #[test]
    fn two_processes_append_one_hundred_complete_sequenced_records() {
        let project = TempProject::new();
        let executable = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for _ in 0..2 {
            children.push(
                Command::new(&executable)
                    .args([
                        "--exact",
                        "log::tests::append_process_worker",
                        "--nocapture",
                    ])
                    .env("MV_STATE_APPEND_TEST_PROJECT", &project.0)
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
            );
        }
        fs::write(project.0.join("start"), b"go").unwrap();
        let start = Instant::now();
        let mut failure = None;
        for child in &mut children {
            loop {
                match child.try_wait().unwrap() {
                    Some(status) => {
                        if !status.success() {
                            failure = Some(format!("child exited {status}"));
                        }
                        break;
                    }
                    None if start.elapsed() < Duration::from_secs(20) => {
                        thread::sleep(Duration::from_millis(10))
                    }
                    None => {
                        child.kill().unwrap();
                        child.wait().unwrap();
                        failure = Some("child timed out".into());
                        break;
                    }
                }
            }
        }
        assert!(failure.is_none(), "{failure:?}");
        let log = project.log();
        let records = index::records(log.path(), Selector::All).unwrap();
        assert_eq!(records.len(), 100);
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.parse().unwrap().seq, Some(i as u64 + 1));
            assert!(record.bytes.ends_with(b"\n"));
        }
        assert_eq!(
            index::read_index(log.path())
                .unwrap()
                .unwrap()
                .last()
                .unwrap()
                .high_water_seq,
            100
        );
    }
}

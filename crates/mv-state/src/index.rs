//! Sidecar index over `routing.log`: JSON-lines entries, a
//! fingerprint, and a rebuild path when the fingerprint mismatches.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use mv_core::atomic::write_atomic;
use serde::{Deserialize, Serialize};

use crate::log::{looks_versioned, split_fields, DamagedRecord, LogRecord};

const TASK_EVENTS: &[&str] = &[
    "dispatch",
    "started",
    "result",
    "supersede",
    "limit",
    "acknowledged",
    "stale-pane",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selector {
    All,
    Tasks,
    Backlog,
    Updates,
    Versioned,
}

impl Selector {
    fn matches(self, entry: &IndexEntry) -> bool {
        match self {
            Self::All => true,
            Self::Tasks => TASK_EVENTS.contains(&entry.event.as_str()),
            Self::Backlog => {
                !entry.item.is_empty() && matches!(entry.event.as_str(), "backlog" | "annotation")
            }
            Self::Updates => entry.event == "update",
            Self::Versioned => entry.versioned,
        }
    }
}

impl std::str::FromStr for Selector {
    type Err = io::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "all" => Ok(Self::All),
            "tasks" => Ok(Self::Tasks),
            "backlog" => Ok(Self::Backlog),
            "updates" => Ok(Self::Updates),
            "versioned" => Ok(Self::Versioned),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown log selector",
            )),
        }
    }
}

/// Python-compatible JSON-lines metadata. Offsets and lengths count bytes,
/// including the line's terminating newline when present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexEntry {
    pub v: u32,
    pub ordinal: u64,
    pub offset: u64,
    pub length: u64,
    pub high_water_seq: u64,
    pub event: String,
    pub task: String,
    pub item: String,
    pub seq: Option<u64>,
    pub versioned: bool,
    pub invalid_utf8: bool,
}

/// A selected physical line. Damaged/invalid-UTF-8 lines remain visible;
/// callers can classify them rather than silently losing evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexedRecord {
    pub entry: IndexEntry,
    pub bytes: Vec<u8>,
}

impl IndexedRecord {
    pub fn parse(&self) -> Result<LogRecord, DamagedRecord> {
        let line = std::str::from_utf8(&self.bytes).map_err(|_| DamagedRecord {
            reason: "invalid UTF-8".into(),
            seq: self.entry.seq,
        })?;
        crate::log::parse(line)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Fingerprint {
    dev: u64,
    ino: u64,
    hash: String,
    boundary: u64,
}

struct Snapshot {
    bytes: Vec<u8>,
    dev: u64,
    ino: u64,
}

impl Snapshot {
    fn read(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let meta = file.metadata()?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self {
            bytes,
            dev: meta.dev(),
            ino: meta.ino(),
        })
    }

    fn fingerprint(&self, boundary: usize) -> Fingerprint {
        let bytes = &self.bytes[..boundary];
        let mut sampled = bytes[..bytes.len().min(4096)].to_vec();
        sampled.extend_from_slice(&bytes[bytes.len().saturating_sub(4096)..]);
        Fingerprint {
            dev: self.dev,
            ino: self.ino,
            hash: sha1_hex(&sampled),
            boundary: boundary as u64,
        }
    }
}

pub fn index_path(log_path: &Path) -> PathBuf {
    let mut name = log_path.as_os_str().to_os_string();
    name.push(".index");
    name.into()
}

pub fn fingerprint_path(log_path: &Path) -> PathBuf {
    let mut name = index_path(log_path).into_os_string();
    name.push(".fp");
    name.into()
}

fn metadata(bytes: &[u8], ordinal: u64, offset: u64, previous: u64) -> IndexEntry {
    let text = String::from_utf8_lossy(bytes);
    let line = text.strip_suffix('\n').unwrap_or(&text);
    let (_, fields) = split_fields(line);
    let seq = fields
        .get("seq")
        .and_then(|value| value.parse::<u64>().ok());
    IndexEntry {
        v: 1,
        ordinal,
        offset,
        length: bytes.len() as u64,
        high_water_seq: previous.max(seq.unwrap_or(0)),
        event: fields.get("event").cloned().unwrap_or_default(),
        task: fields.get("task").cloned().unwrap_or_default(),
        item: fields.get("item").cloned().unwrap_or_default(),
        seq,
        versioned: looks_versioned(&fields),
        invalid_utf8: std::str::from_utf8(bytes).is_err(),
    }
}

fn scan(bytes: &[u8]) -> Vec<IndexEntry> {
    let mut entries = Vec::new();
    let mut offset = 0;
    let mut high_water = 0;
    for (ordinal, line) in bytes.split_inclusive(|byte| *byte == b'\n').enumerate() {
        let entry = metadata(line, ordinal as u64, offset, high_water);
        offset += entry.length;
        high_water = entry.high_water_seq;
        entries.push(entry);
    }
    entries
}

fn decode(bytes: &[u8]) -> Option<Vec<IndexEntry>> {
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        return None;
    }
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| serde_json::from_slice(line).ok())
        .collect()
}

/// Validate *all* metadata against the ledger, not just offsets or a sampled
/// fingerprint. A tampered event/task/seq must never change what a selector sees.
fn trusted_entries(log_path: &Path, snapshot: &Snapshot) -> Option<Vec<IndexEntry>> {
    let fingerprint: Fingerprint =
        serde_json::from_slice(&fs::read(fingerprint_path(log_path)).ok()?).ok()?;
    if fingerprint.boundary != snapshot.bytes.len() as u64
        || fingerprint != snapshot.fingerprint(snapshot.bytes.len())
    {
        return None;
    }
    let entries = decode(&fs::read(index_path(log_path)).ok()?)?;
    (entries == scan(&snapshot.bytes)).then_some(entries)
}

/// Read-only: absent/corrupt/stale caches are a full-scan fallback. Rendering
/// can call this without creating any files in the project.
pub fn records(log_path: &Path, selector: Selector) -> io::Result<Vec<IndexedRecord>> {
    let snapshot = match Snapshot::read(log_path) {
        Ok(snapshot) => snapshot,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let entries = trusted_entries(log_path, &snapshot).unwrap_or_else(|| scan(&snapshot.bytes));
    Ok(entries
        .into_iter()
        .filter(|entry| selector.matches(entry))
        .map(|entry| {
            let start = entry.offset as usize;
            let end = start + entry.length as usize;
            IndexedRecord {
                entry,
                bytes: snapshot.bytes[start..end].to_vec(),
            }
        })
        .collect())
}

/// Returns None for an untrusted sidecar; log I/O failures remain errors.
pub fn read_index(log_path: &Path) -> io::Result<Option<Vec<IndexEntry>>> {
    match Snapshot::read(log_path) {
        Ok(snapshot) => Ok(trusted_entries(log_path, &snapshot)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn json_bytes(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_index(log_path: &Path, snapshot: &Snapshot, entries: &[IndexEntry]) -> io::Result<()> {
    let mut bytes = Vec::new();
    for entry in entries {
        bytes.extend(json_bytes(entry)?);
    }
    write_atomic(&index_path(log_path), &bytes)?;
    write_atomic(
        &fingerprint_path(log_path),
        &json_bytes(&snapshot.fingerprint(snapshot.bytes.len()))?,
    )
}

/// Caller must hold the routing-log project lock. Never rewrites the ledger.
pub fn rebuild(log_path: &Path) -> io::Result<Vec<IndexEntry>> {
    let snapshot = Snapshot::read(log_path)?;
    let entries = scan(&snapshot.bytes);
    write_index(log_path, &snapshot, &entries)?;
    Ok(entries)
}

/// Update after a durable append, under the same project lock. A missing index,
/// wrong boundary, rotation, truncation, or changed metadata rebuilds the cache.
pub fn append(log_path: &Path, append_offset: u64) -> io::Result<()> {
    let snapshot = Snapshot::read(log_path)?;
    let entries = scan(&snapshot.bytes);
    let old_entries = fs::read(index_path(log_path))
        .ok()
        .and_then(|bytes| decode(&bytes));
    let old_fingerprint = fs::read(fingerprint_path(log_path))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Fingerprint>(&bytes).ok());
    let boundary = usize::try_from(append_offset).ok();
    let valid = boundary.is_some_and(|boundary| {
        boundary <= snapshot.bytes.len()
            && (boundary == 0 || snapshot.bytes[boundary - 1] == b'\n')
            && old_fingerprint.as_ref() == Some(&snapshot.fingerprint(boundary))
            && old_entries.as_ref() == Some(&scan(&snapshot.bytes[..boundary]))
    });
    if !valid {
        return write_index(log_path, &snapshot, &entries);
    }
    let previous_count = old_entries.as_ref().map_or(0, Vec::len);
    let mut file = crate::log::private_file(&index_path(log_path), true)?;
    for entry in &entries[previous_count..] {
        file.write_all(&json_bytes(entry)?)?;
    }
    file.sync_all()?;
    write_atomic(
        &fingerprint_path(log_path),
        &json_bytes(&snapshot.fingerprint(snapshot.bytes.len()))?,
    )
}

// SHA-1 is a compatibility fingerprint, not an authentication primitive.
// Keep the Python sidecar's hash format without expanding this lane's manifest.
fn sha1_hex(bytes: &[u8]) -> String {
    let mut padded = bytes.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&(bytes.len() as u64 * 8).to_be_bytes());
    let mut state = [
        0x6745_2301u32,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0u32; 80];
        for (word, chunk) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*chunk);
        }
        for i in 16..80 {
            words[i] = (words[i - 3] ^ words[i - 8] ^ words[i - 14] ^ words[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (i, word) in words.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (value, add) in state.iter_mut().zip([a, b, c, d, e]) {
            *value = value.wrapping_add(add);
        }
    }
    state.iter().map(|value| format!("{value:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::{
        encode_versioned,
        tests::{TempProject, WHEN},
    };
    use std::fs::OpenOptions;

    #[test]
    fn fingerprint_sha1_matches_python_hashlib_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(&vec![b'a'; 1000]),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba"
        );
    }

    #[test]
    fn all_selectors_preserve_byte_offsets_and_shared_high_water() {
        let project = TempProject::new();
        let log = project.log();
        let guard = log.lock().unwrap();
        let legacy = format!("{WHEN}\tevent=dispatch\ttask=20261102-fixer-aaaaaa\trole=fixer");
        guard.append(&legacy).unwrap();
        for (event, seq) in [
            ("message", 8),
            ("update", 3),
            ("backlog", 9),
            ("annotation", 10),
            ("close", 11),
        ] {
            let fields: &[(&str, &str)] = match event {
                "backlog" | "annotation" => &[("item", "20261102-b-aaaaaa"), ("note", "\"é\"")],
                _ => &[("note", "\"é\"")],
            };
            guard
                .append(&encode_versioned(WHEN, seq, event, fields).unwrap())
                .unwrap();
        }
        let all = records(log.path(), Selector::All).unwrap();
        assert_eq!(all.len(), 6);
        assert_eq!(all[1].entry.offset, legacy.len() as u64 + 1);
        assert_eq!(all[2].entry.high_water_seq, 8);
        assert_eq!(all.last().unwrap().entry.high_water_seq, 11);
        let bytes = fs::read(log.path()).unwrap();
        for record in &all {
            let start = record.entry.offset as usize;
            assert_eq!(
                record.bytes,
                bytes[start..start + record.entry.length as usize]
            );
        }
        for (selector, count) in [
            (Selector::Tasks, 1),
            (Selector::Backlog, 2),
            (Selector::Updates, 1),
            (Selector::Versioned, 5),
        ] {
            assert_eq!(records(log.path(), selector).unwrap().len(), count);
        }
        for (text, selector) in [
            ("all", Selector::All),
            ("tasks", Selector::Tasks),
            ("backlog", Selector::Backlog),
            ("updates", Selector::Updates),
            ("versioned", Selector::Versioned),
        ] {
            assert_eq!(text.parse::<Selector>().unwrap(), selector);
        }
        assert!("misspelled".parse::<Selector>().is_err());
    }

    #[test]
    fn tampered_index_metadata_falls_back_to_log_then_rebuilds() {
        let project = TempProject::new();
        let log = project.log();
        log.append_versioned(WHEN, "update", &[("task", "20261102-fixer-aaaaaa")])
            .unwrap();
        let good = read_index(log.path()).unwrap().unwrap();
        for mutate in 0..7 {
            let mut bad = good.clone();
            match mutate {
                0 => bad[0].event = "backlog".into(),
                1 => bad[0].task = "other-task".into(),
                2 => bad[0].seq = Some(99),
                3 => bad[0].high_water_seq = 99,
                4 => bad[0].offset = u64::MAX,
                5 => bad[0].ordinal = 100,
                _ => bad[0].versioned = false,
            }
            fs::write(index_path(log.path()), json_bytes(&bad[0]).unwrap()).unwrap();
            assert!(read_index(log.path()).unwrap().is_none());
            assert_eq!(records(log.path(), Selector::Updates).unwrap().len(), 1);
            assert!(records(log.path(), Selector::Backlog).unwrap().is_empty());
        }
        log.append_versioned(WHEN, "message", &[]).unwrap();
        assert_eq!(read_index(log.path()).unwrap().unwrap().len(), 2);
    }

    #[test]
    fn same_size_rewrite_rotation_truncation_and_growth_invalidate_cache() {
        let project = TempProject::new();
        let log = project.log();
        let guard = log.lock().unwrap();
        let old = format!("{WHEN}\tevent=dispatch\ttask=old-task\n");
        let new = old.replace("old-task", "new-task");
        guard.append(old.trim_end_matches('\n')).unwrap();
        fs::write(log.path(), &new).unwrap();
        assert!(read_index(log.path()).unwrap().is_none());
        assert_eq!(
            records(log.path(), Selector::Tasks).unwrap()[0]
                .parse()
                .unwrap()
                .fields["task"],
            "new-task"
        );
        rebuild(log.path()).unwrap();
        let replacement = project.0.join("replacement");
        fs::write(&replacement, &new).unwrap();
        fs::rename(replacement, log.path()).unwrap();
        assert!(
            read_index(log.path()).unwrap().is_none(),
            "rotation changes inode even for equal bytes"
        );
        rebuild(log.path()).unwrap();
        fs::write(log.path(), b"").unwrap();
        assert!(read_index(log.path()).unwrap().is_none());
        assert!(records(log.path(), Selector::All).unwrap().is_empty());
        rebuild(log.path()).unwrap();
        let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
        file.write_all(old.as_bytes()).unwrap();
        assert!(read_index(log.path()).unwrap().is_none());
        append(log.path(), 0).unwrap();
        assert_eq!(read_index(log.path()).unwrap().unwrap().len(), 1);
    }

    #[test]
    fn middle_edit_outside_fingerprint_samples_is_still_detected() {
        let project = TempProject::new();
        let log = project.log();
        let _guard = log.lock().unwrap();
        let padding = format!("legacy\tevent=padding\tnote={}\n", "x".repeat(5000));
        let middle = "legacy\tevent=dispatch\ttask=old\n";
        let bytes = format!("{padding}{middle}{padding}");
        fs::write(log.path(), &bytes).unwrap();
        rebuild(log.path()).unwrap();
        fs::write(log.path(), bytes.replace("task=old", "task=new")).unwrap();
        assert!(read_index(log.path()).unwrap().is_none());
        assert_eq!(
            records(log.path(), Selector::Tasks).unwrap()[0].entry.task,
            "new"
        );
    }

    #[test]
    fn missing_corrupt_and_unwritable_sidecars_allow_read_only_fallback() {
        let project = TempProject::new();
        let log = project.log();
        assert!(records(log.path(), Selector::All).unwrap().is_empty());
        assert!(!project.0.join(".multiverse").exists());
        let _guard = log.lock().unwrap();
        fs::write(log.path(), b"legacy\tevent=dispatch\ttask=test\n").unwrap();
        assert_eq!(records(log.path(), Selector::Tasks).unwrap().len(), 1);
        assert!(!index_path(log.path()).exists());
        for bytes in [b"not json\n".as_slice(), b"{}\n", b""] {
            fs::write(index_path(log.path()), bytes).unwrap();
            fs::write(fingerprint_path(log.path()), b"corrupt").unwrap();
            assert_eq!(records(log.path(), Selector::Tasks).unwrap().len(), 1);
            assert_eq!(fs::read(index_path(log.path())).unwrap(), bytes);
            assert_eq!(fs::read(fingerprint_path(log.path())).unwrap(), b"corrupt");
        }
        fs::remove_file(index_path(log.path())).unwrap();
        fs::create_dir(index_path(log.path())).unwrap();
        assert_eq!(records(log.path(), Selector::Tasks).unwrap().len(), 1);
    }

    #[test]
    fn damaged_crc_invalid_utf8_and_unterminated_lines_remain_visible() {
        let project = TempProject::new();
        let log = project.log();
        let _guard = log.lock().unwrap();
        let line = encode_versioned(WHEN, 7, "message", &[("body", "\"hello\"")])
            .unwrap()
            .replace("hello", "jello");
        let mut bytes = format!("{line}\n").into_bytes();
        bytes.extend_from_slice(b"legacy\tevent=dispatch\ttask=\xff\nunterminated");
        fs::write(log.path(), &bytes).unwrap();
        rebuild(log.path()).unwrap();
        let all = records(log.path(), Selector::All).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].parse().is_err());
        assert!(all[1].entry.invalid_utf8);
        assert!(all[1].parse().is_err());
        assert_eq!(all[2].bytes, b"unterminated");
        assert_eq!(all[2].entry.high_water_seq, 7);
        assert!(records(log.path(), Selector::Versioned).unwrap()[0]
            .parse()
            .is_err());
    }
}

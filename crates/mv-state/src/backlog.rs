//! Backlog card storage: add/list/projection, independent of the
//! council logic that consumes it.
//!
//! Durable, per-project work-not-yet-started, stored as `event=backlog`
//! (and sibling `event=annotation`) records on the project's own
//! `.multiverse/routing.log` — the same file, append format, lock, and
//! `v=1` envelope every other writer on that log uses (`crate::log`,
//! `crate::index`). Ported from Python's `multiverse/backlog.py`.
//!
//! Three states: `open`, `asked`, and `done`. `asked` is a question
//! waiting on a human — a backlog item in a new state, not a new record
//! kind. `done` closes an item; `reopen` is the explicit transition back
//! to `open`. Everything else (a title, a priority, a due date) is
//! last-wins per `item=`: a mutation line omits whatever did not change,
//! so folding can never blank it.
//!
//! Identifier validation, crc32, UTC timestamp formatting, and redaction
//! come from `mv-core`; locking, append durability, and sequence
//! allocation come from this crate's own `log`/`index` modules
//! (`ProjectLog`/`ProjectLock`, `crate::index::records`). Only the
//! backlog-specific pieces live here: the item/annotation shapes, the
//! `event=backlog`/`event=annotation` field decoding and folding, the
//! state-transition rules, and the numbered-question shape.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use time::{OffsetDateTime, UtcOffset};

use mv_core::error::InvalidIdentifierError;
use mv_core::id::is_valid_identifier;
use mv_core::redact::redact_text;
use mv_core::time::{format_seconds_utc, now_utc};

use crate::index::{records as index_records, Selector};
use crate::log::{LogRecord, ProjectLock, ProjectLog};

const DEFAULT_PRIORITY_STR: &str = "P2";

/// What [`done`] records when `--evidence` is omitted, blank, or
/// whitespace-only. Recording this marker instead of leaving the field
/// unset means a future `show` always prints an `evidence:` line for a
/// closed item — "no reason was given" reads as a positive statement, not
/// a blank a reader has to interpret.
pub const NO_EVIDENCE_GIVEN: &str = "(no reason given)";

const MAX_BODY_BYTES: usize = 4096;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Every reason a backlog operation could not complete.
///
/// Mirrors the `BacklogError` family in `multiverse/backlog.py`, one
/// variant per Python exception class, so a caller can match on the same
/// distinctions the CLI's exit-code mapping needs.
#[derive(Debug, thiserror::Error)]
pub enum BacklogError {
    /// A field that must be a bare identifier is not field-safe.
    #[error(transparent)]
    InvalidIdentifier(#[from] InvalidIdentifierError),

    /// `priority` is not one of the closed set this reader accepts.
    #[error("priority {value:?} is not one of: P1, P2, P3")]
    InvalidPriority { value: String },

    /// `due` is not a real `YYYY-MM-DD` calendar date.
    #[error("due date {value:?} is not a valid YYYY-MM-DD date")]
    InvalidDueDate { value: String },

    /// No `event=backlog` line for this item exists in this project's log.
    ///
    /// Quoted with literal single quotes, not `{item_id:?}` (Rust's `Debug`
    /// renders a `String` with double quotes) — `UnknownItemError`'s
    /// `f"...{item_id!r}..."` in the frozen oracle quotes a `str` with
    /// single quotes, and `conformance/diff_fuzz.py` compares this text
    /// against that oracle's own stderr byte-for-byte (seed 0 caught the
    /// double-quote form here, after the exit-code mismatch it was hiding
    /// behind was fixed).
    #[error(
        "no backlog item '{item_id}' found in this project's routing log; \
         nothing was written"
    )]
    UnknownItem { item_id: String },

    /// A prefix matched more than one item in this project's log.
    #[error("ambiguous: {}", .matches.join(", "))]
    AmbiguousItem { token: String, matches: Vec<String> },

    /// This item is already `done` — `done` again writes nothing.
    #[error("backlog item '{item_id}' is already done; nothing was written")]
    TerminalItem { item_id: String },

    /// `mark_asked` only accepts an item currently `open`.
    #[error("backlog item '{item_id}' is '{state}', not open; nothing was written")]
    ItemNotOpen { item_id: String, state: String },

    /// `reopen` only accepts an item currently `done`.
    #[error("backlog item '{item_id}' is '{state}', not done; nothing was written")]
    ItemNotDone { item_id: String, state: String },

    /// The numbered-menu shape an `asked` item requires was not met.
    #[error("{0}")]
    InvalidQuestion(String),

    /// A free-text field failed the shared 4 KiB/NUL validator.
    #[error("{field}: {reason}")]
    InvalidField { field: String, reason: String },

    /// 64 candidate ids collided; the log is pathologically busy for one day.
    #[error("could not mint a unique backlog item id")]
    IdExhausted,

    /// The project lock, the append, or sequence allocation failed.
    #[error(transparent)]
    Log(#[from] crate::log::LogError),

    /// Reading the routing log (or its sidecar index) failed.
    #[error("could not read the routing log: {0}")]
    Io(#[from] std::io::Error),
}

fn invalid_identifier(kind: &str, value: &str) -> BacklogError {
    InvalidIdentifierError::new(kind, value.to_string()).into()
}

// ---------------------------------------------------------------------------
// Priority
// ---------------------------------------------------------------------------

/// The closed sort-key set `--priority` accepts.
///
/// `P0` is deliberately absent — it belongs to a later, hub-only `urgent`
/// command this module does not build (mirrors `backlog.Priority`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Priority {
    P1,
    #[default]
    P2,
    P3,
}

impl Priority {
    pub fn as_str(self) -> &'static str {
        match self {
            Priority::P1 => "P1",
            Priority::P2 => "P2",
            Priority::P3 => "P3",
        }
    }
}

impl std::fmt::Display for Priority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Priority {
    type Err = BacklogError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "P1" => Ok(Priority::P1),
            "P2" => Ok(Priority::P2),
            "P3" => Ok(Priority::P3),
            _ => Err(BacklogError::InvalidPriority {
                value: value.to_string(),
            }),
        }
    }
}

fn normalize_priority(value: &str) -> Result<String, BacklogError> {
    value.parse::<Priority>().map(|p| p.as_str().to_string())
}

// ---------------------------------------------------------------------------
// Public data shapes
// ---------------------------------------------------------------------------

/// One append-only note against an existing item, never a mutation of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub note: String,
    pub by: String,
    pub when: String,
}

/// One backlog item, folded from every `event=backlog` line naming it,
/// plus every `event=annotation` line naming it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklogItem {
    pub item: String,
    pub state: String,
    pub priority: String,
    pub title: String,
    pub why: String,
    pub due: String,
    pub role: String,
    pub source: String,
    pub by: String,
    pub evidence: String,
    pub created_at: String,
    pub updated_at: String,
    pub annotations: Vec<Annotation>,
    /// The numbered menu an `asked` item carries — empty for an `open` or
    /// `done` item.
    pub options: Vec<String>,
    /// 1-based index into `options`. `None` when there is no question.
    pub recommendation: Option<u32>,
    /// The dispatched task currently working this item, when a dispatch
    /// goal named the item id. Empty when never linked.
    pub task_id: String,
}

impl BacklogItem {
    /// Strictly `state == "open"` — `asked` is not open.
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// Whether `due` has passed, as of `today` (`YYYY-MM-DD`) — a badge,
    /// never a trigger. Only an open item can be overdue.
    pub fn is_overdue(&self, today: &str) -> bool {
        if self.state != "open" || self.due.is_empty() {
            return false;
        }
        if !is_valid_due_date_str(&self.due) {
            return false;
        }
        self.due.as_str() < today
    }
}

/// Whether `item` has an annotation recorded as the human's reply.
pub fn has_human_annotation(item: &BacklogItem) -> bool {
    item.annotations.iter().any(|a| a.by == "human")
}

/// Exact id, else a unique prefix of one.
///
/// Several matches return [`BacklogError::AmbiguousItem`] whose message is
/// `ambiguous: <ids>`. Zero matches return [`BacklogError::UnknownItem`]
/// for the token as typed. An exact match always wins, even if it is also
/// a prefix of a longer id.
pub fn resolve_item_id<'a, I>(known_ids: I, token: &str) -> Result<String, BacklogError>
where
    I: IntoIterator<Item = &'a str>,
{
    let ids: Vec<&str> = known_ids.into_iter().collect();
    if ids.contains(&token) {
        return Ok(token.to_string());
    }
    let mut matches: Vec<&str> = ids.into_iter().filter(|id| id.starts_with(token)).collect();
    matches.sort_unstable();
    match matches.len() {
        1 => Ok(matches[0].to_string()),
        0 => Err(BacklogError::UnknownItem {
            item_id: token.to_string(),
        }),
        _ => Err(BacklogError::AmbiguousItem {
            token: token.to_string(),
            matches: matches.into_iter().map(String::from).collect(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Body / date validation specific to backlog fields (identifier validation,
// crc32, timestamps, and redaction all come from `mv-core`; this is the
// part none of that owns: free-text size limits and a date-only badge.)
// ---------------------------------------------------------------------------

/// `activity._validate_body`: refuse a NUL byte or a body over 4 KiB,
/// re-raised here naming the failing field (mirrors `_validate_field`).
fn validate_field(value: &str, field: &str) -> Result<(), BacklogError> {
    let bytes = value.as_bytes();
    if bytes.contains(&0) {
        return Err(BacklogError::InvalidField {
            field: field.to_string(),
            reason: "message body must not contain a NUL byte".to_string(),
        });
    }
    if bytes.len() > MAX_BODY_BYTES {
        return Err(BacklogError::InvalidField {
            field: field.to_string(),
            reason: format!(
                "message body is {} bytes; the limit is {} (4 KiB)",
                bytes.len(),
                MAX_BODY_BYTES
            ),
        });
    }
    Ok(())
}

fn is_valid_calendar_date(year: i64, month: u32, day: u32) -> bool {
    if !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    const DAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let max_day = if month == 2 && leap {
        29
    } else {
        DAYS[(month - 1) as usize]
    };
    day <= max_day
}

/// `backlog._DATE_RE` plus `datetime.date.fromisoformat`'s own calendar check.
fn is_valid_due_date_str(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return false;
    }
    for (i, &b) in bytes.iter().enumerate() {
        if i == 4 || i == 7 {
            continue;
        }
        if !b.is_ascii_digit() {
            return false;
        }
    }
    let year: i64 = value[0..4].parse().unwrap_or(0);
    let month: u32 = value[5..7].parse().unwrap_or(0);
    let day: u32 = value[8..10].parse().unwrap_or(0);
    is_valid_calendar_date(year, month, day)
}

// ---------------------------------------------------------------------------
// JSON string/list encode-decode, ASCII-escaped on write to match Python's
// `json.dumps(..., ensure_ascii=True)` byte-for-byte. No shared ASCII-JSON
// encoder exists in `mv-core` yet, so this stays backlog-local.
// ---------------------------------------------------------------------------

pub(crate) fn json_encode_str(value: &str) -> String {
    let quoted = serde_json::to_string(value).expect("string serialization cannot fail");
    let mut out = String::with_capacity(quoted.len());
    for ch in quoted.chars() {
        let cp = ch as u32;
        if cp <= 0x7E {
            out.push(ch);
        } else if cp <= 0xFFFF {
            out.push_str(&format!("\\u{cp:04x}"));
        } else {
            let n = cp - 0x10000;
            let hi = 0xD800 + (n >> 10);
            let lo = 0xDC00 + (n & 0x3FF);
            out.push_str(&format!("\\u{hi:04x}\\u{lo:04x}"));
        }
    }
    out
}

fn json_encode_str_list(items: &[String]) -> String {
    let inner = items
        .iter()
        .map(|s| json_encode_str(s))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{inner}]")
}

fn json_decode_str(raw: &str) -> Result<String, ()> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::String(s)) => Ok(s),
        _ => Err(()),
    }
}

fn json_decode_str_list(raw: &str) -> Result<Vec<String>, ()> {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    serde_json::Value::String(s) => out.push(s),
                    _ => return Err(()),
                }
            }
            Ok(out)
        }
        _ => Err(()),
    }
}

// ---------------------------------------------------------------------------
// Folding (mirrors `backlog._classify_backlog`, `_classify_annotation`,
// `_fold`, `_item_from_entry`). The shared envelope/checksum validation
// that Python's `_validate_v1_envelope` does is already performed by
// `crate::log::parse` before a `LogRecord` reaches here — only the
// backlog-specific field decoding is this module's job.
// ---------------------------------------------------------------------------

struct DecodedBacklog {
    item: String,
    when: String,
    state: Option<String>,
    priority: Option<String>,
    title: Option<String>,
    why: Option<String>,
    evidence: Option<String>,
    due: Option<String>,
    role: Option<String>,
    source: Option<String>,
    by: Option<String>,
    task: Option<String>,
    options: Option<Vec<String>>,
    recommendation: Option<u32>,
}

fn classify_backlog(record: &LogRecord) -> Option<DecodedBacklog> {
    if !record.versioned || record.event() != "backlog" {
        return None;
    }
    let fields = &record.fields;
    let item_id = fields.get("item").map(String::as_str).unwrap_or("");
    if item_id.is_empty() || !is_valid_identifier(item_id) {
        return None;
    }

    let mut decoded = DecodedBacklog {
        item: item_id.to_string(),
        when: record.when.clone(),
        state: None,
        priority: None,
        title: None,
        why: None,
        evidence: None,
        due: None,
        role: None,
        source: None,
        by: None,
        task: None,
        options: None,
        recommendation: None,
    };

    if let Some(state) = fields.get("state") {
        if !matches!(state.as_str(), "open" | "asked" | "done") {
            return None;
        }
        decoded.state = Some(state.clone());
    }
    if let Some(priority) = fields.get("priority") {
        if !matches!(priority.as_str(), "P1" | "P2" | "P3") {
            return None;
        }
        decoded.priority = Some(priority.clone());
    }
    for (key, slot) in [
        ("title", &mut decoded.title),
        ("why", &mut decoded.why),
        ("evidence", &mut decoded.evidence),
    ] {
        if let Some(raw) = fields.get(key) {
            *slot = Some(json_decode_str(raw).ok()?);
        }
    }
    if let Some(due) = fields.get("due") {
        if !is_valid_due_date_str(due) {
            return None;
        }
        decoded.due = Some(due.clone());
    }
    for (key, slot) in [
        ("role", &mut decoded.role),
        ("source", &mut decoded.source),
        ("by", &mut decoded.by),
        ("task", &mut decoded.task),
    ] {
        if let Some(raw) = fields.get(key) {
            if !is_valid_identifier(raw) {
                return None;
            }
            *slot = Some(raw.clone());
        }
    }
    if let Some(raw) = fields.get("options") {
        decoded.options = Some(json_decode_str_list(raw).ok()?);
    }
    if let Some(raw) = fields.get("recommendation") {
        if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let recommendation: u32 = raw.parse().ok()?;
        let options_len = decoded.options.as_ref().map_or(0, Vec::len);
        if recommendation < 1 || recommendation as usize > options_len {
            return None;
        }
        decoded.recommendation = Some(recommendation);
    }
    Some(decoded)
}

struct DecodedAnnotation {
    item: String,
    note: String,
    by: String,
    when: String,
}

fn classify_annotation(record: &LogRecord) -> Option<DecodedAnnotation> {
    if !record.versioned || record.event() != "annotation" {
        return None;
    }
    let fields = &record.fields;
    let item_id = fields.get("item").map(String::as_str).unwrap_or("");
    if item_id.is_empty() || !is_valid_identifier(item_id) {
        return None;
    }
    let note = json_decode_str(fields.get("note")?).ok()?;
    let by = fields.get("by").map(String::as_str).unwrap_or("");
    if by.is_empty() || !is_valid_identifier(by) {
        return None;
    }
    Some(DecodedAnnotation {
        item: item_id.to_string(),
        note,
        by: by.to_string(),
        when: record.when.clone(),
    })
}

#[derive(Clone, Default)]
struct FoldedEntry {
    state: String,
    priority: String,
    title: Option<String>,
    why: Option<String>,
    due: Option<String>,
    role: Option<String>,
    source: Option<String>,
    by: Option<String>,
    evidence: Option<String>,
    task: Option<String>,
    options: Option<Vec<String>>,
    recommendation: Option<u32>,
    created_at: String,
    updated_at: String,
    annotations: Vec<Annotation>,
}

/// Fold every valid `event=backlog`/`event=annotation` record by `item=`,
/// last-wins for backlog fields, accumulating for annotations. A record
/// that fails classification is skipped rather than raised — one damaged
/// or legacy line must not blank the rest of the listing.
fn fold(records: &[LogRecord]) -> (Vec<String>, HashMap<String, FoldedEntry>) {
    let mut order: Vec<String> = Vec::new();
    let mut folded: HashMap<String, FoldedEntry> = HashMap::new();
    for record in records {
        if let Some(decoded) = classify_backlog(record) {
            let entry = folded.entry(decoded.item.clone()).or_insert_with(|| {
                order.push(decoded.item.clone());
                FoldedEntry {
                    state: "open".to_string(),
                    priority: DEFAULT_PRIORITY_STR.to_string(),
                    created_at: decoded.when.clone(),
                    ..Default::default()
                }
            });
            if let Some(v) = decoded.state {
                entry.state = v;
            }
            if let Some(v) = decoded.priority {
                entry.priority = v;
            }
            if decoded.title.is_some() {
                entry.title = decoded.title;
            }
            if decoded.why.is_some() {
                entry.why = decoded.why;
            }
            if decoded.evidence.is_some() {
                entry.evidence = decoded.evidence;
            }
            if decoded.due.is_some() {
                entry.due = decoded.due;
            }
            if decoded.role.is_some() {
                entry.role = decoded.role;
            }
            if decoded.source.is_some() {
                entry.source = decoded.source;
            }
            if decoded.by.is_some() {
                entry.by = decoded.by;
            }
            if decoded.task.is_some() {
                entry.task = decoded.task;
            }
            if decoded.options.is_some() {
                entry.options = decoded.options;
            }
            if decoded.recommendation.is_some() {
                entry.recommendation = decoded.recommendation;
            }
            entry.updated_at = decoded.when;
            continue;
        }
        if let Some(annotated) = classify_annotation(record) {
            if let Some(entry) = folded.get_mut(&annotated.item) {
                entry.updated_at = annotated.when.clone();
                entry.annotations.push(Annotation {
                    note: annotated.note,
                    by: annotated.by,
                    when: annotated.when,
                });
            }
        }
    }
    (order, folded)
}

fn item_from_entry(item_id: String, entry: &FoldedEntry) -> BacklogItem {
    BacklogItem {
        item: item_id,
        state: entry.state.clone(),
        priority: entry.priority.clone(),
        title: entry.title.clone().unwrap_or_default(),
        why: entry.why.clone().unwrap_or_default(),
        due: entry.due.clone().unwrap_or_default(),
        role: entry.role.clone().unwrap_or_default(),
        source: entry.source.clone().unwrap_or_default(),
        by: entry.by.clone().unwrap_or_default(),
        evidence: entry.evidence.clone().unwrap_or_default(),
        created_at: entry.created_at.clone(),
        updated_at: entry.updated_at.clone(),
        annotations: entry.annotations.clone(),
        options: entry.options.clone().unwrap_or_default(),
        recommendation: entry.recommendation,
        task_id: entry.task.clone().unwrap_or_default(),
    }
}

fn lookup_folded<'a>(
    folded: &'a HashMap<String, FoldedEntry>,
    token: &str,
) -> Result<(String, &'a FoldedEntry), BacklogError> {
    let ids: Vec<&str> = folded.keys().map(String::as_str).collect();
    let resolved = resolve_item_id(ids, token)?;
    let entry = folded.get(&resolved).expect("resolved id must be present");
    Ok((resolved, entry))
}

/// A fresh `YYYYMMDD-b-<6hex>` id, unique among this log's own `item=`
/// values.
fn mint_item_id(records: &[LogRecord], moment: OffsetDateTime) -> Result<String, BacklogError> {
    let utc = moment.to_offset(UtcOffset::UTC);
    let date_part = format!(
        "{:04}{:02}{:02}",
        utc.year(),
        u8::from(utc.month()),
        utc.day()
    );
    let existing: HashSet<&str> = records
        .iter()
        .filter(|r| r.versioned && r.event() == "backlog")
        .filter_map(|r| r.fields.get("item").map(String::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    for _ in 0..64 {
        let candidate = format!("{date_part}-b-{}", random_hex6());
        if !existing.contains(candidate.as_str()) {
            return Ok(candidate);
        }
    }
    Err(BacklogError::IdExhausted)
}

/// A process-unpredictable 6-hex-digit suffix (mirrors `secrets.token_hex(3)`'s
/// role here, not its CSPRNG guarantee — `mv-core` does not expose one, and
/// adding a `rand` dependency is outside this file's write scope). `RandomState`'s
/// per-instance OS-seeded keys, combined with the clock and a call counter, are
/// enough to keep same-day ids from colliding.
fn random_hex6() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_i128(OffsetDateTime::now_utc().unix_timestamp_nanos());
    hasher.write_usize(std::process::id() as usize);
    hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    format!("{:06x}", hasher.finish() & 0xFF_FFFF)
}

// ---------------------------------------------------------------------------
// Locking + reads, built on `crate::log::ProjectLog` and
// `crate::index::records`.
//
// This module reads with `Selector::All` and classifies for itself rather
// than `Selector::Backlog`, so a fold sees every event type it needs
// (`backlog`, `annotation`) without depending on the selector's exact
// membership rules.
// ---------------------------------------------------------------------------

fn load_records(path: &Path) -> std::io::Result<Vec<LogRecord>> {
    let indexed = index_records(path, Selector::All)?;
    Ok(indexed.iter().filter_map(|r| r.parse().ok()).collect())
}

/// Hold the project's routing-log lock across a read-check-append
/// transaction, the same guarantee `accounts._project_lock` gives every
/// other writer on this log, then hand the caller the lock (to append
/// through) and every record already on it (to fold/resolve against).
fn with_project_lock<T>(
    project_root: &Path,
    body: impl FnOnce(&ProjectLock, &[LogRecord]) -> Result<T, BacklogError>,
) -> Result<T, BacklogError> {
    let lock = ProjectLog::new(project_root).lock()?;
    let records = load_records(lock.path())?;
    body(&lock, &records)
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Every backlog item in `project_root`'s own log, folded to its current
/// state, creation order preserved.
///
/// Advisory: a missing or unreadable log is simply an empty list, never an
/// error. Takes no lock — a best-effort snapshot, not a delivery
/// guarantee, the same posture `read_items` has in the Python original.
pub fn read_items(project_root: &Path, open_only: bool) -> Vec<BacklogItem> {
    let path = ProjectLog::new(project_root).path().to_path_buf();
    let records = match load_records(&path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let (order, folded) = fold(&records);
    let mut items: Vec<BacklogItem> = order
        .into_iter()
        .map(|id| {
            let entry = folded
                .get(&id)
                .expect("every ordered id has a folded entry");
            item_from_entry(id, entry)
        })
        .collect();
    if open_only {
        items.retain(BacklogItem::is_open);
    }
    items
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Append one new, open backlog item to `project_root`'s own log.
#[allow(clippy::too_many_arguments)]
pub fn add(
    project_root: &Path,
    title: &str,
    by: &str,
    why: &str,
    due: Option<&str>,
    source: Option<&str>,
    role: Option<&str>,
    priority: &str,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(by) {
        return Err(invalid_identifier("project name", by));
    }
    let priority_value = normalize_priority(priority)?;
    validate_field(title, "title")?;
    if !why.is_empty() {
        validate_field(why, "why")?;
    }
    if let Some(s) = source {
        if !is_valid_identifier(s) {
            return Err(invalid_identifier("source", s));
        }
    }
    if let Some(r) = role {
        if !is_valid_identifier(r) {
            return Err(invalid_identifier("role", r));
        }
    }
    let due_value = match due {
        Some(d) if !d.is_empty() => {
            if !is_valid_due_date_str(d) {
                return Err(BacklogError::InvalidDueDate {
                    value: d.to_string(),
                });
            }
            d.to_string()
        }
        _ => String::new(),
    };

    let moment = when.unwrap_or_else(now_utc);
    let stamp = format_seconds_utc(moment);
    let title_r = redact_text(title);
    let why_r = redact_text(why);
    let title_json = json_encode_str(&title_r);
    let why_json = json_encode_str(&why_r);

    with_project_lock(project_root, |lock, records| {
        let item_id = mint_item_id(records, moment)?;
        let mut fields = vec![
            ("item", item_id.as_str()),
            ("state", "open"),
            ("priority", priority_value.as_str()),
            ("title", title_json.as_str()),
        ];
        if !why_r.is_empty() {
            fields.push(("why", why_json.as_str()));
        }
        if !due_value.is_empty() {
            fields.push(("due", due_value.as_str()));
        }
        if let Some(r) = role {
            fields.push(("role", r));
        }
        if let Some(s) = source {
            fields.push(("source", s));
        }
        fields.push(("by", by));
        lock.append_versioned(&stamp, "backlog", &fields)?;

        Ok(BacklogItem {
            item: item_id,
            state: "open".to_string(),
            priority: priority_value.clone(),
            title: title_r.clone(),
            why: why_r.clone(),
            due: due_value.clone(),
            role: role.unwrap_or("").to_string(),
            source: source.unwrap_or("").to_string(),
            by: by.to_string(),
            evidence: String::new(),
            created_at: stamp.clone(),
            updated_at: stamp.clone(),
            annotations: Vec::new(),
            options: Vec::new(),
            recommendation: None,
            task_id: String::new(),
        })
    })
}

/// Record that a dispatched task is working a backlog item. Idempotent for
/// the same `(item, task)` pair.
pub fn link_task(
    project_root: &Path,
    item_id: &str,
    task_id: &str,
    when: Option<OffsetDateTime>,
) -> Result<bool, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    if !is_valid_identifier(task_id) {
        return Err(invalid_identifier("task id", task_id));
    }
    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry.task.as_deref() == Some(task_id) {
            return Ok(false);
        }
        lock.append_versioned(
            &stamp,
            "backlog",
            &[("item", &resolved_id), ("task", task_id)],
        )?;
        Ok(true)
    })
}

/// Change an open item's `priority=` — last-wins, same folding rule every
/// other mutation here uses.
pub fn reprioritize(
    project_root: &Path,
    item_id: &str,
    priority: &str,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    let priority_value = normalize_priority(priority)?;
    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry.state == "done" {
            return Err(BacklogError::TerminalItem {
                item_id: resolved_id,
            });
        }
        lock.append_versioned(
            &stamp,
            "backlog",
            &[("item", &resolved_id), ("priority", &priority_value)],
        )?;

        let mut updated = entry.clone();
        updated.priority = priority_value.clone();
        updated.updated_at = stamp.clone();
        Ok(item_from_entry(resolved_id, &updated))
    })
}

/// Close a backlog item. Abandoned is `done` with `--evidence`, not a
/// state of its own; omitted/blank/whitespace-only evidence is recorded as
/// [`NO_EVIDENCE_GIVEN`] rather than left unset.
pub fn done(
    project_root: &Path,
    item_id: &str,
    evidence: &str,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    let stated = !evidence.trim().is_empty();
    if stated {
        validate_field(evidence, "evidence")?;
    }
    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));
    let recorded_evidence = if stated {
        redact_text(evidence)
    } else {
        NO_EVIDENCE_GIVEN.to_string()
    };
    let evidence_json = json_encode_str(&recorded_evidence);

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry.state == "done" {
            return Err(BacklogError::TerminalItem {
                item_id: resolved_id,
            });
        }
        lock.append_versioned(
            &stamp,
            "backlog",
            &[
                ("item", &resolved_id),
                ("state", "done"),
                ("evidence", &evidence_json),
            ],
        )?;

        let mut updated = entry.clone();
        updated.state = "done".to_string();
        updated.evidence = Some(recorded_evidence.clone());
        updated.updated_at = stamp.clone();
        Ok(item_from_entry(resolved_id, &updated))
    })
}

/// Append an append-only note against an existing item — idempotent on the
/// exact `(by, note)` pair. A `done` item is not refused: the note is
/// additive, the card stays closed.
pub fn append_annotation_if_absent(
    project_root: &Path,
    item_id: &str,
    by: &str,
    note: &str,
    when: Option<OffsetDateTime>,
) -> Result<bool, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    if !is_valid_identifier(by) {
        return Err(invalid_identifier("project name", by));
    }
    if note.trim().is_empty() {
        return Err(BacklogError::InvalidField {
            field: "note".to_string(),
            reason: "annotation note must not be blank".to_string(),
        });
    }
    validate_field(note, "note")?;

    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));
    let note_r = redact_text(note);
    let note_json = json_encode_str(&note_r);

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry
            .annotations
            .iter()
            .any(|a| a.by == by && a.note == note_r)
        {
            return Ok(false);
        }
        lock.append_versioned(
            &stamp,
            "annotation",
            &[("item", &resolved_id), ("note", &note_json), ("by", by)],
        )?;
        Ok(true)
    })
}

/// The numbered-menu shape every question carries: at least two options,
/// none blank, and `recommendation` a valid 1-based index into them.
fn validate_options(options: &[String], recommendation: u32) -> Result<Vec<String>, BacklogError> {
    if options.len() < 2 {
        return Err(BacklogError::InvalidQuestion(format!(
            "a question needs at least two options; got {}",
            options.len()
        )));
    }
    for option in options {
        if option.trim().is_empty() {
            return Err(BacklogError::InvalidQuestion(
                "an option cannot be blank".to_string(),
            ));
        }
        validate_field(option, "option")?;
    }
    if recommendation < 1 || recommendation as usize > options.len() {
        return Err(BacklogError::InvalidQuestion(format!(
            "recommendation {} is not a valid option number (1-{})",
            recommendation,
            options.len()
        )));
    }
    Ok(options.to_vec())
}

/// Append one new item straight into `asked` state: a question that exists
/// on the board in BLOCKED the instant it is written, never passing
/// through `open` at all.
#[allow(clippy::too_many_arguments)]
pub fn ask(
    project_root: &Path,
    question: &str,
    options: &[String],
    recommendation: u32,
    by: &str,
    why: &str,
    source: Option<&str>,
    role: Option<&str>,
    priority: &str,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(by) {
        return Err(invalid_identifier("project name", by));
    }
    let priority_value = normalize_priority(priority)?;
    validate_field(question, "question")?;
    if !why.is_empty() {
        validate_field(why, "why")?;
    }
    if let Some(s) = source {
        if !is_valid_identifier(s) {
            return Err(invalid_identifier("source", s));
        }
    }
    if let Some(r) = role {
        if !is_valid_identifier(r) {
            return Err(invalid_identifier("role", r));
        }
    }
    let options_value = validate_options(options, recommendation)?;

    let moment = when.unwrap_or_else(now_utc);
    let stamp = format_seconds_utc(moment);
    let question_r = redact_text(question);
    let why_r = redact_text(why);
    let options_r: Vec<String> = options_value.iter().map(|o| redact_text(o)).collect();
    let question_json = json_encode_str(&question_r);
    let why_json = json_encode_str(&why_r);
    let options_json = json_encode_str_list(&options_r);
    let recommendation_str = recommendation.to_string();

    with_project_lock(project_root, |lock, records| {
        let item_id = mint_item_id(records, moment)?;
        let mut fields = vec![
            ("item", item_id.as_str()),
            ("state", "asked"),
            ("priority", priority_value.as_str()),
            ("title", question_json.as_str()),
            ("options", options_json.as_str()),
            ("recommendation", recommendation_str.as_str()),
        ];
        if !why_r.is_empty() {
            fields.push(("why", why_json.as_str()));
        }
        if let Some(r) = role {
            fields.push(("role", r));
        }
        if let Some(s) = source {
            fields.push(("source", s));
        }
        fields.push(("by", by));
        lock.append_versioned(&stamp, "backlog", &fields)?;

        Ok(BacklogItem {
            item: item_id,
            state: "asked".to_string(),
            priority: priority_value.clone(),
            title: question_r.clone(),
            why: why_r.clone(),
            due: String::new(),
            role: role.unwrap_or("").to_string(),
            source: source.unwrap_or("").to_string(),
            by: by.to_string(),
            evidence: String::new(),
            created_at: stamp.clone(),
            updated_at: stamp.clone(),
            annotations: Vec::new(),
            options: options_r.clone(),
            recommendation: Some(recommendation),
            task_id: String::new(),
        })
    })
}

/// Move an existing open item to `asked`, attaching the numbered menu.
/// `title` itself is left untouched.
pub fn mark_asked(
    project_root: &Path,
    item_id: &str,
    options: &[String],
    recommendation: u32,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    let options_value = validate_options(options, recommendation)?;
    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));
    let options_r: Vec<String> = options_value.iter().map(|o| redact_text(o)).collect();
    let options_json = json_encode_str_list(&options_r);
    let recommendation_str = recommendation.to_string();

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry.state != "open" {
            return Err(BacklogError::ItemNotOpen {
                item_id: resolved_id,
                state: entry.state.clone(),
            });
        }
        lock.append_versioned(
            &stamp,
            "backlog",
            &[
                ("item", &resolved_id),
                ("state", "asked"),
                ("options", &options_json),
                ("recommendation", &recommendation_str),
            ],
        )?;

        let mut updated = entry.clone();
        updated.state = "asked".to_string();
        updated.options = Some(options_r.clone());
        updated.recommendation = Some(recommendation);
        updated.updated_at = stamp.clone();
        Ok(item_from_entry(resolved_id, &updated))
    })
}

/// Move a `done` item back to `open` — the recorded inverse of `done`.
/// Writes a `state=open` mutation line, then an annotation `reopened` from
/// `by`, so `show` can name the transition rather than leaving the reader
/// to infer it from `updated_at` alone.
pub fn reopen(
    project_root: &Path,
    item_id: &str,
    by: &str,
    when: Option<OffsetDateTime>,
) -> Result<BacklogItem, BacklogError> {
    if !is_valid_identifier(item_id) {
        return Err(invalid_identifier("item id", item_id));
    }
    if !is_valid_identifier(by) {
        return Err(invalid_identifier("project name", by));
    }
    let stamp = format_seconds_utc(when.unwrap_or_else(now_utc));
    let note_json = json_encode_str("reopened");

    with_project_lock(project_root, |lock, records| {
        let (_order, folded) = fold(records);
        let (resolved_id, entry) = lookup_folded(&folded, item_id)?;
        if entry.state != "done" {
            return Err(BacklogError::ItemNotDone {
                item_id: resolved_id,
                state: entry.state.clone(),
            });
        }
        lock.append_versioned(
            &stamp,
            "backlog",
            &[("item", &resolved_id), ("state", "open")],
        )?;
        lock.append_versioned(
            &stamp,
            "annotation",
            &[("item", &resolved_id), ("note", &note_json), ("by", by)],
        )?;

        let mut updated = entry.clone();
        updated.state = "open".to_string();
        updated.updated_at = stamp.clone();
        updated.annotations.push(Annotation {
            note: "reopened".to_string(),
            by: by.to_string(),
            when: stamp.clone(),
        });
        Ok(item_from_entry(resolved_id, &updated))
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::tests::{TempProject, WHEN};
    use std::io::Write as _;
    use time::macros::datetime;

    fn write_named_item(project: &TempProject, item_id: &str, title: &str, seq: u64, state: &str) {
        let title_json = json_encode_str(title);
        let line = crate::log::encode_versioned(
            WHEN,
            seq,
            "backlog",
            &[
                ("item", item_id),
                ("state", state),
                ("priority", "P2"),
                ("title", &title_json),
                ("by", "example"),
            ],
        )
        .unwrap();
        project.log().lock().unwrap().append(&line).unwrap();
    }

    // --- json round trip -----------------------------------------------------

    #[test]
    fn json_encode_escapes_non_ascii_with_surrogate_pairs() {
        assert_eq!(json_encode_str(""), "\"\"");
        assert_eq!(
            json_encode_str("hello \"world\""),
            "\"hello \\\"world\\\"\""
        );
        assert_eq!(json_encode_str("é"), "\"\\u00e9\"");
        assert_eq!(json_encode_str("😀"), "\"\\ud83d\\ude00\"");
        assert_eq!(json_encode_str("a\tb\nc"), "\"a\\tb\\nc\"");
    }

    #[test]
    fn json_round_trips_through_decode() {
        let encoded = json_encode_str("héllo\t😀");
        assert_eq!(json_decode_str(&encoded).unwrap(), "héllo\t😀");
    }

    // --- redaction at the backlog write boundary ------------------------------
    // (redact_text's own regex correctness is mv-core's own test coverage;
    // these pin that backlog's write paths actually call it — ported from
    // test_backlog_evidence_is_redacted_before_persistence et al.)

    #[test]
    fn evidence_is_redacted_before_persistence() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "Investigate",
            "project",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let closed = done(&project.0, &item.item, "password=supersecretvalue", None).unwrap();

        let raw = std::fs::read_to_string(project.log().path()).unwrap();
        assert!(!raw.contains("supersecretvalue"));
        assert_eq!(closed.evidence, "password=…[redacted]");
        assert_eq!(read_items(&project.0, false)[0].evidence, closed.evidence);
    }

    #[test]
    fn free_text_is_redacted_before_persistence() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "Check password=title-secret",
            "project",
            "password=why-secret",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        assert!(append_annotation_if_absent(
            &project.0,
            &item.item,
            "project",
            "password=note-secret",
            None
        )
        .unwrap());
        assert!(!append_annotation_if_absent(
            &project.0,
            &item.item,
            "project",
            "password=note-secret",
            None
        )
        .unwrap());

        let raw = std::fs::read_to_string(project.log().path()).unwrap();
        assert!(!raw.contains("title-secret"));
        assert!(!raw.contains("why-secret"));
        assert!(!raw.contains("note-secret"));
        let saved = &read_items(&project.0, false)[0];
        assert_eq!(item.title, saved.title);
        assert_eq!(saved.title, "Check password=…[redacted]");
        assert_eq!(item.why, saved.why);
        assert_eq!(saved.why, "password=…[redacted]");
        assert_eq!(saved.annotations[0].note, "password=…[redacted]");
    }

    #[test]
    fn question_text_and_options_are_redacted_before_persistence() {
        let project = TempProject::new();
        let options = vec!["password=option-secret".to_string(), "Skip".to_string()];
        let asked = ask(
            &project.0,
            "Check password=question-secret",
            &options,
            1,
            "project",
            "password=why-secret",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let item = add(
            &project.0,
            "Another question",
            "project",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let marked_options = vec!["password=marked-secret".to_string(), "Skip".to_string()];
        let marked = mark_asked(&project.0, &item.item, &marked_options, 1, None).unwrap();

        let raw = std::fs::read_to_string(project.log().path()).unwrap();
        for value in [
            "question-secret",
            "why-secret",
            "option-secret",
            "marked-secret",
        ] {
            assert!(!raw.contains(value));
        }
        assert_eq!(asked.title, "Check password=…[redacted]");
        assert_eq!(asked.why, "password=…[redacted]");
        let expected = vec!["password=…[redacted]".to_string(), "Skip".to_string()];
        assert_eq!(asked.options, expected);
        assert_eq!(marked.options, expected);
    }

    // --- add / done / reprioritize / read_items -----------------------------

    #[test]
    fn add_creates_an_open_p2_item_by_default() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "fix the thing",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        assert_eq!(item.state, "open");
        assert_eq!(item.priority, "P2");
        assert_eq!(item.title, "fix the thing");
        assert_eq!(item.by, "example");
        let items = read_items(&project.0, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item, item.item);
    }

    #[test]
    fn add_priority_p0_is_rejected() {
        let project = TempProject::new();
        let err = add(&project.0, "x", "example", "", None, None, None, "P0", None).unwrap_err();
        assert!(matches!(err, BacklogError::InvalidPriority { .. }));
    }

    #[test]
    fn add_refuses_a_by_value_that_is_not_a_safe_identifier() {
        let project = TempProject::new();
        let err = add(
            &project.0,
            "x",
            "not a safe project name",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidIdentifier(_)));
    }

    #[test]
    fn identifier_with_trailing_newline_is_rejected_and_nothing_is_written() {
        let project = TempProject::new();
        let err = add(
            &project.0,
            "x",
            "example\n",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidIdentifier(_)));
        assert!(!project.log().path().exists());
    }

    #[test]
    fn add_refuses_a_malformed_due_date() {
        let project = TempProject::new();
        let err = add(
            &project.0,
            "x",
            "example",
            "",
            Some("not-a-date"),
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidDueDate { .. }));
    }

    #[test]
    fn add_refuses_an_oversized_title() {
        let project = TempProject::new();
        let title = "x".repeat(MAX_BODY_BYTES + 1);
        let err = add(
            &project.0, &title, "example", "", None, None, None, "P2", None,
        )
        .unwrap_err();
        match err {
            BacklogError::InvalidField { field, reason } => {
                assert_eq!(field, "title");
                assert!(reason.contains(&MAX_BODY_BYTES.to_string()));
            }
            other => panic!("expected InvalidField, got {other:?}"),
        }
        assert!(read_items(&project.0, false).is_empty());
    }

    #[test]
    fn add_mints_distinct_ids_for_two_items_the_same_day() {
        let project = TempProject::new();
        let first = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        let second = add(
            &project.0, "two", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        assert_ne!(first.item, second.item);
        let prefix = second.item.split("-b-").next().unwrap();
        assert!(first.item.starts_with(prefix));
    }

    #[test]
    fn an_item_minted_on_a_stale_date_still_parses() {
        let project = TempProject::new();
        let stale_when = datetime!(2026-09-01 12:00:00 UTC);
        let item = add(
            &project.0,
            "old scheme item",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            Some(stale_when),
        )
        .unwrap();
        assert!(item.item.starts_with("20260901-b-"));
        assert_eq!(read_items(&project.0, false).len(), 1);

        done(&project.0, &item.item, "", None).unwrap();
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "done");
    }

    #[test]
    fn reprioritize_changes_only_priority_last_wins() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P3", None,
        )
        .unwrap();
        let updated = reprioritize(&project.0, &item.item, "P1", None).unwrap();
        assert_eq!(updated.priority, "P1");
        assert_eq!(updated.title, "one");
        let items = read_items(&project.0, false);
        assert_eq!(items[0].priority, "P1");
        assert_eq!(items[0].title, "one");
    }

    #[test]
    fn reprioritize_refuses_an_unknown_item() {
        let project = TempProject::new();
        let err = reprioritize(&project.0, "20260910-b-ffffff", "P1", None).unwrap_err();
        assert!(matches!(err, BacklogError::UnknownItem { .. }));
    }

    #[test]
    fn reprioritize_refuses_an_already_done_item() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        done(&project.0, &item.item, "", None).unwrap();
        let err = reprioritize(&project.0, &item.item, "P1", None).unwrap_err();
        assert!(matches!(err, BacklogError::TerminalItem { .. }));
    }

    #[test]
    fn done_is_terminal_and_can_carry_evidence() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        let closed = done(&project.0, &item.item, "abandoned; superseded", None).unwrap();
        assert_eq!(closed.state, "done");
        assert_eq!(closed.evidence, "abandoned; superseded");
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "done");
        assert_eq!(items[0].evidence, "abandoned; superseded");
    }

    #[test]
    fn link_task_records_the_dispatch_to_the_backlog_item() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "ship the fix",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        assert!(link_task(&project.0, &item.item, "20261021-fixer-abc123", None).unwrap());
        assert!(!link_task(&project.0, &item.item, "20261021-fixer-abc123", None).unwrap());
        let items = read_items(&project.0, false);
        assert_eq!(items[0].task_id, "20261021-fixer-abc123");
    }

    #[test]
    fn done_without_evidence_records_the_no_reason_marker() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        let closed = done(&project.0, &item.item, "", None).unwrap();
        assert_eq!(closed.evidence, NO_EVIDENCE_GIVEN);
        assert_eq!(read_items(&project.0, false)[0].evidence, NO_EVIDENCE_GIVEN);
    }

    #[test]
    fn done_with_whitespace_only_evidence_is_the_same_hole_by_another_door() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        let closed = done(&project.0, &item.item, "   \n\t  ", None).unwrap();
        assert_eq!(closed.evidence, NO_EVIDENCE_GIVEN);
    }

    #[test]
    fn done_refuses_an_unknown_item() {
        let project = TempProject::new();
        let err = done(&project.0, "20260910-b-ffffff", "", None).unwrap_err();
        assert!(matches!(err, BacklogError::UnknownItem { .. }));
    }

    #[test]
    fn done_refuses_an_already_done_item() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        done(&project.0, &item.item, "", None).unwrap();
        let err = done(&project.0, &item.item, "", None).unwrap_err();
        assert!(matches!(err, BacklogError::TerminalItem { .. }));
    }

    #[test]
    fn two_added_one_reprioritised_one_done_list_open_shows_exactly_one() {
        let project = TempProject::new();
        let keep = add(
            &project.0,
            "keep going",
            "example",
            "",
            None,
            None,
            None,
            "P3",
            None,
        )
        .unwrap();
        let finish = add(
            &project.0, "wrap up", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        reprioritize(&project.0, &keep.item, "P1", None).unwrap();
        done(&project.0, &finish.item, "shipped", None).unwrap();

        let open_items = read_items(&project.0, true);
        assert_eq!(open_items.len(), 1);
        assert_eq!(open_items[0].item, keep.item);
        assert_eq!(open_items[0].priority, "P1");
    }

    #[test]
    fn read_items_folds_across_lines_without_blanking_unchanged_fields() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "original title",
            "example",
            "because",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        reprioritize(&project.0, &item.item, "P1", None).unwrap();

        let items = read_items(&project.0, false);
        assert_eq!(items[0].title, "original title");
        assert_eq!(items[0].why, "because");
        assert_eq!(items[0].priority, "P1");
    }

    #[test]
    fn read_items_is_tolerant_of_a_damaged_backlog_line() {
        let project = TempProject::new();
        let good = add(
            &project.0, "fine", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        // Deliberately damaged: the checksum does not match the prefix.
        // `ProjectLock::append` refuses to write a line like this at all, so
        // to exercise `read_items`' tolerance of one that reached the file
        // some other way (a hand edit, a torn write from a future bug) this
        // appends the raw bytes directly, bypassing the log API.
        let damaged = format!("{WHEN}\tv=1\tseq=999\tevent=backlog\titem=20260910-b-deadbe\tstate=open\tcrc32=00000000");
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(project.log().path())
            .unwrap();
        writeln!(file, "{damaged}").unwrap();

        let items = read_items(&project.0, false);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].item, good.item);
    }

    #[test]
    fn read_items_skips_an_asked_item_with_an_invalid_recommendation() {
        for recommendation in [0, 3] {
            let project = TempProject::new();
            let options_json = json_encode_str_list(&["personal".to_string(), "work".to_string()]);
            let title_json = json_encode_str("which account?");
            let recommendation_str = recommendation.to_string();
            let line = crate::log::encode_versioned(
                WHEN,
                1,
                "backlog",
                &[
                    ("item", "20260910-b-abc123"),
                    ("state", "asked"),
                    ("title", &title_json),
                    ("options", &options_json),
                    ("recommendation", &recommendation_str),
                ],
            )
            .unwrap();
            project.log().lock().unwrap().append(&line).unwrap();

            assert!(read_items(&project.0, false).is_empty());
        }
    }

    #[test]
    fn is_overdue_is_true_only_for_open_items_with_a_past_due_date() {
        let base = BacklogItem {
            item: "x".into(),
            state: "open".into(),
            priority: "P2".into(),
            title: "t".into(),
            why: String::new(),
            due: "2020-01-01".into(),
            role: String::new(),
            source: String::new(),
            by: "example".into(),
            evidence: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
            annotations: Vec::new(),
            options: Vec::new(),
            recommendation: None,
            task_id: String::new(),
        };
        let open_past = base.clone();
        let mut open_today = base.clone();
        open_today.due = "2026-09-10".into();
        let mut done_past = base.clone();
        done_past.state = "done".into();

        assert!(open_past.is_overdue("2026-09-10"));
        assert!(!open_today.is_overdue("2026-09-10"));
        assert!(!done_past.is_overdue("2026-09-10"));
    }

    // --- asked (the third state) --------------------------------------------

    #[test]
    fn ask_creates_an_item_directly_in_asked_state() {
        let project = TempProject::new();
        let options = vec!["personal".to_string(), "work".to_string()];
        let item = ask(
            &project.0,
            "which account?",
            &options,
            2,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        assert_eq!(item.state, "asked");
        assert_eq!(item.title, "which account?");
        assert_eq!(item.options, options);
        assert_eq!(item.recommendation, Some(2));
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "asked");
        assert_eq!(items[0].options, options);
        assert_eq!(items[0].recommendation, Some(2));
    }

    #[test]
    fn ask_refuses_fewer_than_two_options() {
        let project = TempProject::new();
        let options = vec!["personal".to_string()];
        let err = ask(
            &project.0,
            "which account?",
            &options,
            1,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidQuestion(_)));
    }

    #[test]
    fn ask_refuses_a_blank_option() {
        let project = TempProject::new();
        let options = vec!["personal".to_string(), "   ".to_string()];
        let err = ask(
            &project.0,
            "which account?",
            &options,
            1,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidQuestion(_)));
    }

    #[test]
    fn ask_refuses_a_recommendation_outside_the_option_range() {
        let project = TempProject::new();
        let options = vec!["personal".to_string(), "work".to_string()];
        let err = ask(
            &project.0,
            "which account?",
            &options,
            3,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidQuestion(_)));
    }

    #[test]
    fn ask_item_is_not_open() {
        let project = TempProject::new();
        let options = vec!["personal".to_string(), "work".to_string()];
        let item = ask(
            &project.0,
            "which account?",
            &options,
            1,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        assert!(!item.is_open());
    }

    #[test]
    fn mark_asked_moves_an_open_item_and_attaches_the_menu() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "pick an account",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let options = vec!["personal".to_string(), "work".to_string()];
        let moved = mark_asked(&project.0, &item.item, &options, 1, None).unwrap();
        assert_eq!(moved.state, "asked");
        assert_eq!(moved.title, "pick an account");
        assert_eq!(moved.options, options);
        assert_eq!(moved.recommendation, Some(1));
        assert_eq!(read_items(&project.0, false)[0].state, "asked");
    }

    #[test]
    fn mark_asked_refuses_an_unknown_item() {
        let project = TempProject::new();
        let options = vec!["a".to_string(), "b".to_string()];
        let err = mark_asked(&project.0, "20260910-b-ffffff", &options, 1, None).unwrap_err();
        assert!(matches!(err, BacklogError::UnknownItem { .. }));
    }

    #[test]
    fn mark_asked_refuses_an_already_asked_item() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "pick an account",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let options = vec!["a".to_string(), "b".to_string()];
        mark_asked(&project.0, &item.item, &options, 1, None).unwrap();
        let err = mark_asked(&project.0, &item.item, &options, 1, None).unwrap_err();
        assert!(matches!(err, BacklogError::ItemNotOpen { .. }));
    }

    #[test]
    fn mark_asked_refuses_a_done_item() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "pick an account",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        done(&project.0, &item.item, "handled", None).unwrap();
        let options = vec!["a".to_string(), "b".to_string()];
        let err = mark_asked(&project.0, &item.item, &options, 1, None).unwrap_err();
        assert!(matches!(err, BacklogError::ItemNotOpen { .. }));
    }

    #[test]
    fn asked_item_can_still_be_answered_and_closed() {
        let project = TempProject::new();
        let options = vec!["personal".to_string(), "work".to_string()];
        let item = ask(
            &project.0,
            "which account?",
            &options,
            1,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();

        let appended =
            append_annotation_if_absent(&project.0, &item.item, "human", "use personal", None)
                .unwrap();
        assert!(appended);
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "asked");
        assert_eq!(items[0].annotations[0].note, "use personal");

        let closed = done(&project.0, &item.item, "used personal", None).unwrap();
        assert_eq!(closed.state, "done");
    }

    #[test]
    fn read_items_open_only_excludes_asked_items() {
        let project = TempProject::new();
        add(
            &project.0,
            "stays open",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let options = vec!["a".to_string(), "b".to_string()];
        ask(
            &project.0,
            "which account?",
            &options,
            1,
            "example",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();

        let open_items = read_items(&project.0, true);
        assert_eq!(open_items.len(), 1);
        assert_eq!(open_items[0].title, "stays open");
    }

    // --- annotate ------------------------------------------------------------

    #[test]
    fn annotate_appends_a_note_without_mutating_the_items_own_fields() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "original title",
            "example",
            "because",
            None,
            None,
            None,
            "P3",
            None,
        )
        .unwrap();

        let appended = append_annotation_if_absent(
            &project.0,
            &item.item,
            "reviewer",
            "EVIDENCE: reproduced on staging",
            None,
        )
        .unwrap();

        assert!(appended);
        let items = read_items(&project.0, false);
        assert_eq!(items[0].title, item.title);
        assert_eq!(items[0].why, item.why);
        assert_eq!(items[0].priority, item.priority);
        assert_eq!(items[0].created_at, item.created_at);
        assert_eq!(items[0].annotations.len(), 1);
        assert_eq!(
            items[0].annotations[0].note,
            "EVIDENCE: reproduced on staging"
        );
        assert_eq!(items[0].annotations[0].by, "reviewer");
    }

    #[test]
    fn annotate_advances_updated_at_to_the_annotation_time() {
        let project = TempProject::new();
        let created = datetime!(2026-09-10 12:00:00 UTC);
        let annotated = datetime!(2026-09-10 12:07:00 UTC);
        let item = add(
            &project.0,
            "one",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            Some(created),
        )
        .unwrap();

        append_annotation_if_absent(
            &project.0,
            &item.item,
            "reviewer",
            "seen it",
            Some(annotated),
        )
        .unwrap();

        let items = read_items(&project.0, false);
        assert_eq!(items[0].updated_at, format_seconds_utc(annotated));
    }

    #[test]
    fn annotate_is_idempotent_the_same_note_twice_appends_once() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();

        let first =
            append_annotation_if_absent(&project.0, &item.item, "reviewer", "seen it", None)
                .unwrap();
        let second =
            append_annotation_if_absent(&project.0, &item.item, "reviewer", "seen it", None)
                .unwrap();

        assert!(first);
        assert!(!second);
        assert_eq!(read_items(&project.0, false)[0].annotations.len(), 1);
    }

    #[test]
    fn annotate_two_distinct_notes_both_accumulate_in_order() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();

        append_annotation_if_absent(&project.0, &item.item, "reviewer", "first note", None)
            .unwrap();
        append_annotation_if_absent(&project.0, &item.item, "reviewer", "second note", None)
            .unwrap();

        let items = read_items(&project.0, false);
        let notes: Vec<&str> = items[0]
            .annotations
            .iter()
            .map(|a| a.note.as_str())
            .collect();
        assert_eq!(notes, vec!["first note", "second note"]);
    }

    #[test]
    fn annotate_refuses_an_unknown_item() {
        let project = TempProject::new();
        let err =
            append_annotation_if_absent(&project.0, "20260910-b-ffffff", "reviewer", "x", None)
                .unwrap_err();
        assert!(matches!(err, BacklogError::UnknownItem { .. }));
    }

    #[test]
    fn annotate_rejects_an_empty_or_whitespace_only_note() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();

        let err = append_annotation_if_absent(&project.0, &item.item, "reviewer", " \t\n", None)
            .unwrap_err();
        assert!(matches!(err, BacklogError::InvalidField { .. }));
        assert!(read_items(&project.0, false)[0].annotations.is_empty());
    }

    #[test]
    fn annotate_succeeds_on_an_already_done_item_and_state_stays_done() {
        let project = TempProject::new();
        let item = add(
            &project.0, "one", "example", "", None, None, None, "P2", None,
        )
        .unwrap();
        done(&project.0, &item.item, "", None).unwrap();

        let appended =
            append_annotation_if_absent(&project.0, &item.item, "reviewer", "too late", None)
                .unwrap();

        assert!(appended);
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "done");
        assert_eq!(items[0].annotations[0].note, "too late");
    }

    // --- resolve / reopen ------------------------------------------------------

    #[test]
    fn resolve_item_id_unique_prefix_and_ambiguous() {
        let project = TempProject::new();
        write_named_item(&project, "20261022-b-aaa111", "one", 1, "open");
        write_named_item(&project, "20261022-b-aaa222", "two", 2, "open");
        write_named_item(&project, "20261022-b-bbb333", "three", 3, "open");
        let ids: Vec<String> = read_items(&project.0, false)
            .into_iter()
            .map(|i| i.item)
            .collect();
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();

        assert_eq!(
            resolve_item_id(id_refs.clone(), "20261022-b-aaa111").unwrap(),
            "20261022-b-aaa111"
        );
        assert_eq!(
            resolve_item_id(id_refs.clone(), "20261022-b-bbb").unwrap(),
            "20261022-b-bbb333"
        );
        match resolve_item_id(id_refs.clone(), "20261022-b-aaa").unwrap_err() {
            BacklogError::AmbiguousItem { matches, .. } => {
                assert_eq!(matches, vec!["20261022-b-aaa111", "20261022-b-aaa222"]);
            }
            other => panic!("expected AmbiguousItem, got {other:?}"),
        }
        assert!(matches!(
            resolve_item_id(id_refs, "20261022-b-ffff").unwrap_err(),
            BacklogError::UnknownItem { .. }
        ));
    }

    #[test]
    fn reopen_moves_a_done_item_back_to_open_and_records_it() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "land the linked fix",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        done(&project.0, &item.item, "merged", None).unwrap();

        let reopened = reopen(&project.0, &item.item, "example", None).unwrap();

        assert_eq!(reopened.state, "open");
        assert_eq!(reopened.item, item.item);
        assert_eq!(
            reopened
                .annotations
                .iter()
                .map(|a| a.note.as_str())
                .collect::<Vec<_>>(),
            vec!["reopened"]
        );
        let items = read_items(&project.0, false);
        assert_eq!(items[0].state, "open");
        assert_eq!(items[0].evidence, "merged");
        assert_eq!(
            items[0]
                .annotations
                .iter()
                .map(|a| a.note.as_str())
                .collect::<Vec<_>>(),
            vec!["reopened"]
        );
    }

    #[test]
    fn reopen_refuses_an_item_that_is_not_done() {
        let project = TempProject::new();
        let item = add(
            &project.0,
            "still open",
            "example",
            "",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let err = reopen(&project.0, &item.item, "example", None).unwrap_err();
        assert!(matches!(err, BacklogError::ItemNotDone { .. }));
    }
}

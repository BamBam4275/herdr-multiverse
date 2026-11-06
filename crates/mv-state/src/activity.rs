//! Activity send/inbox/update records, plus cursor and
//! last-seen/last-ack bookkeeping.
//!
//! Ports the routing-log messaging protocol documented in
//! `.multiverse/input/spec-mail.md` §1 (`multiverse/activity.py`): one
//! append-only `v=1`/`crc32` envelope per line in a project's
//! `.multiverse/routing.log`, a file shared with `event=dispatch`/
//! `event=result`/`event=backlog`/… records this crate's `log`/`index`
//! modules (owned by another lane — L7, `architecture.md` §9) will
//! eventually parse generically.
//!
//! `support` retains the wire-format helpers used by `mail.rs` and
//! `notify.rs`. Lock acquisition and atomic writes delegate to `mv-core`;
//! the atomic-write adapter preserves directory creation and the callers'
//! existing mode argument.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

/// Wire-format helpers and compatibility adapters shared with mail/notify.
pub(crate) mod support {
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::path::Path;
    use std::sync::OnceLock;

    use regex::Regex;
    use time::OffsetDateTime;

    /// `activity._IDENTIFIER_RE` (`activity.py:304-309`): 1-64 characters,
    /// starting with a letter or digit, the rest `[A-Za-z0-9._-]`. A
    /// character-class rule, not a role enum — ids minted under retired
    /// role names keep validating (`mv-core-partial.patch`'s own test for
    /// the same regex covers this).
    fn identifier_re() -> &'static Regex {
        static RE: OnceLock<Regex> = OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
                .expect("static identifier regex compiles")
        })
    }

    pub(crate) fn is_valid_identifier(value: &str) -> bool {
        identifier_re().is_match(value)
    }

    /// `zlib.crc32`-compatible CRC32, lower-case `%08x` — same algorithm
    /// `crc32fast` implements, pinned by the golden vectors in
    /// `mv-core-partial.patch`.
    pub(crate) fn crc32_hex(data: &[u8]) -> String {
        format!("{:08x}", crc32fast::hash(data))
    }

    /// `datetime.now(timezone.utc).isoformat(timespec="seconds")`: always a
    /// numeric `+00:00` offset, never a trailing `Z`.
    pub(crate) fn now_stamp() -> String {
        format_seconds_utc(OffsetDateTime::now_utc())
    }

    pub(crate) fn format_seconds_utc(instant: OffsetDateTime) -> String {
        let utc = instant.to_offset(time::UtcOffset::UTC);
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
            utc.year(),
            u8::from(utc.month()),
            utc.day(),
            utc.hour(),
            utc.minute(),
            utc.second(),
        )
    }

    /// Readers accept `Z` or a numeric offset — RFC 3339 already specifies
    /// both as equivalent, so no bespoke `Z`-stripping is needed.
    pub(crate) fn parse_flexible(value: &str) -> Option<OffsetDateTime> {
        OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
    }

    /// `json.dumps(value, ensure_ascii=True)` for a single string: quotes,
    /// escapes control characters and `"`/`\`, and escapes every non-ASCII
    /// `char` as `\uXXXX` (UTF-16 surrogate pairs for astral code points)
    /// instead of emitting raw UTF-8 bytes. `serde_json` emits raw UTF-8 by
    /// default (`architecture.md` §3 flags exactly this mismatch), so this
    /// is hand-rolled to stay byte-stable with the Python writer.
    pub(crate) fn json_quote_ascii(value: &str) -> String {
        let mut out = String::with_capacity(value.len() + 2);
        out.push('"');
        for c in value.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c if c.is_ascii() => out.push(c),
                c => {
                    let mut buf = [0u16; 2];
                    for unit in c.encode_utf16(&mut buf) {
                        out.push_str(&format!("\\u{:04x}", unit));
                    }
                }
            }
        }
        out.push('"');
        out
    }

    /// The inverse of [`json_quote_ascii`]. Decoding is ASCII-escaping-
    /// agnostic, so the standard JSON string grammar (which `serde_json`
    /// implements correctly in both directions) is enough here.
    pub(crate) fn json_unquote(value: &str) -> Result<String, serde_json::Error> {
        serde_json::from_str(value)
    }

    /// Preserve parent creation and the legacy mode argument while using
    /// the shared durable writer. Mail, notify and activity all use 0600.
    pub(crate) fn atomic_write(path: &Path, contents: &[u8], mode: u32) -> io::Result<()> {
        let dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(dir)?;
        let path = dir.join(path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "atomic write path has no filename",
            )
        })?);
        mv_core::atomic::write_atomic(&path, contents)?;
        #[cfg(unix)]
        if mode != 0o600 {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn ensure_dir_mode(path: &Path, mode: u32) -> io::Result<()> {
        fs::create_dir_all(path)?;
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }

    pub(crate) fn open_lock_file(path: &Path) -> io::Result<File> {
        if let Some(dir) = path.parent() {
            // Corrected to 0700 on every acquisition, not just the first
            // (`accounts.py:2823-2827`) — a plain `create_dir_all` here
            // left `.multiverse/` at the process umask's mode, and
            // `append_line`'s own correction only ran when it saw the
            // directory get created, so it never ran at all once the lock
            // had already created it.
            #[cfg(unix)]
            ensure_dir_mode(dir, 0o700)?;
            #[cfg(not(unix))]
            fs::create_dir_all(dir)?;
        }
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(path)
    }

    /// Hub-first, then alphabetical — the one true lock order
    /// (`activity.py:80-91`, `architecture.md` §8's "lock order to
    /// preserve"). `multiverse` is always `HUB_PROJECT_NAME`.
    pub(crate) fn lock_order(hub: &str, mut projects: Vec<String>) -> Vec<String> {
        projects.sort();
        projects.dedup();
        let mut ordered = Vec::with_capacity(projects.len());
        if let Some(pos) = projects.iter().position(|p| p == hub) {
            ordered.push(projects.remove(pos));
        }
        ordered.extend(projects);
        ordered
    }
}

/// `multiverse`: the only project `broadcast`/`fan_out` may originate from
/// (`activity.py:2262-2354`, structural check, not a passed flag).
pub const HUB_PROJECT_NAME: &str = "multiverse";

/// `activity.py:206` — the cap on a message body's encoded length, checked
/// before redaction/encoding.
pub const MAX_BODY_BYTES: usize = 4096;

/// `activity.py:240` — `activity update` suppresses a repeat inside this
/// window unless `phase == Wait`, which always posts.
pub const UPDATE_COOLDOWN: Duration = Duration::from_secs(60);

/// Bounded wait for the per-project `routing.log.lock` flock, matching the
/// 30s timeout `mv_core::lock`/`LockAcquireTimeoutError` is documented to
/// use.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Failures from every operation in this file. Mirrors the shape of
/// `ActivityError` and its Python siblings (`InvalidIdentifierError`,
/// `NotHubError`, `ActivityReadError`, …) closely enough that `mv-cli`'s
/// exit-code mapping (`architecture.md` §5) can match on it the same way.
#[derive(Debug, Error)]
pub enum ActivityError {
    #[error(
        "{kind} {value:?} cannot be written into the routing log safely — only \
         letters, digits, '.', '_' and '-' are allowed there."
    )]
    InvalidIdentifier { kind: &'static str, value: String },

    #[error("message body is {actual} bytes, over the {MAX_BODY_BYTES}-byte limit")]
    BodyTooLarge { actual: usize },

    #[error("sending from {from:?} to {to:?} is not permitted by [activity.pairs]")]
    NotPermitted { from: String, to: String },

    #[error(
        "only {HUB_PROJECT_NAME} may broadcast or fan out a message; caller project was {0:?}"
    )]
    NotHub(String),

    #[error("routing-log record is damaged: {0}")]
    DamagedRecord(String),

    #[error(
        "the read cursor is corrupt (not a whole number); delete activity.cursor in the \
         project's own .multiverse directory if you are sure everything already shown may \
         be treated as seen, or restore it from backup"
    )]
    CursorCorrupt,

    #[error(
        "the read cursor ({cursor}) is ahead of the log's own high-water mark ({high_water}); \
         the log looks like it was reset or replaced since the cursor was last written, so \
         this reader will not guess which of its current records, if any, were already seen"
    )]
    CursorAheadOfLog { cursor: u64, high_water: u64 },

    #[error(
        "no ask #{0} found in this project's own routing log; \
         nothing to reply to or close"
    )]
    AskNotFound(u64),

    #[error("{raw:?} is not a SEQ or Project#SEQ")]
    InvalidAskRef { raw: String },

    #[error(
        "project '{name}' is configured but its directory does not exist. \
         Nothing was created and nothing was sent — fix the path in config, \
         or create the directory there, first."
    )]
    RecipientDirectoryMissing { name: String },

    #[error(
        "no dispatched card '{card}' found in this project's routing log; \
         nothing was posted"
    )]
    UnknownCard { card: String },

    #[error(
        "card '{card}' is already terminal; its result is what counts now, \
         and nothing was posted"
    )]
    TerminalCard { card: String },

    #[error(
        "cannot close {project}#{seq}: that ask is held by '{holder}', \
         not '{project}'; nothing was closed"
    )]
    AskNotHeldHere {
        project: String,
        seq: u64,
        holder: String,
    },

    #[error(
        "broadcast {msg_id} was delivered to: {reached}; NOT delivered to: {lost}. \
         Retry with the same message id to reach only the rest."
    )]
    FanOutPartial {
        msg_id: String,
        reached: String,
        lost: String,
    },

    /// `critic-engine-state.md` finding 4: the sequence allocators used an
    /// unchecked `high_water_seq + 1`, which panics in debug builds and
    /// wraps to 0 in release once a record with `seq = u64::MAX` is on the
    /// log — reusing a sequence number and corrupting cursor ordering.
    /// `mv-state::log`'s own allocator already guards this with
    /// `checked_add`; this mirrors it.
    #[error("routing-log sequence exhausted")]
    SequenceExhausted,

    #[error(transparent)]
    Io(#[from] io::Error),
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), ActivityError> {
    if support::is_valid_identifier(value) {
        Ok(())
    } else {
        Err(ActivityError::InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

/// `UpdatePhase` (`activity.py:895-912`): `activity update`'s three
/// phases. `Wait` is the one phase the cooldown never suppresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdatePhase {
    Working,
    Finding,
    Wait,
}

impl UpdatePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdatePhase::Working => "working",
            UpdatePhase::Finding => "finding",
            UpdatePhase::Wait => "wait",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "working" => Some(UpdatePhase::Working),
            "finding" => Some(UpdatePhase::Finding),
            "wait" => Some(UpdatePhase::Wait),
            _ => None,
        }
    }
}

/// A validated `event=message` record (`activity.py:1014-1104`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    pub seq: u64,
    pub when: String,
    pub id: String,
    pub from_project: String,
    pub to_project: String,
    pub body: String,
    pub ask: bool,
    pub reply_to: Option<u64>,
}

/// A validated `event=update` record (`activity.py:1107-1161`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateRecord {
    pub seq: u64,
    pub when: String,
    pub task: String,
    pub phase: UpdatePhase,
    pub note: String,
}

/// A validated `event=close` record (`activity.py:1181-1231`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseRecord {
    pub seq: u64,
    pub when: String,
    pub reply_to: u64,
    pub reason: String,
}

/// One line's worth of envelope, already crc32-verified, before the
/// event-specific fields are interpreted. `_classify_record`'s "not
/// damaged, just not mine" case (activity.py:1021-1028) is every
/// `Classified::Other` here.
#[derive(Debug, Clone)]
struct RawEnvelope {
    when: String,
    seq: u64,
    event: String,
    fields: Vec<(String, String)>,
}

impl RawEnvelope {
    fn field(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Decode one `routing.log` line as a `v=1` envelope: tab-separated
/// `key=value`, crc32 over every byte up to (not including) the trailing
/// `\tcrc32=` field (`activity.py:964-996`). Any failure is a
/// [`ActivityError::DamagedRecord`] — never silently dropped, never treated
/// as a valid message.
fn decode_v1(line: &str) -> Result<RawEnvelope, ActivityError> {
    const CRC_MARKER: &str = "\tcrc32=";
    let marker_at = line
        .rfind(CRC_MARKER)
        .ok_or_else(|| ActivityError::DamagedRecord("missing trailing crc32= field".into()))?;
    let prefix = &line[..marker_at];
    let on_disk_crc = &line[marker_at + CRC_MARKER.len()..];
    let expected_crc = support::crc32_hex(prefix.as_bytes());
    if on_disk_crc != expected_crc {
        return Err(ActivityError::DamagedRecord(format!(
            "crc32 mismatch: on-disk {on_disk_crc:?}, computed {expected_crc:?}"
        )));
    }

    let mut parts = prefix.split('\t');
    let when = parts
        .next()
        .ok_or_else(|| ActivityError::DamagedRecord("empty line".into()))?
        .to_string();

    let mut fields = Vec::new();
    for part in parts {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| ActivityError::DamagedRecord(format!("field without '=': {part:?}")))?;
        fields.push((key.to_string(), value.to_string()));
    }

    let version = fields
        .iter()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.as_str());
    if version != Some("1") {
        return Err(ActivityError::DamagedRecord(format!(
            "unsupported envelope version: {version:?}"
        )));
    }
    let seq = fields
        .iter()
        .find(|(k, _)| k == "seq")
        .and_then(|(_, v)| v.parse::<u64>().ok())
        .ok_or_else(|| ActivityError::DamagedRecord("missing or invalid seq=".into()))?;
    let event = fields
        .iter()
        .find(|(k, _)| k == "event")
        .map(|(_, v)| v.clone())
        .ok_or_else(|| ActivityError::DamagedRecord("missing event=".into()))?;

    Ok(RawEnvelope {
        when,
        seq,
        event,
        fields,
    })
}

/// The raw `seq=` field's value from one line, parsed without requiring
/// the line to pass crc32/version/field validation — used only to
/// compute [`ProjectLog::high_water_seq`]'s reservation, never to accept
/// a record as a real [`MessageRecord`]/[`UpdateRecord`]/[`CloseRecord`].
fn raw_seq_field(line: &str) -> Option<u64> {
    line.split('\t')
        .filter_map(|part| part.split_once('='))
        .find(|(key, _)| *key == "seq")
        .and_then(|(_, value)| value.parse::<u64>().ok())
}

/// Python's `_fields` (`activity.py`): crc-optional tab-separated `key=value`
/// pairs. Used by the dispatch/result card scan so a checksum-damaged
/// `event=dispatch` still counts as "this card exists", matching
/// `tasks.read_log`'s looseness.
fn raw_fields(line: &str) -> Vec<(&str, &str)> {
    line.split('\t')
        .filter_map(|part| part.split_once('='))
        .collect()
}

fn card_event_present(text: &str, event: &str, card: &str) -> bool {
    text.lines().filter(|line| !line.is_empty()).any(|line| {
        let fields = raw_fields(line);
        fields.iter().any(|(k, v)| *k == "event" && *v == event)
            && fields.iter().any(|(k, v)| *k == "task" && *v == card)
    })
}

fn path_free_io(error: &io::Error) -> String {
    error
        .raw_os_error()
        .map(|code| io::Error::from_raw_os_error(code).to_string())
        .unwrap_or_else(|| error.kind().to_string())
}

fn require_existing_root(log: &ProjectLog) -> Result<(), ActivityError> {
    if log.root.exists() {
        Ok(())
    } else {
        Err(ActivityError::RecipientDirectoryMissing {
            name: log.project.clone(),
        })
    }
}

/// Split a bare SEQ or `Project#SEQ` into `(project or None, seq)`.
/// `activity.py:parse_ask_ref`.
pub fn parse_ask_ref(raw: &str) -> Result<(Option<String>, u64), ActivityError> {
    let text = raw.trim();
    let invalid = || ActivityError::InvalidAskRef {
        raw: raw.to_string(),
    };
    if let Some((name, rest)) = text.split_once('#') {
        if name.is_empty()
            || rest.is_empty()
            || !rest.chars().all(|c| c.is_ascii_digit())
            || !support::is_valid_identifier(name)
        {
            return Err(invalid());
        }
        let seq = rest.parse().map_err(|_| invalid())?;
        return Ok((Some(name.to_string()), seq));
    }
    if text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    let seq = text.parse().map_err(|_| invalid())?;
    Ok((None, seq))
}

/// Build one `v=1` line (without the trailing newline): shared envelope
/// fields first, then `extra` in the order given, then the crc32 trailer.
fn encode_v1(when: &str, seq: u64, event: &str, extra: &[(&str, String)]) -> String {
    let mut prefix = format!("{when}\tv=1\tseq={seq}\tevent={event}");
    for (key, value) in extra {
        prefix.push('\t');
        prefix.push_str(key);
        prefix.push('=');
        prefix.push_str(value);
    }
    let crc = support::crc32_hex(prefix.as_bytes());
    format!("{prefix}\tcrc32={crc}")
}

#[allow(clippy::too_many_arguments)]
fn encode_message(
    when: &str,
    seq: u64,
    id: &str,
    from_project: &str,
    to_project: &str,
    body: &str,
    ask: bool,
    reply_to: Option<u64>,
) -> String {
    let mut extra: Vec<(&str, String)> = vec![
        ("id", id.to_string()),
        ("from", from_project.to_string()),
        ("to", to_project.to_string()),
        ("body", support::json_quote_ascii(body)),
    ];
    if ask {
        extra.push(("ask", "1".to_string()));
    }
    if let Some(re) = reply_to {
        extra.push(("re", re.to_string()));
    }
    encode_v1(when, seq, "message", &extra)
}

fn encode_update(when: &str, seq: u64, task: &str, phase: UpdatePhase, note: &str) -> String {
    let extra: Vec<(&str, String)> = vec![
        ("task", task.to_string()),
        ("phase", phase.as_str().to_string()),
        ("note", support::json_quote_ascii(note)),
    ];
    encode_v1(when, seq, "update", &extra)
}

fn encode_close(when: &str, seq: u64, reply_to: u64, reason: &str) -> String {
    let extra: Vec<(&str, String)> = vec![
        ("re", reply_to.to_string()),
        ("reason", support::json_quote_ascii(reason)),
    ];
    encode_v1(when, seq, "close", &extra)
}

fn classify_message(env: &RawEnvelope) -> Result<Option<MessageRecord>, ActivityError> {
    if env.event != "message" {
        return Ok(None);
    }
    let id = env
        .field("id")
        .ok_or_else(|| ActivityError::DamagedRecord("message missing id=".into()))?
        .to_string();
    let from_project = env
        .field("from")
        .ok_or_else(|| ActivityError::DamagedRecord("message missing from=".into()))?
        .to_string();
    let to_project = env
        .field("to")
        .ok_or_else(|| ActivityError::DamagedRecord("message missing to=".into()))?
        .to_string();
    let body_raw = env
        .field("body")
        .ok_or_else(|| ActivityError::DamagedRecord("message missing body=".into()))?;
    let body = support::json_unquote(body_raw).map_err(|e| {
        ActivityError::DamagedRecord(format!("message body is not valid JSON: {e}"))
    })?;
    let ask = env.field("ask") == Some("1");
    let reply_to = match env.field("re") {
        Some(v) => Some(v.parse::<u64>().map_err(|_| {
            ActivityError::DamagedRecord(format!("message has non-numeric re=: {v:?}"))
        })?),
        None => None,
    };
    Ok(Some(MessageRecord {
        seq: env.seq,
        when: env.when.clone(),
        id,
        from_project,
        to_project,
        body,
        ask,
        reply_to,
    }))
}

fn classify_update(env: &RawEnvelope) -> Result<Option<UpdateRecord>, ActivityError> {
    if env.event != "update" {
        return Ok(None);
    }
    let task = env
        .field("task")
        .ok_or_else(|| ActivityError::DamagedRecord("update missing task=".into()))?
        .to_string();
    let phase_raw = env
        .field("phase")
        .ok_or_else(|| ActivityError::DamagedRecord("update missing phase=".into()))?;
    let phase = UpdatePhase::parse(phase_raw).ok_or_else(|| {
        ActivityError::DamagedRecord(format!("update has unknown phase={phase_raw:?}"))
    })?;
    let note_raw = env
        .field("note")
        .ok_or_else(|| ActivityError::DamagedRecord("update missing note=".into()))?;
    let note = support::json_unquote(note_raw)
        .map_err(|e| ActivityError::DamagedRecord(format!("update note is not valid JSON: {e}")))?;
    Ok(Some(UpdateRecord {
        seq: env.seq,
        when: env.when.clone(),
        task,
        phase,
        note,
    }))
}

fn classify_close(env: &RawEnvelope) -> Result<Option<CloseRecord>, ActivityError> {
    if env.event != "close" {
        return Ok(None);
    }
    let reply_to = env
        .field("re")
        .ok_or_else(|| ActivityError::DamagedRecord("close missing re=".into()))?
        .parse::<u64>()
        .map_err(|_| ActivityError::DamagedRecord("close has non-numeric re=".into()))?;
    let reason_raw = env
        .field("reason")
        .ok_or_else(|| ActivityError::DamagedRecord("close missing reason=".into()))?;
    let reason = support::json_unquote(reason_raw).map_err(|e| {
        ActivityError::DamagedRecord(format!("close reason is not valid JSON: {e}"))
    })?;
    Ok(Some(CloseRecord {
        seq: env.seq,
        when: env.when.clone(),
        reply_to,
        reason,
    }))
}

/// A single project's `.multiverse/routing.log`: the append-under-lock
/// primitive this file uses until `log.rs` (L7) lands. One project, one
/// file, one sibling `.lock`.
pub struct ProjectLog {
    project: String,
    root: PathBuf,
    log_path: PathBuf,
    lock_path: PathBuf,
}

impl ProjectLog {
    /// `project_root/.multiverse/routing.log` (`accounts.py:2367`,
    /// `routing_log_path`, cited by `.multiverse/input/spec-mail.md`).
    pub fn open(project: impl Into<String>, project_root: &Path) -> Self {
        let state_dir = project_root.join(".multiverse");
        Self {
            project: project.into(),
            root: project_root.to_path_buf(),
            log_path: state_dir.join("routing.log"),
            lock_path: state_dir.join("routing.log.lock"),
        }
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    fn read_raw(&self) -> io::Result<String> {
        match fs::read_to_string(&self.log_path) {
            Ok(text) => Ok(text),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    /// Every line that decodes as a valid `v=1` envelope, in file order.
    /// Lines that fail to decode are skipped, not surfaced — a caller that
    /// needs to know about damage should scan with [`Self::lines`]
    /// directly.
    fn envelopes(&self) -> io::Result<Vec<RawEnvelope>> {
        let text = self.read_raw()?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty() && l.contains("\tcrc32="))
            .filter_map(|l| decode_v1(l).ok())
            .collect())
    }

    /// The highest `seq` found in *any* tab-separated `seq=` field in the
    /// log, including lines that fail crc32/version/structural validation
    /// elsewhere — Python scans raw fields and reserves every integer
    /// `seq` it can parse, damaged record or not (`activity.py:1288-1308`),
    /// so that a checksum-damaged record's `seq` is never handed out again
    /// to a newly appended one. Using `Self::envelopes` here (crc32-
    /// validated only) would let a damaged record's `seq` silently come
    /// back into circulation.
    pub fn high_water_seq(&self) -> io::Result<u64> {
        let text = self.read_raw()?;
        Ok(text
            .lines()
            .filter(|l| !l.is_empty())
            .filter_map(raw_seq_field)
            .max()
            .unwrap_or(0))
    }

    fn contains_id(&self, id: &str) -> io::Result<bool> {
        Ok(self
            .envelopes()?
            .iter()
            .any(|e| e.event == "message" && e.field("id") == Some(id)))
    }

    /// Hold `routing.log.lock` exclusively (bounded, 30s) for the duration
    /// of `f`. Generic over `f`'s error type (constrained to anything
    /// [`ActivityError`] can absorb) so a caller that needs more than a
    /// bare `io::Result` under the same lock — [`acknowledge`] and
    /// [`post_update`] read-compare-write cursor/cooldown state here, not
    /// just append a line — can still use it, matching Python's one
    /// project-lock covering the whole read/compare/write sequence
    /// (`activity.py:2759-2774`).
    fn with_lock<T, E>(&self, f: impl FnOnce() -> Result<T, E>) -> Result<T, ActivityError>
    where
        ActivityError: From<E>,
    {
        let _lock =
            mv_core::lock::acquire(&self.lock_path, "activity", LOCK_TIMEOUT).map_err(|error| {
                match error {
                    mv_core::lock::LockError::Timeout(error) => {
                        io::Error::new(io::ErrorKind::TimedOut, error)
                    }
                    mv_core::lock::LockError::Io(error) => error,
                }
            })?;
        f().map_err(ActivityError::from)
    }

    /// Append `line` + `\n` durably: open append-only, write, fsync file,
    /// fsync the `.multiverse/` dir on first creation
    /// (`accounts._append`, `accounts.py:3769-3833`).
    fn append_line(&self, line: &str) -> io::Result<()> {
        let dir = self.log_path.parent().expect("log path has a parent");
        let dir_existed = dir.exists();
        // Corrected unconditionally, matching Python's "again before
        // append" (`accounts.py:3804-3807`) — this must not be skipped
        // just because `with_lock`'s own lock-file open already created
        // the directory first.
        #[cfg(unix)]
        support::ensure_dir_mode(dir, 0o700)?;
        #[cfg(not(unix))]
        fs::create_dir_all(dir)?;
        let log_existed = self.log_path.exists();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)?;
        writeln!(file, "{line}")?;
        file.sync_all()?;
        #[cfg(unix)]
        if !log_existed {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        if !dir_existed || !log_existed {
            if let Ok(dir_file) = File::open(dir) {
                let _ = dir_file.sync_all();
            }
        }
        Ok(())
    }

    /// `_append_message_if_absent` (`activity.py:1696-1744`): idempotent on
    /// `id`, allocates `seq = max_seq_in(log) + 1` under the project lock.
    fn append_message_if_absent(
        &self,
        id: &str,
        build_line: impl Fn(u64) -> String,
    ) -> Result<AppendOutcome, ActivityError> {
        self.with_lock(|| -> Result<AppendOutcome, ActivityError> {
            if self.contains_id(id)? {
                return Ok(AppendOutcome::AlreadyPresent);
            }
            let seq = self
                .high_water_seq()?
                .checked_add(1)
                .ok_or(ActivityError::SequenceExhausted)?;
            self.append_line(&build_line(seq))?;
            Ok(AppendOutcome::Appended { seq })
        })
    }

    /// The unlocked core of [`Self::append_versioned`]: allocate the next
    /// sequence number and append. Callers that must run this alongside
    /// another check under the *same* lock acquisition — [`post_update`]'s
    /// cooldown check, so the check and the append are one transaction
    /// rather than two lock acquisitions racing each other — call this
    /// directly from inside their own [`Self::with_lock`] closure instead
    /// of going through [`Self::append_versioned`], which would try to
    /// take the lock a second time and deadlock against itself.
    fn append_versioned_locked(
        &self,
        build_line: impl Fn(u64) -> String,
    ) -> Result<u64, ActivityError> {
        let seq = self
            .high_water_seq()?
            .checked_add(1)
            .ok_or(ActivityError::SequenceExhausted)?;
        self.append_line(&build_line(seq))?;
        Ok(seq)
    }

    /// Every `event=message` record addressed `to == self.project`, in
    /// file order. Mirrors how `read_inbox` sources its candidate set
    /// before cursor/limit filtering.
    pub fn inbox_messages(&self) -> Result<Vec<MessageRecord>, ActivityError> {
        Ok(self.inbox_and_closes()?.0)
    }

    /// One pass over this log: inbox messages plus the `event=close`
    /// `re=` set [`is_closed`] would consult. [`MailIndex`] uses this so
    /// a fleet sweep does not re-parse the same `routing.log` once per
    /// ask.
    ///
    /// A damaged `event=close` record (missing/non-numeric `re=`, missing
    /// or non-JSON `reason=`) is skipped like any other unreadable line,
    /// not propagated — matching `board_log_snapshot`'s own per-line
    /// tolerance (`activity.py:788-800`: a line that fails `_classify_close`
    /// is simply never added to `closed`, the scan carries on). Before
    /// [`MailIndex`] (card `20261103-fixer-2faef1` finding 2), a close this
    /// far from the holder itself never mattered: `is_closed` only ever
    /// read the holder's own log. Now one project's log feeds every
    /// holder's lookup, so propagating this error here — as `classify_
    /// message`'s own fleet-wide-on-error posture already does, a separate,
    /// pre-existing condition this fix does not touch — would have let a
    /// single damaged close record anywhere in the fleet silently disable
    /// unlinked-reply detection for every project, every pass.
    fn inbox_and_closes(&self) -> Result<(Vec<MessageRecord>, HashSet<u64>), ActivityError> {
        let envelopes = self.envelopes()?;
        let mut inbox = Vec::new();
        let mut closed = HashSet::new();
        for env in &envelopes {
            if let Some(msg) = classify_message(env)? {
                if msg.to_project == self.project {
                    inbox.push(msg);
                }
            }
            if let Ok(Some(close)) = classify_close(env) {
                closed.insert(close.reply_to);
            }
        }
        Ok((inbox, closed))
    }

    /// Every record of any kind addressed `to == self.project` *or*
    /// classified as `event=update`/`event=close` on this project's own
    /// log — used by [`find_open_ask`] to locate an ask this project is
    /// holding.
    fn own_updates_and_closes(
        &self,
    ) -> Result<(Vec<UpdateRecord>, Vec<CloseRecord>), ActivityError> {
        let mut updates = Vec::new();
        let mut closes = Vec::new();
        for env in self.envelopes()? {
            if let Some(u) = classify_update(&env)? {
                updates.push(u);
            }
            if let Some(c) = classify_close(&env)? {
                closes.push(c);
            }
        }
        Ok((updates, closes))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppendOutcome {
    Appended { seq: u64 },
    AlreadyPresent,
}

/// What `send`/`send_message` reports back (`activity.py:1608-1816`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    Sent { seq: u64 },
    AlreadySent,
}

/// `send`/`send_message` (`activity.py:1608-1816`): append one
/// `event=message` to the **recipient's** log — a project reads its own
/// inbox from its own log, so a message lands where its reader will look
/// for it. `redact` is applied to the body before it is ever written
/// (`activity.py:1653`); `may_send` is the caller-supplied
/// `[activity.pairs]` gate (`config.py:1570-1601`) — this module does not
/// know the config schema, so it takes the already-evaluated decision
/// rather than guessing at TOML shape `mv-config` has not landed to parse.
#[allow(clippy::too_many_arguments)]
pub fn send_message(
    recipient_log: &ProjectLog,
    id: &str,
    from_project: &str,
    to_project: &str,
    body: &str,
    ask: bool,
    reply_to: Option<u64>,
    may_send: bool,
    redact: &dyn Fn(&str) -> String,
) -> Result<SendOutcome, ActivityError> {
    validate_identifier("message id", id)?;
    validate_identifier("sender project name", from_project)?;
    validate_identifier("recipient project name", to_project)?;
    if !may_send {
        return Err(ActivityError::NotPermitted {
            from: from_project.to_string(),
            to: to_project.to_string(),
        });
    }
    // Directory existence is a config-aware `send`/`fan_out` check
    // (`activity.py:1776-1804`, `activity.py:2318-2330`). This primitive
    // still creates `.multiverse/` on first write, matching
    // `_append`/`ensure_state_dir`, so library callers such as the hub
    // disk notice can land a message in a project root they have not
    // mkdir'd yet.
    if body.len() > MAX_BODY_BYTES {
        return Err(ActivityError::BodyTooLarge { actual: body.len() });
    }
    let redacted = redact(body);
    let when = support::now_stamp();
    let outcome = recipient_log.append_message_if_absent(id, |seq| {
        encode_message(
            &when,
            seq,
            id,
            from_project,
            to_project,
            &redacted,
            ask,
            reply_to,
        )
    })?;
    Ok(match outcome {
        AppendOutcome::Appended { seq } => SendOutcome::Sent { seq },
        AppendOutcome::AlreadyPresent => SendOutcome::AlreadySent,
    })
}

/// One recipient's outcome within a [`FanOutReport`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FanOutResult {
    Sent { seq: u64 },
    AlreadySent,
    Skipped { reason: String },
}

/// `FanOutReport` (`activity.py:2262-2354`): named per-recipient outcomes,
/// not just a count.
#[derive(Debug, Clone, Default)]
pub struct FanOutReport {
    pub results: Vec<(String, FanOutResult)>,
}

impl FanOutReport {
    pub fn sent(&self) -> impl Iterator<Item = &str> {
        self.results.iter().filter_map(|(p, r)| match r {
            FanOutResult::Sent { .. } | FanOutResult::AlreadySent => Some(p.as_str()),
            FanOutResult::Skipped { .. } => None,
        })
    }

    pub fn skipped(&self) -> impl Iterator<Item = (&str, &str)> {
        self.results.iter().filter_map(|(p, r)| match r {
            FanOutResult::Skipped { reason } => Some((p.as_str(), reason.as_str())),
            _ => None,
        })
    }
}

/// `fan_out`/`broadcast` (`activity.py:2262-2354`): hub-only — structural,
/// not a passed flag. One shared `msg_id` is written into every permitted
/// recipient's log so a retry with the same id reaches only the leaves
/// that never got it. Pairing refusals go in the report as `skipped`; a
/// permitted recipient whose directory is missing raises
/// [`ActivityError::RecipientDirectoryMissing`] before the first write;
/// IO failures continue then raise [`ActivityError::FanOutPartial`].
pub fn broadcast(
    from_project: &str,
    body: &str,
    recipients: &[(ProjectLog, bool)],
    msg_id: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<FanOutReport, ActivityError> {
    if from_project != HUB_PROJECT_NAME {
        return Err(ActivityError::NotHub(from_project.to_string()));
    }
    // Hub-first, then alphabetical (`activity.py:80-91`) — fan-out touches
    // several projects' logs in sequence, so it follows the same lock
    // order a multi-project caller must use to avoid the deadlock `df7ce05`
    // fixed, even though each individual append below only ever holds one
    // project's lock at a time.
    let names: Vec<String> = recipients
        .iter()
        .map(|(log, _)| log.project().to_string())
        .collect();
    let ordered_names = support::lock_order(HUB_PROJECT_NAME, names);
    let mut by_name: std::collections::HashMap<&str, &(ProjectLog, bool)> =
        recipients.iter().map(|r| (r.0.project(), r)).collect();
    let mut report = FanOutReport::default();
    let mut permitted = Vec::new();
    for name in &ordered_names {
        let Some(&(ref log, may_send)) = by_name.remove(name.as_str()) else {
            continue;
        };
        if !may_send {
            report.results.push((
                log.project().to_string(),
                FanOutResult::Skipped {
                    reason: "not permitted by [activity.pairs]".to_string(),
                },
            ));
            continue;
        }
        require_existing_root(log)?;
        permitted.push(log);
    }
    let mut sent_names = Vec::new();
    let mut failed = Vec::new();
    for log in permitted {
        match send_message(
            log,
            msg_id,
            from_project,
            log.project(),
            body,
            false,
            None,
            true,
            redact,
        ) {
            Ok(SendOutcome::Sent { seq }) => {
                report
                    .results
                    .push((log.project().to_string(), FanOutResult::Sent { seq }));
                sent_names.push(log.project().to_string());
            }
            Ok(SendOutcome::AlreadySent) => {
                report
                    .results
                    .push((log.project().to_string(), FanOutResult::AlreadySent));
                sent_names.push(log.project().to_string());
            }
            Err(ActivityError::Io(error)) => {
                failed.push((log.project().to_string(), path_free_io(&error)));
            }
            Err(e) => return Err(e),
        }
    }
    if !failed.is_empty() {
        let reached = if sent_names.is_empty() {
            "(none)".to_string()
        } else {
            sent_names.join(", ")
        };
        let lost = failed
            .iter()
            .map(|(name, reason)| format!("{name} — {reason}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(ActivityError::FanOutPartial {
            msg_id: msg_id.to_string(),
            reached,
            lost,
        });
    }
    Ok(report)
}

/// An ask this project is holding: a received message with `ask=1` that
/// has not yet been answered or explicitly closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAsk {
    pub seq: u64,
    pub from_project: String,
    pub body: String,
}

/// `find_ask`: scan `log` (the caller's own log, i.e. the ask **holder**'s —
/// the project that *received* the `ask=1` message) for an unclosed ask at
/// `seq`. A reply lands in the far side's log (the original asker's), not
/// this one, so "closed" is tracked entirely within the holder's own log:
/// [`reply`] writes a confirming `event=close` here immediately after its
/// answer is durably sent, and [`close_ask`] writes the same kind of record
/// for a decline. Either way, `close_ask` never messages the asker
/// directly (`activity.py:1181-1231`'s "ask holder's own log, never
/// messaged" — this module's equivalent of that invariant, chosen because
/// the real `is_closed`'s exact mechanism was not in this task's source
/// excerpts; see this file's `result.md` entry on the point).
pub fn find_open_ask(log: &ProjectLog, seq: u64) -> Result<OpenAsk, ActivityError> {
    let messages = log.inbox_messages()?;
    let ask_message = messages
        .iter()
        .find(|m| m.seq == seq && m.ask)
        .ok_or(ActivityError::AskNotFound(seq))?;
    let (_, closes) = log.own_updates_and_closes()?;
    if closes.iter().any(|c| c.reply_to == seq) {
        return Err(ActivityError::AskNotFound(seq));
    }
    Ok(OpenAsk {
        seq: ask_message.seq,
        from_project: ask_message.from_project.clone(),
        body: ask_message.body.clone(),
    })
}

/// The `ask=1` message at `seq` in `log`, even if it has already been
/// closed. `None` if `seq` names nothing, or names a non-ask.
/// `activity.py:find_ask`.
pub fn find_ask(log: &ProjectLog, seq: u64) -> Result<Option<MessageRecord>, ActivityError> {
    for env in log.envelopes()? {
        if let Some(msg) = classify_message(&env)? {
            if msg.ask && msg.seq == seq {
                return Ok(Some(msg));
            }
        }
    }
    Ok(None)
}

/// Open (unclosed) asks this project is holding, in file order.
pub fn open_asks(log: &ProjectLog) -> Result<Vec<MessageRecord>, ActivityError> {
    let (messages, closed) = log.inbox_and_closes()?;
    Ok(messages
        .into_iter()
        .filter(|m| m.ask && !closed.contains(&m.seq))
        .collect())
}

/// Whether `log` already has an `event=close` with `re=ask_seq`.
/// `activity.py:is_closed` without `config` — local close record only.
pub fn has_close_record(log: &ProjectLog, ask_seq: u64) -> Result<bool, ActivityError> {
    let (_, closes) = log.own_updates_and_closes()?;
    Ok(closes.iter().any(|c| c.reply_to == ask_seq))
}

/// `reply` (`activity.py:2007-2206`): the caller — the **recipient** of the
/// original ask, holding it in `own_log` — resolves the ask against their
/// own log (`find_open_ask`), writes the answer as an ordinary message
/// into the **original asker's** log so the asker finds it in their own
/// inbox, and then marks the ask closed in `own_log` so a second `reply`
/// or `close_ask` against the same `seq` is refused. No reply-exemption in
/// `may_send` — the reverse direction must itself be a permitted pair.
#[allow(clippy::too_many_arguments)]
pub fn reply(
    own_log: &ProjectLog,
    asker_log: &ProjectLog,
    ask_seq: u64,
    reply_id: &str,
    reply_body: &str,
    may_send_reverse: bool,
    redact: &dyn Fn(&str) -> String,
) -> Result<SendOutcome, ActivityError> {
    let ask = find_open_ask(own_log, ask_seq)?;
    debug_assert_eq!(ask.from_project, asker_log.project());
    let outcome = send_message(
        asker_log,
        reply_id,
        own_log.project(),
        asker_log.project(),
        reply_body,
        false,
        Some(ask_seq),
        may_send_reverse,
        redact,
    )?;
    close_ask(own_log, ask_seq, "answered", &no_op_redact)?;
    Ok(outcome)
}

/// `"answered"`/`"declined"`-style reasons [`reply`]/[`close_ask`] write
/// internally are not user-supplied text, so they skip the caller's
/// redactor rather than requiring one just for this literal.
fn no_op_redact(s: &str) -> String {
    s.to_string()
}

/// `close_ask`/`resolve_ask` (`activity.py:1181-1231`): write an
/// `event=close` into the **holder's own log**, never messaged elsewhere.
pub fn close_ask(
    holder_log: &ProjectLog,
    ask_seq: u64,
    reason: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<u64, ActivityError> {
    let redacted = redact(reason);
    let when = support::now_stamp();
    holder_log.with_lock(|| -> Result<u64, ActivityError> {
        let ask = find_ask(holder_log, ask_seq)?.ok_or(ActivityError::AskNotFound(ask_seq))?;
        if ask.to_project != holder_log.project() {
            return Err(ActivityError::AskNotHeldHere {
                project: holder_log.project().to_string(),
                seq: ask_seq,
                holder: ask.to_project,
            });
        }
        holder_log.append_versioned_locked(|seq| encode_close(&when, seq, ask_seq, &redacted))
    })
}

/// `is_closed`: whether `ask_seq` has been resolved — answered via
/// [`reply`] or explicitly declined via [`close_ask`] — from the
/// **holder's** own log.
pub fn is_closed(log: &ProjectLog, ask_seq: u64) -> Result<bool, ActivityError> {
    match find_open_ask(log, ask_seq) {
        Ok(_) => Ok(false),
        Err(ActivityError::AskNotFound(_)) => Ok(true),
        Err(e) => Err(e),
    }
}

/// `post_update` (`activity.py:1107-1161`, cooldown at `activity.py:240`):
/// appends `event=update` into `log` unless the same task posted a
/// non-`Wait` update inside [`UPDATE_COOLDOWN`]. `Wait` always posts. The
/// cooldown check and the append run under one `with_lock` acquisition —
/// reading the envelopes to check the cooldown before a *separate*
/// `append_versioned` call (which takes and releases the lock on its own)
/// let two concurrent callers both read "no recent update" and both post,
/// defeating the cooldown exactly the way an unlocked check always can
/// (`activity.py:2476-2501`'s one project lock covering check+allocate+
/// append).
pub fn post_update(
    log: &ProjectLog,
    task: &str,
    phase: UpdatePhase,
    note: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<Option<u64>, ActivityError> {
    validate_identifier("task id", task)?;
    let redacted = redact(note);
    log.with_lock(|| -> Result<Option<u64>, ActivityError> {
        let text = log.read_raw()?;
        if !card_event_present(&text, "dispatch", task) {
            return Err(ActivityError::UnknownCard {
                card: task.to_string(),
            });
        }
        if card_event_present(&text, "result", task) {
            return Err(ActivityError::TerminalCard {
                card: task.to_string(),
            });
        }
        let envelopes = log.envelopes()?;
        if phase != UpdatePhase::Wait {
            let last_for_task = envelopes
                .iter()
                .rfind(|e| e.event == "update" && e.field("task") == Some(task));
            if let Some(last) = last_for_task {
                if last.field("phase") != Some(UpdatePhase::Wait.as_str()) {
                    if let Some(last_when) = support::parse_flexible(&last.when) {
                        let now = time::OffsetDateTime::now_utc();
                        if now - last_when < UPDATE_COOLDOWN {
                            return Ok(None);
                        }
                    }
                }
            }
        }
        let when = support::now_stamp();
        let seq =
            log.append_versioned_locked(|seq| encode_update(&when, seq, task, phase, &redacted))?;
        Ok(Some(seq))
    })
}

/// `activity.cursor`: a bare nonnegative integer, newline-terminated,
/// tmp+rename, 0600 (`.multiverse/input/spec-on-disk-contract.md` §B).
/// Missing file reads as 0. Per the audit
/// (`.multiverse/input/audit-mail-backlog-board-disk.md` P1), reading must
/// fail closed — not just on malformed/negative text, but also when the
/// stored cursor is *ahead of* the log's current high-water `seq`, which
/// means the log was reset or replaced, not that the inbox is empty.
pub fn read_cursor(state_dir: &Path, log: &ProjectLog) -> Result<u64, ActivityError> {
    let path = state_dir.join("activity.cursor");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(ActivityError::from(e)),
    };
    let trimmed = text.trim();
    let cursor: u64 = trimmed.parse().map_err(|_| ActivityError::CursorCorrupt)?;
    let high_water = log.high_water_seq()?;
    if cursor > high_water {
        return Err(ActivityError::CursorAheadOfLog { cursor, high_water });
    }
    Ok(cursor)
}

/// `_write_cursor` (`activity.py:1375-1403`): monotonic — a write that
/// would move the cursor backward is a silent no-op, never a regression
/// (fixed in `df7ce05` per `spec-mail.md`'s regression list: a bare
/// `write_text` could be observed mid-truncate by an unlocked reader).
pub fn write_cursor(state_dir: &Path, new_value: u64) -> Result<(), ActivityError> {
    let path = state_dir.join("activity.cursor");
    let current = match fs::read_to_string(&path) {
        Ok(t) => t.trim().parse::<u64>().ok(),
        Err(_) => None,
    };
    if let Some(current) = current {
        if new_value <= current {
            return Ok(());
        }
    }
    support::atomic_write(&path, format!("{new_value}\n").as_bytes(), 0o600)?;
    Ok(())
}

/// `activity.lastseen` (`activity.py:1419-1493`): `<seq>\t<iso>\n`, written
/// on every `read_inbox` call, peek or not. Tolerant: corrupt or missing
/// folds to "never looked" rather than raising.
pub fn read_marker(state_dir: &Path) -> Option<(u64, String)> {
    let path = state_dir.join("activity.lastseen");
    let text = fs::read_to_string(path).ok()?;
    let mut parts = text.trim_end().splitn(2, '\t');
    let seq = parts.next()?.parse::<u64>().ok()?;
    let when = parts.next()?.to_string();
    Some((seq, when))
}

pub fn write_marker(state_dir: &Path, seq: u64) -> Result<(), ActivityError> {
    let path = state_dir.join("activity.lastseen");
    let when = support::now_stamp();
    support::atomic_write(&path, format!("{seq}\t{when}\n").as_bytes(), 0o600)?;
    Ok(())
}

/// `activity.lastack` (`activity.py:1511-1573`): `<count>\t<through_seq>\t
/// <iso>\n`, written only by `acknowledge`. Same tolerant-read posture as
/// the marker.
pub fn read_lastack(state_dir: &Path) -> Option<(u64, u64, String)> {
    let path = state_dir.join("activity.lastack");
    let text = fs::read_to_string(path).ok()?;
    let mut parts = text.trim_end().splitn(3, '\t');
    let count = parts.next()?.parse::<u64>().ok()?;
    let through_seq = parts.next()?.parse::<u64>().ok()?;
    let when = parts.next()?.to_string();
    Some((count, through_seq, when))
}

fn write_lastack(state_dir: &Path, count: u64, through_seq: u64) -> Result<(), ActivityError> {
    let path = state_dir.join("activity.lastack");
    let when = support::now_stamp();
    support::atomic_write(
        &path,
        format!("{count}\t{through_seq}\t{when}\n").as_bytes(),
        0o600,
    )?;
    Ok(())
}

/// One page of `read_inbox`/`peek` (`activity.py:2535-2780`).
#[derive(Debug, Clone, Default)]
pub struct InboxPage {
    pub messages: Vec<MessageRecord>,
    /// The highest `seq` among `messages` — what `acknowledge` would
    /// advance the cursor to, capped by `limit` the same way
    /// `ack_through_seq` is (a truncated view must not
    /// acknowledge more than it showed).
    pub through_seq: Option<u64>,
    /// The log's own high-water `seq` across every event kind, as of this
    /// read — what the CLI's "current through event N" line reports. Seq
    /// numbers are a per-project-log position, not a count of messages
    /// addressed to this project, so this can run ahead of `through_seq`
    /// once `update`/`close` records share the same log.
    pub current_seq: u64,
    /// `now_stamp()` taken for this read, paired with `current_seq` as the
    /// "as of" timestamp.
    pub current_at: String,
    /// `activity.lastseen` as written by this very call (seq, iso-when) —
    /// what the CLI's "last read/peeked at ..., through event N" line
    /// reports.
    pub marker_seq: u64,
    pub marker_at: String,
    /// Whether this call's `activity.lastseen` write actually succeeded.
    /// `false` means the write failed — a full disk, a permissions
    /// problem — and `marker_seq`/`marker_at` above are whatever the
    /// marker already held before this call, not this read's own
    /// high-water mark. The marker is diagnostic, so its failure must
    /// never turn an otherwise-successful inbox read into an error
    /// (`activity.py:2573-2581,2676-2703`'s `marker_saved` on `Inbox`).
    pub marker_saved: bool,
    /// How many unread messages existed beyond what `limit` let this read
    /// show — `Inbox.more_unread` (`activity.py:859-861`). 0 whenever
    /// nothing was truncated, including no `limit` ever being reached.
    pub more_unread: u64,
}

/// `read_inbox` (`activity.py:2535-2780`), default `limit = 20`
/// (`cli.py:1979-1989`). `activity.lastseen` is written on every call,
/// peek or not (`activity.py:1419-1493`'s own contract, matching this
/// file's module-level doc comment) — `replay_inbox` is the function that
/// touches neither cursor nor marker. This function never advances the
/// cursor or last-ack trail itself; that is the caller's job via
/// [`acknowledge`], which a peek is expected to skip (acking
/// must not silently consume more than the caller saw). `peek` is kept in
/// the signature for callers that branch on it, even though `read_inbox`'s
/// own disk effects no longer depend on it.
pub fn read_inbox(
    state_dir: &Path,
    log: &ProjectLog,
    limit: usize,
    _peek: bool,
) -> Result<InboxPage, ActivityError> {
    let cursor = read_cursor(state_dir, log)?;
    let current_seq = log.high_water_seq()?;
    let mut unread: Vec<MessageRecord> = log
        .inbox_messages()?
        .into_iter()
        .filter(|m| m.seq > cursor)
        .collect();
    unread.sort_by_key(|m| m.seq);
    let total_unread = unread.len();
    unread.truncate(limit);
    let more_unread = (total_unread - unread.len()) as u64;
    let through_seq = unread.last().map(|m| m.seq);

    // The marker records the log's own high-water mark, not just what this
    // (possibly `limit`-capped) read showed — `Inbox.high_water_seq` vs
    // `Inbox.ack_through_seq` (`activity.py:838-858`); using `through_seq`
    // here under-reported how current a peek actually was whenever `limit`
    // truncated the page or newer non-message records (update/close) had
    // landed since. Still written even when nothing was unread, peek or
    // not — that is what lets a project's own "never looked" state be told
    // apart from "peeked carefully, every time" (PR-E, module-level doc).
    //
    // A write failure here is diagnostic-only and must not fail the read
    // that already succeeded — `marker_saved` tells the caller it happened
    // without raising past the mail this call did manage to read.
    let marker_saved = write_marker(state_dir, current_seq).is_ok();
    let (marker_seq, marker_at) =
        read_marker(state_dir).unwrap_or_else(|| (current_seq, support::now_stamp()));

    Ok(InboxPage {
        messages: unread,
        through_seq,
        current_seq,
        current_at: support::now_stamp(),
        marker_seq,
        marker_at,
        more_unread,
        marker_saved,
    })
}

/// `acknowledge` (`activity.py:2535-2780`):
/// compares-and-advances against a freshly read cursor rather than writing
/// unconditionally, and records the last-ack trail. A no-op if
/// `through_seq` is not ahead of the current cursor. The whole
/// read/compare/write/write-lastack sequence runs under `log`'s own
/// `routing.log.lock` — two overlapping acknowledgements (e.g. targeting
/// 10 and 20) that each read the cursor unlocked could otherwise both see
/// 0 and race their renames, letting the smaller value land last and move
/// the cursor backward. Python holds the project lock across exactly this
/// sequence for the same reason (`activity.py:2759-2774`).
pub fn acknowledge(
    state_dir: &Path,
    log: &ProjectLog,
    through_seq: u64,
    shown_count: u64,
) -> Result<(), ActivityError> {
    log.with_lock(|| -> Result<(), ActivityError> {
        let current = read_cursor(state_dir, log)?;
        if through_seq <= current {
            return Ok(());
        }
        write_cursor(state_dir, through_seq)?;
        write_lastack(state_dir, shown_count, through_seq)?;
        Ok(())
    })
}

/// `replay_inbox` (`activity.py:3204-3320`):
/// `--replay-since` — every message with `seq > since`, up to `limit`,
/// never touching cursor or marker.
pub fn replay_inbox(
    log: &ProjectLog,
    since_seq: u64,
    limit: usize,
) -> Result<Vec<MessageRecord>, ActivityError> {
    let mut messages: Vec<MessageRecord> = log
        .inbox_messages()?
        .into_iter()
        .filter(|m| m.seq > since_seq)
        .collect();
    messages.sort_by_key(|m| m.seq);
    messages.truncate(limit);
    Ok(messages)
}

/// How far `project_root`'s read cursor trails the highest *message* the
/// last-seen marker could see, or [`None`] when there is nothing to flag.
///
/// Mirrors `activity.mail_cursor_behind` (`activity.py:2805-2845`), card
/// `20261021-fixer-23c807` cause (e): `read_inbox` writes the PR-E marker
/// on every call, peek or not; only [`acknowledge`] advances the durable
/// cursor. A project that only ever peeks can drift the two apart. The
/// marker's high-water sequence also includes bookkeeping records such
/// as `event=close`, so this compares against the highest *message* seq
/// at or below the marker rather than the marker itself — a project that
/// has acknowledged all mail and then looked again after an ask was
/// closed must not be flagged. A missing marker is "never looked"
/// (`None`). A corrupt or unreadable cursor, or an unreadable log, is
/// swallowed as `None` rather than raised a second time — the same
/// diagnostic posture as Python's `ActivityReadError` handling here.
pub fn mail_cursor_behind(project_root: &Path) -> Option<u64> {
    let state_dir = project_root.join(".multiverse");
    let (marker_seq, _) = read_marker(&state_dir)?;
    let log = ProjectLog::open("", project_root);
    let cursor = match read_cursor(&state_dir, &log) {
        Ok(cursor) => cursor,
        Err(_) => return None,
    };
    let envelopes = match log.envelopes() {
        Ok(envelopes) => envelopes,
        Err(_) => return None,
    };
    let seen_message_seq = envelopes
        .iter()
        .filter_map(|env| match classify_message(env) {
            Ok(Some(msg)) if msg.seq <= marker_seq => Some(msg.seq),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if seen_message_seq > cursor {
        Some(seen_message_seq - cursor)
    } else {
        None
    }
}

// ---------------------------------------------------------------------
// `find_unlinked_replies` (card `20261021-fixer-23c807`, ported for task
// `20261102-fixer-c628da` / card `20261102-b-0d8b17`): the reconciliation
// pass for an open ask a later, unlinked ordinary message to the same
// asker may have already answered in words. Mirrors `board.
// find_unlinked_replies`/`board._references_ask`. `mail_cursor_behind`
// (the same card's cause (e)) is [`mail_cursor_behind`] above.
// ---------------------------------------------------------------------

/// An open ask `holder` still shows as owed, that a later ordinary
/// message to the same asker — sent with no linked `re=` of its own —
/// already answered in words. Found by [`find_unlinked_replies`]; this
/// module never closes one itself (`close_ask` is a separate call), the
/// same read-only posture `board.find_unlinked_replies` takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnlinkedReply {
    pub ask: MessageRecord,
    pub answer: MessageRecord,
    /// Whether `answer` carries concrete evidence it addresses `ask` (an
    /// explicit `#N` reference, an exact phrase match, or strong word
    /// overlap) — direction and timing alone are only a *possible*
    /// answer, never enough to close the ask automatically.
    pub references_ask: bool,
}

/// Every occurrence of `#<digits>` in `body` where neither the `#` nor the
/// digits run touches an alphanumeric neighbour — Rust's `regex` crate has
/// no lookaround, so this walks char boundaries by hand instead. Mirrors
/// `board._ASK_REFERENCE_RE` (`(?<![A-Za-z0-9])#\s*(\d+)(?![A-Za-z0-9])`).
fn ask_number_references(body: &str) -> Vec<u64> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    for i in 0..chars.len() {
        if chars[i] != '#' {
            continue;
        }
        if i > 0 && chars[i - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut j = i + 1;
        while j < chars.len() && chars[j].is_whitespace() {
            j += 1;
        }
        let digits_start = j;
        while j < chars.len() && chars[j].is_ascii_digit() {
            j += 1;
        }
        if j == digits_start {
            continue;
        }
        if j < chars.len() && chars[j].is_ascii_alphanumeric() {
            continue;
        }
        if let Ok(n) = chars[digits_start..j]
            .iter()
            .collect::<String>()
            .parse::<u64>()
        {
            out.push(n);
        }
    }
    out
}

/// `board._ASK_STOPWORDS`, verbatim.
const ASK_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "can", "do", "for", "from", "how", "i",
    "if", "in", "is", "it", "me", "my", "need", "of", "on", "or", "our", "please", "should",
    "that", "the", "this", "to", "us", "we", "what", "when", "which", "who", "why", "will",
    "would", "you", "your",
];

/// Lowercased `[a-z0-9]+` runs in `text`, minus [`ASK_STOPWORDS`]. Mirrors
/// `board._references_ask`'s own `re.findall(r"[a-z0-9]+", ...)` word
/// extraction.
fn content_words(text: &str) -> std::collections::HashSet<String> {
    let mut words = std::collections::HashSet::new();
    let mut current = String::new();
    for ch in text.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            current.push(ch);
        } else if !current.is_empty() {
            words.insert(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.insert(current);
    }
    words.retain(|w| !ASK_STOPWORDS.contains(&w.as_str()));
    words
}

/// Whether `answer` contains concrete evidence it addresses `ask` — an
/// explicit `#N` reference, an exact (whitespace-normalized) phrase match,
/// or strong word overlap (at least three shared content words, or a
/// Jaccard similarity of at least one half). Mirrors `board._references_ask`
/// (card `20261022-b-e398a6`'s stronger-than-one-word threshold).
pub fn references_ask(ask: &MessageRecord, answer: &MessageRecord) -> bool {
    let body = answer.body.to_lowercase();
    if ask_number_references(&body).contains(&ask.seq) {
        return true;
    }
    let ask_text = ask
        .body
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !ask_text.is_empty() && body.contains(&ask_text) {
        return true;
    }
    let words = content_words(&ask.body);
    if words.is_empty() {
        return false;
    }
    let answer_words = content_words(&answer.body);
    let shared = words.intersection(&answer_words).count();
    if shared >= 3 {
        return true;
    }
    let union = words.union(&answer_words).count();
    union > 0 && (shared as f64 / union as f64) >= 0.5
}

/// Pre-parsed inboxes and close-sets for every configured project, so
/// one `sweep` pass can answer [`find_unlinked_replies`] for each holder
/// without re-reading every `routing.log` once per ask (and once per
/// holder × peer). Equivalent to the per-call scans: this type is
/// read-only, and [`find_unlinked_replies`] writes nothing.
pub struct MailIndex {
    inboxes: HashMap<String, Vec<MessageRecord>>,
    closed: HashMap<String, HashSet<u64>>,
}

impl MailIndex {
    /// Load every project's inbox and close-set. An unreadable log, or a
    /// damaged `event=message`, fails the whole index, matching
    /// [`find_unlinked_replies`]'s own "any peer unreadable → Err" posture.
    /// A damaged `event=close` does not (see `ProjectLog::inbox_and_closes`'s
    /// own doc comment) — that record class is simply skipped, project by
    /// project, the same as a direct call would tolerate.
    pub fn load(all_projects: &[(String, PathBuf)]) -> Result<Self, ActivityError> {
        let mut inboxes = HashMap::new();
        let mut closed = HashMap::new();
        for (name, root) in all_projects {
            let log = ProjectLog::open(name.clone(), root);
            let (inbox, closes) = log.inbox_and_closes()?;
            inboxes.insert(name.clone(), inbox);
            closed.insert(name.clone(), closes);
        }
        Ok(Self { inboxes, closed })
    }
}

/// Every open ask `holder_name` holds (at `holder_root`) that a later,
/// unlinked ordinary message to the asker already answered — the gap
/// `open_ask_messages`'s own `replied` scan cannot close, because that
/// scan only recognises a message carrying a linked `re=<ask.seq>`.
/// Mirrors `board.find_unlinked_replies`.
///
/// `all_projects` is every configured project's `(name, root)` — wider
/// than just `holder_name` — because a reply lands in the *asker's* own
/// log, read from `all_projects` below exactly the same way
/// `open_ask_messages`'s own `replied` scan does (its own doc comment:
/// scoping this to a narrower display set once lost the only evidence of
/// an answer, card `20261015-fixer-69e5fc`).
///
/// An "unlinked reply" is the earliest ordinary message (`ask=false`,
/// no linked `re=`) `holder_name` sent to the ask's own sender, timestamped
/// no earlier than the ask itself, read from the asker's own log. The
/// result marks whether that message references the ask; direction and
/// timing alone are only a possible answer, never enough to close it
/// automatically — that decision (and any write) is left to the caller
/// ([`close_ask`]), the same read-only posture `board.find_unlinked_replies`
/// takes.
pub fn find_unlinked_replies(
    holder_name: &str,
    holder_root: &Path,
    all_projects: &[(String, PathBuf)],
) -> Result<Vec<UnlinkedReply>, ActivityError> {
    let mut projects: Vec<(String, PathBuf)> = all_projects.to_vec();
    if !projects.iter().any(|(name, _)| name == holder_name) {
        projects.push((holder_name.to_string(), holder_root.to_path_buf()));
    }
    let index = MailIndex::load(&projects)?;
    Ok(find_unlinked_replies_indexed(holder_name, &index))
}

/// [`find_unlinked_replies`] against an already-loaded [`MailIndex`].
/// Sweep loads the index once per pass so eight holders do not re-parse
/// the same eight logs.
pub fn find_unlinked_replies_indexed(holder_name: &str, index: &MailIndex) -> Vec<UnlinkedReply> {
    let mut replied: HashSet<(String, String, u64)> = HashSet::new();
    for (name, messages) in &index.inboxes {
        for message in messages {
            if let Some(re_seq) = message.reply_to {
                replied.insert((message.from_project.clone(), name.clone(), re_seq));
            }
        }
    }

    let empty_inbox: Vec<MessageRecord> = Vec::new();
    let empty_closed: HashSet<u64> = HashSet::new();
    let holder_inbox = index.inboxes.get(holder_name).unwrap_or(&empty_inbox);
    let holder_closed = index.closed.get(holder_name).unwrap_or(&empty_closed);

    let mut open_asks = Vec::new();
    for message in holder_inbox {
        if !message.ask {
            continue;
        }
        if holder_closed.contains(&message.seq) {
            continue;
        }
        if replied.contains(&(
            holder_name.to_string(),
            message.from_project.clone(),
            message.seq,
        )) {
            continue;
        }
        open_asks.push(message);
    }

    let mut found = Vec::new();
    for ask in open_asks {
        let Some(asker_inbox) = index.inboxes.get(ask.from_project.as_str()) else {
            continue;
        };
        let mut candidates: Vec<&MessageRecord> = asker_inbox
            .iter()
            .filter(|m| {
                m.from_project == holder_name
                    && m.to_project == ask.from_project
                    && !m.ask
                    && m.reply_to.is_none()
                    && m.when >= ask.when
            })
            .collect();
        if candidates.is_empty() {
            continue;
        }
        candidates.sort_by(|a, b| (a.when.as_str(), a.seq).cmp(&(b.when.as_str(), b.seq)));
        let answer = candidates
            .iter()
            .find(|m| references_ask(ask, m))
            .copied()
            .unwrap_or(candidates[0]);
        let references = references_ask(ask, answer);
        found.push(UnlinkedReply {
            ask: ask.clone(),
            answer: answer.clone(),
            references_ask: references,
        });
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn log_in(dir: &Path, project: &str) -> ProjectLog {
        ProjectLog::open(project, dir)
    }

    fn no_redact(s: &str) -> String {
        s.to_string()
    }

    fn seed_dispatch(log: &ProjectLog, task: &str) {
        let when = support::now_stamp();
        log.with_lock(|| {
            log.append_versioned_locked(|seq| {
                encode_v1(&when, seq, "dispatch", &[("task", task.to_string())])
            })
        })
        .unwrap();
    }

    fn seed_result(log: &ProjectLog, task: &str) {
        let when = support::now_stamp();
        log.with_lock(|| {
            log.append_versioned_locked(|seq| {
                encode_v1(&when, seq, "result", &[("task", task.to_string())])
            })
        })
        .unwrap();
    }

    #[test]
    fn activity_lock_reenters_the_shared_project_lock() {
        let dir = tempfile::tempdir().unwrap();
        let log = ProjectLog::open("demo", dir.path());
        let _held = mv_core::lock::acquire_default(&log.lock_path, "test").unwrap();
        log.with_lock(|| -> io::Result<()> { Ok(()) }).unwrap();
    }

    #[test]
    fn identifier_rules_match_the_shared_regex() {
        assert!(support::is_valid_identifier("20000101-fixer-000001"));
        assert!(support::is_valid_identifier("20000101-explorer-000001"));
        assert!(!support::is_valid_identifier("../escape"));
        assert!(!support::is_valid_identifier(""));
    }

    #[test]
    fn json_quote_ascii_matches_ensure_ascii_true() {
        assert_eq!(support::json_quote_ascii("hello world"), "\"hello world\"");
        assert_eq!(
            support::json_quote_ascii("a\tb\nc\"d"),
            "\"a\\tb\\nc\\\"d\""
        );
        // 'é' is U+00E9 -> \u00e9 under ensure_ascii=True.
        assert_eq!(support::json_quote_ascii("é"), "\"\\u00e9\"");
    }

    #[test]
    fn crc32_matches_the_golden_routing_log_line_from_mv_core_partial_patch() {
        let line = encode_message(
            "2026-11-02T12:00:00+00:00",
            42,
            "20000101-fixer-000001",
            "multiverse",
            "example",
            "hello world",
            false,
            None,
        );
        let prefix = "2026-11-02T12:00:00+00:00\tv=1\tseq=42\tevent=message\t\
                       id=20000101-fixer-000001\tfrom=multiverse\tto=example\t\
                       body=\"hello world\"";
        assert_eq!(support::crc32_hex(prefix.as_bytes()), "00fbbb8f");
        assert_eq!(line, format!("{prefix}\tcrc32=00fbbb8f"));
    }

    #[test]
    fn a_damaged_crc_is_reported_not_silently_dropped() {
        let line = "2026-11-02T12:00:00+00:00\tv=1\tseq=1\tevent=message\tid=a\tfrom=b\tto=c\tbody=\"x\"\tcrc32=00000000";
        let err = decode_v1(line).unwrap_err();
        assert!(matches!(err, ActivityError::DamagedRecord(_)));
    }

    #[test]
    fn send_then_read_inbox_round_trips_the_message() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let recipient = log_in(dir.path(), "example");

        let outcome = send_message(
            &recipient,
            "20000101-fixer-000001",
            "multiverse",
            "example",
            "hello world",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        assert_eq!(outcome, SendOutcome::Sent { seq: 1 });

        let page = read_inbox(&state_dir, &recipient, 20, true).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].body, "hello world");
        assert_eq!(page.through_seq, Some(1));

        // Peek must not touch the cursor.
        assert_eq!(read_cursor(&state_dir, &recipient).unwrap(), 0);
    }

    #[test]
    fn high_water_seq_reserves_a_damaged_records_seq_so_a_new_append_cannot_reuse_it() {
        let dir = tempdir().unwrap();
        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        // Hand-append a line with a bad crc32 (so `envelopes`/`decode_v1`
        // skip it as damaged) but a readable `seq=100` — the real failure
        // mode this guards: a checksum-damaged record whose seq must still
        // never be handed out again.
        let damaged = "2026-11-02T12:00:00+00:00\tv=1\tseq=100\tevent=message\tid=z\t\
                        from=multiverse\tto=example\tbody=\"x\"\tcrc32=00000000";
        {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(log.log_path())
                .unwrap();
            writeln!(file, "{damaged}").unwrap();
        }

        assert_eq!(log.high_water_seq().unwrap(), 100);
        let outcome = send_message(
            &log,
            "b",
            "multiverse",
            "example",
            "two",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        assert_eq!(
            outcome,
            SendOutcome::Sent { seq: 101 },
            "a new append must not reuse 100, or anything the damaged record already claimed"
        );
    }

    /// `critic-engine-state.md` finding 4: a log whose highest `seq` is
    /// already `u64::MAX` must refuse the next append with a typed error
    /// instead of panicking (debug) or wrapping to `seq=0` (release) and
    /// reusing a sequence number.
    #[test]
    fn appending_past_the_max_sequence_is_refused_not_wrapped() {
        let dir = tempdir().unwrap();
        let log = log_in(dir.path(), "example");
        let maxed = format!(
            "2026-11-02T12:00:00+00:00\tv=1\tseq={}\tevent=message\tid=z\t\
             from=multiverse\tto=example\tbody=\"x\"\tcrc32=00000000",
            u64::MAX
        );
        fs::create_dir_all(log.log_path().parent().unwrap()).unwrap();
        {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(log.log_path())
                .unwrap();
            writeln!(file, "{maxed}").unwrap();
        }
        assert_eq!(log.high_water_seq().unwrap(), u64::MAX);

        let err = send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(
            matches!(err, ActivityError::SequenceExhausted),
            "expected SequenceExhausted, got {err:?}"
        );
    }

    #[test]
    fn a_limit_truncated_read_reports_more_unread_and_the_logs_true_high_water() {
        // Regression for the real-fleet rehearsal gap (card 20261102-b-7daa40):
        // `read_inbox` used to write the last-seen marker from the capped
        // `through_seq` (the last *shown* message's seq) instead of the
        // log's own high-water mark, so a limit-truncated or peeked read
        // under-reported "through event N" — e.g. 4863 vs Python's 4877 on
        // the real Multiverse routing log — and never surfaced
        // `more_unread` at all, so `activity inbox` could never print
        // Python's "N more unread, run inbox again" line.
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let recipient = log_in(dir.path(), "example");
        for (id, body) in [("a", "one"), ("b", "two"), ("c", "three")] {
            send_message(
                &recipient,
                id,
                "multiverse",
                "example",
                body,
                false,
                None,
                true,
                &no_redact,
            )
            .unwrap();
        }
        // A non-message v=1 record (seq 4) past the last message: the
        // log's high-water mark must still include it even though
        // `inbox_messages` never will.
        seed_dispatch(&recipient, "task-1");

        let page = read_inbox(&state_dir, &recipient, 2, true).unwrap();
        assert_eq!(page.messages.len(), 2);
        assert_eq!(page.through_seq, Some(2), "last shown message's own seq");
        assert_eq!(page.more_unread, 1, "one message left unshown by the limit");
        assert_eq!(
            page.current_seq, 4,
            "log high-water across every event kind"
        );
        assert_eq!(
            page.marker_seq, 4,
            "the marker must record the log's high-water, not the capped through_seq"
        );
    }

    #[test]
    fn sending_the_same_id_twice_is_idempotent() {
        let dir = tempdir().unwrap();
        let recipient = log_in(dir.path(), "example");
        let first = send_message(
            &recipient,
            "dup-1",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        let second = send_message(
            &recipient,
            "dup-1",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        assert_eq!(first, SendOutcome::Sent { seq: 1 });
        assert_eq!(second, SendOutcome::AlreadySent);
        assert_eq!(recipient.inbox_messages().unwrap().len(), 1);
    }

    #[test]
    fn send_rejects_an_invalid_identifier() {
        let dir = tempdir().unwrap();
        let recipient = log_in(dir.path(), "example");
        let err = send_message(
            &recipient,
            "../escape",
            "multiverse",
            "example",
            "x",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, ActivityError::InvalidIdentifier { .. }));
    }

    #[test]
    fn send_rejects_an_oversized_body() {
        let dir = tempdir().unwrap();
        let recipient = log_in(dir.path(), "example");
        let body = "x".repeat(MAX_BODY_BYTES + 1);
        let err = send_message(
            &recipient,
            "id-1",
            "multiverse",
            "example",
            &body,
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, ActivityError::BodyTooLarge { .. }));
    }

    #[test]
    fn send_is_refused_when_may_send_is_false() {
        let dir = tempdir().unwrap();
        let recipient = log_in(dir.path(), "example");
        let err = send_message(
            &recipient, "id-1", "a", "example", "x", false, None, false, &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, ActivityError::NotPermitted { .. }));
        assert_eq!(recipient.inbox_messages().unwrap().len(), 0);
    }

    #[test]
    fn acknowledge_advances_cursor_and_writes_lastack() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let recipient = log_in(dir.path(), "example");
        send_message(
            &recipient,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        send_message(
            &recipient,
            "b",
            "multiverse",
            "example",
            "two",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let page = read_inbox(&state_dir, &recipient, 20, false).unwrap();
        assert_eq!(page.through_seq, Some(2));
        acknowledge(
            &state_dir,
            &recipient,
            page.through_seq.unwrap(),
            page.messages.len() as u64,
        )
        .unwrap();

        assert_eq!(read_cursor(&state_dir, &recipient).unwrap(), 2);
        let (count, through, _) = read_lastack(&state_dir).unwrap();
        assert_eq!((count, through), (2, 2));

        // A page with nothing new after acking shows an empty inbox.
        let page2 = read_inbox(&state_dir, &recipient, 20, false).unwrap();
        assert!(page2.messages.is_empty());
    }

    #[test]
    fn concurrent_acknowledgements_never_move_the_cursor_backward() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let state_dir = project_root.join(".multiverse");
        let log = log_in(&project_root, "example");
        for (id, body) in [
            ("a", "one"),
            ("b", "two"),
            ("c", "three"),
            ("d", "four"),
            ("e", "five"),
        ] {
            send_message(
                &log,
                id,
                "multiverse",
                "example",
                body,
                false,
                None,
                true,
                &no_redact,
            )
            .unwrap();
        }

        // Several threads race to acknowledge out of order, each opening
        // its own `ProjectLog`/lock-file handle the way independent
        // callers would — before the fix, two overlapping acknowledgements
        // could both read the cursor as 0 and let the smaller target land
        // last, regressing it.
        let handles: Vec<_> = [2u64, 5, 1, 4, 3]
            .into_iter()
            .map(|target| {
                let root = project_root.clone();
                std::thread::spawn(move || {
                    let log = log_in(&root, "example");
                    let state_dir = root.join(".multiverse");
                    acknowledge(&state_dir, &log, target, target).unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            read_cursor(&state_dir, &log).unwrap(),
            5,
            "the highest acknowledged value must win regardless of thread interleaving"
        );
    }

    #[test]
    #[cfg(unix)]
    fn an_activity_write_corrects_an_existing_loosely_permissioned_state_dir_to_0700() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        // Simulate the directory already existing at the process umask's
        // mode (e.g. 0755) — the exact state the old unconditional-create
        // path left behind once anything had already created it.
        fs::create_dir_all(&state_dir).unwrap();
        fs::set_permissions(&state_dir, fs::Permissions::from_mode(0o755)).unwrap();

        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let mode = fs::metadata(&state_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "an activity write must correct .multiverse/ to 0700, not just set it on first creation"
        );
    }

    #[test]
    fn concurrent_post_updates_for_the_same_task_cannot_both_bypass_the_cooldown() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().to_path_buf();
        let log = log_in(&project_root, "example");
        seed_dispatch(&log, "task-1");
        post_update(&log, "task-1", UpdatePhase::Working, "first", &no_redact)
            .unwrap()
            .expect("the seed update always posts");

        // Three callers race to post for the same task inside the cooldown
        // window. Before the fix, each one read the envelopes (to check
        // the cooldown) outside any lock, so all three could see only the
        // seed update and all decide to post — duplicating cooldown-
        // suppressed telemetry even though the appends themselves were
        // individually serialized.
        let handles: Vec<_> = ["second", "third", "fourth"]
            .into_iter()
            .map(|note| {
                let root = project_root.clone();
                std::thread::spawn(move || {
                    let log = log_in(&root, "example");
                    post_update(&log, "task-1", UpdatePhase::Working, note, &no_redact).unwrap()
                })
            })
            .collect();
        let results: Vec<Option<u64>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(
            results.iter().all(Option::is_none),
            "every concurrent call inside the cooldown must be suppressed: {results:?}"
        );

        let update_count = log
            .envelopes()
            .unwrap()
            .into_iter()
            .filter(|e| e.event == "update")
            .count();
        assert_eq!(
            update_count, 1,
            "only the seed update may exist; a race would duplicate it"
        );
    }

    #[test]
    fn a_failed_lastseen_write_does_not_fail_the_inbox_read() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        // Force the marker write to fail: `atomic_write`'s rename-into-
        // place cannot replace a directory with a file.
        fs::create_dir_all(state_dir.join("activity.lastseen")).unwrap();

        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let page = read_inbox(&state_dir, &log, 20, true).unwrap();
        assert_eq!(page.messages.len(), 1, "the read itself must still succeed");
        assert!(
            !page.marker_saved,
            "the marker write failure must be reported, not swallowed silently"
        );
    }

    #[test]
    fn cursor_write_never_regresses() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        write_cursor(&state_dir, 5).unwrap();
        write_cursor(&state_dir, 3).unwrap();
        let text = fs::read_to_string(state_dir.join("activity.cursor")).unwrap();
        assert_eq!(text.trim(), "5");
    }

    #[test]
    fn a_malformed_cursor_is_reported_not_silently_reset() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("activity.cursor"), b"not-a-number\n").unwrap();
        let log = log_in(dir.path(), "example");
        let err = read_cursor(&state_dir, &log).unwrap_err();
        assert!(matches!(err, ActivityError::CursorCorrupt));
    }

    #[test]
    fn a_cursor_ahead_of_the_log_high_water_fails_closed() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("activity.cursor"), b"99\n").unwrap();
        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        let err = read_cursor(&state_dir, &log).unwrap_err();
        assert!(matches!(
            err,
            ActivityError::CursorAheadOfLog {
                cursor: 99,
                high_water: 1
            }
        ));
    }

    #[test]
    fn missing_cursor_reads_as_zero() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        assert_eq!(read_cursor(&state_dir, &log).unwrap(), 0);
    }

    #[test]
    fn a_corrupt_marker_still_lets_inbox_deliver_mail() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(
            state_dir.join("activity.lastseen"),
            b"garbage, not tab separated",
        )
        .unwrap();
        assert_eq!(read_marker(&state_dir), None);

        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        let page = read_inbox(&state_dir, &log, 20, false).unwrap();
        assert_eq!(page.messages.len(), 1);
    }

    #[test]
    fn a_corrupt_lastack_still_reads_as_tolerant_none() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        fs::create_dir_all(&state_dir).unwrap();
        fs::write(state_dir.join("activity.lastack"), b"not\tenough").unwrap();
        assert_eq!(read_lastack(&state_dir), None);
    }

    #[test]
    fn broadcast_is_refused_from_a_non_hub_project() {
        let dir = tempdir().unwrap();
        let recipient = log_in(dir.path(), "example");
        let err = broadcast(
            "not-the-hub",
            "announcement",
            &[(recipient, true)],
            "bcast-1",
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, ActivityError::NotHub(_)));
    }

    #[test]
    fn broadcast_from_the_hub_sends_to_every_permitted_recipient_and_skips_the_rest() {
        let dir_a = tempdir().unwrap();
        let dir_b = tempdir().unwrap();
        let a = log_in(dir_a.path(), "project-a");
        let b = log_in(dir_b.path(), "project-b");

        let report = broadcast(
            HUB_PROJECT_NAME,
            "fleet announcement",
            &[(a, true), (b, false)],
            "bcast-1",
            &no_redact,
        )
        .unwrap();

        let sent: Vec<&str> = report.sent().collect();
        assert_eq!(sent, vec!["project-a"]);
        let skipped: Vec<(&str, &str)> = report.skipped().collect();
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "project-b");
    }

    #[test]
    fn reply_resolves_the_ask_and_lands_in_the_askers_log() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        // The ask is a message delivered into the holder's log (the
        // recipient reads their own inbox from their own log).
        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "will you do X?",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let outcome = reply(
            &holder_log,
            &asker_log,
            1,
            "reply-1",
            "yes",
            true,
            &no_redact,
        )
        .unwrap();
        assert_eq!(outcome, SendOutcome::Sent { seq: 1 });

        let asker_inbox = asker_log.inbox_messages().unwrap();
        assert_eq!(asker_inbox.len(), 1);
        assert_eq!(asker_inbox[0].body, "yes");
        assert_eq!(asker_inbox[0].reply_to, Some(1));

        assert!(is_closed(&holder_log, 1).unwrap());
    }

    #[test]
    fn close_ask_closes_without_a_reply_and_is_scoped_to_the_holders_own_log() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "will you do X?",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        assert!(!is_closed(&holder_log, 1).unwrap());

        close_ask(&holder_log, 1, "declined", &no_redact).unwrap();
        assert!(is_closed(&holder_log, 1).unwrap());

        // The close record never touched the asker's log.
        assert!(asker_log.inbox_messages().unwrap().is_empty());
    }

    #[test]
    fn replay_inbox_never_touches_cursor_or_marker() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let replayed = replay_inbox(&log, 0, 20).unwrap();
        assert_eq!(replayed.len(), 1);
        assert_eq!(read_cursor(&state_dir, &log).unwrap(), 0);
        assert_eq!(read_marker(&state_dir), None);
    }

    #[test]
    fn post_update_suppresses_a_repeat_within_the_cooldown_except_wait() {
        let dir = tempdir().unwrap();
        let log = log_in(dir.path(), "example");
        seed_dispatch(&log, "task-1");
        let first =
            post_update(&log, "task-1", UpdatePhase::Working, "note one", &no_redact).unwrap();
        assert!(first.is_some());
        let second =
            post_update(&log, "task-1", UpdatePhase::Working, "note two", &no_redact).unwrap();
        assert_eq!(
            second, None,
            "repeat inside the cooldown window must be suppressed"
        );
        let waited = post_update(&log, "task-1", UpdatePhase::Wait, "waiting", &no_redact).unwrap();
        assert!(waited.is_some(), "phase=wait always posts");
    }

    #[test]
    fn post_update_refuses_unknown_and_terminal_cards_before_write() {
        let dir = tempdir().unwrap();
        let log = log_in(dir.path(), "example");
        let unknown =
            post_update(&log, "task-1", UpdatePhase::Working, "n", &no_redact).unwrap_err();
        assert!(matches!(unknown, ActivityError::UnknownCard { .. }));
        assert!(!log.log_path().exists());

        seed_dispatch(&log, "task-1");
        seed_result(&log, "task-1");
        let terminal =
            post_update(&log, "task-1", UpdatePhase::Working, "n", &no_redact).unwrap_err();
        assert!(matches!(terminal, ActivityError::TerminalCard { .. }));
        let text = fs::read_to_string(log.log_path()).unwrap();
        assert!(!text.contains("event=update"));
    }

    #[test]
    fn send_message_creates_the_recipient_state_dir_when_missing() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("ghost");
        assert!(!missing.exists());
        let log = ProjectLog::open("ghost", &missing);
        send_message(
            &log,
            "id-1",
            "multiverse",
            "ghost",
            "hello",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        assert!(missing.join(".multiverse/routing.log").exists());
    }

    #[test]
    fn parse_ask_ref_accepts_a_bare_seq_or_project_hash_seq() {
        assert_eq!(parse_ask_ref("7").unwrap(), (None, 7));
        assert_eq!(
            parse_ask_ref("project-a#12").unwrap(),
            (Some("project-a".into()), 12)
        );
        assert!(matches!(
            parse_ask_ref("not-a-ref").unwrap_err(),
            ActivityError::InvalidAskRef { .. }
        ));
    }

    #[test]
    fn broadcast_refuses_before_any_write_when_a_permitted_directory_is_missing() {
        let present = tempdir().unwrap();
        let missing = std::env::temp_dir().join(format!(
            "mv-state-missing-broadcast-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let a = log_in(present.path(), "project-a");
        let ghost = ProjectLog::open("ghost", &missing);
        let err = broadcast(
            HUB_PROJECT_NAME,
            "hello",
            &[(a, true), (ghost, true)],
            "shared-1",
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ActivityError::RecipientDirectoryMissing { .. }
        ));
        assert!(!present.path().join(".multiverse/routing.log").exists());
        assert!(!missing.exists());
    }

    #[test]
    fn broadcast_partial_io_failure_names_the_shared_id() {
        let a_dir = tempdir().unwrap();
        let b_dir = tempdir().unwrap();
        let a = log_in(a_dir.path(), "project-a");
        let b_path = b_dir.path().join("project-b");
        fs::write(&b_path, b"not-a-directory").unwrap();
        let b = ProjectLog::open("project-b", &b_path);
        let err = broadcast(
            HUB_PROJECT_NAME,
            "hello",
            &[(a, true), (b, true)],
            "shared-fanout",
            &no_redact,
        )
        .unwrap_err();
        match err {
            ActivityError::FanOutPartial {
                msg_id,
                reached,
                lost,
            } => {
                assert_eq!(msg_id, "shared-fanout");
                assert!(reached.contains("project-a"), "{reached}");
                assert!(lost.contains("project-b"), "{lost}");
            }
            other => panic!("expected FanOutPartial, got {other:?}"),
        }
    }

    #[test]
    fn lock_order_puts_the_hub_first_then_sorts_the_rest() {
        let order = support::lock_order(
            HUB_PROJECT_NAME,
            vec!["zeta".into(), "multiverse".into(), "alpha".into()],
        );
        assert_eq!(order, vec!["multiverse", "alpha", "zeta"]);
    }

    // -- `find_unlinked_replies` (card `20261102-b-0d8b17`) ---------------

    fn all_projects(dirs: &[(&str, &Path)]) -> Vec<(String, PathBuf)> {
        dirs.iter()
            .map(|(name, path)| (name.to_string(), path.to_path_buf()))
            .collect()
    }

    #[test]
    fn find_unlinked_replies_matches_an_explicit_hash_reference() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "please rotate the staging creds before 9am",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        send_message(
            &asker_log,
            "reply-1",
            "holder",
            "asker",
            "done, see #1 - rotated already",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let projects = all_projects(&[("asker", asker_dir.path()), ("holder", holder_dir.path())]);
        let found = find_unlinked_replies("holder", holder_dir.path(), &projects).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ask.seq, 1);
        assert_eq!(found[0].answer.body, "done, see #1 - rotated already");
        assert!(found[0].references_ask);
    }

    #[test]
    fn find_unlinked_replies_matches_strong_word_overlap_without_an_explicit_reference() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "please review pull request 42 before merging",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        send_message(
            &asker_log,
            "reply-1",
            "holder",
            "asker",
            "reviewed pull request 42 and merged it",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let projects = all_projects(&[("asker", asker_dir.path()), ("holder", holder_dir.path())]);
        let found = find_unlinked_replies("holder", holder_dir.path(), &projects).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            found[0].references_ask,
            "three shared content words (pull/request/42) must count as strong evidence"
        );
    }

    #[test]
    fn find_unlinked_replies_reports_a_possible_answer_without_strong_evidence() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "can we sync at 3pm today",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        send_message(
            &asker_log,
            "reply-1",
            "holder",
            "asker",
            "sounds good, see you then",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let projects = all_projects(&[("asker", asker_dir.path()), ("holder", holder_dir.path())]);
        let found = find_unlinked_replies("holder", holder_dir.path(), &projects).unwrap();
        assert_eq!(found.len(), 1);
        assert!(
            !found[0].references_ask,
            "direction and timing alone are only a possible answer"
        );
    }

    #[test]
    fn find_unlinked_replies_ignores_an_ask_already_answered_via_a_linked_reply() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "will you deploy the staging fix today?",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        // A properly linked reply (`re=1`) already answers it — this must
        // never show up as an *unlinked* reply too.
        reply(
            &holder_log,
            &asker_log,
            1,
            "reply-1",
            "yes, deployed",
            true,
            &no_redact,
        )
        .unwrap();

        let projects = all_projects(&[("asker", asker_dir.path()), ("holder", holder_dir.path())]);
        let found = find_unlinked_replies("holder", holder_dir.path(), &projects).unwrap();
        assert!(
            found.is_empty(),
            "a linked reply already resolves the ask; it is no longer open"
        );
    }

    #[test]
    fn find_unlinked_replies_ignores_an_already_closed_ask() {
        let asker_dir = tempdir().unwrap();
        let holder_dir = tempdir().unwrap();
        let asker_log = log_in(asker_dir.path(), "asker");
        let holder_log = log_in(holder_dir.path(), "holder");

        send_message(
            &holder_log,
            "ask-1",
            "asker",
            "holder",
            "will you deploy the staging fix today?",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        close_ask(&holder_log, 1, "declined", &no_redact).unwrap();
        send_message(
            &asker_log,
            "reply-1",
            "holder",
            "asker",
            "deployed the staging fix just now",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();

        let projects = all_projects(&[("asker", asker_dir.path()), ("holder", holder_dir.path())]);
        let found = find_unlinked_replies("holder", holder_dir.path(), &projects).unwrap();
        assert!(found.is_empty(), "a closed ask is no longer open");
    }

    // -- `mail_cursor_behind` (card `20261021-fixer-23c807` cause (e)) ----

    #[test]
    fn mail_cursor_behind_is_none_before_anything_has_ever_looked() {
        let dir = tempdir().unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), None);
    }

    #[test]
    fn mail_cursor_behind_is_none_after_a_plain_read_is_acknowledged() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        send_message(
            &log,
            "a",
            "multiverse",
            "example",
            "one",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        let page = read_inbox(&state_dir, &log, 20, false).unwrap();
        acknowledge(
            &state_dir,
            &log,
            page.through_seq.unwrap(),
            page.messages.len() as u64,
        )
        .unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), None);
    }

    #[test]
    fn mail_cursor_behind_ignores_a_close_record_after_all_mail_is_acknowledged() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        let ask = send_message(
            &log,
            "ask-1",
            "multiverse",
            "example",
            "need the number",
            true,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        let SendOutcome::Sent { seq } = ask else {
            panic!("expected the ask to be sent");
        };
        let page = read_inbox(&state_dir, &log, 20, false).unwrap();
        acknowledge(
            &state_dir,
            &log,
            page.through_seq.unwrap(),
            page.messages.len() as u64,
        )
        .unwrap();
        close_ask(&log, seq, "answered", &no_redact).unwrap();
        // The close is a valid routing-log event but not mail. A later peek
        // must not make it look as though an acknowledged message is behind
        // the cursor.
        read_inbox(&state_dir, &log, 20, true).unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), None);
    }

    #[test]
    fn mail_cursor_behind_reports_the_gap_after_a_peek_with_no_acknowledge() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        for (id, body) in [("a", "one"), ("b", "two"), ("c", "three")] {
            send_message(
                &log,
                id,
                "multiverse",
                "example",
                body,
                false,
                None,
                true,
                &no_redact,
            )
            .unwrap();
        }
        read_inbox(&state_dir, &log, 20, true).unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), Some(3));
    }

    #[test]
    fn mail_cursor_behind_shrinks_once_the_gap_is_partly_acknowledged() {
        let dir = tempdir().unwrap();
        let state_dir = dir.path().join(".multiverse");
        let log = log_in(dir.path(), "example");
        for (id, body) in [("a", "one"), ("b", "two"), ("c", "three")] {
            send_message(
                &log,
                id,
                "multiverse",
                "example",
                body,
                false,
                None,
                true,
                &no_redact,
            )
            .unwrap();
        }
        read_inbox(&state_dir, &log, 20, true).unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), Some(3));

        let partial = read_inbox(&state_dir, &log, 1, false).unwrap();
        acknowledge(
            &state_dir,
            &log,
            partial.through_seq.unwrap(),
            partial.messages.len() as u64,
        )
        .unwrap();
        assert_eq!(mail_cursor_behind(dir.path()), Some(2));
    }

    #[test]
    fn mail_index_load_tolerates_a_damaged_close_record_in_one_project() {
        // Card `20261103-fixer-2faef1` finding 2: before `MailIndex`, a
        // damaged close only disabled its own project's own leg (`is_closed`
        // reads only the holder's own log). `MailIndex::load` now runs
        // `classify_close` over every configured project's log and used to
        // propagate the first `Err`, so one damaged close anywhere in the
        // fleet silently disabled unlinked-reply detection for every
        // project. A damaged close must now be skipped like any other
        // unreadable line, not fail the load.
        let good_dir = tempdir().unwrap();
        let bad_dir = tempdir().unwrap();
        let bad_log = log_in(bad_dir.path(), "bad");

        // A valid record `bad` actually holds, which must survive the
        // damaged line appended after it.
        send_message(
            &bad_log,
            "msg-1",
            "good",
            "bad",
            "fyi, rollout is already paused",
            false,
            None,
            true,
            &no_redact,
        )
        .unwrap();
        // `re=` is non-numeric: a hard `DamagedRecord` in `classify_close`,
        // but still a crc32-valid, well-formed `v=1` envelope so it is not
        // filtered out by `envelopes()`'s own structural check.
        let damaged = encode_v1(
            "2026-11-02T12:00:00+00:00",
            99,
            "close",
            &[
                ("re", "notanumber".to_string()),
                ("reason", "\"x\"".to_string()),
            ],
        );
        bad_log.append_line(&damaged).unwrap();

        let projects = all_projects(&[("good", good_dir.path()), ("bad", bad_dir.path())]);
        let index = MailIndex::load(&projects)
            .expect("one project's damaged close record must not fail the whole fleet index");
        assert_eq!(
            index.inboxes.get("bad").map(Vec::len),
            Some(1),
            "a valid message before the damaged close line must not be lost"
        );
    }
}

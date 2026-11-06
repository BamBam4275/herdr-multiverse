//! On-disk mail types, content-hashed ids. No CLI surface — mail has
//! never had one.
//!
//! Ports the fleet mailbox documented in `.multiverse/input/spec-mail.md`
//! §2 (`multiverse/mail.py`): a **separate, storage-only** subsystem from
//! `activity.rs`'s routing-log messaging — one directory per message under
//! a fleet-wide mail root (never under a project's `.multiverse/`), no
//! `seq`/envelope/crc32, no wiring to a dispatcher yet in Python either.
//! Shares only the identifier grammar and the "redact before it ever
//! touches disk" rule with [`crate::activity`]; everything else here is
//! independent on purpose, per that spec's own finding that the two must
//! not be conflated.
//!
//! Reuses `crate::activity::support` for the identifier regex, ASCII
//! JSON-ish timestamp helpers, and the atomic-write primitive, rather than
//! duplicating them — see that module's doc comment for why they are a
//! private, self-contained stand-in for `mv-core` until it lands.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::activity::support;

/// `mail.py:57` — the cap on a message/reply body's encoded length.
pub const MAX_BODY_BYTES: usize = 32_768;

/// `mail.py:79` — caps one `events` line so concurrent `O_APPEND` writers
/// can't interleave mid-line on Darwin (regular-file appends are only
/// atomic up to a small, filesystem-dependent size).
pub const MAX_EVENT_LINE_BYTES: usize = 512;

#[derive(Debug, Error)]
pub enum MailError {
    #[error(
        "{kind} {value:?} cannot be written into a mail record safely — only \
         letters, digits, '.', '_' and '-' are allowed there."
    )]
    InvalidIdentifier { kind: &'static str, value: String },

    #[error("mail body is {actual} bytes, over the {MAX_BODY_BYTES}-byte limit")]
    BodyTooLarge { actual: usize },

    #[error("mail body contains a NUL byte")]
    BodyContainsNul,

    #[error(
        "mail body contains a control character {character:?}; terminal control sequences \
         are not allowed in a mail body"
    )]
    BodyContainsControlCharacter { character: char },

    #[error("kind must be exactly \"question\" or \"notice\", got {value:?}")]
    InvalidKind { value: String },

    #[error("mail event line would be {actual} bytes, over the {MAX_EVENT_LINE_BYTES}-byte limit")]
    EventLineTooLarge { actual: usize },

    #[error("{0:?} is not a valid mail event name")]
    UnknownEvent(String),

    #[error("sending from {from:?} to {to:?} is not permitted by [mail.pairs]")]
    NotPermitted { from: String, to: String },

    #[error(
        "a message with id {id:?} already exists with different content — this id's content \
         hash no longer matches what is on disk"
    )]
    ContentMismatch { id: String },

    #[error("{id:?}: corrupt committed message: {detail}")]
    CorruptMessage { id: String, detail: String },

    #[error(transparent)]
    Io(#[from] io::Error),
}

/// `mv_core::lock`'s own timeout carries no `io::Error`, so it is folded
/// into [`MailError::Io`] here rather than growing `MailError` a variant
/// for a failure mode this module otherwise never produces.
fn lock_err(error: mv_core::lock::LockError) -> MailError {
    match error {
        mv_core::lock::LockError::Timeout(timeout) => {
            MailError::Io(io::Error::other(timeout.to_string()))
        }
        mv_core::lock::LockError::Io(io) => MailError::Io(io),
    }
}

fn validate_identifier(kind: &'static str, value: &str) -> Result<(), MailError> {
    if support::is_valid_identifier(value) {
        Ok(())
    } else {
        Err(MailError::InvalidIdentifier {
            kind,
            value: value.to_string(),
        })
    }
}

/// `_validate_kind` (`mail.py:156-161`): `kind` must be exactly `"question"`
/// or `"notice"` — a two-value check, not the general identifier charset
/// [`validate_identifier`] enforces elsewhere.
fn validate_kind(kind: &str) -> Result<(), MailError> {
    if kind == "question" || kind == "notice" {
        Ok(())
    } else {
        Err(MailError::InvalidKind {
            value: kind.to_string(),
        })
    }
}

/// C0 controls and DEL, excluding the whitespace a Markdown body
/// legitimately uses (`\n`, `\t`, `\r`) — `mail.py`'s own `_CONTROL_RE`
/// (`mail.py:61-64`). Catches NUL (0x00) and ESC (0x1b), the byte that
/// begins a terminal escape sequence.
fn is_disallowed_control(c: char) -> bool {
    matches!(c as u32, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f)
}

/// `_validate_body` (`mail.py:156-180`): reject a body over
/// [`MAX_BODY_BYTES`], containing a NUL byte, or any other C0/DEL control
/// character — run on the already-redacted body, matching Python's own
/// `body = redact_text(body)` before `_validate_body(body)` (`mail.py:293-
/// 295`; scout gap #31's ordering finding, folded into this one).
fn validate_body(body: &str) -> Result<(), MailError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(MailError::BodyTooLarge { actual: body.len() });
    }
    if body.contains('\0') {
        return Err(MailError::BodyContainsNul);
    }
    if let Some(character) = body.chars().find(|&c| is_disallowed_control(c)) {
        return Err(MailError::BodyContainsControlCharacter { character });
    }
    Ok(())
}

/// `status` (`done|blocked|failed`), written last, atomically — the same
/// three terminal states a dispatched task's own `status` file uses
/// (`.multiverse/input/spec-on-disk-contract.md` §B).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailStatus {
    Done,
    Blocked,
    Failed,
}

impl MailStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MailStatus::Done => "done",
            MailStatus::Blocked => "blocked",
            MailStatus::Failed => "failed",
        }
    }
}

/// `VALID_EVENTS` (`mail.py:...`): the only three event names `events`
/// accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailEvent {
    DeliveryAttempted,
    Delivered,
    Acknowledged,
}

impl MailEvent {
    pub fn as_str(self) -> &'static str {
        match self {
            MailEvent::DeliveryAttempted => "delivery-attempted",
            MailEvent::Delivered => "delivered",
            MailEvent::Acknowledged => "acknowledged",
        }
    }
}

/// A lowercase, `[a-z0-9-]`-only slug: collapses any other byte to `-`,
/// then trims and collapses repeats so `message_id` stays stable and
/// filesystem-safe regardless of what identifier-shaped text the caller
/// passes as `from`/`to`.
fn slug(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_dash = false;
    for c in value.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_was_dash = false;
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// `message_id` (`mail.py:133-153`): deterministic —
/// `YYYYMMDD-mail-<from-slug>-<to-slug>-<sha256(...)[:6]>`. An identical
/// message sent again the same day reuses the same id, which is what makes
/// a resend an idempotent no-op rather than a duplicate.
///
/// The hash material is `from\0to\0kind\0body` — `day` names only the id's
/// own prefix, it is not part of the hash (`mail.py:133-153`, cited in
/// `critic-history-compat.md` finding 7). An earlier Rust version hashed
/// `day\0from\0to\0body` with no `kind` at all, which both omitted a real
/// input and let two different-day sends of the same content collide on
/// id material in a way Python's never would.
pub fn message_id(day: &str, from: &str, to: &str, kind: &str, body: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(from.as_bytes());
    hasher.update(b"\0");
    hasher.update(to.as_bytes());
    hasher.update(b"\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(body.as_bytes());
    let digest = hasher.finalize();
    let short_hash = digest
        .iter()
        .take(3)
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    format!("{day}-mail-{}-{}-{short_hash}", slug(from), slug(to))
}

/// What [`create_message`] reports: whether this call actually created the
/// directory, or found an identical one already there (`SendResult`,
/// `mail.py`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendResult {
    pub id: String,
    pub created: bool,
}

fn mail_dir(mail_root: &Path, id: &str) -> PathBuf {
    mail_root.join(id)
}

/// `_render_envelope` (`mail.py:202-210`): field names and order must match
/// the Python cutover format byte-for-byte, since history readers parse
/// this file directly — `created`/`body_sha256`, not a Rust-only `when`
/// (`critic-engine-state.md` finding 1).
fn envelope_toml(
    id: &str,
    from: &str,
    to: &str,
    kind: &str,
    created: &str,
    body_sha256: &str,
) -> String {
    format!(
        "id = \"{id}\"\nfrom = \"{from}\"\nto = \"{to}\"\nkind = \"{kind}\"\ncreated = \"{created}\"\nbody_sha256 = \"{body_sha256}\"\n",
        id = toml_escape(id),
        from = toml_escape(from),
        to = toml_escape(to),
        kind = toml_escape(kind),
        created = toml_escape(created),
        body_sha256 = toml_escape(body_sha256),
    )
}

fn toml_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn toml_unescape(quoted: &str) -> Option<String> {
    let inner = quoted.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next()? {
                '\\' => out.push('\\'),
                '"' => out.push('"'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// A minimal reader for the handful of `key = "value"` lines
/// [`envelope_toml`] writes — no general TOML parser is a dependency of
/// this crate, and none is needed for a format this crate also writes.
struct ParsedEnvelope {
    id: String,
    from: String,
    to: String,
    kind: String,
    created: String,
    body_sha256: String,
    deadline: Option<String>,
}

fn parse_envelope_toml(text: &str) -> Option<ParsedEnvelope> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, raw_value) = line.split_once('=')?;
        fields.insert(key.trim().to_string(), toml_unescape(raw_value.trim())?);
    }
    Some(ParsedEnvelope {
        id: fields.remove("id")?,
        from: fields.remove("from")?,
        to: fields.remove("to")?,
        kind: fields.remove("kind")?,
        created: fields.remove("created")?,
        body_sha256: fields.remove("body_sha256")?,
        deadline: fields.remove("deadline"),
    })
}

/// `mail.py`'s `Envelope` dataclass (`mail.py:384-392`): the parsed,
/// immutable contents of one message's `envelope.toml`, as [`read_envelope`]
/// returns it. `deadline` is `None` for every message [`create_message`]
/// writes today — nothing here writes it yet (scout gap #32) — but a reader
/// tolerates one being present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub id: String,
    pub from: String,
    pub to: String,
    pub kind: String,
    pub created: String,
    pub body_sha256: String,
    pub deadline: Option<String>,
}

/// `read_envelope` (`mail.py:395-411`): read and parse one message's
/// `envelope.toml`. A missing or malformed envelope is
/// [`MailError::CorruptMessage`], named by this directory's own id (its
/// last path component) rather than the id inside the file, since a
/// malformed envelope cannot be trusted to name itself correctly.
pub fn read_envelope(message_dir: &Path) -> Result<Envelope, MailError> {
    let id = message_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let text = fs::read_to_string(message_dir.join("envelope.toml")).map_err(|e| {
        MailError::CorruptMessage {
            id: id.clone(),
            detail: format!("envelope.toml: {e}"),
        }
    })?;
    let parsed = parse_envelope_toml(&text).ok_or_else(|| MailError::CorruptMessage {
        id: id.clone(),
        detail: "envelope.toml: malformed".to_string(),
    })?;
    Ok(Envelope {
        id: parsed.id,
        from: parsed.from,
        to: parsed.to,
        kind: parsed.kind,
        created: parsed.created,
        body_sha256: parsed.body_sha256,
        deadline: parsed.deadline,
    })
}

/// `list_message_ids` (`mail.py:378-392`): every committed message id under
/// `mail_root`, sorted. A `.tmp-*` directory is an incomplete send and is
/// always skipped — `create_message` may still be mid-write, or may have
/// died there, and either way nothing there is a message yet. A missing
/// root reads as no messages, not an error.
pub fn list_message_ids(mail_root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(mail_root) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|t| t.is_dir())
                && !entry.file_name().to_string_lossy().starts_with(".tmp-")
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort();
    ids
}

/// `mail.py`'s `Event` dataclass (`mail.py:424-429`): one parsed line from a
/// message's `events` file, as [`read_events`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailEventRecord {
    pub when: String,
    pub name: String,
    pub fields: Vec<(String, String)>,
}

/// `_parse_event_line` (`mail.py:466-483`): tolerant of a torn or truncated
/// line — the file is append-only and read long after it was written, so a
/// line a crash cut short is skipped rather than allowed to make the rest
/// of the file unreadable.
fn parse_event_line(line: &str) -> Option<MailEventRecord> {
    let mut parts = line.trim_end_matches('\n').split('\t');
    let when = parts.next()?;
    if when.is_empty() {
        return None;
    }
    let mut name = String::new();
    let mut fields = Vec::new();
    for part in parts {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        if key == "event" {
            name = value.to_string();
        } else {
            fields.push((key.to_string(), value.to_string()));
        }
    }
    if name.is_empty() {
        return None;
    }
    Some(MailEventRecord {
        when: when.to_string(),
        name,
        fields,
    })
}

/// `read_events` (`mail.py:486-492`): every readable event in
/// `message_dir/events`, in file order. A missing file reads as no events.
pub fn read_events(message_dir: &Path) -> Vec<MailEventRecord> {
    let Ok(text) = fs::read_to_string(message_dir.join("events")) else {
        return Vec::new();
    };
    text.lines().filter_map(parse_event_line).collect()
}

/// `read_status` (`mail.py:538-547`): the message's own `status`, or `None`
/// if absent or unrecognised — a crash mid-write, or a value no writer here
/// would produce, is treated as absent rather than passed through.
pub fn read_status(message_dir: &Path) -> Option<MailStatus> {
    let raw = fs::read_to_string(message_dir.join("status")).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "done" => Some(MailStatus::Done),
        "blocked" => Some(MailStatus::Blocked),
        "failed" => Some(MailStatus::Failed),
        _ => None,
    }
}

/// `is_terminal` (`mail.py:550-555`): whether this message has reached a
/// real terminal outcome — `answer.md` existing is not enough; only a
/// recognised `status`, written last, makes a reply terminal.
pub fn is_terminal(message_dir: &Path) -> bool {
    read_status(message_dir).is_some()
}

fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// `_validate_committed_message` (`mail.py:353-375`): an idempotent hit must
/// be a complete, matching copy of the requested message, not merely a body
/// match (`critic-engine-state.md` finding 1) — a corrupt or mismatched
/// envelope must surface as an error rather than silently reporting
/// `created: false`. `created` is required and must parse as a timestamp,
/// matching Python's own `envelope.created` presence/`datetime.fromisoformat`
/// check (`mail.py:359-361`; `rereview-engine.md` "idempotent validation
/// omits required `created`") — [`parse_envelope_toml`] already enforces
/// presence by requiring the field to construct a [`ParsedEnvelope`] at all.
fn validate_committed_message(
    dir: &Path,
    id: &str,
    from: &str,
    to: &str,
    kind: &str,
    redacted_body: &str,
) -> Result<(), MailError> {
    let corrupt = |detail: &str| MailError::CorruptMessage {
        id: id.to_string(),
        detail: detail.to_string(),
    };

    let envelope_text = fs::read_to_string(dir.join("envelope.toml"))
        .map_err(|e| corrupt(&format!("envelope.toml: {e}")))?;
    let envelope =
        parse_envelope_toml(&envelope_text).ok_or_else(|| corrupt("envelope.toml: malformed"))?;
    mv_core::time::parse_flexible(&envelope.created)
        .map_err(|_| corrupt("envelope.toml: created is not a valid timestamp"))?;
    let body =
        fs::read_to_string(dir.join("body.md")).map_err(|e| corrupt(&format!("body.md: {e}")))?;

    if envelope.id != id
        || envelope.from != from
        || envelope.to != to
        || envelope.kind != kind
        || envelope.body_sha256 != sha256_hex(redacted_body.as_bytes())
        || body != redacted_body
    {
        return Err(MailError::ContentMismatch { id: id.to_string() });
    }
    Ok(())
}

/// `create_message` (`mail.py:...`): idempotent on content — a same-day
/// resend with the same `from`/`to`/`kind`/`body` returns `created: false`
/// rather than writing again. The idempotency check runs **before** the
/// `may_send` gate (`mail.py:270-278`): an exact resend of a message that
/// already landed succeeds even if the pairing was revoked since, because
/// nothing new is being sent. `may_send` is the caller-evaluated
/// `[mail.pairs]` decision — this module does not parse that config table.
///
/// Builds the message directory in a `.tmp-<id>` sibling first and
/// `rename`s it into place, mirroring Python's `.tmp-<id>/` +
/// `os.replace` (`mail.py:280-291`): the final `<id>/` directory only ever
/// comes into existence via that one atomic rename, so a crash partway
/// through writing `envelope.toml`/`body.md` can never leave a partial,
/// empty `<id>/` behind for a later `resend`/id-recomputation to trip over
/// on (`critic-history-compat.md` finding 7). Any `.tmp-<id>` left by a
/// prior crash is cleared before reuse.
///
/// Held under a per-`id` lock for the whole read-check-write span: `id` is
/// a deterministic hash of `from`/`to`/`kind`/`body`, so two independent
/// callers sending identical content at the same moment target the exact
/// same `.tmp-<id>` and the same final `<id>/` — without the lock, one
/// loser's `rename` can race the winner's already-landed, nonempty `<id>/`
/// (or the winner's own in-flight `.tmp-<id>` writes) and fail outright
/// instead of folding into the idempotent `created: false` path.
#[allow(clippy::too_many_arguments)]
pub fn create_message(
    mail_root: &Path,
    day: &str,
    from: &str,
    to: &str,
    kind: &str,
    body: &str,
    may_send: bool,
    redact: &dyn Fn(&str) -> String,
) -> Result<SendResult, MailError> {
    // `mail.py:291-295`: redact first, then validate kind and the
    // (now-redacted) body, then the sender/recipient identifiers.
    let redacted_body = redact(body);
    validate_kind(kind)?;
    validate_body(&redacted_body)?;
    validate_identifier("sender project name", from)?;
    validate_identifier("recipient project name", to)?;

    let id = message_id(day, from, to, kind, &redacted_body);
    let dir = mail_dir(mail_root, &id);
    let when = support::now_stamp();

    let lock_path = mail_root.join(format!(".lock-{id}"));
    let _guard = mv_core::lock::acquire(
        &lock_path,
        "mail-message",
        mv_core::lock::LOCK_ACQUIRE_TIMEOUT,
    )
    .map_err(lock_err)?;

    if dir.is_dir() {
        validate_committed_message(&dir, &id, from, to, kind, &redacted_body)?;
        return Ok(SendResult { id, created: false });
    }

    if !may_send {
        return Err(MailError::NotPermitted {
            from: from.to_string(),
            to: to.to_string(),
        });
    }

    fs::create_dir_all(mail_root)?;
    let tmp_dir = mail_root.join(format!(".tmp-{id}"));
    let _ = fs::remove_dir_all(&tmp_dir);

    #[cfg(unix)]
    support::ensure_dir_mode(&tmp_dir, 0o700)?;
    #[cfg(not(unix))]
    fs::create_dir_all(&tmp_dir)?;

    let body_sha256 = sha256_hex(redacted_body.as_bytes());
    support::atomic_write(
        &tmp_dir.join("envelope.toml"),
        envelope_toml(&id, from, to, kind, &when, &body_sha256).as_bytes(),
        0o600,
    )?;
    support::atomic_write(&tmp_dir.join("body.md"), redacted_body.as_bytes(), 0o600)?;

    fs::rename(&tmp_dir, &dir)?;
    if let Ok(dir_file) = fs::File::open(mail_root) {
        let _ = dir_file.sync_all();
    }

    Ok(SendResult { id, created: true })
}

/// `append_event` (`mail.py:...`): one `<iso>\tevent=<name>\tkey=value..`
/// line, append-only, capped at [`MAX_EVENT_LINE_BYTES`] so two concurrent
/// writers' `O_APPEND` writes can't interleave mid-line.
pub fn append_event(
    mail_dir: &Path,
    event: MailEvent,
    fields: &[(&str, &str)],
) -> Result<(), MailError> {
    let when = support::now_stamp();
    let mut line = format!("{when}\tevent={}", event.as_str());
    for (key, value) in fields {
        line.push('\t');
        line.push_str(key);
        line.push('=');
        line.push_str(value);
    }
    line.push('\n');
    if line.len() > MAX_EVENT_LINE_BYTES {
        return Err(MailError::EventLineTooLarge { actual: line.len() });
    }

    let path = mail_dir.join("events");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    file.write_all(line.as_bytes())?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Parse `event` as one of the three names `append_event` accepts.
pub fn parse_event(name: &str) -> Result<MailEvent, MailError> {
    match name {
        "delivery-attempted" => Ok(MailEvent::DeliveryAttempted),
        "delivered" => Ok(MailEvent::Delivered),
        "acknowledged" => Ok(MailEvent::Acknowledged),
        other => Err(MailError::UnknownEvent(other.to_string())),
    }
}

/// `write_reply` (`mail.py:...`): writes `answer.md`, redacted. No pairing
/// check at all — a reply's legitimacy comes from having received the
/// message it answers, not from `[mail.pairs]`.
pub fn write_reply(
    mail_dir: &Path,
    body: &str,
    redact: &dyn Fn(&str) -> String,
) -> Result<(), MailError> {
    if body.len() > MAX_BODY_BYTES {
        return Err(MailError::BodyTooLarge { actual: body.len() });
    }
    let redacted = redact(body);
    support::atomic_write(&mail_dir.join("answer.md"), redacted.as_bytes(), 0o600)?;
    Ok(())
}

/// `status` (`done|blocked|failed`): written **last**, atomically — the
/// caller is responsible for the ordering (envelope/body/events/answer all
/// land before this), the same contract a dispatched task's own `status`
/// file keeps.
pub fn write_status(mail_dir: &Path, status: MailStatus) -> Result<(), MailError> {
    support::atomic_write(&mail_dir.join("status"), status.as_str().as_bytes(), 0o600)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn no_redact(s: &str) -> String {
        s.to_string()
    }

    #[test]
    fn message_id_is_deterministic_and_filesystem_safe() {
        let id1 = message_id("20261102", "Multiverse", "demo-app", "question", "hello");
        let id2 = message_id("20261102", "Multiverse", "demo-app", "question", "hello");
        assert_eq!(id1, id2);
        assert!(id1.starts_with("20261102-mail-multiverse-demo-app-"));
        assert!(id1.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }

    #[test]
    fn a_different_body_produces_a_different_id() {
        let id1 = message_id("20261102", "a", "b", "question", "one");
        let id2 = message_id("20261102", "a", "b", "question", "two");
        assert_ne!(id1, id2);
    }

    #[test]
    fn a_different_kind_produces_a_different_id() {
        // Finding 7: the Rust hash dropped `kind` entirely, so two distinct
        // message kinds with the same from/to/body used to collide on id.
        let id1 = message_id("20261102", "a", "b", "question", "same body");
        let id2 = message_id("20261102", "a", "b", "report", "same body");
        assert_ne!(id1, id2);
    }

    #[test]
    fn create_message_writes_envelope_and_body() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "multiverse",
            "demo-app",
            "question",
            "hello world",
            true,
            &no_redact,
        )
        .unwrap();
        assert!(result.created);

        let msg_dir = dir.path().join(&result.id);
        assert!(msg_dir.join("envelope.toml").is_file());
        let envelope = fs::read_to_string(msg_dir.join("envelope.toml")).unwrap();
        assert!(envelope.contains("kind = \"question\""));
        let body = fs::read_to_string(msg_dir.join("body.md")).unwrap();
        assert_eq!(body, "hello world");
        assert!(
            !dir.path().join(format!(".tmp-{}", result.id)).exists(),
            "the tmp staging dir must be renamed away, not left behind"
        );
    }

    #[test]
    fn a_crash_before_the_rename_leaves_no_partial_message_directory() {
        // Finding 7: Python builds `.tmp-<id>/` then `os.replace`s it into
        // place, so the final `<id>/` only ever exists complete. Simulate
        // "crashed after mkdir, before rename" by creating the tmp dir by
        // hand with nothing in it, then creating the message for real.
        let dir = tempdir().unwrap();
        let id = message_id("20261102", "a", "b", "question", "hi");
        fs::create_dir_all(dir.path().join(format!(".tmp-{id}"))).unwrap();
        assert!(!dir.path().join(&id).exists(), "no final dir until renamed");

        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        assert!(result.created);
        assert_eq!(result.id, id);
        let msg_dir = dir.path().join(&id);
        assert!(msg_dir.join("envelope.toml").is_file());
        assert!(msg_dir.join("body.md").is_file());
    }

    #[test]
    fn resending_identical_content_is_idempotent_even_if_no_longer_permitted() {
        let dir = tempdir().unwrap();
        let first = create_message(
            dir.path(),
            "20261102",
            "multiverse",
            "demo-app",
            "question",
            "hello",
            true,
            &no_redact,
        )
        .unwrap();
        assert!(first.created);

        // Pairing revoked since the first send; an exact resend still
        // succeeds because nothing new is being written (mail.py:270-278).
        let second = create_message(
            dir.path(),
            "20261102",
            "multiverse",
            "demo-app",
            "question",
            "hello",
            false,
            &no_redact,
        )
        .unwrap();
        assert!(!second.created);
        assert_eq!(first.id, second.id);
    }

    /// Two concurrent sends of the exact same content race between the
    /// `dir.is_dir()` idempotency check and the `rename` that lands it: one
    /// loser's `tmp-<id>` collides with the winner's already-renamed, now
    /// nonempty `<id>/`, and `rename` over a nonempty directory fails
    /// (confirmed on this host: `ENOTEMPTY`) instead of folding into the
    /// idempotent `created: false` path. A real double-send of an identical
    /// report (both callers reading "no recent delivery" and acting) must
    /// not surface as an `Io` error to one of the two senders.
    #[test]
    fn concurrent_identical_sends_never_surface_as_an_io_error() {
        use std::sync::{Arc, Barrier};
        use std::thread;

        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let root = root.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                create_message(
                    &root,
                    "20261102",
                    "a",
                    "b",
                    "question",
                    "identical content",
                    true,
                    &no_redact,
                )
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        for result in &results {
            assert!(
                result.is_ok(),
                "a concurrent identical send must never fail: {result:?}"
            );
        }
        let created_count = results
            .iter()
            .filter(|r| r.as_ref().unwrap().created)
            .count();
        assert_eq!(
            created_count, 1,
            "exactly one of the two concurrent identical sends should report created=true"
        );
    }

    /// `mail.py:315-322`: the envelope's own field names and the ones
    /// history readers of the cutover format expect are `created` and
    /// `body_sha256`, not a Rust-only `when` with no body hash at all
    /// (`critic-engine-state.md` finding 1).
    #[test]
    fn envelope_toml_matches_the_python_field_names() {
        let body_sha256 = sha256_hex(b"hello world");
        let text = envelope_toml(
            "id1",
            "a",
            "b",
            "question",
            "2026-01-01T00:00:00+00:00",
            &body_sha256,
        );
        assert_eq!(
            text,
            format!(
                "id = \"id1\"\nfrom = \"a\"\nto = \"b\"\nkind = \"question\"\n\
                 created = \"2026-01-01T00:00:00+00:00\"\nbody_sha256 = \"{body_sha256}\"\n"
            )
        );
        assert!(!text.contains("when ="));
    }

    /// `mail.py`'s `_validate_committed_message` checks the whole envelope,
    /// not only the body — a tampered `kind` with an untouched `body.md`
    /// must not be reported as the same idempotent message
    /// (`critic-engine-state.md` finding 1).
    #[test]
    fn an_idempotent_hit_validates_the_full_envelope_not_just_the_body() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        let envelope = fs::read_to_string(msg_dir.join("envelope.toml")).unwrap();
        let tampered = envelope.replace("kind = \"question\"", "kind = \"notice\"");
        fs::write(msg_dir.join("envelope.toml"), tampered).unwrap();

        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::ContentMismatch { .. }));
    }

    /// `mail.py:359-361`: `_validate_committed_message` requires `created`
    /// and rejects it if it does not parse as a timestamp — an envelope
    /// missing the field is corrupt, not an idempotent hit
    /// (`rereview-engine.md` "idempotent validation omits required
    /// `created`").
    #[test]
    fn an_idempotent_resend_rejects_an_envelope_missing_created() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        let envelope = fs::read_to_string(msg_dir.join("envelope.toml")).unwrap();
        let without_created = envelope
            .lines()
            .filter(|line| !line.starts_with("created ="))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        fs::write(msg_dir.join("envelope.toml"), without_created).unwrap();

        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::CorruptMessage { .. }));
    }

    #[test]
    fn a_new_message_is_refused_when_not_permitted() {
        let dir = tempdir().unwrap();
        let err = create_message(
            dir.path(),
            "20261102",
            "multiverse",
            "demo-app",
            "question",
            "hello",
            false,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::NotPermitted { .. }));
        assert!(!dir
            .path()
            .join(message_id(
                "20261102",
                "multiverse",
                "demo-app",
                "question",
                "hello"
            ))
            .exists());
    }

    #[test]
    fn create_message_rejects_an_invalid_identifier() {
        let dir = tempdir().unwrap();
        let err = create_message(
            dir.path(),
            "20261102",
            "../escape",
            "demo-app",
            "question",
            "hello",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::InvalidIdentifier { .. }));
    }

    /// Scout gap #23: `mail.py:156-161`'s `_validate_kind` restricts `kind`
    /// to `"question"`/`"notice"` — Rust used to accept any identifier-
    /// shaped string.
    #[test]
    fn create_message_rejects_a_kind_outside_question_or_notice() {
        let dir = tempdir().unwrap();
        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "bogus",
            "hello",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::InvalidKind { .. }));
    }

    /// Scout gap #22: `mail.py:156-183`'s `_validate_body` rejects NUL and
    /// C0 control characters — explicitly to stop terminal-escape injection
    /// — which Rust's `create_message` used to write verbatim.
    #[test]
    fn create_message_rejects_a_nul_byte_in_the_body() {
        let dir = tempdir().unwrap();
        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hello\u{0}world",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::BodyContainsNul));
    }

    #[test]
    fn create_message_rejects_a_terminal_escape_sequence_in_the_body() {
        let dir = tempdir().unwrap();
        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "danger\u{1b}[31mred\u{1b}[0m",
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            MailError::BodyContainsControlCharacter { .. }
        ));
    }

    #[test]
    fn create_message_allows_ordinary_markdown_whitespace() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "line one\nline two\ttabbed\r\n",
            true,
            &no_redact,
        )
        .unwrap();
        assert!(result.created);
    }

    #[test]
    fn create_message_rejects_an_oversized_body() {
        let dir = tempdir().unwrap();
        let body = "x".repeat(MAX_BODY_BYTES + 1);
        let err = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            &body,
            true,
            &no_redact,
        )
        .unwrap_err();
        assert!(matches!(err, MailError::BodyTooLarge { .. }));
    }

    #[test]
    fn append_event_writes_a_tab_separated_line() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        append_event(&msg_dir, MailEvent::DeliveryAttempted, &[("pane", "p1")]).unwrap();
        append_event(&msg_dir, MailEvent::Delivered, &[]).unwrap();

        let events = fs::read_to_string(msg_dir.join("events")).unwrap();
        let lines: Vec<&str> = events.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("event=delivery-attempted"));
        assert!(lines[0].contains("pane=p1"));
        assert!(lines[1].contains("event=delivered"));
    }

    #[test]
    fn append_event_rejects_an_oversized_line() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        let huge_value = "x".repeat(MAX_EVENT_LINE_BYTES);
        let err =
            append_event(&msg_dir, MailEvent::Delivered, &[("note", &huge_value)]).unwrap_err();
        assert!(matches!(err, MailError::EventLineTooLarge { .. }));
    }

    #[test]
    fn write_reply_never_checks_pairing() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        write_reply(&msg_dir, "here is my answer", &no_redact).unwrap();
        let answer = fs::read_to_string(msg_dir.join("answer.md")).unwrap();
        assert_eq!(answer, "here is my answer");
    }

    #[test]
    fn write_status_is_one_of_the_three_terminal_words() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        write_status(&msg_dir, MailStatus::Done).unwrap();
        let status = fs::read_to_string(msg_dir.join("status")).unwrap();
        assert_eq!(status, "done");
    }

    #[test]
    fn parse_event_rejects_unknown_names() {
        assert!(parse_event("delivered").is_ok());
        assert!(matches!(
            parse_event("bogus"),
            Err(MailError::UnknownEvent(_))
        ));
    }

    #[test]
    fn slug_is_filesystem_safe_for_odd_project_names() {
        assert_eq!(slug("My Project!!"), "my-project");
        assert_eq!(slug("already-safe"), "already-safe");
        assert_eq!(slug("--leading"), "leading");
    }

    /// Scout gap #24: the entire mail *read* API
    /// (`read_envelope`/`read_events`/`list_message_ids`/`read_status`/
    /// `is_terminal`) had no Rust counterpart at all. Round-trip a written
    /// message through all five.
    #[test]
    fn a_written_message_round_trips_through_the_read_api() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hello",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);

        assert_eq!(list_message_ids(dir.path()), vec![result.id.clone()]);

        let envelope = read_envelope(&msg_dir).unwrap();
        assert_eq!(envelope.id, result.id);
        assert_eq!(envelope.from, "a");
        assert_eq!(envelope.to, "b");
        assert_eq!(envelope.kind, "question");
        assert!(envelope.deadline.is_none());

        assert!(!is_terminal(&msg_dir));
        assert!(read_status(&msg_dir).is_none());

        append_event(&msg_dir, MailEvent::DeliveryAttempted, &[("pane", "p1")]).unwrap();
        append_event(&msg_dir, MailEvent::Delivered, &[]).unwrap();
        let events = read_events(&msg_dir);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name, "delivery-attempted");
        assert_eq!(
            events[0].fields,
            vec![("pane".to_string(), "p1".to_string())]
        );
        assert_eq!(events[1].name, "delivered");

        write_status(&msg_dir, MailStatus::Blocked).unwrap();
        assert_eq!(read_status(&msg_dir), Some(MailStatus::Blocked));
        assert!(is_terminal(&msg_dir));
    }

    /// `list_message_ids` must skip an in-progress `.tmp-<id>` send, never
    /// report it as a message.
    #[test]
    fn list_message_ids_skips_tmp_directories_and_sorts() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".tmp-not-a-message")).unwrap();
        create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "z",
            true,
            &no_redact,
        )
        .unwrap();
        create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "a",
            true,
            &no_redact,
        )
        .unwrap();
        let ids = list_message_ids(dir.path());
        assert_eq!(ids.len(), 2);
        assert!(ids.iter().all(|id| !id.starts_with(".tmp-")));
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
    }

    #[test]
    fn read_status_treats_a_missing_or_unrecognised_file_as_absent() {
        let dir = tempdir().unwrap();
        let result = create_message(
            dir.path(),
            "20261102",
            "a",
            "b",
            "question",
            "hi",
            true,
            &no_redact,
        )
        .unwrap();
        let msg_dir = dir.path().join(&result.id);
        assert!(read_status(&msg_dir).is_none());

        fs::write(msg_dir.join("status"), "not-a-real-status").unwrap();
        assert!(read_status(&msg_dir).is_none());
        assert!(!is_terminal(&msg_dir));
    }

    #[test]
    fn read_envelope_reports_a_missing_file_as_corrupt() {
        let dir = tempdir().unwrap();
        let err = read_envelope(&dir.path().join("no-such-message")).unwrap_err();
        assert!(matches!(err, MailError::CorruptMessage { .. }));
    }
}

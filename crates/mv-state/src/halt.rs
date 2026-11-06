//! The fleet-wide halt marker: `multiverse halt on|off|status`'s on-disk state.
//!
//! The marker stays in the resolved fleet runtime directory, at the exact
//! path and shape `config.py`'s `write_fleet_halt`/`read_fleet_halt` use —
//! `<state_dir>/halt`, holding `{"reason": "...", "since": "..."}` (JSON,
//! `sort_keys=True`, trailing newline). This is not a format this build
//! gets to choose freely: an operator can halt the fleet with the Python
//! CLI and clear it with this one (or the reverse) at cutover, so the two
//! must read and write the same bytes or neither sees the other's halt.
//! Earlier Rust builds wrote `<state_dir>/halt.state` as three plain-text
//! lines (`true`/reason/timestamp) instead — a format only this binary
//! ever produced or read. That file is still read as a fallback when the
//! canonical `halt` is absent, so a state dir carrying only the old marker
//! does not silently read as "not halted"; every write goes out in the
//! Python form only, never the legacy one.
//!
//! Clearing a halt removes the canonical marker — and the legacy one too,
//! if a stale copy from an earlier build is still sitting beside it, so an
//! `off` always leaves both tools agreeing the fleet is running again.
//! Repeated `off` calls distinguish a cleared halt from an already-running
//! fleet. Writes replace the marker atomically.

use std::path::{Path, PathBuf};

pub struct HaltState {
    pub halted: bool,
    pub reason: Option<String>,
    pub at: Option<String>,
}

/// The name `config.py`'s `FLEET_HALT_FILENAME` uses: a bare filename "to
/// keep it easy to inspect and avoid making the halt state look like
/// another config table."
fn halt_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("halt")
}

/// Where an earlier Rust build (pre-cutover) wrote the same state in its
/// own plain-text format. Read-only: never written by this build, so a
/// state dir touched only by the current code never has one.
fn legacy_halt_state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("halt.state")
}

/// A minimal JSON value, just enough to walk `{"reason": "...", "since":
/// "..."}`. Kept hand-rolled rather than built on `serde_json` (already a
/// dependency here) because the write side must match Python's
/// `json.dumps(..., ensure_ascii=True)` byte-for-byte, including surrogate
/// pairs for codepoints above the BMP — `serde_json`'s writer does not
/// escape non-ASCII by default, and reshaping its output to match would be
/// no simpler than this.
#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b) if b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn parse(text: &'a str) -> Option<Json> {
        let mut parser = Self::new(text);
        let value = parser.parse_value()?;
        parser.skip_ws();
        if parser.pos == parser.bytes.len() {
            Some(value)
        } else {
            None
        }
    }

    fn parse_value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Json::String),
            b't' => self.parse_literal("true", Json::Bool(true)),
            b'f' => self.parse_literal("false", Json::Bool(false)),
            b'n' => self.parse_literal("null", Json::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> Option<Json> {
        let end = self.pos + literal.len();
        if self.bytes.get(self.pos..end) == Some(literal.as_bytes()) {
            self.pos = end;
            Some(value)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Json::Number)
    }

    fn parse_string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let escape = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?)
                                .ok()?;
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            if (0xd800..=0xdbff).contains(&code) {
                                // A high surrogate half: `write_fleet_halt`
                                // emits these only as a `\uXXXX\uXXXX` pair
                                // for a codepoint above the BMP (see
                                // `json_escape`), so the low half must
                                // follow immediately.
                                if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.pos += 2;
                                let low_hex =
                                    std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?)
                                        .ok()?;
                                let low = u32::from_str_radix(low_hex, 16).ok()?;
                                if !(0xdc00..=0xdfff).contains(&low) {
                                    return None;
                                }
                                self.pos += 4;
                                let combined = 0x10000 + ((code - 0xd800) << 10) + (low - 0xdc00);
                                out.push(char::from_u32(combined)?);
                            } else {
                                out.push(char::from_u32(code)?);
                            }
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Re-decode this byte (and any continuation bytes) as UTF-8
                    // rather than assuming ASCII.
                    let rest = &self.bytes[self.pos - 1..];
                    let ch_len = utf8_len(b);
                    let slice = rest.get(..ch_len)?;
                    out.push_str(std::str::from_utf8(slice).ok()?);
                    self.pos += ch_len - 1;
                }
            }
        }
    }

    fn parse_array(&mut self) -> Option<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_object(&mut self) -> Option<Json> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(entries));
                }
                _ => return None,
            }
        }
    }
}

fn utf8_len(first_byte: u8) -> usize {
    if first_byte & 0x80 == 0 {
        1
    } else if first_byte & 0xE0 == 0xC0 {
        2
    } else if first_byte & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

/// Encode one JSON string the way `json.dumps(..., ensure_ascii=True)`
/// (Python's default, and what `write_fleet_halt` uses) does: a quoted
/// backslash-escaped ASCII string, every non-ASCII or control character
/// folded to a `\uXXXX` escape (a surrogate pair above the BMP) rather
/// than written as a raw UTF-8 byte.
fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        let code = ch as u32;
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ if (0x20..=0x7e).contains(&code) => out.push(ch),
            _ if code <= 0xffff => out.push_str(&format!("\\u{code:04x}")),
            _ => {
                let v = code - 0x10000;
                let high = 0xd800 + (v >> 10);
                let low = 0xdc00 + (v & 0x3ff);
                out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    out.push('"');
    out
}

/// Parse the marker's payload; missing or invalid fields never clear a halt.
fn parse_halt_json(text: &str) -> Option<HaltState> {
    let object = JsonParser::parse(text)?;
    let reason = object
        .get("reason")
        .and_then(Json::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("halt state is unreadable")
        .to_owned();
    let at = object
        .get("since")
        .and_then(Json::as_str)
        .filter(|value| {
            time::OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
                .is_ok()
        })
        .map(str::to_owned);
    Some(HaltState {
        halted: true,
        reason: Some(reason),
        at,
    })
}

/// Parse a legacy `halt.state` file's three plain-text lines
/// (`true`/reason/timestamp) — the only format any Rust build before this
/// one ever wrote.
fn parse_legacy_halt_text(text: &str) -> HaltState {
    let not_halted = || HaltState {
        halted: false,
        reason: None,
        at: None,
    };
    let mut lines = text.lines();
    let halted = lines
        .next()
        .map(|line| line.trim() == "true")
        .unwrap_or(false);
    if !halted {
        return not_halted();
    }
    let reason = lines
        .next()
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    let at = lines
        .next()
        .map(str::to_string)
        .filter(|value| !value.is_empty());
    HaltState { halted, reason, at }
}

/// Read the canonical halt first. Only genuine absence permits the legacy
/// fallback; unreadable or corrupt state fails closed.
pub fn read_halt_state(state_dir: &Path) -> HaltState {
    let path = halt_state_path(state_dir);
    let unreadable = || HaltState {
        halted: true,
        reason: Some("halt state is unreadable".into()),
        at: None,
    };
    let mut state = match std::fs::read_to_string(&path) {
        Ok(text) => parse_halt_json(&text).unwrap_or_else(unreadable),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::read_to_string(legacy_halt_state_path(state_dir)) {
                Ok(text) => parse_legacy_halt_text(&text),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => HaltState {
                    halted: false,
                    reason: None,
                    at: None,
                },
                Err(_) => unreadable(),
            };
        }
        Err(_) => unreadable(),
    };
    if state.at.is_none() {
        let since = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .map(time::OffsetDateTime::from)
            .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
        state.at = since
            .format(&time::format_description::well_known::Rfc3339)
            .ok();
    }
    state
}

/// Write the halt marker in the Python form only, at the canonical path —
/// never the legacy `halt.state` shape, regardless of which one a prior
/// read fell back to.
pub fn write_halt_state(state_dir: &Path, state: &HaltState) -> std::io::Result<()> {
    std::fs::create_dir_all(state_dir)?;
    let reason = state.reason.as_deref().unwrap_or("");
    let since = state.at.as_deref().unwrap_or("");
    let body = format!(
        "{{\"reason\": {}, \"since\": {}}}\n",
        json_escape(reason),
        json_escape(since),
    );
    mv_core::atomic::write_atomic(&halt_state_path(state_dir), body.as_bytes())
}

/// Clear the halt: remove the canonical marker, and a leftover legacy one
/// too if an earlier Rust build left it sitting beside it — either one
/// still being present would make the next `read_halt_state` see a halt
/// the operator just turned off. Reports whether *anything* was removed,
/// so `off` can still tell a genuine clear from a no-op one.
pub fn clear_halt_state(state_dir: &Path) -> std::io::Result<bool> {
    let canonical = remove_if_present(&halt_state_path(state_dir))?;
    let legacy = remove_if_present(&legacy_halt_state_path(state_dir))?;
    if canonical || legacy {
        mv_core::atomic::fsync_dir(state_dir)?;
    }
    Ok(canonical || legacy)
}

fn remove_if_present(path: &Path) -> std::io::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub fn format_halt_line(state: &HaltState) -> String {
    if !state.halted {
        return "fleet is not halted.".to_string();
    }
    let reason = state.reason.as_deref().unwrap_or("no reason given");
    match &state.at {
        Some(at) => format!("fleet halted since {at}: {reason}"),
        None => format!("fleet halted: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mv-halt-test-{label}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_state_reads_as_not_halted() {
        let dir = temp_dir("missing");
        assert_eq!(
            format_halt_line(&read_halt_state(&dir)),
            "fleet is not halted."
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn on_then_status_round_trips_the_reason() {
        let dir = temp_dir("roundtrip");
        let state = HaltState {
            halted: true,
            reason: Some("maintenance".to_string()),
            at: Some("2026-11-02T12:00:00+00:00".to_string()),
        };
        write_halt_state(&dir, &state).unwrap();
        let read_back = read_halt_state(&dir);
        assert!(read_back.halted);
        assert_eq!(read_back.reason.as_deref(), Some("maintenance"));
        assert_eq!(
            format_halt_line(&read_back),
            "fleet halted since 2026-11-02T12:00:00+00:00: maintenance"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn off_clears_a_prior_halt() {
        let dir = temp_dir("clear");
        write_halt_state(
            &dir,
            &HaltState {
                halted: true,
                reason: Some("x".to_string()),
                at: Some("2026-11-02T12:00:00+00:00".to_string()),
            },
        )
        .unwrap();
        assert!(clear_halt_state(&dir).unwrap());
        assert!(!halt_state_path(&dir).exists());
        assert!(!clear_halt_state(&dir).unwrap());
        assert_eq!(
            format_halt_line(&read_halt_state(&dir)),
            "fleet is not halted."
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn corrupt_or_unreadable_canonical_halt_fails_closed() {
        let dir = temp_dir("corrupt-closed");
        for bytes in [b"\xff\xfe not text".as_slice(), b"garbage", b"{}", b"null"] {
            std::fs::write(halt_state_path(&dir), bytes).unwrap();
            let state = read_halt_state(&dir);
            assert!(state.halted);
            assert_eq!(state.reason.as_deref(), Some("halt state is unreadable"));
        }
        std::fs::remove_file(halt_state_path(&dir)).unwrap();
        std::fs::create_dir(halt_state_path(&dir)).unwrap();
        assert!(read_halt_state(&dir).halted);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Regression for card 20261102-b-c09e29: the operator halts the
    /// fleet with the Python CLI (`config.write_fleet_halt`), which writes
    /// `<state_dir>/halt` as JSON, then clears it with this binary at
    /// cutover. Before this fix, `read_halt_state` only ever looked at
    /// `<state_dir>/halt.state` and so never saw this file at all — this
    /// is the exact byte-for-byte shape `write_fleet_halt` produces
    /// (confirmed against a real halted fleet's own state file).
    #[test]
    fn reads_a_halt_file_written_by_the_python_cli() {
        let dir = temp_dir("python-authored");
        std::fs::write(
            halt_state_path(&dir),
            b"{\"reason\": \"maintenance window\", \"since\": \"2026-11-01T23:04:52+00:00\"}\n",
        )
        .unwrap();
        let state = read_halt_state(&dir);
        assert!(state.halted);
        assert_eq!(state.reason.as_deref(), Some("maintenance window"));
        assert_eq!(
            format_halt_line(&state),
            "fleet halted since 2026-11-01T23:04:52+00:00: maintenance window"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `write_fleet_halt` (`config.py`) does `json.dumps({"reason": ...,
    /// "since": ...}, sort_keys=True) + "\n"` — `sort_keys` puts `reason`
    /// before `since` (already alphabetical), with `json.dumps`'s default
    /// `", "`/`": "` separators and no indentation. A byte mismatch here
    /// is exactly the kind of silent divergence `diff_trees.py` (the
    /// on-disk-state half of `write_parity.sh`) is built to catch.
    #[test]
    fn write_matches_pythons_exact_byte_shape() {
        let dir = temp_dir("byte-shape");
        write_halt_state(
            &dir,
            &HaltState {
                halted: true,
                reason: Some("maintenance".to_string()),
                at: Some("2026-11-02T12:00:00+00:00".to_string()),
            },
        )
        .unwrap();
        let written = std::fs::read_to_string(halt_state_path(&dir)).unwrap();
        assert_eq!(
            written,
            "{\"reason\": \"maintenance\", \"since\": \"2026-11-02T12:00:00+00:00\"}\n"
        );
        assert!(!legacy_halt_state_path(&dir).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Non-ASCII reason text round-trips: `json.dumps` with
    /// `ensure_ascii=True` (Python's default, unchanged by
    /// `write_fleet_halt`) folds it to `\uXXXX` escapes rather than raw
    /// UTF-8 bytes, so a Rust writer has to match that or a later Python
    /// `read_fleet_halt` (or a byte-level diff) sees a different file.
    #[test]
    fn write_escapes_non_ascii_reason_text_like_python() {
        let dir = temp_dir("unicode");
        write_halt_state(
            &dir,
            &HaltState {
                halted: true,
                reason: Some("café \u{1f6d1}".to_string()),
                at: Some("2026-11-02T12:00:00+00:00".to_string()),
            },
        )
        .unwrap();
        let written = std::fs::read_to_string(halt_state_path(&dir)).unwrap();
        assert_eq!(
            written,
            "{\"reason\": \"caf\\u00e9 \\ud83d\\uded1\", \"since\": \"2026-11-02T12:00:00+00:00\"}\n"
        );
        let read_back = read_halt_state(&dir);
        assert_eq!(read_back.reason.as_deref(), Some("café \u{1f6d1}"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A state dir touched only by a pre-cutover Rust build — canonical
    /// `halt` absent, only the old `halt.state` plain-text marker present
    /// — still reads as halted rather than silently clearing on upgrade.
    #[test]
    fn falls_back_to_a_legacy_halt_state_file_when_the_canonical_one_is_absent() {
        let dir = temp_dir("legacy-fallback");
        std::fs::write(
            legacy_halt_state_path(&dir),
            "true\nold build\n2026-01-01T00:00:00+00:00\n",
        )
        .unwrap();
        let state = read_halt_state(&dir);
        assert!(state.halted);
        assert_eq!(state.reason.as_deref(), Some("old build"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Canonical `halt` wins over a stale legacy `halt.state` sitting
    /// beside it — e.g. right after this binary first writes the new
    /// form but an old marker from a prior build has not been cleaned up.
    #[test]
    fn the_canonical_file_wins_when_both_are_present() {
        let dir = temp_dir("both-present");
        std::fs::write(
            legacy_halt_state_path(&dir),
            "true\nstale\n2020-01-01T00:00:00+00:00\n",
        )
        .unwrap();
        write_halt_state(
            &dir,
            &HaltState {
                halted: true,
                reason: Some("current".to_string()),
                at: Some("2026-11-02T12:00:00+00:00".to_string()),
            },
        )
        .unwrap();
        let state = read_halt_state(&dir);
        assert_eq!(state.reason.as_deref(), Some("current"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `off` must not leave a stale legacy marker behind to be
    /// misread later — both the canonical and (if present) the legacy
    /// file are removed by one `clear_halt_state` call.
    #[test]
    fn off_clears_both_the_canonical_and_a_leftover_legacy_file() {
        let dir = temp_dir("clear-both");
        std::fs::write(
            legacy_halt_state_path(&dir),
            "true\nold build\n2020-01-01T00:00:00+00:00\n",
        )
        .unwrap();
        write_halt_state(
            &dir,
            &HaltState {
                halted: true,
                reason: Some("current".to_string()),
                at: Some("2026-11-02T12:00:00+00:00".to_string()),
            },
        )
        .unwrap();
        assert!(clear_halt_state(&dir).unwrap());
        assert!(!halt_state_path(&dir).exists());
        assert!(!legacy_halt_state_path(&dir).exists());
        assert_eq!(
            format_halt_line(&read_halt_state(&dir)),
            "fleet is not halted."
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

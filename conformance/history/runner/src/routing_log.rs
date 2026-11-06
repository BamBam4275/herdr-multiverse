//! `routing.log` parsing.
//!
//! `mv-state::log` (crates/mv-state/src/log.rs) has since landed a real
//! line parser. This module was evaluated against it for the swap the
//! lane convention called for, and the decision is **no swap** —
//! documented here rather than left as an open TODO:
//!
//! - `mv_state::log::parse` is deliberately field-order-agnostic (it
//!   collects whatever `key=value` pairs follow the timestamp into a
//!   `BTreeMap`, and only requires `crc32=` to be the trailing field via
//!   `rsplit_once`), because a production reader must tolerate however a
//!   writer ordered its fields. [`RoutingLogLine::parse`] below is
//!   strict about position (`v=` at field 1, `seq=` at field 2, `event=`
//!   at field 3) on purpose: this runner is the independent oracle that
//!   checks the on-disk contract was *produced* correctly, so a
//!   reordering is exactly the kind of drift it exists to flag, not
//!   silently tolerate.
//! - `mv_state::log`'s own doc comment says the storage codec
//!   "deliberately leaves role names and event payloads opaque" and
//!   pushes semantic validation to other modules. This runner folds
//!   structural and semantic checks (role names, `event=backlog`'s
//!   `options=`/`recommendation=` bounds) into one parse per line
//!   so a single bad record produces one finding at one file:line —
//!   routing through `mv_state::log` would split that back apart.
//! - Doing the swap would make this oracle partly test "does mv-state
//!   parse its own output" rather than "does this on-disk data conform
//!   to the spec", for the same independence reason this lane already
//!   applied to the fleet-store swap (`README.md`'s "Why `runner/` is
//!   its own Cargo workspace").
//!
//! The call site in `main.rs` and the variant/finding bookkeeping stay
//! as they are.

use std::path::Path;

use crate::finding::Report;

/// Dispatch-path role names accepted on `event=dispatch`/`event=result`.
/// Both eras must be accepted at once: AGENTS.md requires history readers
/// to keep accepting the old names, and `spec-on-disk-contract.md` §D
/// confirms the fleet's `routing.log` is old-named today. New names per
/// `crates/mv-config/src/roles.rs`'s doc comment; the old->new mapping
/// (oracle->Seer, librarian->Archivist, designer->Artisan,
/// observer->Watcher) is inferred from each role's stated purpose in
/// `docs/roles.md`, not sourced from a committed mapping table — flagged
/// in this lane's result.md.
const OLD_ROLES: &[&str] = &[
    "orchestrator",
    "explorer",
    "fixer",
    "reviewer",
    "oracle",
    "librarian",
    "council",
    "designer",
    "observer",
];

const NEW_ROLES: &[&str] = &[
    "nexus",
    "scout",
    "builder",
    "critic",
    "seer",
    "archivist",
    "council",
    "artisan",
    "watcher",
];

fn is_known_role(role: &str) -> bool {
    OLD_ROLES.contains(&role) || NEW_ROLES.contains(&role)
}

fn is_old_role(role: &str) -> bool {
    OLD_ROLES.contains(&role)
}

/// A free-text identifier charset check for `event=backlog`'s `role=`
/// field, which `multiverse/backlog.py` validates only as an identifier
/// (`activity._IDENTIFIER_RE`), never against the closed `RoleId` enum
/// (`spec-on-disk-contract.md` §D).
fn looks_like_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn looks_like_timestamp(s: &str) -> bool {
    // Loose ISO-8601 shape check: `YYYY-MM-DDTHH:MM:SS` then `Z` or a
    // `+HH:MM`/`-HH:MM` offset. Good enough to catch a missing/garbled
    // timestamp field without pulling in a date/time crate for a harness
    // that only needs to notice shape, not validate calendar correctness.
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    let digit = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let lit = |i: usize, c: u8| bytes.get(i) == Some(&c);
    digit(0)
        && digit(1)
        && digit(2)
        && digit(3)
        && lit(4, b'-')
        && digit(5)
        && digit(6)
        && lit(7, b'-')
        && digit(8)
        && digit(9)
        && lit(10, b'T')
        && digit(11)
        && digit(12)
        && lit(13, b':')
        && digit(14)
        && digit(15)
        && lit(16, b':')
        && digit(17)
        && digit(18)
        && (s[19..] == *"Z" || s.len() == 25 && (lit(19, b'+') || lit(19, b'-')))
}

/// Splits a `key=value` field on the first `=`. Values may themselves
/// contain `=` (e.g. `reason=first entry in Explorer chain for X`), so
/// this must not be a naive `split('=')`.
fn split_field(field: &str) -> Option<(&str, &str)> {
    field.split_once('=')
}

pub struct RoutingLogLine<'a> {
    pub event: &'a str,
    pub seq: Option<u64>,
    pub fields: Vec<(&'a str, &'a str)>,
}

impl<'a> RoutingLogLine<'a> {
    /// Parses one already-UTF-8-validated line (no trailing `\n`).
    /// Reports every shape it recognizes as a variant, and every shape it
    /// rejects as a finding tied to `file:line`.
    fn parse(line: &'a str, file: &Path, line_no: usize, report: &mut Report) -> Option<Self> {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.is_empty() || parts[0].is_empty() {
            report.fail(file, Some(line_no), "empty line in routing.log");
            return None;
        }
        if !looks_like_timestamp(parts[0]) {
            report.fail(
                file,
                Some(line_no),
                format!("leading field is not an ISO-8601 timestamp: {:?}", parts[0]),
            );
            return None;
        }

        let versioned = parts.get(1).is_some_and(|f| f.starts_with("v="));
        if versioned {
            report.note("routing_log.v1_envelope");
        } else {
            report.note("routing_log.legacy_envelope");
        }

        let (event_idx, seq) = if versioned {
            let Some(("v", _version)) = parts.get(1).and_then(|f| split_field(f)) else {
                report.fail(file, Some(line_no), "malformed v= field");
                return None;
            };
            let seq = match parts.get(2).and_then(|f| split_field(f)) {
                Some(("seq", v)) => match v.parse::<u64>() {
                    Ok(n) => Some(n),
                    Err(_) => {
                        report.fail(
                            file,
                            Some(line_no),
                            format!("seq= is not an integer: {v:?}"),
                        );
                        return None;
                    }
                },
                _ => {
                    report.fail(file, Some(line_no), "versioned line missing seq= field");
                    return None;
                }
            };
            (3, seq)
        } else {
            (1, None)
        };

        let Some(("event", event)) = parts.get(event_idx).and_then(|f| split_field(f)) else {
            report.fail(
                file,
                Some(line_no),
                format!("expected event= at field {event_idx}"),
            );
            return None;
        };

        let mut body_end = parts.len();
        if versioned {
            match parts.last().and_then(|f| split_field(f)) {
                Some(("crc32", hex)) => {
                    let prefix = parts[..parts.len() - 1].join("\t");
                    let computed = crc32fast::hash(prefix.as_bytes());
                    let expected = u32::from_str_radix(hex, 16).ok();
                    if expected != Some(computed) {
                        report.fail(
                            file,
                            Some(line_no),
                            format!(
                                "crc32 mismatch: on-disk {hex:?}, computed {computed:08x} — damaged record"
                            ),
                        );
                        return None;
                    }
                    body_end -= 1;
                }
                _ => {
                    report.fail(file, Some(line_no), "versioned line missing crc32= trailer");
                    return None;
                }
            }
        }

        let mut fields = Vec::new();
        for field in &parts[event_idx + 1..body_end] {
            match split_field(field) {
                Some(kv) => fields.push(kv),
                None => {
                    report.fail(
                        file,
                        Some(line_no),
                        format!("field is not key=value: {field:?}"),
                    );
                    return None;
                }
            }
        }

        Some(RoutingLogLine { event, seq, fields })
    }

    fn field(&self, key: &str) -> Option<&'a str> {
        self.fields.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }
}

/// Parses every line of one project's `routing.log`, checking the
/// envelope/crc contract plus the role-field and backlog-ask rules this
/// lane's input audit (`audit-mail-backlog-board-disk.md`, P1 finding)
/// calls out. Returns the highest versioned `seq` seen, for
/// `activity.rs`'s cursor-vs-high-water check.
pub fn check_routing_log(file: &Path, report: &mut Report) -> Option<u64> {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(_) => return None,
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(e) => {
            report.fail(
                file,
                None,
                format!(
                    "routing.log is not valid UTF-8 at byte {}",
                    e.utf8_error().valid_up_to()
                ),
            );
            return None;
        }
    };

    let mut high_water = 0u64;
    for (idx, line) in text.lines().enumerate() {
        if line.is_empty() {
            continue;
        }
        let line_no = idx + 1;
        let Some(parsed) = RoutingLogLine::parse(line, file, line_no, report) else {
            continue;
        };
        if let Some(seq) = parsed.seq {
            high_water = high_water.max(seq);
        }

        match parsed.event {
            "dispatch" | "result" => {
                if let Some(role) = parsed.field("role") {
                    if !is_known_role(role) {
                        report.fail(
                            file,
                            Some(line_no),
                            format!("event={} has unrecognized role={role:?}", parsed.event),
                        );
                    } else if is_old_role(role) {
                        report.note("routing_log.dispatch_role.old_name");
                    } else {
                        report.note("routing_log.dispatch_role.new_name");
                    }
                }
            }
            "backlog" => {
                if let Some(role) = parsed.field("role") {
                    if looks_like_identifier(role) {
                        report.note("routing_log.backlog_role.free_text");
                    } else {
                        report.fail(
                            file,
                            Some(line_no),
                            format!(
                                "event=backlog role={role:?} is not a valid free-text identifier"
                            ),
                        );
                    }
                }
                check_backlog_ask(&parsed, file, line_no, report);
            }
            _ => {}
        }
    }

    Some(high_water)
}

/// Corrected-spec rule from `audit-mail-backlog-board-disk.md` P1:
/// "validate persisted `options` as a JSON list of strings and
/// `recommendation` as a positive integer within that list's 1-based
/// bounds. Skip the malformed record only."
fn check_backlog_ask(line: &RoutingLogLine<'_>, file: &Path, line_no: usize, report: &mut Report) {
    let Some(state) = line.field("state") else {
        return;
    };
    if state != "asked" {
        return;
    }
    report.note("routing_log.backlog_asked");

    let options: Option<Vec<String>> = line
        .field("options")
        .and_then(|raw| serde_json::from_str(raw).ok());
    if line.field("options").is_some() && options.is_none() {
        report.fail(
            file,
            Some(line_no),
            "backlog state=asked options= is not a JSON array of strings",
        );
        return;
    }

    if let Some(rec) = line.field("recommendation") {
        let n: Option<usize> = rec.parse().ok();
        let in_bounds = match (&n, &options) {
            (Some(n), Some(opts)) => *n >= 1 && *n <= opts.len(),
            (Some(n), None) => *n >= 1,
            (None, _) => false,
        };
        if !in_bounds {
            report.fail(
                file,
                Some(line_no),
                format!(
                    "backlog state=asked recommendation={rec:?} is not a positive integer within options' 1-based bounds"
                ),
            );
        }
    }
}

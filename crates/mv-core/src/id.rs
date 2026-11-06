//! `TaskId` and the shared identifier pattern: `^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$`.
//!
//! This is `activity._IDENTIFIER_RE` (`activity.py:304-309`, also pinned by
//! a comment at `activity.py:335` cross-referenced from architecture.md):
//! one to 64 characters, starting with a letter or digit, the rest letters,
//! digits, `.`, `_`, or `-`. It is a *character-class* rule, not a role or
//! kind enum — a history-era task id like `20000101-explorer-000001` or
//! `20000101-b-000001` validates exactly the same as a current one, so old
//! role names embedded in historical ids keep reading fine without this
//! module knowing anything about role names at all.
//!
//! Used for task ids, project names, message ids, and every other
//! free-text field the routing log writes verbatim into a `key=value`
//! line (`activity.py:1061,1137,1589`, `accounts.py:2550-2551`).

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;

use crate::error::InvalidIdentifierError;

fn identifier_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$").expect("static identifier regex compiles")
    })
}

/// Whether `value` matches the shared identifier grammar: 1-64 characters,
/// starting with a letter or digit, the rest `[A-Za-z0-9._-]`.
pub fn is_valid_identifier(value: &str) -> bool {
    identifier_re().is_match(value)
}

/// A task id: an [identifier](is_valid_identifier) known, at construction
/// time, to be safe to write verbatim into a routing-log `key=value` field.
///
/// Validation happens once, at the boundary ([`TaskId::new`]); everywhere
/// else a `TaskId` is passed around it is already known-good, the same way
/// `activity.py`'s callers validate with `_IDENTIFIER_RE.fullmatch` before
/// ever formatting a record (`accounts.py:2550-2551`).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskId(String);

impl TaskId {
    /// Validate `value` against the shared identifier grammar.
    ///
    /// Mirrors `_validate_task_id` (`accounts.py:2550-2551`), which raises
    /// `InvalidIdentifierError("task id", task)` on the same mismatch.
    pub fn new(value: impl Into<String>) -> Result<Self, InvalidIdentifierError> {
        let value = value.into();
        if is_valid_identifier(&value) {
            Ok(Self(value))
        } else {
            Err(InvalidIdentifierError::new("task id", value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl AsRef<str> for TaskId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TaskId {
    type Error = InvalidIdentifierError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<&str> for TaskId {
    type Error = InvalidIdentifierError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_current_style_task_id() {
        assert!(is_valid_identifier("20000101-fixer-000001"));
        assert!(TaskId::new("20000101-fixer-000001").is_ok());
    }

    #[test]
    fn accepts_old_role_names_embedded_in_history_ids() {
        // The regex is a character-class rule, not a role enum: ids minted
        // under Python's old role names (fixer/explorer/oracle/…) must keep
        // reading as valid task ids forever, per architecture.md's cutover
        // checklist ("history readers still map fixer -> Builder
        // internally" is a *display* concern layered on top of this, not a
        // reason for this regex to reject the old spelling).
        for id in [
            "20000101-explorer-000001",
            "20000101-b-000001",
            "20261016-fixer-abcdef",
            "20260911-council-0011aa",
            // Python council mint `{date}-council-{seat}-{digest}` and the
            // pre-parity Rust mint `{date}-council-{digest}-{seat}` are
            // both identifiers; lane naming (`safe_lane_root`) and
            // routing-log `task=` fields must keep reading either.
            "20260907-council-cl-1bee5f",
            "20260907-council-1bee5f-cl",
        ] {
            assert!(
                is_valid_identifier(id),
                "{id:?} should be a valid identifier"
            );
        }
    }

    #[test]
    fn rejects_a_leading_dot_dash_or_underscore() {
        for id in [".hidden", "-dash", "_underscore"] {
            assert!(!is_valid_identifier(id), "{id:?} should be rejected");
        }
    }

    #[test]
    fn rejects_path_traversal_shaped_values() {
        for id in ["../escape", "a/b", "a b", ""] {
            assert!(!is_valid_identifier(id), "{id:?} should be rejected");
        }
    }

    #[test]
    fn rejects_values_over_64_characters() {
        let too_long = "a".repeat(65);
        assert!(!is_valid_identifier(&too_long));
        let exactly_64 = "a".repeat(64);
        assert!(is_valid_identifier(&exactly_64));
    }

    #[test]
    fn task_id_new_reports_the_offending_value_and_field_kind() {
        let err = TaskId::new("../escape").unwrap_err();
        assert_eq!(err.kind, "task id");
        assert_eq!(err.value, "../escape");
    }

    #[test]
    fn task_id_display_round_trips_the_original_text() {
        let id = TaskId::new("20000101-fixer-000001").unwrap();
        assert_eq!(id.to_string(), "20000101-fixer-000001");
        assert_eq!(id.as_str(), "20000101-fixer-000001");
    }
}

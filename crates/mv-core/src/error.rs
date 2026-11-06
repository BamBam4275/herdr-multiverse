//! Shared leaf error types built on `thiserror`, used by the typed
//! errors in every other crate.

use std::fmt;

/// A value meant to go into the routing log (a task id, a project name, a
/// message id, …) that does not match [`crate::id::is_valid_identifier`].
///
/// Mirrors Python's `activity.InvalidIdentifierError(kind, value)`: `kind`
/// names which field failed (`"task id"`, `"sender project name"`, …) so the
/// message stays specific without every caller re-deriving its own wording.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{kind} {value:?} cannot be written into the routing log safely — only \
     letters, digits, '.', '_' and '-' are allowed there."
)]
pub struct InvalidIdentifierError {
    pub kind: String,
    pub value: String,
}

impl InvalidIdentifierError {
    pub fn new(kind: impl fmt::Display, value: impl Into<String>) -> Self {
        Self {
            kind: kind.to_string(),
            value: value.into(),
        }
    }
}

//! `ConfigError` and friends, surfaced at the `mv-cli` boundary as
//! exit code 3 (needs config).
//!
//! Self-contained for now: `mv-core`'s own error leaves
//! (`InvalidIdentifierError` and friends) have since landed on `main`
//! (`crates/mv-core/src/error.rs`), but `ConfigError` still does not build
//! on them, the same way `mv-herdr::error::HerdrError` chose to stay
//! self-contained rather than block on `mv-core`. This type remains a
//! candidate to grow a `#[source]` leaf from `mv-core`, but nothing here
//! depends on that happening.

use std::fmt;
use std::path::Path;

/// A configuration problem Mike needs to fix — never a crash.
///
/// Maps to exit code 3 (`EXIT_NEEDS_CONFIG`) at the `mv-cli` boundary
/// (`cli.py:14915-14940`). Carries one fully-formatted message rather
/// than structured fields, matching the Python original's
/// `ConfigError(str)`: every call site already knows the exact wording
/// it wants (which file, which key, which value), so one message type
/// keeps that wording at the call site instead of splitting it across
/// a `Display` impl the call site can't see while writing it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(String);

impl ConfigError {
    /// A message with no file attribution yet.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// A message prefixed with `path`, exactly once.
    ///
    /// Mirrors `Config._error()`'s double-prefix guard (regression
    /// `c8f8f65`, `config.py:1946-1964`): call this at the point an
    /// error is first attributed to a file, not again on the way back
    /// up the call stack, or the message ends up naming the path
    /// twice.
    pub fn at(path: &Path, message: impl fmt::Display) -> Self {
        Self(format!("{}: {}", path.display(), message))
    }

    /// The formatted message, with no path implied one way or another.
    pub fn message(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn at_prefixes_the_path_exactly_once() {
        let err = ConfigError::at(&PathBuf::from("/tmp/config.toml"), "malformed TOML: oops");
        assert_eq!(err.message(), "/tmp/config.toml: malformed TOML: oops");
        assert_eq!(err.to_string(), err.message());
    }

    #[test]
    fn new_carries_the_message_verbatim() {
        let err = ConfigError::new("no project contains /some/path");
        assert_eq!(err.message(), "no project contains /some/path");
    }
}

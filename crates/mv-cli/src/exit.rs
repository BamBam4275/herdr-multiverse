//! The single exception-to-exit boundary. Library errors from the real
//! business handlers are translated here.

use std::fmt;

pub const OK: u8 = 0;
pub const ERROR: u8 = 1;
pub const UNKNOWN: u8 = 2;
pub const NEEDS_CONFIG: u8 = 3;

// These families mirror the frozen Python boundary. Some cannot arise until
// the engine/config handlers are implemented; keep the mapping explicit.
#[allow(dead_code)]
#[derive(Debug)]
pub enum Failure {
    Config(String),
    AccountSelection(String),
    UnknownRole(String),
    RoleNotImplemented(String),
    Operation(String),
    Interrupted,
    /// A caller named an item (by id or token) that doesn't exist, is
    /// ambiguous, or is in the wrong state for the requested transition —
    /// `mv_state::backlog`'s `UnknownItem`/`AmbiguousItem`/`TerminalItem`/
    /// `ItemNotOpen`/`ItemNotDone` family, not a config problem, but mapped
    /// to the same exit code the frozen Python oracle uses for exactly
    /// this case (`cli.py`'s `EXIT_NEEDS_CONFIG`, whose own doc comment
    /// already reads "a question for the human", not only a bad config
    /// file). Displayed with no added prefix, same as `AccountSelection` —
    /// unlike `Config`, Python's own text for these cases carries none.
    ItemLookup(String),
    /// The command already printed its full report to stdout and only
    /// needs to carry a non-OK exit code through the boundary — not a
    /// reportable error, so nothing is ever printed to stderr for this
    /// case. Used by `commands::doctor`/`commands::check` (whose reports
    /// do not fit the single-line `Display` every other variant renders)
    /// and by `commands::presence`'s `status: "unknown"` read: a
    /// successful, honest report that no idle probe exists, which
    /// `architecture.md` §5 still maps to exit 2 ("presence `--json`
    /// carries `"status": "unknown"`"), not a failure message.
    Reported(u8),
}

impl Failure {
    pub fn code(&self) -> u8 {
        match self {
            Self::Config(_)
            | Self::AccountSelection(_)
            | Self::UnknownRole(_)
            | Self::ItemLookup(_) => NEEDS_CONFIG,
            Self::RoleNotImplemented(_) | Self::Operation(_) | Self::Interrupted => ERROR,
            Self::Reported(code) => *code,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "config error: {message}"),
            Self::AccountSelection(message)
            | Self::UnknownRole(message)
            | Self::RoleNotImplemented(message)
            | Self::Operation(message)
            | Self::ItemLookup(message) => f.write_str(message),
            Self::Interrupted => f.write_str(
                "Interrupted; the agent keeps running. Inspect its task status before retrying.",
            ),
            Self::Reported(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_exception_families_keep_their_exit_codes() {
        for error in [
            Failure::Config("bad config".into()),
            Failure::AccountSelection("no eligible seat".into()),
            Failure::UnknownRole("unknown role".into()),
            Failure::ItemLookup("no backlog item 'x' found".into()),
        ] {
            assert_eq!(error.code(), 3);
        }
        for error in [
            Failure::RoleNotImplemented("not implemented".into()),
            Failure::Operation("failed".into()),
            Failure::Interrupted,
        ] {
            assert_eq!(error.code(), 1);
        }
        assert_eq!((OK, ERROR, UNKNOWN, NEEDS_CONFIG), (0, 1, 2, 3));
        assert!(Failure::Config("missing file".into())
            .to_string()
            .starts_with("config error:"));
        assert_eq!(
            Failure::ItemLookup("no backlog item 'x' found".into()).to_string(),
            "no backlog item 'x' found"
        );
        assert!(Failure::Interrupted
            .to_string()
            .contains("agent keeps running"));
    }

    #[test]
    fn reported_carries_its_own_code_and_prints_nothing() {
        assert_eq!(Failure::Reported(UNKNOWN).code(), UNKNOWN);
        assert_eq!(Failure::Reported(OK).code(), OK);
        assert_eq!(Failure::Reported(UNKNOWN).to_string(), "");
    }
}

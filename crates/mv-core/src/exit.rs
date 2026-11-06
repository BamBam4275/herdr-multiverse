//! Process exit codes: 0 ok, 1 error, 2 unknown/usage, 3 needs-config.
//!
//! Mirrors `multiverse/cli.py`'s four constants (`EXIT_OK`, `EXIT_ERROR`,
//! `EXIT_UNKNOWN`, `EXIT_NEEDS_CONFIG`, `cli.py:200-205`) exactly — this is
//! an on-disk/process-boundary contract shared with Herdr and every agent
//! that parses a dispatched task's exit status, not a Rust-side design
//! choice to revisit.

/// One of Multiverse's four process exit codes.
///
/// - [`ExitCode::Ok`] — success, including an honest no-op.
/// - [`ExitCode::Error`] — the operation failed (e.g. a doctor checklist
///   failure).
/// - [`ExitCode::Unknown`] — a presence probe was inconclusive, or clap
///   usage error; this code is already overloaded in the Python original
///   and that overload is kept, not split.
/// - [`ExitCode::NeedsConfig`] — a question for the human, not a crash
///   (missing/invalid config, unresolved account selection, unknown role).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Error = 1,
    Unknown = 2,
    NeedsConfig = 3,
}

impl ExitCode {
    /// The raw process exit status this code represents.
    pub const fn code(self) -> i32 {
        self as i32
    }
}

impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value.code()
    }
}

impl From<ExitCode> for std::process::ExitCode {
    fn from(value: ExitCode) -> Self {
        std::process::ExitCode::from(value.code() as u8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_match_the_python_constants() {
        assert_eq!(ExitCode::Ok.code(), 0);
        assert_eq!(ExitCode::Error.code(), 1);
        assert_eq!(ExitCode::Unknown.code(), 2);
        assert_eq!(ExitCode::NeedsConfig.code(), 3);
    }
}

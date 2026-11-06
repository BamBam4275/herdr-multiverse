//! Typed errors for a missing binary, a timed-out call, and a
//! malformed envelope.
//!
//! Deliberately self-contained: this crate has no dependency on
//! `mv-core`'s own error leaves, so callers can match on these without
//! waiting for that crate to land.

use thiserror::Error;

/// Failures that can arise from running a Herdr command or reading its
/// result.
#[derive(Debug, Error)]
pub enum HerdrError {
    /// `exec` could not start the binary at all: missing, or present but
    /// not executable. The two failures a bad install or a bad
    /// `HERDR_BIN_PATH` can actually produce (`runner.py:492-496`).
    #[error(
        "herdr binary {binary:?} could not be run: {source}.\n\
         Herdr must be installed and on PATH, or HERDR_BIN_PATH set to its path."
    )]
    MissingBinary {
        binary: String,
        #[source]
        source: std::io::Error,
    },

    /// The subprocess did not return within its timeout and was killed
    /// (`runner.py:497-504`). The child is killed on timeout, so nothing
    /// of the command is still running; Herdr's own state for the agent
    /// is the only record of whether anything landed before the kill.
    #[error(
        "{purpose} did not return within {timeout_secs}s and was killed.\n\
         The child is killed on timeout, so nothing of this command is \
         still running; Herdr's own state for the agent is the only \
         record of whether anything landed before the kill."
    )]
    Timeout { purpose: String, timeout_secs: f64 },

    /// The call completed and its reply parsed as JSON, but a value the
    /// caller needed was missing or the wrong shape — e.g. `pane split`
    /// answering with no `pane_id` (`runner.py:549-561`). Distinct from
    /// `CommandResult::protocol_error`, which covers the envelope shape
    /// itself (non-JSON, missing/non-object `result`) rather than a
    /// field inside an otherwise-valid one.
    #[error("{purpose}: {detail}")]
    MalformedEnvelope { purpose: String, detail: String },

    /// `run` was about to exec the bare `herdr` PATH-lookup token — the
    /// shape `argv::herdr_binary()` returns only when `HERDR_BIN_PATH` is
    /// unset or blank — while `MV_TEST_FORBID_REAL_HERDR` forbids it.
    /// Card `20261103-b-1da16f`: a test run from inside a live Herdr pane
    /// once reached that pane's own real `herdr` this exact way and split
    /// empty panes into the operator's tab.
    #[error(
        "refusing to run the bare `herdr` token for {purpose}: HERDR_BIN_PATH is unset/blank \
         and MV_TEST_FORBID_REAL_HERDR forbids falling back to a real herdr on PATH. \
         Set HERDR_BIN_PATH to a fake binary before this call."
    )]
    RealHerdrForbidden { purpose: String },

    /// `{purpose}`'s stdout (for any command that isn't a tailed pane/agent
    /// read) or stderr exceeded [`crate::argv::MAX_OUTPUT_BYTES`]. Card
    /// `20261103-b-57c876`: a huge listing cannot be silently tailed like a
    /// pane read — truncating JSON at an arbitrary byte boundary produces
    /// invalid JSON, not a smaller valid one — so this is raised instead of
    /// handed back as a `CommandResult` built from a partial reply.
    #[error(
        "{purpose} produced more than {limit_bytes} bytes of output; refusing to read the rest \
         rather than risk a truncated reply or exhausting memory."
    )]
    OutputTooLarge { purpose: String, limit_bytes: usize },
}

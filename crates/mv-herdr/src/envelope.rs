//! JSON envelope types returned by the `herdr` CLI.

use serde_json::Value;

use crate::error::HerdrError;

/// Herdr's own process exit codes (`herdr.py:753-761`), distinct from
/// Multiverse's own 0/1/2/3 contract (`mv-core::exit`). Server errors
/// print a JSON `error` envelope on **stderr** and exit 1; CLI syntax
/// errors exit 2. Check the herdr process's own status, never a
/// pipeline's.
pub const EXIT_OK: i32 = 0;
pub const EXIT_API_ERROR: i32 = 1;
pub const EXIT_USAGE_ERROR: i32 = 2;

/// Herdr's agent lifecycle states.
///
/// `Done` does not mean the session ended. `Idle` and `Done` are the
/// same underlying state, split by the **server's** seen state, mutated
/// only by an explicit focus call — not by whether a human has looked at
/// the tab. A CLI read never makes that call, so a CLI-driven caller
/// sees `Done` for essentially every background agent it dispatches.
/// Treating `Done` as "exited" would cold-spawn a replacement for every
/// warm agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentState {
    /// Ready for input; the server's seen state has been marked, via an
    /// explicit focus call.
    Idle,
    /// Ready for input; background work finished unseen. Same state as
    /// `Idle`.
    Done,
    /// Executing.
    Working,
    /// Sitting at an approval or question dialog. Never auto-answer.
    Blocked,
    /// Present but unclassifiable. It does not prove completion.
    Unknown,
}

impl AgentState {
    /// Every state token the CLI can report.
    pub const ALL: [AgentState; 5] = [
        AgentState::Idle,
        AgentState::Done,
        AgentState::Working,
        AgentState::Blocked,
        AgentState::Unknown,
    ];

    /// The Herdr state token (`idle`, `working`, ...) as the CLI reports it.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Done => "done",
            AgentState::Working => "working",
            AgentState::Blocked => "blocked",
            AgentState::Unknown => "unknown",
        }
    }

    /// Whether the agent will accept a prompt. The correct reading of
    /// idle/done: use this rather than comparing against either member
    /// directly.
    pub fn is_ready_for_input(self) -> bool {
        matches!(self, AgentState::Idle | AgentState::Done)
    }

    /// Whether the current turn has settled: what `agent prompt --wait`
    /// waits for — "the first settled idle, done, or blocked state".
    /// `Unknown` is deliberately not settled.
    pub fn is_terminal_for_turn(self) -> bool {
        matches!(
            self,
            AgentState::Idle | AgentState::Done | AgentState::Blocked
        )
    }

    /// Whether this state is evidence the assigned work finished.
    /// Nothing is: even a settled state only says the agent stopped
    /// talking. Read `result.md` only when the *status file* is
    /// terminal.
    pub fn proves_completion(self) -> bool {
        false
    }
}

impl std::fmt::Display for AgentState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for AgentState {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(AgentState::Idle),
            "done" => Ok(AgentState::Done),
            "working" => Ok(AgentState::Working),
            "blocked" => Ok(AgentState::Blocked),
            "unknown" => Ok(AgentState::Unknown),
            other => Err(format!("not a Herdr agent state: {other:?}")),
        }
    }
}

/// Error codes the CLI returns in its JSON error envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HerdrErrorCode {
    /// No server at the socket. Not an agent problem.
    ServerNotRunning,
    /// `agent start` found the agent blocked during startup. The name
    /// stays usable for `agent read`/`agent send-keys`.
    AgentNotReady,
    /// `agent prompt` refused because the agent sits at an approval
    /// dialog. Nothing was sent. Inspect and ask the human; never
    /// auto-answer.
    AgentBlocked,
    /// `agent prompt --wait` observed no `working` or `blocked` activity
    /// within five seconds of submission; a pre-existing `idle`, `done`,
    /// or session change does not satisfy this gate.
    AgentPromptStalled,
    /// `workspace close` refused because the workspace has linked
    /// worktree workspaces; closing it alone would orphan them. Retry
    /// with `--group` only when the caller intends to close those too.
    WorkspaceGroupCloseRequired,
    /// `agent prompt --wait`'s caller-supplied `--timeout` expired
    /// before the wait settled. Distinct from `AgentPromptStalled`,
    /// which fires within the first five seconds for a different
    /// reason (no activity observed at all).
    Timeout,
}

impl HerdrErrorCode {
    /// The wire token (`agent_blocked`, ...) as returned in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            HerdrErrorCode::ServerNotRunning => "server_not_running",
            HerdrErrorCode::AgentNotReady => "agent_not_ready",
            HerdrErrorCode::AgentBlocked => "agent_blocked",
            HerdrErrorCode::AgentPromptStalled => "agent_prompt_stalled",
            HerdrErrorCode::WorkspaceGroupCloseRequired => "workspace_group_close_required",
            HerdrErrorCode::Timeout => "timeout",
        }
    }

    /// Parse a wire code, or `None` for one this version does not
    /// recognize — mirrors treating an unrecognized code from a newer
    /// Herdr release as unclassified rather than a parse failure.
    pub fn parse(code: &str) -> Option<Self> {
        match code {
            "server_not_running" => Some(HerdrErrorCode::ServerNotRunning),
            "agent_not_ready" => Some(HerdrErrorCode::AgentNotReady),
            "agent_blocked" => Some(HerdrErrorCode::AgentBlocked),
            "agent_prompt_stalled" => Some(HerdrErrorCode::AgentPromptStalled),
            "workspace_group_close_required" => Some(HerdrErrorCode::WorkspaceGroupCloseRequired),
            "timeout" => Some(HerdrErrorCode::Timeout),
            _ => None,
        }
    }
}

impl std::fmt::Display for HerdrErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parse `text` as a JSON **object**, or `None` if it is not one. Used
/// to pull an error envelope off Herdr's stderr, or a result off its
/// stdout, without treating a bare string, an array, or a decode
/// failure as a structured reply.
pub(crate) fn try_json_object(text: &str) -> Option<Value> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    parsed.is_object().then_some(parsed)
}

/// The outcome of one `herdr` invocation.
///
/// `payload` is the parsed JSON envelope when there is one. Herdr prints
/// JSON on stdout for successful structured commands and an error
/// envelope on **stderr** with exit 1; syntax errors exit 2 with usage
/// text. A zero-exit reply without a JSON `result` is a protocol error,
/// not a successful empty reply — recorded in `protocol_error` rather
/// than raised, so a caller can inspect a failed call instead of having
/// to catch it.
#[derive(Debug, Clone)]
pub struct CommandResult {
    pub argv: Vec<String>,
    pub returncode: i32,
    pub stdout: String,
    pub stderr: String,
    pub payload: Option<Value>,
    pub protocol_error: Option<String>,
}

impl CommandResult {
    /// Whether the command completed without a process or protocol error.
    pub fn ok(&self) -> bool {
        self.returncode == EXIT_OK && self.protocol_error.is_none()
    }

    /// The server's error code, when it sent a structured one.
    ///
    /// Herdr prints the error envelope on **stderr**. Stderr is preferred
    /// so a non-empty stdout JSON object (a partial listing, a warning
    /// object, extra fields from a newer release) cannot hide the code.
    /// Stdout is the fallback for a Herdr that moved the envelope.
    pub fn error_code(&self) -> Option<HerdrErrorCode> {
        if self.ok() {
            return None;
        }
        let envelope = try_json_object(&self.stderr).or_else(|| self.payload.clone())?;
        let code = envelope.get("error")?.get("code")?.as_str()?;
        HerdrErrorCode::parse(code)
    }

    /// Best available human-readable description of a failure.
    pub fn message(&self) -> String {
        if let Some(protocol_error) = &self.protocol_error {
            return protocol_error.clone();
        }
        let envelope = if self.ok() {
            self.payload.clone()
        } else {
            try_json_object(&self.stderr).or_else(|| self.payload.clone())
        };
        if let Some(envelope) = envelope {
            if let Some(message) = envelope
                .get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
            {
                if !message.is_empty() {
                    return message.to_string();
                }
            }
        }
        let chosen = if !self.stderr.is_empty() {
            self.stderr.as_str()
        } else {
            self.stdout.as_str()
        };
        let trimmed = chosen.trim();
        if trimmed.is_empty() {
            format!("exit {}", self.returncode)
        } else {
            trimmed.to_string()
        }
    }
}

/// Pull the new pane's ID out of a `pane split` envelope.
///
/// Read, never constructed: a live session's IDs were `w1Q:p1`, not the
/// `w1:p1` the documentation's examples show, so any code that assembles
/// an ID from a workspace number is wrong on this machine.
pub fn pane_id_from(result: &CommandResult) -> Result<String, HerdrError> {
    let pane_id = result
        .payload
        .as_ref()
        .and_then(|payload| payload.get("result"))
        .and_then(|result| result.get("pane"))
        .and_then(|pane| pane.get("pane_id"))
        .and_then(Value::as_str);

    pane_id
        .map(str::to_string)
        .ok_or_else(|| HerdrError::MalformedEnvelope {
            purpose: "pane split".to_string(),
            detail: "returned no pane_id".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(payload: Option<Value>, returncode: i32, stderr: &str) -> CommandResult {
        let stdout = payload
            .as_ref()
            .map(|value| value.to_string())
            .unwrap_or_default();
        CommandResult {
            argv: vec!["herdr".to_string()],
            returncode,
            stdout,
            stderr: stderr.to_string(),
            payload,
            protocol_error: None,
        }
    }

    // --- AgentState -----------------------------------------------------

    #[test]
    fn idle_and_done_are_both_ready_for_input() {
        assert!(AgentState::Idle.is_ready_for_input());
        assert!(AgentState::Done.is_ready_for_input());
    }

    #[test]
    fn working_and_blocked_are_not_ready_for_input() {
        assert!(!AgentState::Working.is_ready_for_input());
        assert!(!AgentState::Blocked.is_ready_for_input());
    }

    #[test]
    fn no_state_proves_completion() {
        for state in AgentState::ALL {
            assert!(!state.proves_completion());
        }
    }

    #[test]
    fn unknown_state_is_not_settled() {
        assert!(!AgentState::Unknown.is_terminal_for_turn());
        assert!(!AgentState::Unknown.is_ready_for_input());
    }

    #[test]
    fn settled_states_match_what_wait_waits_for() {
        let settled: std::collections::HashSet<_> = AgentState::ALL
            .into_iter()
            .filter(|state| state.is_terminal_for_turn())
            .collect();
        assert_eq!(
            settled,
            std::collections::HashSet::from([
                AgentState::Idle,
                AgentState::Done,
                AgentState::Blocked
            ])
        );
    }

    #[test]
    fn agent_state_round_trips_through_its_wire_token() {
        for state in AgentState::ALL {
            assert_eq!(state.as_str().parse::<AgentState>().unwrap(), state);
        }
        assert!("brand-new-state".parse::<AgentState>().is_err());
    }

    // --- exit codes -------------------------------------------------

    #[test]
    fn exit_code_contract() {
        assert_eq!(EXIT_OK, 0);
        assert_eq!(EXIT_API_ERROR, 1);
        assert_eq!(EXIT_USAGE_ERROR, 2);
    }

    // --- HerdrErrorCode -----------------------------------------------

    #[test]
    fn agent_prompt_stalled_is_a_known_error_code() {
        assert_eq!(
            HerdrErrorCode::parse("agent_prompt_stalled"),
            Some(HerdrErrorCode::AgentPromptStalled)
        );
    }

    #[test]
    fn unrecognised_error_code_does_not_explode() {
        assert_eq!(HerdrErrorCode::parse("brand_new"), None);
    }

    // --- CommandResult --------------------------------------------------

    #[test]
    fn failure_is_read_from_the_process_status() {
        assert!(result(Some(serde_json::json!({"ok": true})), EXIT_OK, "").ok());
        assert!(!result(None, EXIT_API_ERROR, "").ok());
        assert!(!result(None, EXIT_USAGE_ERROR, "").ok());
    }

    #[test]
    fn a_successful_result_has_no_error_code() {
        assert_eq!(
            result(Some(serde_json::json!({})), EXIT_OK, "").error_code(),
            None
        );
    }

    #[test]
    fn error_envelope_is_read_from_stderr() {
        let stderr =
            serde_json::json!({"error": {"code": "agent_blocked", "message": "at an approval dialog"}})
                .to_string();
        let outcome = result(None, EXIT_API_ERROR, &stderr);
        assert_eq!(outcome.error_code(), Some(HerdrErrorCode::AgentBlocked));
        assert_eq!(outcome.message(), "at an approval dialog");
    }

    #[test]
    fn unrecognised_error_code_in_the_envelope_does_not_explode() {
        let stderr = serde_json::json!({"error": {"code": "brand_new"}}).to_string();
        assert_eq!(result(None, EXIT_API_ERROR, &stderr).error_code(), None);
    }

    #[test]
    fn message_falls_back_to_the_exit_code_when_nothing_else_is_usable() {
        assert_eq!(result(None, EXIT_API_ERROR, "").message(), "exit 1");
    }

    // --- pane_id_from -----------------------------------------------------

    #[test]
    fn pane_id_is_read_from_the_envelope() {
        let payload = serde_json::json!({"result": {"pane": {"pane_id": "w1Q:p5"}}});
        let outcome = result(Some(payload), EXIT_OK, "");
        assert_eq!(pane_id_from(&outcome).unwrap(), "w1Q:p5");
    }

    #[test]
    fn pane_id_missing_is_an_error_not_a_none() {
        let payload = serde_json::json!({"result": {}});
        let outcome = result(Some(payload), EXIT_OK, "");
        let error = pane_id_from(&outcome).unwrap_err();
        assert!(matches!(error, HerdrError::MalformedEnvelope { .. }));
        assert!(error.to_string().contains("no pane_id"));
    }

    #[test]
    fn extra_envelope_fields_do_not_hide_pane_id() {
        let payload = serde_json::json!({
            "id": "cli:pane:split",
            "extra": {"from": "0.9.5"},
            "result": {
                "type": "pane_split",
                "pane": {
                    "pane_id": "w1Q:p5",
                    "agent_session": {"agent": "claude", "kind": "id"}
                }
            }
        });
        let outcome = result(Some(payload), EXIT_OK, "");
        assert_eq!(pane_id_from(&outcome).unwrap(), "w1Q:p5");
    }

    #[test]
    fn renamed_pane_id_field_is_malformed_not_silently_empty() {
        let payload = serde_json::json!({"result": {"pane": {"id": "w1Q:p5"}}});
        let outcome = result(Some(payload), EXIT_OK, "");
        assert!(matches!(
            pane_id_from(&outcome).unwrap_err(),
            HerdrError::MalformedEnvelope { .. }
        ));
    }

    #[test]
    fn unicode_pane_id_is_returned_verbatim() {
        let payload = serde_json::json!({"result": {"pane": {"pane_id": "w1Q:p-日本語"}}});
        let outcome = result(Some(payload), EXIT_OK, "");
        assert_eq!(pane_id_from(&outcome).unwrap(), "w1Q:p-日本語");
    }

    #[test]
    fn extra_fields_on_an_error_envelope_are_ignored() {
        let stderr = serde_json::json!({
            "id": "cli:agent:prompt",
            "error": {
                "code": "agent_blocked",
                "message": "at an approval dialog",
                "retryable": false
            },
            "trace": "new-in-0.9.x"
        })
        .to_string();
        let outcome = result(None, EXIT_API_ERROR, &stderr);
        assert_eq!(outcome.error_code(), Some(HerdrErrorCode::AgentBlocked));
        assert_eq!(outcome.message(), "at an approval dialog");
    }

    #[test]
    fn stderr_error_envelope_wins_over_stdout_json_on_nonzero_exit() {
        let payload = serde_json::json!({"result": {"agents": []}, "warning": true});
        let stderr = serde_json::json!({
            "error": {"code": "agent_blocked", "message": "at an approval dialog"}
        })
        .to_string();
        let mut outcome = result(Some(payload), EXIT_API_ERROR, &stderr);
        outcome.stdout = "{\"result\":{\"agents\":[]}}".into();
        assert!(!outcome.ok());
        assert_eq!(outcome.error_code(), Some(HerdrErrorCode::AgentBlocked));
        assert_eq!(outcome.message(), "at an approval dialog");
    }

    #[test]
    fn nonzero_exit_with_a_success_shaped_json_body_is_not_ok() {
        let payload = serde_json::json!({"result": {"pane": {"pane_id": "w1:p1"}}});
        let outcome = result(Some(payload), EXIT_API_ERROR, "");
        assert!(!outcome.ok());
        assert_eq!(outcome.error_code(), None);
        assert_eq!(pane_id_from(&outcome).unwrap(), "w1:p1");
    }

    #[test]
    fn error_envelope_on_stdout_still_parses_when_stderr_is_empty() {
        let payload = serde_json::json!({
            "error": {"code": "timeout", "message": "wait expired"}
        });
        let outcome = result(Some(payload), EXIT_API_ERROR, "");
        assert_eq!(outcome.error_code(), Some(HerdrErrorCode::Timeout));
        assert_eq!(outcome.message(), "wait expired");
    }
}

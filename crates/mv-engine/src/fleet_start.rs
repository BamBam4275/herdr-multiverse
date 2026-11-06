//! Opening and closing a project's Nexus workspace — the Herdr-facing
//! half of `multiverse start`/`stop`.
//!
//! Argument parsing, project selection, the seed-file contents, and the
//! exit-code contract stay in `mv-cli`; this module only talks to Herdr
//! (through [`crate::runner::Herdr`], the same seam `crate::runner` and
//! `crate::council` already use) and reports what happened without ever
//! raising for an ordinary launch failure — a workspace that opened is a
//! success even when seeding the seat inside it did not land, matching
//! Python's `cli._seed_orchestrator` contract.
//!
//! Ported from Python's `cli.cmd_start`/`cli.cmd_stop` and
//! `cli._seed_orchestrator` (`multiverse/cli.py`). [`FleetState::
//! orchestrator_agent`] and `crate::runner::deliver_prompt_to` (card
//! `20261102-b-a01bbc`) now give `commands::start` what it needs to
//! re-point an already-open workspace's live session — dialog/blocked
//! detection, orchestrator-identity-by-name matching, and a retry-with-
//! probe landing confirmation (card `20261102-b-38b78a`). The Claude deny-list
//! launch script is built by `commands::start` through `crate::trust`.
//! The project-level concurrency
//! lock around check-then-create and the `--dashboard` tab live in
//! `commands::start`, which holds this snapshot's [`FleetState::
//! workspace_id`] for the tab.

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use mv_herdr::argv;
use mv_herdr::envelope::AgentState;
use serde_json::Value;

use crate::runner::naming::{self, NEXUS_LEGACY_SHORT, NEXUS_SHORT};
use crate::runner::{attention_reason, Herdr, RunnerError, Timing};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

/// A project's Nexus agent name, Python's `naming.agent_name(project,
/// RoleId.ORCHESTRATOR, seat=account.short, short=project.short)` with the
/// current short code (`nexu`) in the role slot. Callers that look up a
/// live pane must also try [`nexus_legacy_agent_name`] — Python wrote
/// `orch` into that slot, and a cutover has to find those panes rather
/// than open a duplicate. Card `20261102-b-a307bc`: previously duplicated
/// as `commands::start::seed_agent_name` and `commands::sweep::
/// tick_agent_name` in `mv-cli`.
pub fn nexus_agent_name(project: &str, seat: &str, short: &str) -> Result<String, RunnerError> {
    naming::agent_name(project, NEXUS_SHORT, None, seat, short)
}

/// Python-era name for the same Nexus (`orch` in the role slot). Identical
/// slug/hash/seat/truncation; only the four-character role code differs.
pub fn nexus_legacy_agent_name(
    project: &str,
    seat: &str,
    short: &str,
) -> Result<String, RunnerError> {
    naming::agent_name(project, NEXUS_LEGACY_SHORT, None, seat, short)
}

/// The live Nexus pane for this project, if one is named with either the
/// current (`nexu`) or Python-era (`orch`) role code and seated in the
/// project's open workspace. Returns the name that actually matched so
/// callers prompt the occupant they found, not the name they wished for.
pub fn find_orchestrator_agent(
    fleet: &FleetState,
    project_name: &str,
    project_root: &Path,
    seat: &str,
    short: &str,
) -> Option<(String, LiveAgent)> {
    let current = nexus_agent_name(project_name, seat, short).ok()?;
    if let Some(live) = fleet.orchestrator_agent(project_name, project_root, &current) {
        return Some((current, live));
    }
    let legacy = nexus_legacy_agent_name(project_name, seat, short).ok()?;
    if legacy == current {
        return None;
    }
    fleet
        .orchestrator_agent(project_name, project_root, &legacy)
        .map(|live| (legacy, live))
}

fn pause(start: Instant, budget: Duration, poll: Duration) -> bool {
    let remaining = budget.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return false;
    }
    thread::sleep(poll.min(remaining));
    true
}

/// Require the label and a pane rooted inside the canonical project path.
/// Workspace listings carry no cwd; pane listings supply the ownership
/// evidence. Ported from Python's `cli._workspace_matches_project`.
fn workspace_matches_project(
    workspace: &Value,
    project_name: &str,
    project_root: &Path,
    panes: &[Value],
) -> bool {
    let Some(workspace_id) = workspace.get("workspace_id").and_then(Value::as_str) else {
        return false;
    };
    if workspace.get("label").and_then(Value::as_str) != Some(project_name) {
        return false;
    }
    panes.iter().any(|pane| {
        if pane.get("workspace_id").and_then(Value::as_str) != Some(workspace_id) {
            return false;
        }
        let Some(cwd) = pane
            .get("foreground_cwd")
            .or_else(|| pane.get("cwd"))
            .and_then(Value::as_str)
        else {
            return false;
        };
        let cwd = Path::new(cwd);
        if !cwd.is_absolute() {
            return false;
        }
        std::fs::canonicalize(cwd)
            .is_ok_and(|cwd| cwd == project_root || cwd.starts_with(project_root))
    })
}

/// Run one listing call (`workspace list`, `pane list`, `agent list`) and
/// hand back its `result` object. `what` names the call in the error
/// message, matching Python's `f"could not list {what}: {message}"`.
fn call_listing(
    herdr: &mut impl Herdr,
    command: argv::HerdrCommand,
    what: &str,
) -> Result<Value, String> {
    let result = herdr
        .call(&command, COMMAND_TIMEOUT)
        .map_err(|error| format!("could not {what}: {error}"))?;
    if !result.ok() {
        return Err(format!("could not {what}: {}", result.message()));
    }
    Ok(result
        .payload
        .and_then(|payload| payload.get("result").cloned())
        .unwrap_or(Value::Null))
}

fn array(value: &Value, key: &str) -> Vec<Value> {
    value
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// A snapshot of Herdr's open workspaces and panes, fetched once per
/// `start`/`stop` run. Kept opaque (raw JSON never crosses into `mv-cli`,
/// which has no `serde_json`/`mv-herdr` dependency by design — see
/// `commands::start`'s module docs) so every caller reasons about
/// "is this project open," never about the envelope shape underneath it.
pub struct FleetState {
    workspaces: Vec<Value>,
    panes: Vec<Value>,
    agents: Vec<Value>,
}

/// A live agent found by identity in a project's open workspace — enough
/// to type a prompt into its pane and confirm landing with
/// [`crate::runner::deliver_prompt_to`]. Returned by
/// [`FleetState::orchestrator_agent`].
pub struct LiveAgent {
    pub pane_id: String,
    pub state: AgentState,
    pub state_change_seq: u64,
}

impl LiveAgent {
    /// Sitting at an approval or question dialog — never type a prompt
    /// into it, matching Python's dialog/blocked detection before a
    /// re-seed. Exposed as a predicate, not `state` itself, so a caller
    /// (`mv-cli`, which does not depend on `mv-herdr`) can check this
    /// without naming `AgentState`.
    pub fn is_blocked(&self) -> bool {
        self.state == AgentState::Blocked
    }

    /// Mid-turn — never type into it, and never worth re-reading on this
    /// pass. Card `20261102-b-4f86db` (finding 1): `sweep --tick`'s own
    /// `_sweep_tick_orchestrator` reports this case `"skipped (working)"`
    /// before ever reading the pane for a dialog, same reason `is_blocked`
    /// is exposed this way rather than naming `AgentState`.
    pub fn is_working(&self) -> bool {
        self.state == AgentState::Working
    }

    /// Idle or done — Herdr's own "will accept a prompt" reading, before
    /// the pane text itself is checked for a dialog or menu. Neither
    /// [`Self::is_blocked`] nor [`Self::is_working`] on its own tells a
    /// caller the remaining case (`AgentState::Unknown`) apart from this
    /// one, which is why callers that need the full four-way split (the
    /// tick's own `"skipped (unknown)"` case) check this after both.
    pub fn is_ready_for_input(&self) -> bool {
        self.state.is_ready_for_input()
    }
}

impl FleetState {
    /// Fetch `workspace list` and `pane list`, and — when `include_agents`
    /// — `agent list` too. Python's `cmd_start` only calls `agent_list()`
    /// on its first, pre-project-lock listing (`cli.py:5505-5508`); the
    /// second, under-lock relist (`cli.py:5756-5766`) never lists agents
    /// at all. `commands::start`'s second `fetch`, taken under the project
    /// lock, passes `include_agents` anyway — deliberate hardening beyond
    /// Python so the duplicate-Nexus guard checks a roster taken under the
    /// same lock as the listing it guards (see that call site's own
    /// comment). The roster is retained only when `include_agents` was
    /// set, for [`Self::orchestrator_agent`] to search (card
    /// `20261102-b-a01bbc`); a caller that never asked for agents gets an
    /// empty one back, never a stale one from a previous fetch.
    pub fn fetch(herdr: &mut impl Herdr, include_agents: bool) -> Result<Self, String> {
        let workspaces = array(
            &call_listing(herdr, argv::workspace_list(), "list workspaces")?,
            "workspaces",
        );
        let panes = array(
            &call_listing(herdr, argv::pane_list(None), "list panes")?,
            "panes",
        );
        let agents = if include_agents {
            array(
                &call_listing(herdr, argv::agent_list(), "list agents")?,
                "agents",
            )
        } else {
            Vec::new()
        };
        Ok(Self {
            workspaces,
            panes,
            agents,
        })
    }

    /// Whether `project_name`'s workspace is already open, rooted at
    /// `project_root` (not required to be canonical — this canonicalizes
    /// it itself, the same way `commands::stop::select_projects` already
    /// canonicalizes both sides before comparing).
    pub fn is_open(&self, project_name: &str, project_root: &Path) -> bool {
        self.workspace_id(project_name, project_root).is_some()
    }

    /// The Herdr workspace id for `project_name` when [`Self::is_open`]
    /// would be true — what `commands::start` hands `--dashboard` so it
    /// can `tab list`/`tab create` against the already-open workspace
    /// without parsing the listing envelope itself.
    pub fn workspace_id(&self, project_name: &str, project_root: &Path) -> Option<String> {
        let root = std::fs::canonicalize(project_root).ok()?;
        self.workspaces.iter().find_map(|workspace| {
            if workspace_matches_project(workspace, project_name, &root, &self.panes) {
                workspace
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            } else {
                None
            }
        })
    }

    /// Whether the live roster holds an agent of this exact name. Python's
    /// `cmd_start` refuses to create a workspace when the expected
    /// orchestrator name is already leased (`cli.py` "workspace listing
    /// may be stale"); callers pass both the current and Python-era names.
    pub fn has_named_agent(&self, agent_name: &str) -> bool {
        self.agents
            .iter()
            .any(|agent| agent.get("name").and_then(Value::as_str) == Some(agent_name))
    }

    /// Find `agent_name`'s live entry, if it is both named correctly and
    /// seated in `project_name`/`project_root`'s open workspace — the
    /// orchestrator-identity-by-name match Python's
    /// `_orchestrator_agent_of` makes. Requires a fetch with
    /// `include_agents` set; otherwise the roster is empty and this
    /// always returns `None`.
    pub fn orchestrator_agent(
        &self,
        project_name: &str,
        project_root: &Path,
        agent_name: &str,
    ) -> Option<LiveAgent> {
        let root = std::fs::canonicalize(project_root).ok()?;
        let workspace_id = self
            .workspaces
            .iter()
            .find(|workspace| {
                workspace_matches_project(workspace, project_name, &root, &self.panes)
            })?
            .get("workspace_id")
            .and_then(Value::as_str)?;
        self.agents.iter().find_map(|agent| {
            if agent.get("name").and_then(Value::as_str) != Some(agent_name) {
                return None;
            }
            let pane_id = agent.get("pane_id").and_then(Value::as_str)?;
            let seated = self.panes.iter().any(|pane| {
                pane.get("pane_id").and_then(Value::as_str) == Some(pane_id)
                    && pane.get("workspace_id").and_then(Value::as_str) == Some(workspace_id)
            });
            if !seated {
                return None;
            }
            Some(LiveAgent {
                pane_id: pane_id.to_owned(),
                state: agent
                    .get("agent_status")
                    .and_then(Value::as_str)?
                    .parse()
                    .ok()?,
                state_change_seq: agent.get("state_change_seq").and_then(Value::as_u64)?,
            })
        })
    }
}

/// A freshly created workspace and its ready root pane.
#[derive(Debug)]
pub struct CreatedWorkspace {
    pub workspace_id: String,
    pub pane: String,
}

/// Create a workspace for `label`, rooted at `cwd`. Python's
/// `herdr.workspace_create(label=name, cwd=project.path)` — no `--env`,
/// no `--focus`: account environment is carried by the seat's own launch
/// script, not by the workspace itself.
pub fn create_workspace(
    herdr: &mut impl Herdr,
    label: &str,
    cwd: &Path,
) -> Result<CreatedWorkspace, String> {
    let result = herdr
        .call(
            &argv::workspace_create(label, cwd, &[], false),
            COMMAND_TIMEOUT,
        )
        .map_err(|error| error.to_string())?;
    if !result.ok() {
        return Err(result.message());
    }
    // `root_pane` is a SIBLING of `workspace` in the envelope, not a field
    // inside it (Python's `cmd_start` comment, measured against a real
    // `workspace create`).
    let body = result
        .payload
        .as_ref()
        .and_then(|payload| payload.get("result"));
    let workspace_id = body
        .and_then(|body| body.get("workspace"))
        .and_then(|workspace| workspace.get("workspace_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let pane = body
        .and_then(|body| body.get("root_pane"))
        .and_then(|pane| pane.get("pane_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    match (workspace_id, pane) {
        (Some(workspace_id), Some(pane)) => Ok(CreatedWorkspace { workspace_id, pane }),
        (Some(workspace_id), None) => Err(format!(
            "workspace create for {workspace_id} returned no root pane"
        )),
        (None, _) => Err("workspace create returned no workspace id".to_string()),
    }
}

/// Close a workspace and everything in it — the rollback path for a
/// failed create, and the real action behind `stop`/`stop --force`.
pub fn close_workspace(herdr: &mut impl Herdr, workspace_id: &str) -> Result<(), String> {
    let result = herdr
        .call(&argv::workspace_close(workspace_id), COMMAND_TIMEOUT)
        .map_err(|error| error.to_string())?;
    if result.ok() {
        Ok(())
    } else {
        Err(result.message())
    }
}

/// Wait for Herdr to report `pane` ready for input, polling `agent list`
/// keyed by **pane id alone** — on the `pane run` launch route the agent
/// has no name yet, unlike an ordinary `agent start` dispatch. Python's
/// `runner.wait_until_ready(pane, harness=...)` with its default
/// `require_ready=True`.
fn wait_pane_ready(herdr: &mut impl Herdr, pane: &str, timing: &Timing) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if let Ok(result) = herdr.call(&argv::agent_list(), COMMAND_TIMEOUT) {
            if result.ok() {
                let ready = result
                    .payload
                    .as_ref()
                    .and_then(|payload| payload["result"]["agents"].as_array())
                    .and_then(|agents| {
                        agents.iter().find(|agent| {
                            agent.get("pane_id").and_then(Value::as_str) == Some(pane)
                        })
                    })
                    .and_then(|agent| agent.get("agent_status"))
                    .and_then(Value::as_str)
                    .and_then(|state| state.parse::<AgentState>().ok())
                    .is_some_and(AgentState::is_ready_for_input);
                if ready {
                    return Ok(());
                }
            }
        }
        if !pause(start, timing.readiness, timing.poll) {
            return Err(format!("pane {pane} did not become ready in time"));
        }
    }
}

/// One agent's `state_change_seq`, Python's `runner.snapshot(name)`
/// narrowed to the single field this module needs.
fn state_change_seq(herdr: &mut impl Herdr, name: &str) -> Option<u64> {
    let result = herdr.call(&argv::agent_get(name), COMMAND_TIMEOUT).ok()?;
    if !result.ok() {
        return None;
    }
    result
        .payload
        .as_ref()?
        .get("result")?
        .get("agent")?
        .get("state_change_seq")?
        .as_u64()
}

/// What launching and seeding the seat produced. Mirrors Python's
/// `_seed_orchestrator`'s `(status fragment, seeded)` contract: a
/// workspace that opened is a success even when seeding did not land, so
/// [`launch_and_seed`] never raises — every failure folds into `note`.
pub struct SeatOutcome {
    pub seeded: bool,
    pub note: String,
}

fn unseeded(note: String) -> SeatOutcome {
    SeatOutcome {
        seeded: false,
        note,
    }
}

/// Run `launch_command` in `pane`, wait for Herdr to notice and ready the
/// session it starts, name it `agent_name`, and hand it `pointer` once a
/// pane read shows no trust/approval dialog in the way. Two prompt
/// attempts, each confirmed landed by `state_change_seq` moving past its
/// baseline — Python's `_seed_orchestrator` landing check, reused here
/// via [`attention_reason`] rather than re-deriving dialog detection.
pub fn launch_and_seed(
    herdr: &mut impl Herdr,
    pane: &str,
    launch_command: &str,
    agent_name: &str,
    pointer: &str,
    timing: &Timing,
) -> SeatOutcome {
    launch_script_and_seed(
        herdr,
        pane,
        launch_command,
        launch_command,
        agent_name,
        pointer,
        timing,
    )
}

/// Like [`launch_and_seed`], with a separate pane script so permission flags
/// do not replace the configured command in launch-failure diagnostics.
pub fn launch_script_and_seed(
    herdr: &mut impl Herdr,
    pane: &str,
    launch_command: &str,
    script: &str,
    agent_name: &str,
    pointer: &str,
    timing: &Timing,
) -> SeatOutcome {
    match herdr.call(&argv::pane_run(pane, script), COMMAND_TIMEOUT) {
        Ok(result) if result.ok() => {}
        Ok(result) => {
            return unseeded(format!(
                "{launch_command} failed to start: {}",
                result.message()
            ))
        }
        Err(error) => return unseeded(format!("{launch_command} failed to start: {error}")),
    }

    if let Err(reason) = wait_pane_ready(herdr, pane, timing) {
        return unseeded(format!("{launch_command} never came up: {reason}"));
    }

    match herdr.call(&argv::agent_rename(pane, agent_name), COMMAND_TIMEOUT) {
        Ok(result) if result.ok() => {}
        Ok(result) => {
            return unseeded(format!(
                "could not name the session ({}), so it was not handed its seed",
                result.message()
            ))
        }
        Err(error) => {
            return unseeded(format!(
                "could not name the session ({error}), so it was not handed its seed"
            ))
        }
    }

    let probe_text = match herdr.call(
        &argv::agent_read(pane, Some("visible"), None),
        COMMAND_TIMEOUT,
    ) {
        Ok(result) if result.ok() => result.stdout,
        _ => {
            return unseeded(format!(
                "{agent_name} is named but its pane could not be read, so its seed was not sent"
            ))
        }
    };
    if let Some(reason) = attention_reason(&probe_text) {
        return unseeded(format!(
            "{agent_name} is named but is parked on a {reason}, so it was not handed its seed"
        ));
    }

    let Some(baseline) = state_change_seq(herdr, agent_name) else {
        return unseeded(format!(
            "{agent_name} is named but its state could not be read, so its seed was not sent"
        ));
    };
    for _attempt in 0..2 {
        match herdr.call(
            &argv::agent_prompt(agent_name, pointer, false, None),
            COMMAND_TIMEOUT,
        ) {
            Ok(result) if result.ok() => {}
            Ok(result) => {
                return unseeded(format!(
                    "{agent_name} is named but was not handed its seed ({})",
                    result.message()
                ))
            }
            Err(error) => {
                return unseeded(format!(
                    "{agent_name} is named but was not handed its seed ({error})"
                ))
            }
        }
        let start = Instant::now();
        loop {
            if state_change_seq(herdr, agent_name).is_some_and(|seq| seq > baseline) {
                return SeatOutcome {
                    seeded: true,
                    note: String::new(),
                };
            }
            if !pause(start, timing.prompt, timing.poll) {
                break;
            }
        }
    }
    unseeded(format!(
        "{agent_name} accepted its seed twice but showed no sign of reading it"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RunnerError;
    use mv_herdr::envelope::CommandResult;
    use serde_json::json;
    use std::collections::VecDeque;

    #[test]
    fn nexus_agent_name_uses_python_budget_with_current_short_code() {
        assert_eq!(
            nexus_agent_name("sandbox", "ne", "").unwrap(),
            "sandbox-nexu-ne"
        );
        assert_eq!(
            nexus_legacy_agent_name("sandbox", "ne", "").unwrap(),
            "sandbox-orch-ne"
        );
        assert_eq!(
            nexus_agent_name("My Project", "cl", "").unwrap(),
            "my-project-3d7c-nexu-cl"
        );
        assert_eq!(
            nexus_legacy_agent_name("My Project", "cl", "").unwrap(),
            "my-project-3d7c-orch-cl"
        );
        assert_eq!(nexus_agent_name("demo", "gr", "dm").unwrap(), "dm-nexu-gr");
    }

    fn ok(payload: Value) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 0,
            stdout: String::new(),
            stderr: String::new(),
            payload: Some(payload),
            protocol_error: None,
        }
    }

    fn text(stdout: &str) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
            payload: None,
            protocol_error: None,
        }
    }

    fn failed(message: &str) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 1,
            stdout: String::new(),
            stderr: message.to_string(),
            payload: None,
            protocol_error: None,
        }
    }

    struct Scripted {
        replies: VecDeque<(&'static str, CommandResult)>,
        calls: Vec<String>,
    }

    impl Scripted {
        fn new(replies: Vec<(&'static str, CommandResult)>) -> Self {
            Self {
                replies: replies.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl Herdr for Scripted {
        fn call(
            &mut self,
            command: &argv::HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let action = command.argv[1..].join(" ");
            self.calls.push(action.clone());
            let (expected, reply) = self
                .replies
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call: {action}"));
            assert!(
                action.starts_with(expected),
                "expected a call starting with {expected:?}, got {action:?}"
            );
            Ok(reply)
        }
    }

    fn instant_timing() -> Timing {
        Timing {
            readiness: Duration::from_millis(50),
            prompt: Duration::from_millis(50),
            poll: Duration::from_millis(1),
        }
    }

    // --- workspace_matches_project ---------------------------------------

    #[test]
    fn matches_a_workspace_whose_label_and_a_pane_cwd_agree() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let workspace = json!({"label": "demo", "workspace_id": "w1"});
        let panes = vec![json!({"workspace_id": "w1", "foreground_cwd": dir.to_string_lossy()})];
        assert!(workspace_matches_project(&workspace, "demo", &root, &panes));
    }

    #[test]
    fn a_mismatched_label_never_matches_even_with_a_rooted_pane() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let workspace = json!({"label": "other", "workspace_id": "w1"});
        let panes = vec![json!({"workspace_id": "w1", "foreground_cwd": dir.to_string_lossy()})];
        assert!(!workspace_matches_project(
            &workspace, "demo", &root, &panes
        ));
    }

    #[test]
    fn a_pane_outside_the_project_root_never_matches() {
        let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let workspace = json!({"label": "demo", "workspace_id": "w1"});
        let panes = vec![json!({"workspace_id": "w1", "foreground_cwd": "/no/such/path-xyz"})];
        assert!(!workspace_matches_project(
            &workspace, "demo", &root, &panes
        ));
    }

    #[test]
    fn a_pane_in_a_different_workspace_is_not_ownership_evidence() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let workspace = json!({"label": "demo", "workspace_id": "w1"});
        let panes = vec![json!({"workspace_id": "w2", "foreground_cwd": dir.to_string_lossy()})];
        assert!(!workspace_matches_project(
            &workspace, "demo", &root, &panes
        ));
    }

    #[test]
    fn workspace_id_returns_the_matching_open_workspace() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let state = fleet(
            vec![json!({"label": "demo", "workspace_id": "w1"})],
            vec![json!({"workspace_id": "w1", "foreground_cwd": dir.to_string_lossy()})],
            Vec::new(),
        );
        assert_eq!(state.workspace_id("demo", &root).as_deref(), Some("w1"));
        assert!(state.is_open("demo", &root));
        assert!(state.workspace_id("other", &root).is_none());
    }

    // --- FleetState::orchestrator_agent ----------------------------------

    fn fleet(workspaces: Vec<Value>, panes: Vec<Value>, agents: Vec<Value>) -> FleetState {
        FleetState {
            workspaces,
            panes,
            agents,
        }
    }

    #[test]
    fn finds_the_named_agent_seated_in_the_project_s_open_workspace() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let state = fleet(
            vec![json!({"label": "demo", "workspace_id": "w1"})],
            vec![
                json!({"workspace_id": "w1", "pane_id": "w1:p1", "foreground_cwd": dir.to_string_lossy()}),
            ],
            vec![
                json!({"name": "demo-nexus", "pane_id": "w1:p1", "agent_status": "idle", "state_change_seq": 7}),
            ],
        );
        let found = state
            .orchestrator_agent("demo", &root, "demo-nexus")
            .expect("agent should be found");
        assert_eq!(found.pane_id, "w1:p1");
        assert_eq!(found.state, AgentState::Idle);
        assert_eq!(found.state_change_seq, 7);
    }

    #[test]
    fn a_differently_named_agent_in_the_same_pane_is_not_a_match() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let state = fleet(
            vec![json!({"label": "demo", "workspace_id": "w1"})],
            vec![
                json!({"workspace_id": "w1", "pane_id": "w1:p1", "foreground_cwd": dir.to_string_lossy()}),
            ],
            vec![
                json!({"name": "some-other-agent", "pane_id": "w1:p1", "agent_status": "idle", "state_change_seq": 7}),
            ],
        );
        assert!(state
            .orchestrator_agent("demo", &root, "demo-nexus")
            .is_none());
    }

    #[test]
    fn a_same_named_agent_seated_in_a_different_project_s_workspace_is_not_a_match() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let state = fleet(
            vec![
                json!({"label": "demo", "workspace_id": "w1"}),
                json!({"label": "other", "workspace_id": "w2"}),
            ],
            vec![
                json!({"workspace_id": "w1", "pane_id": "w1:p1", "foreground_cwd": dir.to_string_lossy()}),
                json!({"workspace_id": "w2", "pane_id": "w2:p1", "foreground_cwd": "/no/such/path-xyz"}),
            ],
            vec![
                json!({"name": "demo-nexus", "pane_id": "w2:p1", "agent_status": "idle", "state_change_seq": 7}),
            ],
        );
        assert!(state
            .orchestrator_agent("demo", &root, "demo-nexus")
            .is_none());
    }

    #[test]
    fn an_empty_roster_from_a_fetch_without_include_agents_never_matches() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let state = fleet(
            vec![json!({"label": "demo", "workspace_id": "w1"})],
            vec![
                json!({"workspace_id": "w1", "pane_id": "w1:p1", "foreground_cwd": dir.to_string_lossy()}),
            ],
            Vec::new(),
        );
        assert!(state
            .orchestrator_agent("demo", &root, "demo-nexus")
            .is_none());
    }

    #[test]
    fn find_orchestrator_agent_matches_a_python_era_orch_name() {
        let dir = std::env::temp_dir();
        let root = std::fs::canonicalize(&dir).unwrap();
        let current = nexus_agent_name("sandbox", "cl", "").unwrap();
        let legacy = nexus_legacy_agent_name("sandbox", "cl", "").unwrap();
        assert_eq!(current, "sandbox-nexu-cl");
        assert_eq!(legacy, "sandbox-orch-cl");
        let state = fleet(
            vec![json!({"label": "sandbox", "workspace_id": "w1"})],
            vec![json!({
                "workspace_id": "w1",
                "pane_id": "w1:p1",
                "foreground_cwd": dir.to_string_lossy()
            })],
            vec![json!({
                "name": legacy,
                "pane_id": "w1:p1",
                "agent_status": "idle",
                "state_change_seq": 3
            })],
        );
        let (matched, live) =
            find_orchestrator_agent(&state, "sandbox", &root, "cl", "").expect("legacy pane");
        assert_eq!(matched, legacy);
        assert_eq!(live.pane_id, "w1:p1");
        assert!(state.has_named_agent(&legacy));
        assert!(!state.has_named_agent(&current));
    }

    // --- create_workspace / close_workspace ------------------------------

    #[test]
    fn create_workspace_reads_the_sibling_root_pane() {
        let mut herdr = Scripted::new(vec![(
            "workspace create",
            ok(
                json!({"result": {"workspace": {"workspace_id": "w9"}, "root_pane": {"pane_id": "w9:p1"}}}),
            ),
        )]);
        let created = create_workspace(&mut herdr, "demo", Path::new("/tmp/demo")).unwrap();
        assert_eq!(created.workspace_id, "w9");
        assert_eq!(created.pane, "w9:p1");
    }

    #[test]
    fn create_workspace_without_a_root_pane_is_an_error() {
        let mut herdr = Scripted::new(vec![(
            "workspace create",
            ok(json!({"result": {"workspace": {"workspace_id": "w9"}}})),
        )]);
        let error = create_workspace(&mut herdr, "demo", Path::new("/tmp/demo")).unwrap_err();
        assert!(error.contains("no root pane"));
    }

    #[test]
    fn close_workspace_reports_herdrs_own_failure_message() {
        let mut herdr = Scripted::new(vec![("workspace close", failed("no such workspace"))]);
        let error = close_workspace(&mut herdr, "w9").unwrap_err();
        assert_eq!(error, "no such workspace");
    }

    // --- launch_and_seed ---------------------------------------------------

    #[test]
    fn a_clean_launch_lands_the_seed_on_the_first_prompt() {
        let mut herdr = Scripted::new(vec![
            ("pane run", text("")),
            (
                "agent list",
                ok(json!({"result": {"agents": [{"pane_id": "w1:p1", "agent_status": "idle"}]}})),
            ),
            ("agent rename", text("")),
            ("agent read", text("Ready >")),
            (
                "agent get",
                ok(json!({"result": {"agent": {"state_change_seq": 5}}})),
            ),
            ("agent prompt", ok(json!({"result": {"agent": {}}}))),
            (
                "agent get",
                ok(json!({"result": {"agent": {"state_change_seq": 6}}})),
            ),
        ]);
        let outcome = launch_and_seed(
            &mut herdr,
            "w1:p1",
            "claude",
            "demo-nexus",
            "read the seed",
            &instant_timing(),
        );
        assert!(outcome.seeded);
        assert!(outcome.note.is_empty());
    }

    #[test]
    fn a_trust_dialog_after_rename_is_reported_rather_than_typed_into() {
        let mut herdr = Scripted::new(vec![
            ("pane run", text("")),
            (
                "agent list",
                ok(json!({"result": {"agents": [{"pane_id": "w1:p1", "agent_status": "idle"}]}})),
            ),
            ("agent rename", text("")),
            ("agent read", text("Do you trust this folder?")),
        ]);
        let outcome = launch_and_seed(
            &mut herdr,
            "w1:p1",
            "claude",
            "demo-nexus",
            "read the seed",
            &instant_timing(),
        );
        assert!(!outcome.seeded);
        assert!(outcome.note.contains("trust dialog"));
    }

    #[test]
    fn a_prompt_that_never_lands_is_retried_once_then_reported_unseeded() {
        let mut herdr = Scripted::new(vec![
            ("pane run", text("")),
            (
                "agent list",
                ok(json!({"result": {"agents": [{"pane_id": "w1:p1", "agent_status": "idle"}]}})),
            ),
            ("agent rename", text("")),
            ("agent read", text("Ready >")),
            (
                "agent get",
                ok(json!({"result": {"agent": {"state_change_seq": 5}}})),
            ),
            ("agent prompt", ok(json!({"result": {"agent": {}}}))),
            (
                "agent get",
                ok(json!({"result": {"agent": {"state_change_seq": 5}}})),
            ),
            ("agent prompt", ok(json!({"result": {"agent": {}}}))),
            (
                "agent get",
                ok(json!({"result": {"agent": {"state_change_seq": 5}}})),
            ),
        ]);
        // A zero prompt budget means each attempt's landing loop checks
        // `state_change_seq` exactly once before giving up — otherwise the
        // poll loop would keep calling `agent get` until the scripted
        // transcript above ran out.
        let timing = Timing {
            prompt: Duration::ZERO,
            ..instant_timing()
        };
        let outcome = launch_and_seed(
            &mut herdr,
            "w1:p1",
            "claude",
            "demo-nexus",
            "read the seed",
            &timing,
        );
        assert!(!outcome.seeded);
        assert!(outcome.note.contains("twice"));
    }

    #[test]
    fn a_launch_command_that_fails_to_start_is_reported_without_waiting() {
        let mut herdr = Scripted::new(vec![("pane run", failed("not a shell"))]);
        let outcome = launch_and_seed(
            &mut herdr,
            "w1:p1",
            "claude",
            "demo-nexus",
            "read the seed",
            &instant_timing(),
        );
        assert!(!outcome.seeded);
        assert!(outcome.note.contains("failed to start"));
    }

    #[test]
    fn a_launch_script_runs_with_the_bare_command_in_failure_diagnostics() {
        let mut herdr = Scripted::new(vec![("pane run", failed("not a shell"))]);
        let script = "claude --disallowedTools 'Edit(//fleet/directives/**)'";
        let outcome = launch_script_and_seed(
            &mut herdr,
            "w1:p1",
            "claude",
            script,
            "demo-nexus",
            "read the seed",
            &instant_timing(),
        );
        assert_eq!(herdr.calls, vec![format!("pane run w1:p1 {script}")]);
        assert!(!outcome.seeded);
        assert_eq!(outcome.note, "claude failed to start: not a shell");
    }
}

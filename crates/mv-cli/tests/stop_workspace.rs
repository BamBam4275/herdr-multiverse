//! Card `20261116-b-cb6304`: fuzz saw one seed where Python's call list
//! for `stop <project> --force` included `workspace close w1` while
//! Rust's had none. These tests drive the real `multiverse` binary
//! against the repository's fake Herdr, with `MV_FAKE_HERDR_CALLS`
//! recording every argv Herdr actually received, and check whether
//! `workspace close` is among them for each shape that could plausibly
//! make `--force` skip it: a busy agent, an unrecognized agent name, a
//! workspace whose pane is already gone, and a lane holding unrescued
//! task output.
//!
//! Run with `cargo test -p mv-cli --test stop_workspace --locked`.

// This test only needs `Sandbox::new`/`herdr`/`command`/`assert_code`/`stdout`;
// `support`'s other helpers exist for `cli_workflows.rs`'s own coverage and
// would otherwise warn as unused in this binary's own compilation unit (each
// integration test compiles `mod support` fresh) — same reasoning
// `prompt_commands.rs` already states for itself.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;

use serde_json::Value;
use support::{assert_code, stdout, Sandbox};

/// Every argv Herdr actually received, in order, as logged by
/// `MV_FAKE_HERDR_CALLS` (one JSON array of strings per line — see
/// `fake-herdr.rs`'s own `call_log_includes_successful_reads_and_failed_mutations_in_order`).
fn logged_calls(path: &Path) -> Vec<Vec<String>> {
    if !path.exists() {
        return Vec::new();
    }
    fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn closed(calls: &[Vec<String>], workspace_id: &str) -> bool {
    calls.iter().any(|call| {
        call.len() == 3 && call[0] == "workspace" && call[1] == "close" && call[2] == workspace_id
    })
}

/// Opens a workspace labelled "sandbox" (matching the fixture project's
/// configured name — `stop`'s `agents_for` looks a workspace up by exact
/// label match) rooted at the sandbox project path, and returns
/// `(workspace_id, root_pane_id)`.
fn open_sandbox_workspace(sandbox: &Sandbox) -> (String, String) {
    let created = sandbox.herdr(&[
        "workspace",
        "create",
        "--label",
        "sandbox",
        "--cwd",
        sandbox.project.to_str().unwrap(),
        "--no-focus",
    ]);
    let created: Value = serde_json::from_slice(&created.stdout).unwrap();
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    (workspace_id, pane_id)
}

/// Rewrites one named agent's `agent_status` directly in the fake's
/// persisted state — the only way to reach "working"/"blocked"/"unknown"
/// in a test, since the fake only ever starts an agent as "idle" and has
/// no CLI surface to force a later status.
fn set_agent_status(sandbox: &Sandbox, agent_name: &str, status: &str) {
    let state_path = sandbox.root.join("fake-herdr.json");
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let agents = state["agents"].as_array_mut().unwrap();
    let agent = agents
        .iter_mut()
        .find(|a| a["name"] == agent_name)
        .expect("agent must exist before its status can be changed");
    agent["agent_status"] = Value::String(status.to_owned());
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn run_stop_force(sandbox: &Sandbox, calls_path: &Path) -> std::process::Output {
    sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("MV_FAKE_HERDR_CALLS", calls_path)
        .args(["stop", "sandbox", "--force"])
        .output()
        .expect("run built multiverse binary")
}

/// Baseline / regression pin: a busy (non-idle, non-done) agent must
/// never block `--force` from closing the workspace. This is the shape
/// the fuzz seed's harness most plausibly hit, and the one this task
/// turns into a permanent pin regardless of what caused the fuzz
/// divergence, per the brief's "no shape reproduces" instruction.
#[test]
fn stop_force_closes_a_workspace_with_a_busy_agent() {
    let sandbox = Sandbox::new();
    let calls_path = sandbox.root.join("calls.jsonl");
    let (workspace_id, pane_id) = open_sandbox_workspace(&sandbox);
    sandbox.herdr(&[
        "agent",
        "start",
        "busy-agent",
        "--kind",
        "codex",
        "--pane",
        &pane_id,
    ]);
    set_agent_status(&sandbox, "busy-agent", "working");

    let output = run_stop_force(&sandbox, &calls_path);
    assert_code(&output, 0);
    assert!(
        stdout(&output).contains("forcing: ending working agent busy-agent"),
        "{}",
        stdout(&output)
    );
    let calls = logged_calls(&calls_path);
    assert!(
        closed(&calls, &workspace_id),
        "stop --force must close the workspace over a busy agent: {calls:?}"
    );
}

/// Same shape, status "blocked" and "unknown" — the other two statuses
/// `LiveAgent::is_safe_to_stop` treats as busy.
#[test]
fn stop_force_closes_a_workspace_with_a_blocked_or_unknown_agent() {
    for status in ["blocked", "unknown"] {
        let sandbox = Sandbox::new();
        let calls_path = sandbox.root.join("calls.jsonl");
        let (workspace_id, pane_id) = open_sandbox_workspace(&sandbox);
        sandbox.herdr(&[
            "agent",
            "start",
            "busy-agent",
            "--kind",
            "codex",
            "--pane",
            &pane_id,
        ]);
        set_agent_status(&sandbox, "busy-agent", status);

        let output = run_stop_force(&sandbox, &calls_path);
        assert_code(&output, 0);
        assert!(
            stdout(&output).contains(&format!("forcing: ending {status} agent busy-agent")),
            "{}",
            stdout(&output)
        );
        let calls = logged_calls(&calls_path);
        assert!(
            closed(&calls, &workspace_id),
            "stop --force must close the workspace over a {status} agent: {calls:?}"
        );
    }
}

/// Shape: "an agent the roster doesn't recognize" — an agent name that
/// matches no configured role/chain naming convention at all. `stop`'s
/// busy check never consults the roster or `naming::agent_name`, only
/// Herdr's own reported status, so an unrecognized name should have no
/// effect on whether `--force` closes the workspace.
#[test]
fn stop_force_closes_a_workspace_with_an_unrecognized_agent_name() {
    let sandbox = Sandbox::new();
    let calls_path = sandbox.root.join("calls.jsonl");
    let (workspace_id, pane_id) = open_sandbox_workspace(&sandbox);
    sandbox.herdr(&[
        "agent",
        "start",
        "not-a-multiverse-role-name",
        "--kind",
        "codex",
        "--pane",
        &pane_id,
    ]);
    set_agent_status(&sandbox, "not-a-multiverse-role-name", "working");

    let output = run_stop_force(&sandbox, &calls_path);
    assert_code(&output, 0);
    assert!(
        stdout(&output).contains("forcing: ending working agent not-a-multiverse-role-name"),
        "an unrecognized name must still reach the forcing announcement unaltered: {}",
        stdout(&output)
    );
    let calls = logged_calls(&calls_path);
    assert!(
        closed(&calls, &workspace_id),
        "an agent name the roster does not recognize must not block --force: {calls:?}"
    );
}

/// Shape: "the workspace already partially closed" — the workspace's
/// only pane is closed (so `pane list` returns nothing for it) while the
/// workspace record itself is still open. `agents_for` finds the
/// workspace by label alone, independent of whether any of its panes
/// still exist, so `--force` should still close it.
#[test]
fn stop_force_closes_a_workspace_whose_only_pane_is_already_gone() {
    let sandbox = Sandbox::new();
    let calls_path = sandbox.root.join("calls.jsonl");
    let (workspace_id, pane_id) = open_sandbox_workspace(&sandbox);
    sandbox.herdr(&["pane", "close", &pane_id]);

    let output = run_stop_force(&sandbox, &calls_path);
    assert_code(&output, 0);
    let calls = logged_calls(&calls_path);
    assert!(
        closed(&calls, &workspace_id),
        "a workspace whose only pane is already closed must still close under --force: {calls:?}"
    );
}

fn git(sandbox: &Sandbox, root: &Path, args: &[&str]) {
    let output = sandbox
        .command("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Shape: "an unrescued-output lane" — a task worktree under the
/// project's `.multiverse/worktrees` holds an artifact that differs from
/// (or is absent from) the project root. `stop.rs`'s own module doc
/// (stop.rs:27-30) says this keep is intentional *even with* `--force`,
/// since forcing a close would destroy the only record of that task's
/// work. This test documents that the keep does skip `workspace close`
/// under `--force` by design, not as the bug this card is hunting —
/// contrast with the four shapes above, which must all close.
#[test]
fn stop_force_keeps_a_workspace_whose_lane_holds_unrescued_output() {
    let sandbox = Sandbox::new();
    let calls_path = sandbox.root.join("calls.jsonl");
    let (workspace_id, _pane_id) = open_sandbox_workspace(&sandbox);

    let worktrees = mv_state::worktrees::Worktrees::new(&sandbox.project).unwrap();
    let lane = worktrees.create("20260101-builder-aaaaaa", "HEAD").unwrap();
    let relative = Path::new(".multiverse/tasks/20260101-builder-cccccc/result.md");
    fs::create_dir_all(lane.path.join(relative).parent().unwrap()).unwrap();
    fs::write(lane.path.join(relative), "unrescued output\n").unwrap();
    git(
        &sandbox,
        &lane.path,
        &["add", "-f", relative.to_str().unwrap()],
    );
    git(
        &sandbox,
        &lane.path,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "unrescued result",
        ],
    );

    let output = run_stop_force(&sandbox, &calls_path);
    let text = stdout(&output);
    assert!(text.contains("holds unrescued output"), "{text}");
    let calls = logged_calls(&calls_path);
    assert!(
        !closed(&calls, &workspace_id),
        "an unrescued-output lane must keep --force from closing the workspace: {calls:?}"
    );
}

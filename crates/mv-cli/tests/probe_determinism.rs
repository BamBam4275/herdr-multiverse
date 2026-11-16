//! Card `20261106-b-11598f`: the Seer's four deterministic experiments
//! (`card-11598f-seer-ruling.md`, "Deterministic experiment"), made
//! permanent. Rust side only, against the repository's own `fake-herdr`
//! — the Python reference is never involved.
//!
//! The question behind all four: an extra `agent read <pane> --source
//! visible` sometimes appears on one side of a fuzz seed and not the
//! other after a bare (no-`--wait`) dispatch. The Seer's ruling is that
//! this is the `tasks`/`board` stalled-pane probe (`classify`,
//! `crates/mv-cli/src/commands/tasks.rs:231-311`, consumed by
//! `probe_blocking_prompt` at `tasks.rs:1429-1466`; `resolve_one_task` +
//! `probe_task_prompt`, `crates/mv-cli/src/commands/board_data.rs:2443-2482`),
//! not a wall-clock race in the current fake — `fake-herdr.rs` never
//! emits `working` and completes a prompt to `done` synchronously, so a
//! fresh bare dispatch leaves its task `unrecorded` (status file
//! accepted, no `event=result`), and both probes skip that task
//! entirely.
//!
//! A: the probe is a pure function of frozen inputs (two back-to-back
//!    reads against one frozen fleet issue identical Herdr argv).
//! B: fake-herdr does not settle `agent_status` on a timer.
//! C: the probe's own "is the pane `working`" gate, isolated against a
//!    fixed "no accepted status file" baseline.
//! D: the probe's own "is there an accepted status file" gate, isolated
//!    against a fixed `agent_status=done` baseline.
//!
//! Run with `cargo test -p mv-cli --test probe_determinism --locked`.

// This test only needs `Sandbox::new`/`cli`/`command`/`task_dirs`/
// `assert_code`; `support`'s other helpers exist for other integration
// binaries' own coverage and would otherwise warn as unused here, as
// `prompt_commands.rs`/`legacy_name_guard.rs`/`reclaim_pane_label.rs`
// already document next to their own identical `#[allow(dead_code)]`.
#[allow(dead_code)]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use support::{assert_code, Sandbox};

/// Every argv Herdr actually received, in order, as logged by
/// `MV_FAKE_HERDR_CALLS` (one JSON array of strings per line — see
/// `stop_workspace.rs`'s identical helper and `fake-herdr.rs`'s own
/// `call_log_includes_successful_reads_and_failed_mutations_in_order`).
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

/// How many times `calls` contains the probe's own `agent read <pane>
/// --source visible` shape for `pane_id` — the Seer ruling's "the call is
/// `agent read <pane> --source visible` addressed by pane id".
fn visible_reads_of(calls: &[Vec<String>], pane_id: &str) -> usize {
    calls
        .iter()
        .filter(|call| {
            call.len() == 5
                && call[0] == "agent"
                && call[1] == "read"
                && call[2] == pane_id
                && call[3] == "--source"
                && call[4] == "visible"
        })
        .count()
}

fn run_tasks(sandbox: &Sandbox, calls_path: &Path) -> std::process::Output {
    sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("MV_FAKE_HERDR_CALLS", calls_path)
        .args(["tasks", "--no-reconcile", "--read-only"])
        .output()
        .expect("run built multiverse binary")
}

fn run_board(sandbox: &Sandbox, calls_path: &Path) -> std::process::Output {
    sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("MV_FAKE_HERDR_CALLS", calls_path)
        .args([
            "board",
            "sandbox",
            "--no-reconcile",
            "--read-only",
            "--no-color",
        ])
        .output()
        .expect("run built multiverse binary")
}

fn fake_state_path(sandbox: &Sandbox) -> PathBuf {
    sandbox.root.join("fake-herdr.json")
}

fn read_state(sandbox: &Sandbox) -> Value {
    serde_json::from_slice(&fs::read(fake_state_path(sandbox)).unwrap()).unwrap()
}

/// Every pane id `agent list` currently reports — including
/// `Shape::Agent`'s own seeded demo fleet (`fake-herdr.rs`'s default
/// `State`), which a fresh sandbox already carries before this file's
/// own dispatch adds anything. A direct `herdr agent list` call works
/// even before any mutating call has persisted `MV_FAKE_HERDR_STATE` to
/// disk, unlike reading the state file directly.
fn live_pane_ids(sandbox: &Sandbox) -> std::collections::BTreeSet<String> {
    let output = sandbox.herdr(&["agent", "list"]);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|agent| agent["pane_id"].as_str().map(str::to_owned))
        .collect()
}

fn agent_status_of(sandbox: &Sandbox, pane_id: &str) -> String {
    let state = read_state(sandbox);
    state["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|agent| agent["pane_id"] == pane_id)
        .expect("dispatched agent must still exist")["agent_status"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Rewrites the one dispatched agent's `agent_status` directly in the
/// fake's persisted state, the same way `stop_workspace.rs`'s
/// `set_agent_status` does, but keyed by `pane_id` rather than agent
/// name — the Seer ruling's own experiment C/D description edits
/// "the post-dispatch `fleet.json`"'s pane entry directly.
fn set_agent_status(sandbox: &Sandbox, pane_id: &str, status: &str) {
    let state_path = fake_state_path(sandbox);
    let mut state: Value = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let agents = state["agents"].as_array_mut().unwrap();
    let agent = agents
        .iter_mut()
        .find(|agent| agent["pane_id"] == pane_id)
        .expect("dispatched agent must exist before its status can be changed");
    agent["agent_status"] = Value::String(status.to_owned());
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
}

fn status_file_path(sandbox: &Sandbox, task_id: &str) -> PathBuf {
    sandbox
        .project
        .join(".multiverse/tasks")
        .join(task_id)
        .join("status")
}

/// Renames the task's `status` file out of the way and returns where it
/// went, so `read_status`/`read_status_since` sees "absent" rather than
/// "unreadable" — the Seer ruling's "hidden" option (the other, mtime
/// backdating, exercises the same `read_status_since` rejection this
/// already does via a plain `NotFound`).
fn hide_status(status_path: &Path) -> PathBuf {
    let hidden = status_path.with_file_name("status.hidden");
    fs::rename(status_path, &hidden).expect("status file must exist before it can be hidden");
    hidden
}

fn restore_status(hidden_path: &Path, status_path: &Path) {
    fs::rename(hidden_path, status_path).expect("hidden status file must still exist");
}

/// A bare `dispatch --yes --timeout 2` (no `--wait`) against a fresh
/// sandbox: exactly one new task, exactly one new live agent (on top of
/// `Shape::Agent`'s own seeded demo fleet, present even in a fresh
/// sandbox). Confirms the Seer ruling's own inferred baseline along the
/// way — a successful fake dispatch leaves the new task `unrecorded`
/// (status file accepted, no `event=result`) rather than something this
/// file's later mutations would misread.
fn dispatch_bare(sandbox: &Sandbox) -> (String, String) {
    let before = live_pane_ids(sandbox);

    let output = sandbox.cli(&[
        "dispatch",
        sandbox.project.to_str().unwrap(),
        "Probe the tasks/board stalled-pane read determinism",
        "--role",
        "scout",
        "--timeout",
        "2",
        "--yes",
    ]);
    assert_code(&output, 0);

    let dirs = sandbox.task_dirs();
    assert_eq!(
        dirs.len(),
        1,
        "a bare dispatch against a fresh sandbox must create exactly one task: {dirs:?}"
    );
    let task_id = dirs[0].file_name().unwrap().to_str().unwrap().to_owned();

    assert_eq!(
        fs::read_to_string(status_file_path(sandbox, &task_id)).unwrap(),
        "done\n",
        "fake-herdr's complete_prompt only writes the sibling status when the prompt names an \
         existing brief.md; a bare dispatch must still reach that path"
    );

    let state = read_state(sandbox);
    let agents = state["agents"].as_array().unwrap();
    let new_agents: Vec<&Value> = agents
        .iter()
        .filter(|agent| {
            agent["pane_id"]
                .as_str()
                .is_some_and(|pane_id| !before.contains(pane_id))
        })
        .collect();
    assert_eq!(
        new_agents.len(),
        1,
        "a bare dispatch must add exactly one new live agent on top of the seeded demo fleet: \
         {new_agents:?}"
    );
    assert_eq!(new_agents[0]["agent_status"], "done");
    let pane_id = new_agents[0]["pane_id"].as_str().unwrap().to_owned();

    (task_id, pane_id)
}

/// Experiment A: "Prove the probe is a pure function of frozen inputs."
/// Two back-to-back `tasks`/`board` reads against the same unchanged
/// fleet must issue identical Herdr argv. Divergence would mean the
/// ruling is wrong and the missed input is inside the probe process
/// itself (reads do not mutate `fleet.json`, so nothing legitimately
/// distinguishes the first read from the second).
#[test]
fn experiment_a_probe_calls_are_identical_across_repeated_reads() {
    let sandbox = Sandbox::new();
    dispatch_bare(&sandbox);

    let tasks_first = sandbox.root.join("a-tasks-1.jsonl");
    let tasks_second = sandbox.root.join("a-tasks-2.jsonl");
    assert_code(&run_tasks(&sandbox, &tasks_first), 0);
    assert_code(&run_tasks(&sandbox, &tasks_second), 0);
    assert_eq!(
        logged_calls(&tasks_first),
        logged_calls(&tasks_second),
        "experiment A: two reads of `tasks --no-reconcile --read-only` against one frozen \
         fleet must be byte-identical in their Herdr argv"
    );

    let board_first = sandbox.root.join("a-board-1.jsonl");
    let board_second = sandbox.root.join("a-board-2.jsonl");
    assert_code(&run_board(&sandbox, &board_first), 0);
    assert_code(&run_board(&sandbox, &board_second), 0);
    assert_eq!(
        logged_calls(&board_first),
        logged_calls(&board_second),
        "experiment A (board side): two reads of \
         `board --no-reconcile --read-only --no-color` against one frozen fleet must be \
         byte-identical in their Herdr argv"
    );
}

/// Experiment B: "Prove fake-herdr does not settle on a timer." If this
/// fails, the brief says to stop and report it rather than writing C/D —
/// a changing `agent_status` across a plain sleep, with no herdr write in
/// between, would mean a race in the fake itself, not a classification
/// bug in the probe.
#[test]
fn experiment_b_fake_herdr_has_no_settle_timer() {
    let sandbox = Sandbox::new();
    let (task_id, pane_id) = dispatch_bare(&sandbox);
    let status_path = status_file_path(&sandbox, &task_id);

    let before_status = fs::read_to_string(&status_path).unwrap();
    let before_agent_status = agent_status_of(&sandbox, &pane_id);

    std::thread::sleep(Duration::from_secs(2));

    let after_status = fs::read_to_string(&status_path).unwrap();
    let after_agent_status = agent_status_of(&sandbox, &pane_id);

    assert_eq!(
        before_status, after_status,
        "experiment B: the task status file must not change across a 2s sleep with no herdr \
         call in between"
    );
    assert_eq!(
        before_agent_status, after_agent_status,
        "experiment B: fake-herdr has no settle timer, so `agent_status` must not change \
         across a 2s sleep with no herdr call in between; a change here would mean a real race"
    );
}

/// Experiment C: "Isolate the working gate." Fixed baseline: no accepted
/// status file (removed, so `classify`/`resolve_one_task` cannot reach
/// `unrecorded`/`recorded` and the pane's live status alone decides
/// `running` vs. `stalled`). Flip only `agent_status`: `working` must
/// never get the extra visible read; `idle` (not working, no accepted
/// status) must classify `stalled` and get exactly one.
#[test]
fn experiment_c_working_gate_skips_the_extra_read() {
    let sandbox = Sandbox::new();
    let (task_id, pane_id) = dispatch_bare(&sandbox);
    let status_path = status_file_path(&sandbox, &task_id);
    let hidden = hide_status(&status_path);

    set_agent_status(&sandbox, &pane_id, "working");
    let tasks_working = sandbox.root.join("c-tasks-working.jsonl");
    assert_code(&run_tasks(&sandbox, &tasks_working), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&tasks_working), &pane_id),
        0,
        "experiment C: a pane whose agent_status is `working` must never get the extra \
         visible read (tasks)"
    );
    let board_working = sandbox.root.join("c-board-working.jsonl");
    assert_code(&run_board(&sandbox, &board_working), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&board_working), &pane_id),
        0,
        "experiment C: a pane whose agent_status is `working` must never get the extra \
         visible read (board)"
    );

    set_agent_status(&sandbox, &pane_id, "idle");
    let tasks_idle = sandbox.root.join("c-tasks-idle.jsonl");
    assert_code(&run_tasks(&sandbox, &tasks_idle), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&tasks_idle), &pane_id),
        1,
        "experiment C: an idle, live pane with no accepted status file classifies `stalled` \
         and must get exactly one extra visible read (tasks); a count other than 1 while a \
         current-run status file is absent means classify is wrong"
    );
    let board_idle = sandbox.root.join("c-board-idle.jsonl");
    assert_code(&run_board(&sandbox, &board_idle), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&board_idle), &pane_id),
        1,
        "experiment C: an idle, live pane with no accepted status file must get exactly one \
         extra visible read (board)"
    );

    restore_status(&hidden, &status_path);
}

/// Experiment D: "Isolate status-file gating." Fixed baseline:
/// `agent_status=done` throughout (never touched). Flip only whether the
/// status file is accepted: hidden ⇒ `stalled` (live, done, not
/// working) ⇒ the extra read appears; restored ⇒ `unrecorded` ⇒ it
/// disappears.
#[test]
fn experiment_d_status_file_gates_the_extra_read() {
    let sandbox = Sandbox::new();
    let (task_id, pane_id) = dispatch_bare(&sandbox);
    let status_path = status_file_path(&sandbox, &task_id);

    let hidden = hide_status(&status_path);
    let tasks_hidden = sandbox.root.join("d-tasks-hidden.jsonl");
    assert_code(&run_tasks(&sandbox, &tasks_hidden), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&tasks_hidden), &pane_id),
        1,
        "experiment D: hiding the status file drops the task from `unrecorded` to `stalled` \
         (agent live and `done`, i.e. not working) and the extra read must appear (tasks)"
    );
    let board_hidden = sandbox.root.join("d-board-hidden.jsonl");
    assert_code(&run_board(&sandbox, &board_hidden), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&board_hidden), &pane_id),
        1,
        "experiment D: hiding the status file drops the task to `stalled` and the extra read \
         must appear (board)"
    );

    restore_status(&hidden, &status_path);
    let tasks_restored = sandbox.root.join("d-tasks-restored.jsonl");
    assert_code(&run_tasks(&sandbox, &tasks_restored), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&tasks_restored), &pane_id),
        0,
        "experiment D: restoring the status file returns the task to `unrecorded` and the \
         extra read must disappear (tasks)"
    );
    let board_restored = sandbox.root.join("d-board-restored.jsonl");
    assert_code(&run_board(&sandbox, &board_restored), 0);
    assert_eq!(
        visible_reads_of(&logged_calls(&board_restored), &pane_id),
        0,
        "experiment D: restoring the status file returns the task to `unrecorded` and the \
         extra read must disappear (board)"
    );
}

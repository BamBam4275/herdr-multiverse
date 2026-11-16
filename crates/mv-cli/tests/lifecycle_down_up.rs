//! `multiverse down`/`multiverse up` (card `20261116-b-f0612d`, Lane B):
//! the design's Lane B evidence list, driven against the real built
//! `multiverse` binary and the repository's `fake-herdr`, in isolated
//! config/state/HOME (`support::Sandbox`) — never the live fleet.
//!
//! Run with `cargo test -p mv-cli --test lifecycle_down_up --locked`.

#[allow(dead_code)]
mod support;

use std::fs;
use std::process::Output;

use serde_json::Value;
use support::{assert_code, stderr, stdout, Sandbox};

/// Adds a second, unused-in-any-chain interactive account so `start` opens
/// a real Nexus pane — same fixture `legacy_name_guard.rs`'s own
/// `enable_interactive_nexus_and_hub` builds, duplicated here rather than
/// imported (that file is a sibling test binary this crate does not
/// expose as a library; `support::Sandbox::seed_task`'s own doc comment
/// already documents this exact precedent).
fn enable_interactive_nexus(sandbox: &Sandbox) {
    let config_path = sandbox.root.join("config/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str(&format!(
        "\n[accounts.nexus-seat]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = '{codex_home}'\npool = \"codex:nexus\"\ninteractive = true\n",
        codex_home = sandbox.root.join("codex-home").display(),
    ));
    fs::write(&config_path, &config).unwrap();
    fs::copy(
        &config_path,
        sandbox.root.join("home/.config/multiverse/config.toml"),
    )
    .unwrap();
}

fn halt_file(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.root.join("runtime/halt")
}

fn sidecar(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.project.join(".multiverse/passdown.state")
}

fn passdown(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.project.join(".multiverse/PASSDOWN.md")
}

fn herdr_json(output: &Output) -> Value {
    serde_json::from_str(&stdout(output)).expect("fake-herdr reply must be JSON")
}

fn fake_state_path(sandbox: &Sandbox) -> std::path::PathBuf {
    sandbox.root.join("fake-herdr.json")
}

fn read_fake_state(sandbox: &Sandbox) -> Value {
    serde_json::from_slice(&fs::read(fake_state_path(sandbox)).unwrap())
        .expect("MV_FAKE_HERDR_STATE must be JSON")
}

fn write_fake_state(sandbox: &Sandbox, state: &Value) {
    fs::write(fake_state_path(sandbox), serde_json::to_vec(state).unwrap()).unwrap();
}

/// Sets a named agent's `agent_status` directly in the fake Herdr's own
/// persisted state (same "load, flip the field, write it back" shape
/// `fake-herdr`'s own
/// `empty_prompt_is_refused_and_enter_submits_the_pending_composer` test
/// uses) — nothing this fake does on its own ever produces a `working`
/// agent: `agent start` lands `idle`, `agent prompt`/`send-keys` land
/// `done` synchronously, with no asynchronous turn in between to catch
/// mid-flight.
fn set_agent_status(sandbox: &Sandbox, agent_name: &str, status: &str) {
    let mut state = read_fake_state(sandbox);
    let agent = state["agents"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|agent| agent["name"].as_str() == Some(agent_name))
        .unwrap_or_else(|| panic!("no agent named {agent_name} in fake Herdr state"));
    agent["agent_status"] = Value::String(status.to_string());
    write_fake_state(sandbox, &state);
}

/// Splits a second pane into `sandbox`'s already-open `"sandbox"`
/// workspace and starts a named agent in it there — a worker lane
/// sharing the Nexus's workspace, the shape `down`'s busy gate (card
/// `20261116-b-f0612d` Lane B fix round) must also keep the workspace
/// open for, not only the Nexus itself.
fn add_worker_agent(sandbox: &Sandbox, name: &str) {
    let state = read_fake_state(sandbox);
    let workspace_id = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["label"].as_str() == Some("sandbox"))
        .expect("sandbox workspace must already be open")["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let parent_pane = state["panes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pane| pane["workspace_id"].as_str() == Some(workspace_id.as_str()))
        .expect("sandbox workspace must already have a pane")["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let project = sandbox.project.to_str().unwrap();
    let split = sandbox.herdr(&[
        "pane",
        "split",
        "--pane",
        &parent_pane,
        "--direction",
        "right",
        "--cwd",
        project,
    ]);
    let pane_id = herdr_json(&split)["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    sandbox.herdr(&[
        "agent", "start", name, "--kind", "codex", "--pane", &pane_id,
    ]);
}

// ---------------------------------------------------------------------
// (a) `down --dry-run` writes no halt file and no sidecar.
// ---------------------------------------------------------------------

#[test]
fn down_dry_run_writes_no_halt_file_and_no_sidecar() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    let output = sandbox.cli(&["down", "sandbox", "--dry-run"]);
    assert_code(&output, 0);

    assert!(
        !halt_file(&sandbox).exists(),
        "dry-run must not write a halt file"
    );
    assert!(
        !sidecar(&sandbox).exists(),
        "dry-run must not write a sidecar"
    );
    assert_code(&sandbox.cli(&["halt", "status"]), 0);
    assert!(stdout(&sandbox.cli(&["halt", "status"])).contains("not halted"));
}

// ---------------------------------------------------------------------
// (b) `down` writes the halt with the `fleet down:` prefix and a
// `pending` sidecar, and delivers exactly one wrap prompt.
// (d) `down --timeout 1` on a sidecar that never changes exits 1, leaves
// the workspace open, and keeps the halt.
//
// One real Nexus pane never writes its own sidecar in this harness (the
// fake agent completes `agent prompt` synchronously but runs no actual
// wrap logic), so the same invocation proves both (b) and (d): the
// pending state (b) is captured by the same run that inevitably times
// out (d) once nothing ever flips the sidecar to `wrapped`.
// ---------------------------------------------------------------------

#[test]
fn down_halts_writes_pending_sidecar_delivers_one_wrap_prompt_then_times_out() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "1"]);
    let text = stdout(&output);
    assert_code(&output, 1);
    assert_eq!(
        text.matches("wrap prompt delivered").count(),
        1,
        "exactly one wrap prompt must be delivered to sandbox's Nexus: {text}"
    );
    assert!(text.contains("TIMEOUT"), "{text}");

    let halt = fs::read_to_string(halt_file(&sandbox)).unwrap();
    assert!(
        halt.contains("fleet down:"),
        "halt reason must carry the fleet down prefix: {halt}"
    );
    assert_eq!(
        fs::read_to_string(sidecar(&sandbox)).unwrap().trim(),
        "pending"
    );

    // The workspace is still open — `stop` finds it and closes it itself.
    let stopped = sandbox.cli(&["stop", "sandbox"]);
    assert_code(&stopped, 0);
    assert!(
        stdout(&stopped).contains("closed ("),
        "{}",
        stdout(&stopped)
    );

    // The halt down wrote is still in force.
    let status = sandbox.cli(&["halt", "status"]);
    assert!(stdout(&status).contains("fleet down:"));
    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// `down` must find a Nexus whose first seed never reached `agent rename`
// (card `20261116-b-457efa`): `start`'s own `pane rename` call already
// labelled the root pane, but nothing in this harness ever runs the
// orchestrator's own `agent rename` (a real first seed blocked on a
// folder-trust dialog has the same shape, card `20261108-b-e3ca35`).
// `down` must share `start --reseed`'s own labelled-pane fallback rather
// than reporting the pane as not found.
// ---------------------------------------------------------------------

/// Drops the Nexus's roster entry from the fake Herdr state while leaving
/// its pane's `label` untouched — `start`'s `pane rename` call and its
/// `agent rename` call land independently (`fleet_start::
/// launch_script_and_seed`), so this reproduces a first seed that never
/// reached the latter without needing a real blocked dialog. Matches by
/// `cwd` rather than clearing the whole roster: `fake-herdr`'s own default
/// state seeds unrelated demo agents for other fixtures.
fn unname_the_nexus_agent(sandbox: &Sandbox) {
    let mut state = read_fake_state(sandbox);
    let project = sandbox.project.to_str().unwrap();
    let agents = state["agents"].as_array_mut().unwrap();
    let before = agents.len();
    agents.retain(|agent| agent["cwd"].as_str() != Some(project));
    assert_eq!(
        agents.len(),
        before - 1,
        "exactly one agent (the Nexus) must be rooted in the sandbox project: {agents:?}"
    );
    write_fake_state(sandbox, &state);
}

#[test]
fn down_delivers_one_wrap_prompt_to_a_labelled_but_unrenamed_nexus_pane() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    unname_the_nexus_agent(&sandbox);

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "1"]);
    let text = stdout(&output);
    assert_code(&output, 1);
    assert!(
        !text.contains("UNREACHABLE"),
        "the labelled-pane fallback must find this Nexus even without a roster rename: {text}"
    );
    assert_eq!(
        text.matches("wrap prompt delivered").count(),
        1,
        "exactly one wrap prompt must be delivered to the labelled-but-unnamed Nexus pane: {text}"
    );
    assert!(text.contains("TIMEOUT"), "{text}");

    let stopped = sandbox.cli(&["stop", "sandbox"]);
    assert_code(&stopped, 0);
    sandbox.cli(&["halt", "off"]);
}

/// Clears the label `start`'s own `pane rename` left on the Nexus's pane,
/// the other half of [`find_reseed_target`]'s two-step lookup
/// (`unname_the_nexus_agent` above already undoes the roster-name half) —
/// with both gone, neither the named roster entry nor the labelled-pane
/// fallback can find this project's Nexus, which is what `down <p>` must
/// report as `UNREACHABLE` (card `20261116-b-8a45ab`) rather than finding
/// some other pane by accident.
fn blank_the_nexus_pane_label(sandbox: &Sandbox) {
    let mut state = read_fake_state(sandbox);
    let workspace_id = state["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|workspace| workspace["label"].as_str() == Some("sandbox"))
        .expect("sandbox workspace must already be open")["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let panes = state["panes"].as_array_mut().unwrap();
    let mut blanked = 0usize;
    for pane in panes.iter_mut() {
        if pane["workspace_id"].as_str() == Some(workspace_id.as_str()) {
            pane["label"] = Value::String(String::new());
            blanked += 1;
        }
    }
    assert!(
        blanked > 0,
        "the sandbox workspace must have at least one pane to blank"
    );
    write_fake_state(sandbox, &state);
}

// ---------------------------------------------------------------------
// `down` must not wait on a project whose Nexus is UNREACHABLE (card
// `20261116-b-8a45ab`): `deliver_wrap_prompt`'s `Err` branch used to still
// push the project onto `waiting`, so `wait_until` polled out the whole
// `--timeout` and then reported `TIMEOUT` on top of the `UNREACHABLE` it
// had already printed. An unreachable project is reported exactly once,
// is never waited on, and its workspace stays open.
// ---------------------------------------------------------------------

#[test]
fn down_does_not_wait_on_an_unreachable_project() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    unname_the_nexus_agent(&sandbox);
    blank_the_nexus_pane_label(&sandbox);

    let started = std::time::Instant::now();
    let output = sandbox.cli(&["down", "sandbox", "--timeout", "600"]);
    let elapsed = started.elapsed();
    let text = stdout(&output);

    // The real proof is the absence of a TIMEOUT line below; this bound is
    // a generous sanity margin (a fifth of --timeout) against the command
    // hanging, not the thing under test, so widening host load cannot flake
    // it without the command actually waiting out a meaningful fraction of
    // --timeout.
    assert!(
        elapsed.as_secs() < 120,
        "an UNREACHABLE project must not wait out the --timeout: {elapsed:?}, {text}"
    );
    assert_eq!(
        text.matches("UNREACHABLE").count(),
        1,
        "must be reported exactly once: {text}"
    );
    assert!(
        !text.contains("TIMEOUT"),
        "an UNREACHABLE project must never also be reported TIMEOUT: {text}"
    );
    assert_code(&output, 1);

    let state = read_fake_state(&sandbox);
    assert!(
        state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workspace| workspace["label"].as_str() == Some("sandbox")),
        "an unreachable project's workspace must stay open: {text}"
    );

    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// (c) a sidecar that flips to `wrapped` with a non-empty PASSDOWN.md
// makes `down` close the workspace and exit 0 — here pre-written before
// `down` runs, so there is nothing left to poll for; the polling
// mechanism itself (noticing a *later* write across several polls) is
// proved at the unit level in `commands::lifecycle`'s own tests.
// ---------------------------------------------------------------------

#[test]
fn down_closes_an_already_wrapped_project_and_exits_ok() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(passdown(&sandbox), "## STATE\n\nNothing to restart.\n").unwrap();
    fs::write(sidecar(&sandbox), "wrapped").unwrap();

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "2"]);
    let text = stdout(&output);
    assert_code(&output, 0);
    assert!(text.contains("already wrapped"), "{text}");
    assert!(
        !text.contains("wrap prompt delivered"),
        "an already-wrapped project must not be re-prompted: {text}"
    );
    assert!(text.contains("closed"), "{text}");

    let stopped = sandbox.cli(&["stop", "sandbox"]);
    assert_code(&stopped, 0);
    assert!(
        stdout(&stopped).contains("no project workspaces are open"),
        "{}",
        stdout(&stopped)
    );
    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// `down` must not close a workspace while ANY agent in it is busy (card
// `20261116-b-f0612d` Lane B fix round): a second, still-working agent
// sharing the Nexus's workspace must keep a wrapped project open until
// it settles — the same busy rule `stop` applies to every live agent,
// not a Nexus-only check.
// ---------------------------------------------------------------------

#[test]
fn down_keeps_the_workspace_open_while_a_second_agent_is_still_working() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(passdown(&sandbox), "## STATE\n\nNothing to restart.\n").unwrap();
    fs::write(sidecar(&sandbox), "wrapped").unwrap();

    let worker = "busy-worker";
    add_worker_agent(&sandbox, worker);
    set_agent_status(&sandbox, worker, "working");

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "1"]);
    let text = stdout(&output);
    assert_code(&output, 1);
    assert!(
        text.contains("KEPT") && text.contains("working"),
        "a busy worker, not only the Nexus, must keep the workspace open: {text}"
    );
    assert!(
        text.contains("closed 0 of 1"),
        "down must not close a workspace while a worker is still working: {text}"
    );

    // Genuinely still open: `workspace close` never reached fake Herdr.
    let state = read_fake_state(&sandbox);
    assert!(
        state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workspace| workspace["label"].as_str() == Some("sandbox")),
        "down must not have closed the workspace while the worker was busy"
    );

    // Once the worker settles, a repeat `down` against the still-wrapped
    // sidecar closes the workspace and exits 0.
    set_agent_status(&sandbox, worker, "idle");
    let output = sandbox.cli(&["down", "sandbox", "--timeout", "2"]);
    assert_code(&output, 0);
    assert!(stdout(&output).contains("closed"), "{}", stdout(&output));

    let state = read_fake_state(&sandbox);
    assert!(
        !state["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|workspace| workspace["label"].as_str() == Some("sandbox")),
        "the workspace must be closed once the worker is no longer busy"
    );

    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// `down` on a project with no open Nexus is skipped with a warning and
// exits 1 (design decision 5 / Q4) — it never starts an agent itself.
// ---------------------------------------------------------------------

#[test]
fn down_skips_a_project_with_no_open_nexus_and_exits_nonzero() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["down", "sandbox"]);
    assert_code(&output, 1);
    assert!(
        stdout(&output).contains("no open Nexus"),
        "{}",
        stdout(&output)
    );
    // The halt is still written fleet-wide even when every project is
    // skipped — only dispatch is refused; starting one is not this
    // command's job.
    assert!(halt_file(&sandbox).exists());
    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// Security lane A (card `20261116-b-9a5c0c`): a pre-planted symlink at
// the sidecar path, or at `.multiverse` itself, must be refused rather
// than followed — `write_sidecar` checks both before it creates, chmods,
// or writes anything (`crates/mv-cli/src/commands/lifecycle.rs`).
// ---------------------------------------------------------------------

#[test]
fn down_refuses_a_symlinked_sidecar_and_leaves_its_target_untouched() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    let outside = sandbox.root.join("outside-sidecar-target.txt");
    fs::write(&outside, "do not touch").unwrap();
    std::os::unix::fs::symlink(&outside, sidecar(&sandbox)).unwrap();

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "1"]);
    assert_code(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("sandbox") && text.contains("could not write passdown.state"),
        "the project must be reported as refused, not silently skipped: {text}"
    );
    assert_eq!(
        fs::read_to_string(&outside).unwrap(),
        "do not touch",
        "a refused write must never reach the symlink's target"
    );
    assert!(
        fs::symlink_metadata(sidecar(&sandbox))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the planted symlink itself must be left exactly as it was"
    );

    let stopped = sandbox.cli(&["stop", "sandbox"]);
    assert_code(&stopped, 0);
    sandbox.cli(&["halt", "off"]);
}

#[test]
fn down_refuses_a_symlinked_multiverse_directory_and_leaves_its_target_untouched() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus(&sandbox);
    assert_code(&sandbox.cli(&["start", "sandbox"]), 0);

    // `start` already created a real `.multiverse/` for this project —
    // replace it with a symlink to reproduce the pre-planted case
    // `write_sidecar` must still catch before it ever calls
    // `set_permissions` or opens the sidecar file.
    let multiverse_dir = sandbox.project.join(".multiverse");
    fs::remove_dir_all(&multiverse_dir).unwrap();
    let outside = sandbox.root.join("outside-multiverse-target");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("sentinel.txt"), "do not touch").unwrap();
    std::os::unix::fs::symlink(&outside, &multiverse_dir).unwrap();

    let output = sandbox.cli(&["down", "sandbox", "--timeout", "1"]);
    assert_code(&output, 1);
    let text = stdout(&output);
    assert!(
        text.contains("sandbox") && text.contains("could not write passdown.state"),
        "the project must be reported as refused, not silently skipped: {text}"
    );
    assert!(
        !outside.join("passdown.state").exists(),
        "the write must never land inside the symlink's target directory"
    );
    assert_eq!(
        fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
        "do not touch"
    );

    // `stop`'s own lane-output check (`lanes_holding_unrescued_output`)
    // walks `.multiverse/tasks` and refuses the same symlink on its own
    // terms — put a real directory back so cleanup can proceed; that
    // refusal is this test's confirmation that the planted symlink was
    // never replaced by `down`, not a second finding to assert on here.
    fs::remove_file(&multiverse_dir).unwrap();
    fs::create_dir_all(&multiverse_dir).unwrap();

    let stopped = sandbox.cli(&["stop", "sandbox"]);
    assert_code(&stopped, 0);
    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// (e) `up` does not clear a `maintenance` halt.
// ---------------------------------------------------------------------

#[test]
fn up_does_not_clear_a_maintenance_halt() {
    let sandbox = Sandbox::new();
    assert_code(&sandbox.cli(&["halt", "on", "--reason", "maintenance"]), 0);

    let output = sandbox.cli(&["up"]);
    assert_code(&output, 0);

    let status = stdout(&sandbox.cli(&["halt", "status"]));
    assert!(
        status.contains("maintenance"),
        "a maintenance halt must survive `up`: {status}"
    );
    sandbox.cli(&["halt", "off"]);
}

// ---------------------------------------------------------------------
// (f) an unknown project exits 3, for both `down` and `up`.
// ---------------------------------------------------------------------

#[test]
fn down_on_an_unknown_project_exits_three() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["down", "does-not-exist"]);
    assert_code(&output, 3);
    assert!(
        !halt_file(&sandbox).exists(),
        "an unknown name must refuse before any write"
    );
}

#[test]
fn up_on_an_unknown_project_exits_three() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["up", "does-not-exist"]);
    assert_code(&output, 3);
    assert!(
        stderr(&output).contains("does-not-exist"),
        "refusal must name the unknown project: {}",
        stderr(&output)
    );
}

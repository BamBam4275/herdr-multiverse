//! Card `20261116-b-34a189`: proof that `tasks --reclaimable` trusts a
//! live pane as its own recorded task's occupant when that pane's label
//! still names the task (`mv_state::tasks::reclaimable_panes`'s
//! `label_names_task` gate, fed by `AgentPanes::pane_label` -> `pane get`).
//! The sibling fixture in `legacy_name_guard.rs`
//! (`seed_reclaimable_legacy_fixer_task`) dispatches with no `agent=` at
//! all, which short-circuits that gate entirely (`task.agent.is_none()` ->
//! `(true, true)`); this file is the one integration test that actually
//! drives the label-matching path end to end, against the real built
//! `fake-herdr` binary.
//!
//! Run with `cargo test -p mv-cli --test reclaim_pane_label --locked`.

// `support`'s other helpers (`cli_without_curl`, `dispatch`,
// `dispatch_background_with_slow_pane_layout`, `snapshot`, `task_dirs`,
// `seed_task`) exist for other integration binaries' own coverage; each
// integration test compiles `mod support` fresh, so unused ones would
// otherwise warn here, as `prompt_commands.rs`/`legacy_name_guard.rs`
// already document next to their own identical `#[allow(dead_code)]`.
#[allow(dead_code)]
mod support;

use std::fs;

use serde_json::Value;
use support::{assert_code, stdout, Sandbox};

/// A single recorded, done `builder` task whose pane is still live and
/// whose label (set the same way `mv_engine::runner::pane_rename` labels a
/// real dispatch, via `runner::pane_label`'s own
/// `"{agent} · {program} · {model} · {seat} · {role} {task_id}"` shape)
/// still names this exact task. No `branch=` and no
/// `.multiverse/worktrees/<task>` lane, so `reclaim_lane_is_safe` finds
/// nothing to check and falls straight to the occupancy witness this test
/// exists to exercise.
fn seed_live_labelled_builder_task(sandbox: &Sandbox) -> String {
    let workspace = sandbox.herdr(&[
        "workspace",
        "create",
        "--label",
        "reclaim-label",
        "--cwd",
        sandbox.project.to_str().unwrap(),
    ]);
    let workspace: Value = serde_json::from_slice(&workspace.stdout).expect("JSON reply");
    let pane_id = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace create returns a root pane id")
        .to_string();

    let task_id = "20250101-builder-aaaaaa".to_string();
    let agent_name = "x-build-cl";
    sandbox.herdr(&[
        "agent", "start", agent_name, "--pane", &pane_id, "--kind", "codex",
    ]);
    let label =
        format!("{agent_name} · claude-interactive · test-model · test-seat · builder {task_id}");
    sandbox.herdr(&["pane", "rename", &pane_id, &label]);

    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/routing.log"),
        format!(
            "2025-01-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\tproject=sandbox\trole=builder\taccount=test-seat\tpool=codex:integration\toutcome=chain-primary\tagent={agent_name}\n\
             2025-01-01T00:01:00+00:00\tevent=started\ttask={task_id}\tpane={pane_id}\n\
             2025-01-01T00:02:00+00:00\tevent=result\ttask={task_id}\tstatus=done\tstate=done\trevisions=0\n"
        ),
    )
    .unwrap();
    task_id
}

#[test]
fn reclaimable_prints_would_close_for_a_live_pane_whose_label_still_names_its_task() {
    let sandbox = Sandbox::new();
    let task_id = seed_live_labelled_builder_task(&sandbox);

    let output = sandbox.cli(&["tasks", "sandbox", "--no-reconcile", "--reclaimable"]);
    assert_code(&output, 0);
    let text = stdout(&output);

    assert!(
        text.contains(&format!("would close {task_id} ")),
        "a live pane whose label still names its own dispatching task must be \
         listed as a reclaim candidate (\"would close\"), not stale bookkeeping: {text}"
    );
    assert!(
        !text.contains("stale bookkeeping"),
        "a confirmed occupant (matching name and pane label) must never fall \
         through to the stale-bookkeeping list: {text}"
    );
}

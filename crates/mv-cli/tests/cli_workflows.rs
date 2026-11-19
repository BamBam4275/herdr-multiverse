//! Public-process contracts, independent of the CLI's private command traits.
//! Run with `cargo test -p mv-cli --test cli_workflows --locked`.
//! Ignored cases contain the intended acceptance assertions, not stub success
//! checks; remove each ignore when its production CLI adapter is bound.

mod support;

use std::fs;
use std::path::{Path, PathBuf};

use support::{assert_code, stderr, stdout, Sandbox};

#[test]
fn council_requires_yes_before_any_side_effect() {
    let sandbox = Sandbox::new();
    let before = sandbox.snapshot();
    let output = sandbox.cli(&[
        "council",
        sandbox.project.to_str().unwrap(),
        "Which approach should we take?",
    ]);
    assert_code(&output, 1);
    assert!(stdout(&output).is_empty());
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("council requires --yes"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("--dry-run"), "{diagnostic}");
    assert_eq!(sandbox.snapshot(), before);
}

#[test]
fn dispatch_requires_yes_before_any_side_effect() {
    let sandbox = Sandbox::new();
    let before = sandbox.snapshot();
    let output = sandbox.dispatch("Map the repository", &[]);
    assert_code(&output, 1);
    assert!(stdout(&output).is_empty());
    assert!(stderr(&output).contains("dispatch requires --yes"));
    assert_eq!(sandbox.snapshot(), before);
}

/// Card `20261115-b-c8bcb5`: dispatch must not hold the project's
/// `routing.log.lock` across a Herdr call. `mv_engine::runner::start_dispatch`
/// scopes that lock to `prepare`/`validate`/`write_brief` only — pure
/// filesystem work — and drops it before the first Herdr round trip pane
/// placement makes (`pane layout`). A fake Herdr that pauses on exactly that
/// call — held open by a release signal this test controls, not a sleep —
/// lets a concurrent `activity send` into the same project (which takes the
/// identical lock file under `mv_state::activity`'s own "activity" name)
/// prove the lock is already free: the proof is an ordering observation
/// against the layout call's own start/end marker files, not a wall-clock
/// bound, so it cannot flake under load. Before this scoping existed, the
/// send would block until dispatch's pane layout call — and so the whole
/// lock-holding span — had finished, meaning `layout_finished` would already
/// exist by the time `send` returned. If a regression reintroduces that
/// blocking, `send` here blocks on the lock instead of returning, so this
/// test never reaches the point of releasing the paused layout call; that
/// pause is itself bounded (5s, see
/// `support::Sandbox::dispatch_background_with_slow_pane_layout`), so the
/// paused call proceeds on its own after that bound, `layout_finished` is
/// already present by the time the blocked `send` returns, and the
/// assertion below fails — rather than the test hanging.
#[test]
fn dispatch_releases_the_project_lock_before_pane_creation() {
    let sandbox = Sandbox::new();
    let dispatch = sandbox.dispatch_background_with_slow_pane_layout("Map the repository");

    // Let prepare/validate/write_brief — the only span that should ever
    // hold the project lock — finish before racing the slow pane layout
    // call this test is actually about.
    let tasks_dir = sandbox.project.join(".multiverse/tasks");
    let brief_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if tasks_dir
            .read_dir()
            .is_ok_and(|mut entries| entries.next().is_some())
        {
            break;
        }
        assert!(
            std::time::Instant::now() < brief_deadline,
            "dispatch never wrote its task directory"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    // Wait for the pane layout call itself to begin pausing, so the absence
    // of `layout_finished` checked below reflects "still in progress," not
    // "hasn't started yet."
    let layout_deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        if dispatch.layout_started.exists() {
            break;
        }
        assert!(
            std::time::Instant::now() < layout_deadline,
            "dispatch never reached the slow pane layout call"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }

    let send = sandbox.cli_in(
        &sandbox.recipient,
        &[
            "activity",
            "send",
            "sandbox",
            "concurrent while dispatch is slow",
        ],
    );
    assert_code(&send, 0);
    assert!(
        !dispatch.layout_finished.exists(),
        "activity send only completed after dispatch's pane layout call finished \
         pausing — dispatch must have still been holding routing.log.lock \
         across that Herdr call"
    );
    dispatch.release_pane_layout();

    let output = dispatch
        .child
        .wait_with_output()
        .expect("dispatch must exit");
    assert_code(&output, 0);
}

#[test]
fn retired_role_inputs_exit_three_and_hint_at_current_names() {
    let sandbox = Sandbox::new();
    let before = sandbox.snapshot();
    for (old, current) in [
        ("explorer", "scout"),
        ("fixer", "builder"),
        ("librarian", "archivist"),
        ("reviewer", "critic"),
        ("oracle", "seer"),
        ("designer", "artisan"),
        ("observer", "watcher"),
    ] {
        for prefix in [
            vec![
                "dispatch",
                sandbox.project.to_str().unwrap(),
                "Map the repository",
                "--yes",
            ],
            vec!["backlog", "add", "Track the issue"],
            vec![
                "backlog",
                "ask",
                "Choose an approach",
                "--option",
                "first",
                "--option",
                "second",
                "--recommend",
                "1",
            ],
        ] {
            let mut args = prefix;
            args.extend(["--role", old]);
            let output = sandbox.cli(&args);
            assert_code(&output, 3);
            assert!(stdout(&output).is_empty());
            let diagnostic = stderr(&output);
            assert!(diagnostic.contains("unknown role"), "{diagnostic}");
            assert!(diagnostic.contains(old), "{diagnostic}");
            assert!(diagnostic.contains(current), "{diagnostic}");
            assert_eq!(sandbox.snapshot(), before, "{args:?} changed sandbox files");
        }
    }
}

#[test]
fn fake_fixture_completes_only_a_brief_inside_the_sandbox() {
    let sandbox = Sandbox::new();
    let before = stdout(&sandbox.herdr(&["pane", "list"]));
    sandbox.herdr(&[
        "pane",
        "split",
        "--pane",
        "w52:p1",
        "--direction",
        "right",
        "--cwd",
        sandbox.project.to_str().unwrap(),
        "--no-focus",
    ]);
    let panes = stdout(&sandbox.herdr(&["pane", "list"]));
    assert_ne!(panes, before);
    assert!(panes.contains("w52:p100"));
    sandbox.herdr(&[
        "agent",
        "start",
        "integration-worker",
        "--kind",
        "codex",
        "--pane",
        "w52:p100",
    ]);
    let directory = sandbox.project.join("fixture task");
    fs::create_dir(&directory).unwrap();
    let brief = directory.join("brief.md");
    fs::write(&brief, "# Fixture task\n").unwrap();
    sandbox.herdr(&[
        "agent",
        "prompt",
        "integration-worker",
        &format!("Read `{}` and execute it.", brief.display()),
    ]);
    assert_eq!(
        fs::read_to_string(directory.join("status")).unwrap(),
        "done\n"
    );
    assert!(fs::read_to_string(directory.join("result.md"))
        .unwrap()
        .contains("Task completed."));
    let agent = stdout(&sandbox.herdr(&["agent", "get", "integration-worker"]));
    assert!(agent.contains("\"agent_status\":\"done\""), "{agent}");
    sandbox.herdr(&["pane", "close", "w52:p100"]);
    assert_eq!(stdout(&sandbox.herdr(&["pane", "list"])), before);
}

#[test]
fn halt_on_status_and_off_round_trip_only_in_isolated_runtime_state() {
    let sandbox = Sandbox::new();
    let before = sandbox.snapshot();
    let initial = sandbox.cli(&["halt", "status"]);
    assert_code(&initial, 0);
    assert!(stdout(&initial).contains("not halted"));
    assert_eq!(sandbox.snapshot(), before);

    let on = sandbox.cli(&["halt", "on", "--reason", "integration maintenance"]);
    assert_code(&on, 0);
    assert!(stdout(&on).contains("fleet halted since"));
    assert!(stdout(&on).contains("integration maintenance"));
    let halted = sandbox.snapshot();
    let changed: Vec<_> = halted
        .iter()
        .filter(|(path, bytes)| before.get(*path) != Some(*bytes))
        .collect();
    assert!(!changed.is_empty(), "halt must persist its state");
    assert!(changed.iter().all(|(path, _)| path.starts_with("runtime")));
    for (path, bytes) in &before {
        assert_eq!(
            halted.get(path),
            Some(bytes),
            "halt changed an existing fixture file"
        );
    }
    let status = sandbox.cli(&["halt", "status"]);
    assert_code(&status, 0);
    assert_eq!(stdout(&status), stdout(&on));
    assert_eq!(sandbox.snapshot(), halted);

    let off = sandbox.cli(&["halt", "off"]);
    assert_code(&off, 0);
    let status = sandbox.cli(&["halt", "status"]);
    assert_code(&status, 0);
    assert_eq!(stdout(&status), stdout(&initial));
    assert!(!sandbox.root.join("fake-herdr.json").exists());
    assert!(!sandbox.project.join(".multiverse").exists());
}

/// Python's `_sweep` (`cli.py:12592-12594`) only prints `_fleet_halt_line`
/// `if halt is not None` — an unhalted fleet gets no banner line at all,
/// never an explicit "fleet is not halted." (that text is `halt status`'s
/// own, a different command). Found via `conformance/diff_fuzz.py` seed
/// 50001.
#[test]
fn sweep_prints_no_halt_banner_when_the_fleet_is_not_halted() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["sweep", "--dry-run"]);
    assert_code(&output, 0);
    assert!(
        !stdout(&output).contains("not halted"),
        "{}",
        stdout(&output)
    );
}

/// Regression for card `20261102-b-a61e83`: `fake-herdr` now implements
/// `plugin config-dir <id>` alongside `--version`, so `doctor`'s
/// herdr-reachable check has a happy path to run against. Before this test,
/// nothing in this suite ever drove `doctor::run`'s success line at all —
/// a regression there (a broken version parse, a dropped `plugin`
/// subcommand call) would have shipped unnoticed.
#[test]
fn doctor_reports_herdr_reachable_against_the_fake_binary() {
    let sandbox = Sandbox::new();
    let doctor = sandbox.cli(&["doctor"]);
    let output = stdout(&doctor);
    assert!(
        output.contains("herdr 0.9.3, minimum 0.9.0"),
        "doctor did not report the herdr-reachable success line: {output}"
    );
    assert!(
        !output.contains("herdr reachable"),
        "doctor fell back to the herdr-unreachable line: {output}"
    );
}

/// Was `#[ignore]`d (task `20261102-fixer-d5b3b8`): the real planner used to
/// render the retired role name (`Explorer`) in the `--dry-run` card, to
/// match the `dispatch-*.json` conformance scenarios it was scoped to — see
/// `commands::dispatch`'s module doc. Task `20261103-fixer-fa9c70` made
/// dry-run and a real dispatch render the same, current name on every path
/// (release policy: no aliases, old names shown only via a history read),
/// moving the conformance bridge to `conformance/role_translate`'s
/// replay-time translation instead — this fixture's `config.toml` already
/// uses the renamed key (`scout`), so this assertion now matches the card.
#[test]
fn dispatch_dry_run_prints_plan_without_creating_files_or_panes() {
    let sandbox = Sandbox::new();
    let panes = stdout(&sandbox.herdr(&["pane", "list"]));
    let before = sandbox.snapshot();
    let output = sandbox.dispatch("Map the repository", &["--dry-run"]);
    assert_code(&output, 0);
    let plan = stdout(&output);
    assert!(plan.contains("scout") || plan.contains("Scout"), "{plan}");
    assert!(plan.contains("test-seat"), "{plan}");
    assert!(
        plan.contains("Would dispatch") || plan.contains("DRY RUN"),
        "{plan}"
    );
    assert_eq!(sandbox.snapshot(), before);
    assert_eq!(stdout(&sandbox.herdr(&["pane", "list"])), panes);
}

/// Task `20261106-fixer-7c20f3`: a `--wait` dispatch that settles `done` now
/// closes its own pane immediately (`runner::teardown`, matching Python's
/// `close_pane_when_done` in `runner.py:2906-2907`), so the pane this
/// dispatch created is already gone from `pane list` by the time the CLI
/// call returns — there is no live pane left to read a terminal
/// `agent_status` from. The routing log's own `event=started ... pane=...`
/// line is the still-available evidence a pane existed at all; the on-disk
/// `status`/`result.md` files (checked below) are the terminal status
/// itself, exactly as `wait_for_result`'s own usable-result check reads it.
#[test]
fn fake_dispatch_writes_brief_creates_pane_and_reads_terminal_status() {
    let sandbox = Sandbox::new();
    let panes = stdout(&sandbox.herdr(&["pane", "list"]));
    let output = sandbox.dispatch("Map the repository", &["--yes", "--wait"]);
    assert_code(&output, 0);
    let directories = sandbox.task_dirs();
    assert_eq!(directories.len(), 1);
    let task = &directories[0];
    let brief = fs::read_to_string(task.join("brief.md")).unwrap();
    assert!(brief.contains("Map the repository"), "{brief}");
    assert!(
        brief.contains("Scout") || brief.contains("scout"),
        "{brief}"
    );
    assert_eq!(
        fs::read_to_string(task.join("status")).unwrap().trim(),
        "done"
    );
    assert!(fs::read_to_string(task.join("result.md"))
        .unwrap()
        .contains("Task completed."));
    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(log.contains("event=dispatch"), "{log}");
    assert!(
        log.contains("event=started\ttask=") && log.contains("\tpane=w52:p100"),
        "{log}"
    );
    assert!(
        log.contains(task.file_name().unwrap().to_str().unwrap()),
        "{log}"
    );
    let after = stdout(&sandbox.herdr(&["pane", "list"]));
    assert_eq!(
        after, panes,
        "a settled done dispatch tears its own pane down, leaving the \
         workspace exactly as it was before: {after}"
    );
}

/// Card `20261105-b-e79993`: Python's waiting dispatch writes its own
/// completion record (`accounts.append_outcome_log`, called from
/// `DispatchRunner.dispatch`, `runner.py:2869-2877`) the instant `--wait`
/// observes a settled turn with a usable status — unconditionally, never
/// waiting for a later `tasks`/`board`/`sweep` reconcile pass to notice the
/// status file. Without a matching write here, a task that settled under
/// `--wait` stayed `unrecorded` through every later read until something
/// else happened to reconcile it (`conformance/diff_fuzz.py --seed 50007`).
#[test]
fn wait_dispatch_records_its_own_settled_outcome_without_a_later_reconcile_pass() {
    let sandbox = Sandbox::new();
    let output = sandbox.dispatch("Map the repository", &["--yes", "--wait"]);
    assert_code(&output, 0);
    let task = &sandbox.task_dirs()[0];
    let task_id = task.file_name().unwrap().to_str().unwrap();
    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        log.contains(&format!("event=result\ttask={task_id}\tstatus=done")),
        "a --wait dispatch that settled with a status must record its own \
         outcome immediately, not wait for a later reconcile pass: {log}"
    );
    assert!(
        !log.contains("state=reconciled"),
        "the dispatch's own record carries the real settled Herdr state, not a \
         later reconciler's guess: {log}"
    );
    let plain = stdout(&sandbox.cli(&["tasks", "--no-reconcile", "--read-only"]));
    assert!(
        plain.contains("recorded"),
        "a plain, non-reconciling read right after a settled --wait dispatch \
         must already show the task recorded: {plain}"
    );
    assert!(!plain.contains("unrecorded"), "{plain}");
}

/// Card `20261105-b-28cb36`: a second live `--yes` dispatch of the exact
/// same task text (same project/role/day, so `task_id_for`'s hash collides)
/// must rebind to the `-2` suffix rather than refusing or silently
/// overwriting the first — `supersede_collision_predecessor`'s port of
/// Python's `runner.supersede_open_redispatch`, run from
/// `RealDispatchStore::recheck_admission` before the new pane is ever
/// admitted.
///
/// Task `20261106-fixer-7c20f3`: a `--wait` dispatch that settles `done` now
/// closes its own pane immediately (`runner::teardown`, matching Python's
/// `close_pane_when_done` in `runner.py:2906-2907`), so by the time a second
/// dispatch's collision check runs, the predecessor is already both RECORDED
/// and gone from `pane list` — `supersede_collision_predecessor` finds no
/// live occupant at its recorded pane (`live_agent` resolves to `None`) and
/// takes its no-op branch, same as "nothing to close" (never an
/// `event=supersede` correction; that branch's own live-pane case is covered
/// directly, with a still-open fake pane, by
/// `supersede_collision_predecessor_closes_a_clean_idle_recorded_predecessor`
/// in `commands::dispatch`'s unit tests). This test's own job, end to end,
/// is narrower now: the collision still rebinds to a fresh task id and both
/// dispatches still complete cleanly, without tripping over a
/// recorded-and-already-closed predecessor.
#[test]
fn a_colliding_redispatch_rebinds_to_a_fresh_task_id_over_an_already_closed_predecessor() {
    let sandbox = Sandbox::new();
    let first = sandbox.dispatch("Map the repository", &["--yes", "--wait"]);
    assert_code(&first, 0);
    let before_dirs = sandbox.task_dirs();
    assert_eq!(before_dirs.len(), 1);
    let predecessor = before_dirs[0]
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let panes_after_first = stdout(&sandbox.herdr(&["pane", "list"]));
    assert!(
        !panes_after_first.contains("w52:p100"),
        "a --wait dispatch that settles done closes its own pane immediately: \
         {panes_after_first}"
    );

    // `read_writer_status`/`read_status_since` (mirroring Python's
    // `_status_predates_run`) only trust a status file newer than this
    // task's own `event=started`, logged at whole-second resolution — a
    // one-second gap keeps this test's own predecessor-status assertion
    // below deterministic rather than riding that boundary.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    let second = sandbox.dispatch("Map the repository", &["--yes", "--wait"]);
    assert_code(&second, 0);

    let after_dirs = sandbox.task_dirs();
    assert_eq!(after_dirs.len(), 2, "{after_dirs:?}");
    let redispatched = after_dirs
        .iter()
        .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
        .find(|id| id != &predecessor)
        .expect("a second, collision-suffixed task directory");
    assert_eq!(redispatched, format!("{predecessor}-2"), "{after_dirs:?}");

    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        log.contains(&format!("event=result\ttask={predecessor}\tstatus=done")),
        "the predecessor's own --wait dispatch must have recorded its outcome \
         immediately, before any collision was ever detected: {log}"
    );
    assert!(
        !log.contains("event=supersede"),
        "nothing was live at the predecessor's recorded pane, so there is \
         nothing for the collision check to supersede: {log}"
    );
    assert!(
        log.contains(&format!("event=result\ttask={redispatched}\tstatus=done")),
        "the rebound redispatch must settle and record its own outcome too: {log}"
    );

    let panes_after_second = stdout(&sandbox.herdr(&["pane", "list"]));
    assert!(
        !panes_after_second.contains("w52:p100") && !panes_after_second.contains("w52:p101"),
        "both dispatches settled done, so neither leaves its pane open: {panes_after_second}"
    );
}

/// Card `20261103-b-25cc83`: a live `multiverse dispatch --yes` against
/// stock fake-herdr (not a test double wrapping it) must be able to drive
/// `limits::classify`'s vendor-exhaustion path through `RealDispatchStore`
/// — trip the first seat's pool, write redacted evidence, and have the
/// next chain walk fall back to the next seat.
#[test]
fn dispatch_trips_breaker_writes_evidence_and_falls_back_to_the_next_seat() {
    let sandbox = Sandbox::new();
    enable_fallback_chain(&sandbox);
    let fixture = rate_limit_fixture("codex-out-of-credits.txt");
    sandbox.herdr(&["script", "queue-text", "--file", fixture.to_str().unwrap()]);

    let first = sandbox.dispatch("Map the repository", &["--yes"]);
    assert_code(&first, 1);
    let diagnostic = stderr(&first);
    assert!(
        diagnostic.contains("dialog (codex_usage_limit_reached)"),
        "{diagnostic}"
    );

    let listing = stdout(&sandbox.cli(&["accounts"]));
    assert!(listing.contains("test-seat"), "{listing}");
    assert!(listing.contains("FENCED"), "{listing}");
    assert!(listing.contains("codex_usage_limit_reached"), "{listing}");
    assert!(listing.contains("fallback-seat"), "{listing}");

    let evidence_dir = sandbox.project.join(".multiverse/evidence");
    let mut entries: Vec<_> = fs::read_dir(&evidence_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(entries.len(), 1, "{entries:?}");
    let evidence = fs::read_to_string(entries.remove(0)).unwrap();
    assert!(evidence.contains("Usage limit reached"), "{evidence}");

    let second = sandbox.dispatch("Summarize the README", &["--yes", "--wait"]);
    assert_code(&second, 0);
    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(log.contains("account=test-seat"), "{log}");
    assert!(log.contains("pool=codex:integration"), "{log}");
    assert!(log.contains("state=failed-to-start"), "{log}");
    assert!(log.contains("account=fallback-seat"), "{log}");
    assert!(log.contains("pool=codex:fallback"), "{log}");
    assert!(log.contains("outcome=chain-fallback"), "{log}");
    assert!(log.contains("event=started"), "{log}");
}

fn rate_limit_fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/rate-limit")
        .join(name)
        .canonicalize()
        .expect("rate-limit fixture")
}

fn enable_fallback_chain(sandbox: &Sandbox) {
    let config_path = sandbox.root.join("config/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config = config.replace(
        "scout = [\"test-seat\"]",
        "scout = [\"test-seat\", \"fallback-seat\"]",
    );
    config = config.replace("on_exhausted = \"queue\"", "on_exhausted = \"degrade\"");
    config.push_str(&format!(
        "\n[accounts.fallback-seat]\n\
         kind = \"codex\"\n\
         config_dir_var = \"CODEX_HOME\"\n\
         config_dir = '{codex_home}'\n\
         pool = \"codex:fallback\"\n\
         short = \"fb\"\n",
        codex_home = sandbox.root.join("codex-home").display(),
    ));
    fs::write(&config_path, &config).unwrap();
    fs::copy(
        &config_path,
        sandbox.root.join("home/.config/multiverse/config.toml"),
    )
    .unwrap();
    mv_config::load::load_config(&config_path)
        .expect("fallback-chain fixture must be valid under the real config schema");
}

#[test]
fn halt_refuses_dispatch_and_force_overrides_without_clearing_halt() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli(&["halt", "on", "--reason", "integration maintenance"]),
        0,
    );
    let status = sandbox.cli(&["halt", "status"]);
    assert_code(&status, 0);
    assert!(stdout(&status).contains("integration maintenance"));
    let before = sandbox.snapshot();
    let refused = sandbox.dispatch("Map the repository", &["--yes"]);
    assert_code(&refused, 1);
    let diagnostic = stderr(&refused);
    assert!(diagnostic.contains("halt"), "{diagnostic}");
    assert!(diagnostic.contains("--force"), "{diagnostic}");
    assert_eq!(sandbox.snapshot(), before);
    assert_code(
        &sandbox.dispatch("Map the repository", &["--yes", "--force", "--wait"]),
        0,
    );
    assert_eq!(sandbox.task_dirs().len(), 1);
    assert_eq!(stdout(&sandbox.cli(&["halt", "status"])), stdout(&status));
    assert_code(&sandbox.cli(&["halt", "off"]), 0);
}

#[test]
fn tasks_reads_completed_legacy_role_history_without_rewriting_it() {
    let sandbox = Sandbox::new();
    let task = sandbox.seed_task();
    let before = sandbox.snapshot();
    let output = sandbox.cli(&["tasks", "--no-reconcile", "--read-only"]);
    assert_code(&output, 0);
    let tasks = stdout(&output);
    assert!(tasks.contains(task), "{tasks}");
    assert!(tasks.contains("done"), "{tasks}");
    assert!(tasks.contains("test-seat"), "{tasks}");
    assert_eq!(sandbox.snapshot(), before);
}

/// Card `20261106-b-2c3d2f` Decision 1: `tasks --close-stranded --yes` must
/// never record `failed` for a still-live agent that simply has no status
/// file yet. `pane run` with a bare launch command (no `cd` prefix) is
/// real Herdr's own auto-classify route (`fake-herdr`'s
/// `classify_command_route`, card `20261103-b-7febbe`): the pane is live
/// and `agent list` already reports it `idle`, but it carries no name at
/// all until a later `agent rename` supplies one — the live-but-unnamed
/// gap this decision exists to close, distinct from a *confirmed
/// different* name (card `20261108-b-152b33`, which must still read as
/// gone).
#[test]
fn close_stranded_leaves_a_live_idle_task_with_no_status_file_unrecorded() {
    let sandbox = Sandbox::new();

    let split = sandbox.herdr(&[
        "pane",
        "split",
        "--pane",
        "w52:p1",
        "--direction",
        "right",
        "--cwd",
        sandbox.project.to_str().unwrap(),
        "--no-focus",
    ]);
    let split_reply: serde_json::Value = serde_json::from_slice(&split.stdout).unwrap();
    let pane_id = split_reply["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned();
    sandbox.herdr(&["pane", "run", &pane_id, "claude --model test-model"]);

    let task_id = "20261116-scout-aaaaaa";
    fs::create_dir_all(sandbox.project.join(".multiverse/tasks").join(task_id)).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/routing.log"),
        format!(
            "2026-11-16T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
             branch=\trole=scout\taccount=test-seat\tagent=agent-a\n\
             2026-11-16T00:00:01+00:00\tevent=started\ttask={task_id}\tpane={pane_id}\n"
        ),
    )
    .unwrap();

    let output = sandbox.cli(&["tasks", "--close-stranded", "--yes"]);
    assert_code(&output, 0);
    let plain = stdout(&output);
    assert!(
        plain.contains("refusing to close") && plain.contains(task_id),
        "a STALLED row must be refused by name, exactly as a RUNNING row is: {plain}"
    );

    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        !log.contains("event=result"),
        "a still-live agent with no status file must never be recorded as an \
         outcome: {log}"
    );
}

/// Card `20261118-b-fde912` step 1: `tasks --reconcile`'s orphan-pane line.
/// A live Herdr agent is an orphan only when its dispatch-time name
/// matches this project's own lane naming (`naming::agent_name`, the same
/// call a real dispatch mints a name through) *and* its pane has no
/// record anywhere in this project's routing log. Splits a fresh pane and
/// names it directly through the fake Herdr's `agent rename` (the same
/// primitive `close_stranded_leaves_a_live_idle_task_with_no_status_file_
/// unrecorded` above uses) rather than running a real dispatch, so the
/// pane stays live and unrecorded with no task of its own to settle.
fn split_pane(sandbox: &Sandbox) -> String {
    let split = sandbox.herdr(&[
        "pane",
        "split",
        "--pane",
        "w52:p1",
        "--direction",
        "right",
        "--cwd",
        sandbox.project.to_str().unwrap(),
        "--no-focus",
    ]);
    serde_json::from_slice::<serde_json::Value>(&split.stdout).unwrap()["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// The exact name `naming::agent_name` would mint for a lane of
/// `project`'s own `role`, dispatched through the sandbox's `test-seat`
/// account (`load.rs`'s default `short`: the account name's first two
/// lowercase characters, `"te"` for `test-seat`) — never hand-typed, so
/// this test fails the moment the naming algorithm or the fixture's
/// account name changes instead of silently matching the wrong thing.
fn lane_name(project: &str, role: &str) -> String {
    mv_engine::runner::naming::agent_name(project, role, None, "te", "").unwrap()
}

#[test]
fn reconcile_reports_nothing_when_there_are_no_orphan_panes() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    assert!(
        !stdout(&output).contains("orphan pane"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn reconcile_reports_one_orphan_pane_for_a_live_agent_with_no_task_record() {
    let sandbox = Sandbox::new();
    let pane_id = split_pane(&sandbox);
    let name = lane_name("sandbox", "buil");
    sandbox.herdr(&["agent", "rename", &pane_id, &name]);

    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    let plain = stdout(&output);
    let expected = format!(
        "orphan pane {pane_id} (agent {name}): no task record — close it in Herdr if it is \
         not yours"
    );
    assert_eq!(
        plain.matches("orphan pane").count(),
        1,
        "exactly one orphan line: {plain}"
    );
    assert!(plain.contains(&expected), "{plain}");
}

#[test]
fn reconcile_never_reports_a_live_agent_named_for_a_different_project() {
    let sandbox = Sandbox::new();
    let pane_id = split_pane(&sandbox);
    // This project's own naming ("sandbox-buil-te") never collides with
    // another project's ("recipient-buil-te") — `tasks --reconcile`
    // (implicitly scoped to the cwd project, "sandbox") must never report
    // a pane carrying a name minted for "recipient".
    let name = lane_name("recipient", "buil");
    sandbox.herdr(&["agent", "rename", &pane_id, &name]);

    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    assert!(
        !stdout(&output).contains("orphan pane"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn reconcile_never_reports_a_recorded_lanes_pane_as_an_orphan() {
    let sandbox = Sandbox::new();
    let pane_id = split_pane(&sandbox);
    let name = lane_name("sandbox", "scou");
    sandbox.herdr(&["agent", "rename", &pane_id, &name]);

    let task_id = "20261118-scout-aaaaaa";
    fs::create_dir_all(sandbox.project.join(".multiverse/tasks").join(task_id)).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/routing.log"),
        format!(
            "2026-11-18T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
             branch=\trole=scout\taccount=test-seat\tagent={name}\n\
             2026-11-18T00:00:01+00:00\tevent=started\ttask={task_id}\tpane={pane_id}\n"
        ),
    )
    .unwrap();

    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    assert!(
        !stdout(&output).contains("orphan pane"),
        "{}",
        stdout(&output)
    );
}

/// Card `20261118-b-c328a3` M1: `known_panes` used to union every pane a
/// project's log ever named, forever — so once a pane was recorded for
/// one dispatch, it stayed "known" even after Herdr recycled that same
/// pane id for a completely different, never-dispatched lane-named agent.
/// The check must key off (agent name, pane) as the log's *current*
/// record, not raw pane-id membership: renaming the live occupant to a
/// second lane-matching name, with no new routing-log line for it,
/// simulates exactly that recycling without needing Herdr to actually
/// reuse a numeric pane id. Revert check: restore the plain
/// `known_panes.contains(&agent.pane_id)` membership test and this fails
/// (no orphan line — the recorded pane still reads as "known").
#[test]
fn reconcile_reports_an_orphan_when_a_recorded_panes_agent_no_longer_matches() {
    let sandbox = Sandbox::new();
    let pane_id = split_pane(&sandbox);
    let recorded_name = lane_name("sandbox", "buil");
    sandbox.herdr(&["agent", "rename", &pane_id, &recorded_name]);

    let task_id = "20261118-builder-aaaaaa";
    fs::create_dir_all(sandbox.project.join(".multiverse/tasks").join(task_id)).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/routing.log"),
        format!(
            "2026-11-18T00:00:00+00:00\tevent=dispatch\ttask={task_id}\t\
             branch=\trole=builder\taccount=test-seat\tagent={recorded_name}\n\
             2026-11-18T00:00:01+00:00\tevent=started\ttask={task_id}\tpane={pane_id}\n"
        ),
    )
    .unwrap();

    // Herdr recycles the same pane id for a different, never-dispatched
    // lane-named agent (instance 2) — no routing-log line names it.
    let recycled_name =
        mv_engine::runner::naming::agent_name("sandbox", "buil", Some("2"), "te", "").unwrap();
    sandbox.herdr(&["agent", "rename", &pane_id, &recycled_name]);

    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    let plain = stdout(&output);
    let expected = format!(
        "orphan pane {pane_id} (agent {recycled_name}): no task record — close it in Herdr if \
         it is not yours"
    );
    assert_eq!(
        plain.matches("orphan pane").count(),
        1,
        "exactly one orphan line: {plain}"
    );
    assert!(plain.contains(&expected), "{plain}");
}

/// Card `20261118-b-c328a3` M4: `project_lane_agent_names` used to
/// hand-copy `naming::unique_agent_name`'s own collision budget and stop
/// at instance 9 instead of sharing the constant — a tenth same-role/
/// same-seat collision minted a live, unrecorded pane this check silently
/// missed. Revert check: restore the literal `1..=9u32` range and this
/// fails (no orphan line for the instance-10 name).
#[test]
fn reconcile_reports_an_orphan_pane_past_the_old_nine_instance_cap() {
    let sandbox = Sandbox::new();
    let pane_id = split_pane(&sandbox);
    let name =
        mv_engine::runner::naming::agent_name("sandbox", "buil", Some("10"), "te", "").unwrap();
    sandbox.herdr(&["agent", "rename", &pane_id, &name]);

    let output = sandbox.cli(&["tasks", "--reconcile"]);
    assert_code(&output, 0);
    let plain = stdout(&output);
    let expected = format!(
        "orphan pane {pane_id} (agent {name}): no task record — close it in Herdr if it is \
         not yours"
    );
    assert_eq!(
        plain.matches("orphan pane").count(),
        1,
        "exactly one orphan line: {plain}"
    );
    assert!(plain.contains(&expected), "{plain}");
}

#[test]
fn result_reads_completed_legacy_role_artifact_without_rewriting_it() {
    let sandbox = Sandbox::new();
    let task = sandbox.seed_task();
    let before = sandbox.snapshot();
    let output = sandbox.cli(&["result", task]);
    assert_code(&output, 0);
    let expected = fs::read_to_string(
        sandbox
            .project
            .join(".multiverse/tasks")
            .join(task)
            .join("result.md"),
    )
    .unwrap();
    assert_eq!(stdout(&output).trim_end(), expected.trim_end());
    assert_eq!(sandbox.snapshot(), before);
}

#[test]
fn backlog_add_list_show_and_done_persist_a_card() {
    let sandbox = Sandbox::new();
    let added = sandbox.cli(&[
        "backlog",
        "add",
        "Repair fixture issue",
        "--why",
        "A reproducible failure",
        "--role",
        "builder",
    ]);
    assert_code(&added, 0);
    let text = stdout(&added);
    let id = text
        .split_whitespace()
        .nth(1)
        .expect("added <id> to sandbox");
    assert!(text.starts_with("added "), "{text}");
    let listed = sandbox.cli(&["backlog", "list"]);
    assert_code(&listed, 0);
    assert!(stdout(&listed).contains(id));
    assert!(stdout(&listed).contains("Repair fixture issue"));
    let shown = sandbox.cli(&["backlog", "show", id]);
    assert_code(&shown, 0);
    assert!(stdout(&shown).contains("A reproducible failure"));
    assert_code(
        &sandbox.cli(&["backlog", "done", id, "--evidence", "Verified in fixture"]),
        0,
    );
    let shown = sandbox.cli(&["backlog", "show", id]);
    assert_code(&shown, 0);
    assert!(stdout(&shown).contains("done"));
    let log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(log.contains("event=backlog"), "{log}");
    assert!(log.contains("state=open"), "{log}");
    assert!(log.contains("state=done"), "{log}");
    assert!(log.contains("role=builder"), "{log}");
    assert!(!log.contains("role=fixer"), "{log}");
}

#[test]
fn activity_send_is_durable_idempotent_and_peek_keeps_message_visible() {
    let sandbox = Sandbox::new();
    let args = [
        "activity",
        "send",
        "recipient",
        "Ready for review",
        "--id",
        "integration-msg",
    ];
    assert_code(&sandbox.cli(&args), 0);
    let log_path = sandbox.recipient.join(".multiverse/routing.log");
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(log.contains("event=message"), "{log}");
    assert!(log.contains("from=sandbox"), "{log}");
    assert!(log.contains("to=recipient"), "{log}");
    assert!(log.contains("Ready for review"), "{log}");
    assert_code(&sandbox.cli(&args), 0);
    assert_eq!(fs::read_to_string(&log_path).unwrap(), log);
    for _ in 0..2 {
        let inbox = sandbox.cli_in(&sandbox.recipient, &["activity", "inbox", "--peek"]);
        assert_code(&inbox, 0);
        assert!(stdout(&inbox).contains("Ready for review"));
        assert!(stdout(&inbox).contains("sandbox"));
    }
    assert_eq!(fs::read_to_string(&log_path).unwrap(), log);
    assert!(!sandbox.project.join(".multiverse/routing.log").exists());
}

/// `activity.py`'s `_print_unacked_mail_hint` (`cli.py:13270-13288`) backs
/// a successful `send`/`reply`/`close` with a peek at the *caller's own*
/// inbox, purely to print a nudge — but that peek still goes through
/// `read_inbox`, which writes the PR-E `activity.lastseen` marker on every
/// call, peek or not (`activity.py:2573-2581`). So the sender's own
/// `activity.lastseen` is a byproduct of a successful `send`, even though
/// `send` never reads its own inbox for any other reason (card
/// `20261103-b-00824c`). Rust's `send` skipped this peek entirely, so the
/// marker was never written.
#[test]
fn activity_send_writes_the_sender_s_own_lastseen_marker() {
    let sandbox = Sandbox::new();
    let marker_path = sandbox.project.join(".multiverse/activity.lastseen");
    assert!(!marker_path.exists());
    assert_code(
        &sandbox.cli(&["activity", "send", "recipient", "Ready for review"]),
        0,
    );
    assert!(
        marker_path.exists(),
        "sandbox's own activity.lastseen should be written as a side effect of send, \
         same as a successful reply/close (cli.py:13270-13288)"
    );
}

#[test]
fn activity_send_refuses_a_missing_recipient_directory() {
    let sandbox = Sandbox::new();
    fs::remove_dir_all(&sandbox.recipient).unwrap();
    let output = sandbox.cli(&["activity", "send", "recipient", "hello"]);
    assert_code(&output, 3);
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("does not exist"), "{diagnostic}");
    assert!(
        !diagnostic.contains(sandbox.recipient.to_str().unwrap()),
        "missing-dir refusal must be path-free, got {diagnostic}"
    );
    assert!(!sandbox.recipient.exists());
}

#[test]
fn activity_send_question_prints_the_ask_hint() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["activity", "send", "recipient", "are you up?"]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("sent to recipient."), "{body}");
    assert!(body.contains("that reads like a question"), "{body}");
    assert!(body.contains("--ask"), "{body}");
}

/// Card `20261115-b-8d103d`: the sender's own inbox sitting on unacked mail
/// must never block an outgoing `send` — delivery and the existing
/// `print_unacked_mail_hint` nudge both still happen, and the command still
/// exits 0, mirroring the question-heuristic case above rather than
/// introducing a refusal.
#[test]
fn activity_send_delivers_despite_the_sender_s_own_unacked_mail() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli_in(
            &sandbox.recipient,
            &["activity", "send", "sandbox", "heads up"],
        ),
        0,
    );
    let output = sandbox.cli(&["activity", "send", "recipient", "Ready for review"]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("sent to recipient."), "{body}");
    assert!(body.contains("inbox still has unacked mail"), "{body}");
    let log = fs::read_to_string(sandbox.recipient.join(".multiverse/routing.log")).unwrap();
    assert!(log.contains("Ready for review"), "{log}");
}

/// Card `20261115-b-51cb75`: `send --ask` must still land the post when the
/// notify hook (`curl`) cannot be found on `PATH` — a warning names what
/// failed, the post is unaffected, and the command still exits 0.
#[test]
fn activity_send_ask_delivers_and_warns_when_the_notify_hook_binary_is_missing() {
    let sandbox = Sandbox::new();
    fs::write(sandbox.root.join("config/ntfy-topic"), "ask-alerts\n").unwrap();
    let output = sandbox.cli_without_curl(&[
        "activity",
        "send",
        "recipient",
        "is anyone around to help?",
        "--ask",
    ]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("sent to recipient."), "{body}");
    assert!(body.to_lowercase().contains("notify hook"), "{body}");
    assert!(body.to_lowercase().contains("curl"), "{body}");
    let log = fs::read_to_string(sandbox.recipient.join(".multiverse/routing.log")).unwrap();
    assert!(log.contains("is anyone around to help?"), "{log}");
    assert!(log.contains("ask=1"), "{log}");
}

/// Companion to the missing-hook case above: with no ntfy topic configured
/// at all (this sandbox's default), `send --ask` must stay exactly as quiet
/// as a plain send — no notify attempt, no warning.
#[test]
fn activity_send_ask_prints_no_notify_warning_without_a_configured_topic() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&[
        "activity",
        "send",
        "recipient",
        "can you take a look?",
        "--ask",
    ]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("sent to recipient."), "{body}");
    assert!(!body.to_lowercase().contains("notify"), "{body}");
}

#[test]
fn activity_send_auto_routes_through_reply_when_it_answers_one_open_ask() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli_in(
            &sandbox.recipient,
            &[
                "activity",
                "send",
                "sandbox",
                "please rotate the staging creds before 9am",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );
    let output = sandbox.cli(&[
        "activity",
        "send",
        "recipient",
        "rotated the staging creds before 9am",
        "--id",
        "answer-1",
    ]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("linked as reply to #1."), "{body}");
    let holder = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(holder.contains("event=close"), "{holder}");
}

/// Reproduces card `20261110-b-657008`: a plain `activity send` while the
/// recipient has more than one open ask on record must refuse — never
/// silently send nothing while reporting success. It must exit non-zero,
/// write no `event=message` into the recipient's log, and name
/// `multiverse activity reply <seq>` so the caller can act on the
/// refusal without re-deriving the command.
#[test]
fn activity_send_refuses_rather_than_guess_among_several_open_asks() {
    let sandbox = Sandbox::new();
    for (id, seq_body) in [("ask-1", "an ask from days ago"), ("ask-2", "a newer ask")] {
        assert_code(
            &sandbox.cli_in(
                &sandbox.recipient,
                &["activity", "send", "sandbox", seq_body, "--ask", "--id", id],
            ),
            0,
        );
    }
    let output = sandbox.cli(&["activity", "send", "recipient", "a plain status update"]);
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(code, 0, "must not exit 0 having sent nothing: {output:?}");
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("multiverse activity reply"),
        "refusal must name `multiverse activity reply <seq>`: {diagnostic}"
    );
    assert!(diagnostic.contains("#1"), "{diagnostic}");
    assert!(diagnostic.contains("#2"), "{diagnostic}");
    let log_path = sandbox.recipient.join(".multiverse/routing.log");
    if log_path.exists() {
        let log = fs::read_to_string(&log_path).unwrap();
        assert!(
            !log.contains("event=message"),
            "a refused send must record no message event: {log}"
        );
    }
}

#[test]
fn activity_update_refuses_an_unknown_card() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["activity", "update", "not-a-card", "--phase", "working"]);
    assert_code(&output, 3);
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("no dispatched card"), "{diagnostic}");
    assert!(!sandbox.project.join(".multiverse/routing.log").exists());
}

#[test]
fn activity_reply_resolves_a_bare_seq_against_the_holders_own_log() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli_in(
            &sandbox.recipient,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );
    let output = sandbox.cli(&["activity", "reply", "1", "seat-a"]);
    assert_code(&output, 0);
    assert!(stdout(&output).contains("replied to recipient as seq"));
    let holder = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(holder.contains("event=close"), "{holder}");
    let asker = fs::read_to_string(sandbox.recipient.join(".multiverse/routing.log")).unwrap();
    assert!(
        asker.contains("seat-a") && asker.contains("from=sandbox"),
        "{asker}"
    );
}

/// Adds a `[projects.multiverse]` entry — `mv_state::activity::
/// HUB_PROJECT_NAME` — to this sandbox's own config, widening
/// `[activity.pairs]` both ways against `sandbox`, so a test can exercise
/// an ask the hub itself sent (card `20261115-b-91722d`). Appends rather
/// than rebuilding the config from scratch, the same pattern
/// `legacy_name_guard.rs`'s own `enable_interactive_nexus_and_hub` uses —
/// not shared directly, since that helper lives in a sibling test binary
/// this crate does not expose as a library. `[activity.pairs]` is already
/// the last table `Sandbox::new`'s own config defines, so the appended
/// `multiverse = [...]` line lands inside it rather than redefining it.
fn enable_hub_project(sandbox: &Sandbox) -> PathBuf {
    let hub = sandbox.root.join("multiverse");
    fs::create_dir_all(&hub).unwrap();
    let config_path = sandbox.root.join("config/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config = config.replace(
        "sandbox = [\"recipient\"]",
        "sandbox = [\"recipient\", \"multiverse\"]",
    );
    config.push_str(&format!(
        "multiverse = [\"sandbox\", \"recipient\"]\n\n[projects.multiverse]\npath = '{hub}'\n",
        hub = hub.display(),
    ));
    fs::write(&config_path, &config).unwrap();
    fs::copy(
        &config_path,
        sandbox.root.join("home/.config/multiverse/config.toml"),
    )
    .unwrap();
    hub
}

/// Card `20261115-b-91722d`'s exact failing repro: the hub asks a project a
/// question, and answering it with a bare `SEQ` from the hub's own
/// directory fails — the hub's own log holds asks addressed *to* it, never
/// one it sent, so `reply`'s unqualified form has nothing to resolve
/// against. Before this fix that was a flat "not found"; now it names the
/// project to requalify against, and the qualified retry succeeds and lands
/// the answer where the asker (the hub) actually reads it.
#[test]
fn activity_reply_resolves_a_hub_sent_ask_via_project_hash_seq() {
    let sandbox = Sandbox::new();
    let hub = enable_hub_project(&sandbox);
    assert_code(
        &sandbox.cli_in(
            &hub,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );

    let bare = sandbox.cli_in(&hub, &["activity", "reply", "1", "seat-a"]);
    assert_code(&bare, 3);
    let diagnostic = stderr(&bare);
    assert!(
        diagnostic.contains("sandbox#1"),
        "refusal must suggest the holder's qualifier: {diagnostic}"
    );
    assert!(!hub.join(".multiverse/routing.log").exists());

    let qualified = sandbox.cli_in(&hub, &["activity", "reply", "sandbox#1", "seat-a"]);
    assert_code(&qualified, 0);
    assert!(
        stdout(&qualified).contains("replied to multiverse as seq 1, relayed on behalf of sandbox"),
        "the hub composed this reply itself, on sandbox's behalf — its own \
         stdout must say so: {:?}",
        stdout(&qualified)
    );
    let hub_log = fs::read_to_string(hub.join(".multiverse/routing.log")).unwrap();
    assert!(
        hub_log.contains("from=sandbox") && hub_log.contains("seat-a"),
        "the reply must land where the asker (the hub) reads it: {hub_log}"
    );
    assert!(
        hub_log.contains("relayed_by=multiverse"),
        "the hub, not sandbox, actually wrote this reply's words — the stored \
         line must say so (card `20261116-b-57998b` finding 2): {hub_log}"
    );
    let holder_log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(holder_log.contains("event=close"), "{holder_log}");

    // The relay marker must show up wherever this message is printed, not
    // just on disk: `inbox`/`show` both read it back through the hub's own
    // log, which is where the reply actually landed.
    let shown = sandbox.cli_in(&hub, &["activity", "show", "1"]);
    assert_code(&shown, 0);
    assert!(
        stdout(&shown).contains("relayed by: multiverse"),
        "{:?}",
        stdout(&shown)
    );
    let inbox = sandbox.cli_in(&hub, &["activity", "inbox", "--peek"]);
    assert_code(&inbox, 0);
    assert!(
        stdout(&inbox).contains("sandbox (relayed by multiverse):"),
        "{:?}",
        stdout(&inbox)
    );
}

/// Card `20261116-b-40aecf`: naming yourself as an ask's own sender in a
/// `Holder#SEQ` qualifier is authorization to *act* on that ask, not a
/// bypass of `[activity.pairs]` deny-by-default messaging — relaying a
/// reply into the holder's log is itself a send from the caller to that
/// holder, and the same reverse-direction pair `close_ref` already
/// re-checks before acting on a remote ask must hold here too.
///
/// Deliberately avoids `enable_hub_project`: `mv-config`'s own
/// `merge_hub_activity_pairs` (card `20261108-b-81cb2b`) grants the hub
/// project implicit, unrevokable bidirectional `[activity.pairs]` with
/// every other project, so a hub-as-asker fixture could never exercise
/// this refusal. `sandbox`/`recipient` carry no such implicit grant, so
/// revoking one explicit direction between them actually takes effect.
/// The pair is revoked *after* the ask is sent (so sending it still
/// exercised the old permission) and *before* the reply — the same
/// ordering that lets `close_ref`'s own re-check ever matter, since a pair
/// unchanged since the ask was sent would pass either way.
#[test]
fn activity_reply_refuses_a_relay_the_askers_own_pair_does_not_permit() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli_in(
            &sandbox.recipient,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );

    // Revoke `recipient -> sandbox` now that the ask is already sent;
    // `sandbox -> recipient` (the direction `reply`'s own
    // `may_send_reverse` write needs) stays granted, so only the new
    // relay re-check is what's missing.
    let config_path = sandbox.root.join("config/config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    let config = config.replace("recipient = [\"sandbox\"]", "recipient = []");
    fs::write(&config_path, &config).unwrap();

    let qualified = sandbox.cli_in(
        &sandbox.recipient,
        &["activity", "reply", "sandbox#1", "seat-a"],
    );
    let code = qualified.status.code().unwrap_or(-1);
    assert_ne!(
        code, 0,
        "recipient -> sandbox is not permitted; the relay must be refused: {qualified:?}"
    );
    let diagnostic = stderr(&qualified);
    assert!(
        diagnostic.contains("recipient") && diagnostic.contains("sandbox"),
        "{diagnostic}"
    );
    assert!(
        !sandbox.recipient.join(".multiverse/routing.log").exists(),
        "a refused relay must write nothing into the asker's own log"
    );
    let holder_log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        !holder_log.contains("event=close"),
        "the real holder's ask must remain open: {holder_log}"
    );
}

/// A project that is neither the ask's holder nor its original sender may
/// not act on `holder#seq` just by naming it — the same authorization
/// [`close`]'s own qualifier already enforces.
#[test]
fn activity_reply_refuses_project_hash_seq_for_an_uninvolved_caller() {
    let sandbox = Sandbox::new();
    let hub = enable_hub_project(&sandbox);
    assert_code(
        &sandbox.cli_in(
            &hub,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );
    let output = sandbox.cli_in(
        &sandbox.recipient,
        &["activity", "reply", "sandbox#1", "seat-a"],
    );
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(
        code, 0,
        "an uninvolved project must not reply on another's behalf: {output:?}"
    );
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("cannot act on sandbox#1"),
        "{diagnostic}"
    );
}

/// Card `20261116-builder-9a50a2`'s exact failing repro, from the other
/// side of `20261115-b-91722d`'s own fixture: the hub asks `sandbox` a
/// question, and *`sandbox` itself* — the ask's actual recipient, not the
/// hub — answers it from its own directory, qualifying with the ask's own
/// sender (`multiverse#1`) rather than the bare `1` that already worked.
/// Before this fix, the qualified branch always opened the *named*
/// project's own log to look for the ask, so `multiverse#1` opened the
/// hub's log — which never holds an ask addressed to someone else — and
/// missed with the raw "no ask #1 found in this project's own routing log"
/// text, never `sandbox`'s own log where the ask actually lives.
#[test]
fn activity_reply_resolves_a_recipients_bare_or_sender_qualified_seq() {
    for raw_seq in ["1", "multiverse#1"] {
        let sandbox = Sandbox::new();
        let hub = enable_hub_project(&sandbox);
        assert_code(
            &sandbox.cli_in(
                &hub,
                &[
                    "activity",
                    "send",
                    "sandbox",
                    "which seat are you on?",
                    "--ask",
                    "--id",
                    "ask-1",
                ],
            ),
            0,
        );

        // From `sandbox`'s own directory — its actual recipient, not the hub.
        let reply = sandbox.cli(&["activity", "reply", raw_seq, "seat-a"]);
        assert_code(&reply, 0);
        assert!(
            stdout(&reply).contains("replied to multiverse as seq"),
            "{:?}",
            stdout(&reply)
        );

        // The answer lands where the asker (the hub) reads it...
        let hub_log = fs::read_to_string(hub.join(".multiverse/routing.log")).unwrap();
        assert!(
            hub_log.contains("from=sandbox") && hub_log.contains("seat-a"),
            "reply {raw_seq:?}: {hub_log}"
        );
        assert!(
            hub_log.contains("re=1"),
            "reply {raw_seq:?} must carry re=1: {hub_log}"
        );
        // ...and the ask is closed in the holder's (sandbox's) own log.
        let holder_log =
            fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
        assert!(holder_log.contains("event=close"), "{holder_log}");
    }
}

/// `20261116-builder-9a50a2`'s negative case: a project that is neither the
/// ask's recipient nor its sender must not be able to claim it just by
/// naming the sender in the qualifier either — `recipient` was never
/// addressed by the hub's ask to `sandbox`, so `reply multiverse#1` from
/// `recipient`'s own directory must refuse and write nothing anywhere: not
/// into its own log, not into the hub's, and the ask held by `sandbox`
/// must remain open.
#[test]
fn activity_reply_refuses_sender_hash_seq_for_a_project_the_ask_never_reached() {
    let sandbox = Sandbox::new();
    let hub = enable_hub_project(&sandbox);
    assert_code(
        &sandbox.cli_in(
            &hub,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );

    let output = sandbox.cli_in(
        &sandbox.recipient,
        &["activity", "reply", "multiverse#1", "seat-a"],
    );
    let code = output.status.code().unwrap_or(-1);
    assert_ne!(
        code, 0,
        "a project the ask never reached must not be able to answer it: {output:?}"
    );
    assert!(
        !sandbox.recipient.join(".multiverse/routing.log").exists(),
        "nothing must be written into the uninvolved caller's own log"
    );
    assert!(
        !hub.join(".multiverse/routing.log").exists(),
        "nothing must be written into the asker's log"
    );
    let holder_log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        !holder_log.contains("event=close"),
        "the real holder's ask must remain open: {holder_log}"
    );
}

/// Card `20261115-b-d4cf40`: `activity broadcast --ask` carries the same
/// per-recipient semantics as `activity send --ask` — each recipient gets
/// its own `ask=1` message, so each sees its own open ask, either one may
/// resolve it with an ordinary `reply`, and a recipient nobody has answered
/// stays open. This must fail if `broadcast`'s handler (or
/// `mv_state::activity::broadcast`) ever goes back to passing `ask=false`
/// regardless of the flag: neither inbox would show `ASK #`, and the reply
/// below would have nothing open to resolve.
#[test]
fn activity_broadcast_ask_reaches_every_recipient_as_its_own_open_ask() {
    let sandbox = Sandbox::new();
    let hub = enable_hub_project(&sandbox);
    assert_code(
        &sandbox.cli_in(
            &hub,
            &[
                "activity",
                "broadcast",
                "status check — please confirm",
                "--ask",
                "--id",
                "bcast-ask-1",
            ],
        ),
        0,
    );

    // Each recipient sees its own ASK, peeked so neither read advances a
    // cursor the assertions below still depend on.
    let sandbox_inbox = sandbox.cli(&["activity", "inbox", "--peek"]);
    assert_code(&sandbox_inbox, 0);
    assert!(
        stdout(&sandbox_inbox).contains("ASK #1"),
        "{:?}",
        stdout(&sandbox_inbox)
    );
    let recipient_inbox = sandbox.cli_in(&sandbox.recipient, &["activity", "inbox", "--peek"]);
    assert_code(&recipient_inbox, 0);
    assert!(
        stdout(&recipient_inbox).contains("ASK #1"),
        "{:?}",
        stdout(&recipient_inbox)
    );

    // `sandbox` answers from its own directory with a bare SEQ.
    let reply = sandbox.cli(&["activity", "reply", "1", "all clear"]);
    assert_code(&reply, 0);
    assert!(
        stdout(&reply).contains("replied to multiverse as seq"),
        "{:?}",
        stdout(&reply)
    );
    let hub_log = fs::read_to_string(hub.join(".multiverse/routing.log")).unwrap();
    assert!(
        hub_log.contains("from=sandbox")
            && hub_log.contains("re=1")
            && hub_log.contains("all clear"),
        "{hub_log}"
    );
    let sandbox_log = fs::read_to_string(sandbox.project.join(".multiverse/routing.log")).unwrap();
    assert!(
        sandbox_log.contains("event=close"),
        "sandbox's own ask must be closed by its reply: {sandbox_log}"
    );

    // `recipient` never replied: its ask stays open, both in its own log
    // and as seen on a fresh peek.
    let recipient_log =
        fs::read_to_string(sandbox.recipient.join(".multiverse/routing.log")).unwrap();
    assert!(
        !recipient_log.contains("event=close"),
        "recipient's ask must remain open: {recipient_log}"
    );
    let recipient_inbox_again =
        sandbox.cli_in(&sandbox.recipient, &["activity", "inbox", "--peek"]);
    assert_code(&recipient_inbox_again, 0);
    assert!(
        stdout(&recipient_inbox_again).contains("ASK #1"),
        "{:?}",
        stdout(&recipient_inbox_again)
    );
}

#[test]
fn activity_show_prints_one_message_in_full_without_acking() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli(&[
            "activity",
            "send",
            "recipient",
            "Ready for review",
            "--id",
            "msg-1",
        ]),
        0,
    );
    let state_dir = sandbox.recipient.join(".multiverse");
    let marker_before = fs::read_to_string(state_dir.join("activity.lastseen")).ok();

    let output = sandbox.cli_in(&sandbox.recipient, &["activity", "show", "1"]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("Ready for review"), "{body}");
    assert!(body.contains("sandbox"), "{body}");
    assert!(
        !state_dir.join("activity.cursor").exists(),
        "show must never acknowledge"
    );
    assert_eq!(
        fs::read_to_string(state_dir.join("activity.lastseen")).ok(),
        marker_before,
        "show must not touch the recipient's own lastseen marker either"
    );
}

/// `show` accepts the same `Project#SEQ` qualifier `close`/`reply` do, with
/// no `[activity.pairs]` gate — it only ever reads a log already on disk.
#[test]
fn activity_show_reads_another_projects_log_via_project_hash_seq() {
    let sandbox = Sandbox::new();
    let hub = enable_hub_project(&sandbox);
    assert_code(
        &sandbox.cli_in(
            &hub,
            &[
                "activity",
                "send",
                "sandbox",
                "which seat are you on?",
                "--ask",
                "--id",
                "ask-1",
            ],
        ),
        0,
    );
    let output = sandbox.cli_in(&sandbox.recipient, &["activity", "show", "sandbox#1"]);
    assert_code(&output, 0);
    let body = stdout(&output);
    assert!(body.contains("which seat are you on?"), "{body}");
    assert!(body.contains("ask: yes"), "{body}");
}

#[test]
fn activity_show_refuses_an_unknown_seq() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["activity", "show", "99"]);
    assert_code(&output, 3);
    assert!(
        stderr(&output).contains("no message #99"),
        "{}",
        stderr(&output)
    );
}

/// Card `20261115-b-2a0f8f`: `--as-nexus --replay-since`/`--replay-last`
/// used to fail parsing outright ("cannot be used with"); replay is
/// read-only regardless of `--as-nexus`, so the combination must be
/// accepted and must still never advance the read cursor.
#[test]
fn activity_inbox_replay_accepts_as_nexus_and_still_never_acknowledges() {
    let sandbox = Sandbox::new();
    assert_code(
        &sandbox.cli_in(
            &sandbox.recipient,
            &["activity", "send", "sandbox", "hello", "--id", "msg-1"],
        ),
        0,
    );
    for replay in [
        ["--replay-since", "1"].as_slice(),
        ["--replay-last", "1"].as_slice(),
    ] {
        let mut args = vec!["activity", "inbox", "--as-nexus"];
        args.extend_from_slice(replay);
        let output = sandbox.cli(&args);
        assert_code(&output, 0);
        assert!(stdout(&output).contains("hello"), "{}", stdout(&output));
    }
    assert!(!sandbox.project.join(".multiverse/activity.cursor").exists());
}

/// `start sandbox sandbox` — the same project named twice in one
/// invocation (a typo, or a shell glob that expands to a repeat) — must
/// not open two competing workspaces for it. `run`'s `wanted` list used
/// to pass every requested name straight into the per-project loop
/// against one `FleetState` snapshot taken before the loop started, so
/// the snapshot never learned about the workspace the first iteration
/// had just created; the second iteration still saw "not open" and
/// created a second one.
#[test]
fn start_with_a_duplicated_project_name_opens_only_one_workspace() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["start", "sandbox", "sandbox"]);
    assert_code(&output, 0);

    let workspaces = stdout(&sandbox.herdr(&["workspace", "list"]));
    let workspaces: serde_json::Value = serde_json::from_str(&workspaces).unwrap();
    let sandbox_workspaces = workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|w| w["label"] == "sandbox")
        .count();
    assert_eq!(
        sandbox_workspaces, 1,
        "duplicate project name opened {sandbox_workspaces} workspaces for \"sandbox\": {workspaces}"
    );
}

/// Card `20261117-b-07f239`: `[projects.<name>].autostart = false` is a
/// read-only knob consulted only by `start --all`'s own project list.
fn disable_autostart_for(sandbox: &Sandbox, project: &str) {
    let config_path = sandbox.root.join("config/config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    let header = format!("[projects.{project}]\n");
    assert!(config.contains(&header), "{config}");
    let config = config.replacen(&header, &format!("{header}autostart = false\n"), 1);
    fs::write(&config_path, &config).unwrap();
    mv_config::load::load_config(&config_path)
        .expect("autostart fixture must be valid under the real config schema");
}

fn workspace_is_open(sandbox: &Sandbox, label: &str) -> bool {
    let workspaces = stdout(&sandbox.herdr(&["workspace", "list"]));
    let workspaces: serde_json::Value = serde_json::from_str(&workspaces).unwrap();
    workspaces["result"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .any(|w| w["label"] == label)
}

#[test]
fn start_all_skips_a_project_with_autostart_false_and_names_it() {
    let sandbox = Sandbox::new();
    disable_autostart_for(&sandbox, "recipient");

    let output = sandbox.cli(&["start", "--all"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert!(
        text.lines().any(|line| line.contains("recipient")
            && line.contains("skipped")
            && line.contains("autostart = false")),
        "{text}"
    );

    assert!(
        workspace_is_open(&sandbox, "sandbox"),
        "autostart = false on recipient must not stop sandbox from starting"
    );
    assert!(
        !workspace_is_open(&sandbox, "recipient"),
        "recipient must not open under start --all once autostart = false"
    );
}

/// `start <name>` names the project explicitly, so `autostart = false`
/// — a flag `start --all`'s own project list consults — must not apply.
#[test]
fn start_named_project_still_starts_despite_autostart_false() {
    let sandbox = Sandbox::new();
    disable_autostart_for(&sandbox, "recipient");

    let output = sandbox.cli(&["start", "recipient"]);
    assert_code(&output, 0);
    assert!(
        workspace_is_open(&sandbox, "recipient"),
        "naming recipient explicitly must still start it despite autostart = false"
    );
}

fn worktree_git(sandbox: &Sandbox, args: &[&str]) -> String {
    let output = sandbox
        .command("git")
        .args([
            "-c",
            "user.name=Worktree Test",
            "-c",
            "user.email=worktree@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .unwrap();
    assert_code(&output, 0);
    stdout(&output).trim().to_owned()
}

#[test]
fn worktrees_prune_sweeps_recorded_orphans_without_deleting_unique_work() {
    let sandbox = Sandbox::new();
    let merged = sandbox.seed_task();
    let main = worktree_git(&sandbox, &["branch", "--show-current"]);
    let merged_branch = format!("multiverse/{merged}");
    worktree_git(&sandbox, &["branch", &merged_branch]);
    worktree_git(&sandbox, &["branch", "multiverse/unrecorded"]);
    worktree_git(&sandbox, &["checkout", "-b", "multiverse/patch"]);
    fs::write(sandbox.project.join("README.md"), "patch feature\n").unwrap();
    worktree_git(&sandbox, &["commit", "-am", "patch"]);
    let patch_tip = worktree_git(&sandbox, &["rev-parse", "HEAD"]);
    worktree_git(&sandbox, &["checkout", &main]);
    fs::write(sandbox.project.join("other"), "unrelated\n").unwrap();
    worktree_git(&sandbox, &["add", "other"]);
    worktree_git(&sandbox, &["commit", "-m", "unrelated"]);
    worktree_git(&sandbox, &["cherry-pick", &patch_tip]);
    worktree_git(&sandbox, &["checkout", "-b", "multiverse/unique"]);
    fs::write(sandbox.project.join("README.md"), "unique work\n").unwrap();
    worktree_git(&sandbox, &["commit", "-am", "unique"]);
    let unique_tip = worktree_git(&sandbox, &["rev-parse", "HEAD"]);
    worktree_git(&sandbox, &["checkout", &main]);
    worktree_git(&sandbox, &["branch", "multiverse/assessed", &unique_tip]);
    let assessor = sandbox
        .project
        .join(".multiverse/tasks/20260101-seer-dddddd");
    fs::create_dir_all(&assessor).unwrap();
    fs::write(assessor.join("result.md"), "Assessment finished\n").unwrap();
    fs::write(assessor.join("retire"), format!("assessed {unique_tip}\n")).unwrap();
    fs::File::open(assessor.join("retire"))
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(2)),
        )
        .unwrap();
    let log = sandbox.project.join(".multiverse/routing.log");
    let mut records = fs::read_to_string(&log).unwrap();
    records.push_str(
        "2026-01-02T00:00:00+00:00\tevent=dispatch\ttask=20260101-seer-dddddd\trole=seer\n\
         2026-01-02T01:00:00+00:00\tevent=result\ttask=20260101-seer-dddddd\tstatus=done\n",
    );
    for task in ["patch", "unique", "assessed"] {
        records.push_str(&format!(
            "2026-01-01T00:00:00+00:00\tevent=result\ttask={task}\tstatus=done\n"
        ));
    }
    fs::write(log, records).unwrap();

    let before = sandbox.snapshot();
    let preview = sandbox.cli(&[
        "worktrees",
        sandbox.project.to_str().unwrap(),
        "--prune",
        "--dry-run",
    ]);
    assert_code(&preview, 0);
    let report = stdout(&preview);
    assert!(
        report.contains(&format!("reclaimed {merged_branch}")),
        "{report}"
    );
    assert!(report.contains("archived multiverse/patch"), "{report}");
    assert!(report.contains("archived multiverse/assessed"), "{report}");
    assert!(report.contains("kept multiverse/unique"), "{report}");
    assert!(report.contains("kept multiverse/unrecorded"), "{report}");
    assert_eq!(sandbox.snapshot(), before);

    let result = sandbox.cli(&["worktrees", sandbox.project.to_str().unwrap(), "--prune"]);
    assert_code(&result, 0);
    assert!(!sandbox
        .project
        .join(format!(".git/refs/heads/{merged_branch}"))
        .exists());
    assert_eq!(
        worktree_git(&sandbox, &["rev-parse", "refs/multiverse/archive/patch"]),
        patch_tip
    );
    assert!(!sandbox
        .project
        .join(".git/refs/heads/multiverse/patch")
        .exists());
    assert_eq!(
        worktree_git(&sandbox, &["rev-parse", "refs/multiverse/archive/assessed"]),
        unique_tip
    );
    assert_eq!(
        worktree_git(&sandbox, &["rev-parse", "multiverse/unique"]),
        unique_tip
    );
    assert!(sandbox
        .project
        .join(".git/refs/heads/multiverse/unrecorded")
        .exists());
}

#[test]
fn worktrees_deep_requires_prune_before_any_side_effect() {
    let sandbox = Sandbox::new();
    let before = sandbox.snapshot();
    for extra in [vec![], vec!["--rescue"], vec!["--restore", "unknown"]] {
        let mut args = vec!["worktrees", sandbox.project.to_str().unwrap(), "--deep"];
        args.extend(extra);
        let output = sandbox.cli(&args);
        assert_code(&output, 1);
        assert_eq!(
            stderr(&output).trim(),
            "--deep is only meaningful together with --prune"
        );
        assert!(stdout(&output).is_empty());
        assert_eq!(sandbox.snapshot(), before);
    }
}

#[test]
fn worktrees_prune_confirmation_allows_ignored_files_but_never_tracked_changes() {
    for confirmation in ["--yes", "--force"] {
        let sandbox = Sandbox::new();
        let task = sandbox.seed_task();
        fs::write(
            sandbox.project.join(".gitignore"),
            ".multiverse/\nignored/\n",
        )
        .unwrap();
        worktree_git(&sandbox, &["add", ".gitignore"]);
        worktree_git(&sandbox, &["commit", "-m", "ignore fixture state"]);
        let worktrees = mv_state::worktrees::Worktrees::new(&sandbox.project).unwrap();
        let lane = worktrees.create(task, "HEAD").unwrap();
        fs::create_dir(lane.path.join("ignored")).unwrap();
        let ignored = lane.path.join("ignored/only-copy");
        fs::write(&ignored, "confirmed disposal\n").unwrap();
        let path = sandbox.project.to_str().unwrap();
        let preview = sandbox.cli(&["worktrees", path, "--prune", "--dry-run"]);
        assert_code(&preview, 0);
        assert!(stdout(&preview).contains("unrescued ignored file"));
        let confirmed = sandbox.cli(&["worktrees", path, "--prune", "--dry-run", confirmation]);
        assert_code(&confirmed, 0);
        assert!(
            stdout(&confirmed).contains("would remove"),
            "{}",
            stdout(&confirmed)
        );
        assert_eq!(
            fs::read_to_string(&ignored).unwrap(),
            "confirmed disposal\n"
        );
        assert!(worktrees.branch_exists(task).unwrap());

        fs::write(lane.path.join("README.md"), "uncommitted work\n").unwrap();
        let dirty = sandbox.cli(&["worktrees", path, "--prune", confirmation]);
        assert_code(&dirty, 0);
        assert!(stdout(&dirty).contains("lane has uncommitted changes"));
        assert!(ignored.exists());
        worktree_git(
            &sandbox,
            &[
                "-C",
                lane.path.to_str().unwrap(),
                "checkout",
                "--",
                "README.md",
            ],
        );

        let removed = sandbox.cli(&["worktrees", path, "--prune", confirmation]);
        assert_code(&removed, 0);
        assert!(!lane.path.exists(), "{}", stdout(&removed));
        assert!(!worktrees.branch_exists(task).unwrap());
    }
}

/// Card `20261119-b-73759c`: `worktrees --prune` without `--yes` used to
/// delete an orphaned shared-build hash dir anyway, since that sweep read
/// only `--dry-run`. Every combination short of a real `--yes` now leaves
/// it on disk and reports it as `--dry-run` would ("would free").
#[test]
fn worktrees_prune_frees_an_orphaned_build_dir_only_with_yes() {
    let sandbox = Sandbox::new();
    let cargo_home = sandbox.root.join("cargo-home");
    fs::create_dir_all(sandbox.project.join(".cargo")).unwrap();
    fs::write(
        sandbox.project.join(".cargo/config.toml"),
        "[build]\nbuild-dir = \"{cargo-cache-home}/shared-build/{workspace-path-hash}\"\n",
    )
    .unwrap();
    // A dead workspace's own dep-info, under the real two-level
    // `<bucket>/<leaf>` hash-dir shape, naming only a path now gone.
    let vanished = sandbox.root.join("vanished-workspace");
    let orphan = cargo_home.join("shared-build/2a/00000000000009");
    fs::create_dir_all(orphan.join("debug/deps")).unwrap();
    fs::write(
        orphan.join("debug/deps/fixture.d"),
        format!("{}/src/lib.rs\n", vanished.display()),
    )
    .unwrap();
    fs::write(orphan.join("debug/deps/artifact"), vec![0u8; 4096]).unwrap();
    let path = sandbox.project.to_str().unwrap();
    let prune = |extra: &[&str]| {
        sandbox
            .command(env!("CARGO_BIN_EXE_multiverse"))
            .env("CARGO_HOME", &cargo_home)
            .args(["worktrees", path, "--prune"])
            .args(extra)
            .output()
            .expect("run built multiverse binary")
    };

    for extra in [&[][..], &["--dry-run"], &["--dry-run", "--yes"]] {
        let output = prune(extra);
        assert_code(&output, 0);
        let out = stdout(&output);
        assert!(out.contains("would free build-dir ("), "{extra:?}: {out}");
        assert!(!out.contains("    freed "), "{extra:?}: {out}");
        assert!(orphan.exists(), "{extra:?} must not delete the build-dir");
    }

    let confirmed = prune(&["--yes"]);
    assert_code(&confirmed, 0);
    let out = stdout(&confirmed);
    assert!(out.contains("    freed build-dir ("), "{out}");
    assert!(!orphan.exists(), "--prune --yes must delete the build-dir");
}

/// A sandbox whose one merged, finished lane has its own shared-build hash
/// dir under `<root>/cargo-home`; returns that hash dir.
fn lane_with_build_dir(sandbox: &Sandbox) -> PathBuf {
    let task = sandbox.seed_task();
    fs::write(sandbox.project.join(".gitignore"), ".multiverse/\n").unwrap();
    fs::create_dir_all(sandbox.project.join(".cargo")).unwrap();
    fs::write(
        sandbox.project.join(".cargo/config.toml"),
        "[build]\nbuild-dir = \"{cargo-cache-home}/shared-build/{workspace-path-hash}\"\n",
    )
    .unwrap();
    worktree_git(sandbox, &["add", ".gitignore", ".cargo/config.toml"]);
    worktree_git(sandbox, &["commit", "-m", "shared build-dir fixture"]);
    let lane = mv_state::worktrees::Worktrees::new(&sandbox.project)
        .unwrap()
        .create(task, "HEAD")
        .unwrap();
    let build_dir = sandbox
        .root
        .join("cargo-home/shared-build/3b/00000000000013");
    fs::create_dir_all(build_dir.join("debug/deps")).unwrap();
    // rustc's dep-info escapes each space in a path as `\ `; the sandbox
    // project path has spaces (card `20261119-b-087c6c`).
    fs::write(
        build_dir.join("debug/deps/fixture.d"),
        format!(
            "{}/src/lib.rs\n",
            lane.path.display().to_string().replace(' ', "\\ ")
        ),
    )
    .unwrap();
    fs::write(build_dir.join("debug/deps/artifact"), vec![0u8; 4096]).unwrap();
    build_dir
}

/// Card `20261119-b-c3f9f5`: plain `--prune` removes an eligible lane but
/// only measures its build-dir, and the orphaned shared-build sweep in the
/// same run used to report that same hash dir a second time. Each hash dir
/// is now reported once per run, and `--yes` still frees it exactly once.
#[test]
fn worktrees_prune_reports_a_removed_lanes_build_dir_once_per_run() {
    let prune = |sandbox: &Sandbox, extra: &[&str]| {
        let output = sandbox
            .command(env!("CARGO_BIN_EXE_multiverse"))
            .env("CARGO_HOME", sandbox.root.join("cargo-home"))
            .args(["worktrees", sandbox.project.to_str().unwrap(), "--prune"])
            .args(extra)
            .output()
            .expect("run built multiverse binary");
        assert_code(&output, 0);
        stdout(&output)
    };

    let sandbox = Sandbox::new();
    let build_dir = lane_with_build_dir(&sandbox);
    let out = prune(&sandbox, &[]);
    assert!(out.contains("    removed"), "{out}");
    assert_eq!(out.matches("build-dir (").count(), 1, "{out}");
    assert!(out.contains("    would free build-dir ("), "{out}");
    assert!(!out.contains("cargo shared-build"), "{out}");
    assert!(build_dir.exists(), "plain --prune must not delete it");
    // The lane is gone now, so only the sweep still sees the hash dir.
    let out = prune(&sandbox, &["--yes"]);
    assert_eq!(out.matches("build-dir (").count(), 1, "{out}");
    assert!(out.contains("    freed build-dir ("), "{out}");
    assert!(!build_dir.exists(), "--prune --yes must delete it");

    let sandbox = Sandbox::new();
    let build_dir = lane_with_build_dir(&sandbox);
    let out = prune(&sandbox, &["--yes"]);
    assert!(out.contains("    removed"), "{out}");
    assert_eq!(out.matches("build-dir (").count(), 1, "{out}");
    assert!(out.contains("    freed build-dir ("), "{out}");
    assert!(!out.contains("cargo shared-build"), "{out}");
    assert!(!build_dir.exists(), "--prune --yes must delete it");
}

/// Card `20261119-b-73759c`'s `--deep` half: a redundant
/// `.multiverse/worktree-state/` quarantine copy is only previewed
/// ("would remove") by `--prune --deep` without `--yes`.
#[test]
fn worktrees_prune_deep_removes_a_quarantine_copy_only_with_yes() {
    let sandbox = Sandbox::new();
    let state = sandbox.project.join(".multiverse");
    let copy = state.join("worktree-state/20260101-builder-aaaaaa");
    for root in [&state, &copy] {
        fs::create_dir_all(root.join("tasks/20260101-builder-aaaaaa")).unwrap();
        fs::write(root.join("tasks/20260101-builder-aaaaaa/status"), "done\n").unwrap();
    }
    let path = sandbox.project.to_str().unwrap();

    let unconfirmed = sandbox.cli(&["worktrees", path, "--prune", "--deep"]);
    assert_code(&unconfirmed, 0);
    let out = stdout(&unconfirmed);
    assert!(
        out.contains("deep would remove .multiverse/worktree-state/20260101-builder-aaaaaa"),
        "{out}"
    );
    assert!(out.contains("quarantine cleanup: would free"), "{out}");
    assert!(copy.exists(), "--deep without --yes must keep the copy");

    let confirmed = sandbox.cli(&["worktrees", path, "--prune", "--deep", "--yes"]);
    assert_code(&confirmed, 0);
    let out = stdout(&confirmed);
    assert!(out.contains("quarantine cleanup: freed"), "{out}");
    assert!(!copy.exists(), "--deep --yes must remove the copy");
}

#[test]
fn backlog_uses_configured_project_from_subdirectories_and_lanes() {
    let sandbox = Sandbox::new();
    let created = sandbox.cli(&["backlog", "add", "project root card"]);
    assert_code(&created, 0);
    let item = stdout(&created)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    let lane = sandbox.project.join(".multiverse/worktrees/fixture/src");
    fs::create_dir_all(lane.join(".multiverse")).unwrap();
    let shown = sandbox.cli_in(&lane, &["backlog", "show", &item]);
    assert_code(&shown, 0);
    assert!(stdout(&shown).contains("project root card"));
    let added = sandbox.cli_in(&lane, &["backlog", "add", "from lane"]);
    assert_code(&added, 0);
    assert_eq!(
        mv_state::backlog::read_items(&sandbox.project, false).len(),
        2
    );
    assert!(!lane.join(".multiverse/backlog.jsonl").exists());
    let outside = sandbox.root.join("outside");
    fs::create_dir_all(outside.join(".multiverse")).unwrap();
    assert_code(&sandbox.cli_in(&outside, &["backlog", "add", "refuse"]), 3);
}

#[test]
fn add_non_git_refusal_keeps_config_and_project_unchanged() {
    let sandbox = Sandbox::new();
    let project = sandbox.root.join("non-git");
    fs::create_dir(&project).unwrap();
    let config = sandbox.root.join("config/config.toml");
    let before = fs::read(&config).unwrap();
    for flags in [vec![], vec!["--dry-run"]] {
        let mut args = vec!["add", project.to_str().unwrap()];
        args.extend(flags);
        let output = sandbox.cli(&args);
        assert_code(&output, 3);
        assert!(stderr(&output).contains("git init"));
        assert_eq!(fs::read(&config).unwrap(), before);
        assert_eq!(fs::read_dir(&project).unwrap().count(), 0);
    }
}

#[test]
fn accounts_usage_read_only_returns_codex_gauge_without_writing_it() {
    let sandbox = Sandbox::new();
    let sessions = sandbox.root.join("codex-home/sessions");
    fs::create_dir(&sessions).unwrap();
    fs::write(
        sessions.join("rollout.jsonl"),
        r#"{"type":"token_count","payload":{"rate_limits":{"primary":{"used_percent":40}}}}"#,
    )
    .unwrap();
    let output = sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("MULTIVERSE_READ_ONLY", "1")
        .args(["accounts", "usage"])
        .output()
        .unwrap();
    assert_code(&output, 0);
    assert!(stdout(&output).contains("60% left"));
    assert!(!sandbox.root.join("runtime/gauge-state.json").exists());
    assert!(!sandbox.root.join("runtime/gauge-state.lock").exists());
}

#[test]
fn check_reports_oversized_logs_docker_context_and_idle_sleep_without_writes() {
    use std::os::unix::fs::PermissionsExt;
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    let log = sandbox.project.join(".multiverse/routing.log");
    let content = "comment\n".repeat(10_001);
    fs::write(&log, &content).unwrap();
    fs::write(
        sandbox.project.join("Dockerfile"),
        "FROM scratch\nCOPY . /app\n",
    )
    .unwrap();
    let fake_pmset = sandbox.root.join("fake-pmset");
    fs::write(
        &fake_pmset,
        "#!/bin/sh\nprintf '%s\\n' 'System-wide power settings:' 'Currently in use:' ' sleep 10'\n",
    )
    .unwrap();
    fs::set_permissions(&fake_pmset, fs::Permissions::from_mode(0o700)).unwrap();
    let output = sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("MULTIVERSE_PMSET_BIN", &fake_pmset)
        .arg("check")
        .output()
        .unwrap();
    assert_code(&output, 0);
    let text = stdout(&output);
    assert!(text.contains("routing log is 10,001 lines"), "{text}");
    assert!(
        text.contains("without excluding .multiverse in .dockerignore"),
        "{text}"
    );
    assert!(text.contains("idle-sleep"), "{text}");
    assert_eq!(fs::read_to_string(log).unwrap(), content);
    assert!(!sandbox.root.join("runtime/breaker-state.json").exists());
}

/// Row #39: a Claude seat without `command` exhausts the chain that names
/// it (`SkipCause.NO_COMMAND` → `ChainExhaustedError`). `check` reports
/// the row and exits 3, matching `test_check_reports_a_missing_claude_command_wrapper`.
#[test]
fn check_reports_unresolved_claude_chain_as_needs_config() {
    let sandbox = Sandbox::new();
    let config = format!(
        r#"[accounts.primary]
kind = "claude"
short = "pr"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "{home}"
pool = "claude:primary"

[roles.scout]
write_scope = "none"
effort = "low"
on_exhausted = "degrade"
skills = []

[roles.scout.models]
claude = "m"

[projects.sandbox]
path = '{project}'

[projects.sandbox.chains]
scout = ["primary"]
"#,
        home = sandbox.root.join("codex-home").display(),
        project = sandbox.project.display(),
    );
    fs::write(sandbox.root.join("config/config.toml"), config).unwrap();
    let output = sandbox.cli(&["check"]);
    let text = stdout(&output);
    assert_code(&output, 3);
    assert!(text.contains("UNRESOLVED: ChainExhaustedError"), "{text}");
    assert!(text.contains("note: account primary"), "{text}");
    assert!(text.contains("required command wrapper"), "{text}");
    assert!(text.contains("accounts.primary.command"), "{text}");
}

/// Row #39: `--verbose` names a seed whose digest no longer matches the
/// current dispatch table / role prompt. Written at the legacy filename
/// (not `nexus.md`) to prove `check` still judges a project that has not
/// been reseeded since the rename, rather than reporting it missing.
#[test]
fn check_verbose_reports_a_stale_nexus_seed() {
    let sandbox = Sandbox::new();
    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/orchestrator.md"),
        "# Nexus — sandbox\n<!-- multiverse:seed-digest 0123456789abcdef -->\nstale body\n",
    )
    .unwrap();
    let output = sandbox.cli(&["check", "--verbose"]);
    let text = stdout(&output);
    assert_code(&output, 0);
    assert!(text.contains("Nexus seed is stale"), "{text}");
    assert!(!text.to_lowercase().contains("orchestrator"), "{text}");
}

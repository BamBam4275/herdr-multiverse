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

#[test]
fn activity_update_refuses_an_unknown_card() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["activity", "update", "not-a-card", "--phase", "working"]);
    assert_code(&output, 3);
    let diagnostic = stderr(&output);
    assert!(diagnostic.contains("no dispatched card"), "{diagnostic}");
    assert!(!sandbox.project.join(".multiverse/routing.log").exists());
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
/// current dispatch table / role prompt.
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
    assert!(text.contains("orchestrator assignment is stale"), "{text}");
}

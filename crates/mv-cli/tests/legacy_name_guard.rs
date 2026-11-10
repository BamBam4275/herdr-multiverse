//! Card `20261108-b-d51eab` (Lane I): a guard against *any* user-visible CLI
//! surface printing a retired role name (`orchestrator`, `explorer`,
//! `fixer`, `librarian`, `oracle`, `designer`, `observer`, `reviewer`)
//! instead of its current replacement. The fixes this guards are already on
//! `main` (board STATUS title, board role footers, backlog done/reopen
//! refusals, activity error, check stale-seed note, dispatch review-budget
//! error, start batch summary, seed file `nexus.md`); this file is the
//! regression net, not a fix.
//!
//! Every fixture below seeds its sandbox with history recorded *under the
//! old names* (a routing log carrying `role=fixer`/`role=librarian`/
//! `role=orchestrator` dispatch lines, task directories for them, and a
//! backlog card added with an old-key role) — proving each surface
//! *translates* legacy on-disk data to its current name, not merely that it
//! echoes back whatever a caller typed.
//!
//! Run with `cargo test -p mv-cli --test legacy_name_guard --locked`.

// This test only needs `Sandbox::new`/`cli`/`command`/`dispatch`/`project`;
// `support`'s other helpers (`herdr`, `snapshot`, `task_dirs`, `seed_task`)
// exist for `cli_workflows.rs`'s own coverage and would otherwise warn as
// unused in this binary's own compilation unit (each integration test
// compiles `mod support` fresh) — same reasoning `prompt_commands.rs`
// already documents next to its own identical `#[allow(dead_code)]`.
#[allow(dead_code)]
mod support;

use std::fs;

use serde_json::Value;
use support::{assert_code, stderr, stdout, Sandbox};

// ---------------------------------------------------------------------
// The scan itself: retired words, word-boundary matching, and the three
// narrow, explicitly documented exceptions.
// ---------------------------------------------------------------------

/// The nine Python roles minus `council` (unchanged in both name lists) —
/// every config/CLI key this rename actually touched.
const RETIRED_WORDS: [&str; 8] = [
    "orchestrator",
    "explorer",
    "fixer",
    "librarian",
    "oracle",
    "designer",
    "observer",
    "reviewer",
];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Exception 1: a task id of the form `YYYYMMDD-<word>-<hex6>` — the id
/// itself is a historical identifier, not user-facing role prose, and
/// `support::Sandbox::seed_task`/this file's own fixtures mint them with a
/// retired word on purpose.
fn covered_by_task_id(bytes: &[u8], start: usize, end: usize) -> bool {
    if start < 9 {
        return false;
    }
    let date = &bytes[start - 9..start - 1];
    if bytes[start - 1] != b'-' || !date.iter().all(u8::is_ascii_digit) {
        return false;
    }
    if start >= 10 && is_word_byte(bytes[start - 10]) {
        return false; // the date itself must start a token
    }
    if end >= bytes.len() || bytes[end] != b'-' {
        return false;
    }
    let hex_start = end + 1;
    let hex_end = hex_start + 6;
    if hex_end > bytes.len() || !bytes[hex_start..hex_end].iter().all(u8::is_ascii_hexdigit) {
        return false;
    }
    if hex_end < bytes.len() && is_word_byte(bytes[hex_end]) {
        return false; // the hex suffix must end a token
    }
    true
}

/// Exception 2: the `orchestrator-pass` marker file name, if it ever
/// prints (named explicitly in this task's brief; nothing in this guard
/// currently drives a surface that prints it, so this exists defensively).
fn covered_by_orchestrator_pass_marker(bytes: &[u8], end: usize) -> bool {
    const SUFFIX: &[u8] = b"-pass";
    let suffix_end = end + SUFFIX.len();
    if suffix_end > bytes.len() || &bytes[end..suffix_end] != SUFFIX {
        return false;
    }
    suffix_end == bytes.len() || !is_word_byte(bytes[suffix_end])
}

/// Every retired-word hit in `text` that is not covered by one of the two
/// on-disk-identifier exceptions above. Exception 3 (`config migrate`'s own
/// help text, which must document the old keys it rewrites) is handled by
/// the caller skipping the scan entirely for that one surface — see
/// `help_text_never_leaks_a_retired_role_name`.
fn find_retired_hits(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let lower: Vec<u8> = bytes.iter().map(u8::to_ascii_lowercase).collect();
    let mut hits = Vec::new();
    for word in RETIRED_WORDS {
        let wb = word.as_bytes();
        let wlen = wb.len();
        if lower.len() < wlen {
            continue;
        }
        for start in 0..=lower.len() - wlen {
            if &lower[start..start + wlen] != wb {
                continue;
            }
            let end = start + wlen;
            let left_ok = start == 0 || !is_word_byte(lower[start - 1]);
            let right_ok = end == lower.len() || !is_word_byte(lower[end]);
            if !left_ok || !right_ok {
                continue;
            }
            if covered_by_task_id(&lower, start, end) {
                continue;
            }
            if word == "orchestrator" && covered_by_orchestrator_pass_marker(&lower, end) {
                continue;
            }
            let snippet_start = start.saturating_sub(24);
            let snippet_end = (end + 24).min(bytes.len());
            hits.push(format!(
                "{word:?} at byte {start}: ...{}...",
                String::from_utf8_lossy(&bytes[snippet_start..snippet_end])
            ));
        }
    }
    hits
}

fn assert_no_retired_words(context: &str, text: &str) {
    let hits = find_retired_hits(text);
    assert!(
        hits.is_empty(),
        "{context} leaked a retired role name: {hits:#?}\n--- full output ---\n{text}\n--- end ---"
    );
}

/// Scans both streams of a captured CLI invocation.
fn assert_surface_clean(context: &str, output: &std::process::Output) {
    assert_no_retired_words(&format!("{context} (stdout)"), &stdout(output));
    assert_no_retired_words(&format!("{context} (stderr)"), &stderr(output));
}

// ---------------------------------------------------------------------
// Fixtures: history recorded under the old names.
// ---------------------------------------------------------------------

/// Three unrecorded, terminal tasks — one per retired writing/non-writing
/// role this task's brief calls out (`fixer`, `librarian`, `orchestrator`)
/// — each with a task directory and a routing-log `event=dispatch` line
/// carrying the old `role=` key and no matching `event=result`. Mirrors
/// `board_data`'s own
/// `an_unrecorded_terminal_status_with_no_live_agents_gets_the_generic_note`
/// fixture shape, which is what proves `display_role` (not this test) does
/// the translating; this file only proves the CLI surface built on top of
/// it never regresses back to printing the raw key.
///
/// Returns the three task ids, in the order fixer, librarian, orchestrator.
fn seed_legacy_dispatch_history(sandbox: &Sandbox) -> [String; 3] {
    let tasks_dir = sandbox.project.join(".multiverse/tasks");
    let mut log = String::new();
    let mut ids = Vec::new();
    for (n, role) in ["fixer", "librarian", "orchestrator"].iter().enumerate() {
        let task_id = format!("2025010{}-{role}-{:06x}", n + 1, 0xabcdef_u32 + n as u32);
        let dir = tasks_dir.join(&task_id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("brief.md"), format!("# Historical {role} task\n")).unwrap();
        fs::write(dir.join("status"), "done\n").unwrap();
        log.push_str(&format!(
            "2025-01-0{}T00:00:00+00:00\tevent=dispatch\ttask={task_id}\tproject=sandbox\trole={role}\taccount=old-seat\tpool=codex:legacy\toutcome=chain-primary\n",
            n + 1
        ));
        ids.push(task_id);
    }
    fs::write(sandbox.project.join(".multiverse/routing.log"), log).unwrap();
    ids.try_into().unwrap()
}

/// A backlog card whose `role` field was recorded under an old config key
/// — the CLI's own `backlog add --role fixer` refuses that spelling on new
/// input (`retired_role_inputs_exit_three_and_hint_at_current_names`,
/// `cli_workflows.rs`), so this goes through `mv_state::backlog::add`
/// directly, the same way `support::Sandbox::seed_task` writes a legacy
/// routing-log line by hand instead of driving it through the CLI.
fn seed_legacy_backlog_card(sandbox: &Sandbox) -> String {
    let item = mv_state::backlog::add(
        &sandbox.project,
        "Legacy-authored backlog card",
        "sandbox",
        "recorded before the role-key rename",
        None,
        None,
        Some("fixer"),
        "P2",
        None,
    )
    .expect("role is just a free-form historical string on this write path");
    item.item
}

/// `.multiverse/orchestrator.md` — the pre-rename seed filename — present
/// and stale, so `check --verbose` must report it under the current
/// terminology. Exact fixture `check_verbose_reports_a_stale_nexus_seed`
/// (`cli_workflows.rs`) already uses to prove the same thing for `check`
/// alone; duplicated here (not imported) because that test lives in a
/// sibling binary this crate does not expose as a library.
fn seed_stale_legacy_nexus_seed(sandbox: &Sandbox) {
    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/orchestrator.md"),
        "# Nexus — sandbox\n<!-- multiverse:seed-digest 0123456789abcdef -->\nstale body\n",
    )
    .unwrap();
}

/// Adds a second, unused-in-any-chain interactive account and a
/// `[projects.multiverse]` entry, so `start` can seed both a project Nexus
/// and the home hub Nexus in one batch. A fresh account (not `test-seat`,
/// already named in `scout`/`builder` chains) sidesteps
/// `validate_interactive_seats`'s refusal of an interactive seat also used
/// as a worker seat without `lend_interactive_to_workers` — this fixture
/// changes nothing about the base sandbox's existing chains.
fn enable_interactive_nexus_and_hub(sandbox: &Sandbox) -> std::path::PathBuf {
    let config_path = sandbox.root.join("config/config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    let hub_home = sandbox.root.join("hub-home");
    config.push_str(&format!(
        "\n[accounts.nexus-seat]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = '{codex_home}'\npool = \"codex:nexus\"\ninteractive = true\n\n\
         [projects.multiverse]\nshort = \"mv\"\npath = '{hub_home}'\n",
        codex_home = sandbox.root.join("codex-home").display(),
        hub_home = hub_home.display(),
    ));
    fs::write(&config_path, &config).unwrap();
    fs::copy(
        &config_path,
        sandbox.root.join("home/.config/multiverse/config.toml"),
    )
    .unwrap();
    hub_home
}

/// A live, dispatched (non-Nexus) scout agent, parked at the fake's first
/// opened pane (`w52:p100`, same literal every other integration test in
/// this crate relies on — the fake's pane ids are sequential from a fresh
/// sandbox). Returns that pane id, for a caller to impersonate via
/// `HERDR_PANE_ID` when driving a command that resolves "am I a dispatched
/// task" through live pane identity.
fn dispatch_live_scout(sandbox: &Sandbox) -> &'static str {
    let output = sandbox.dispatch("Map the repository", &["--yes"]);
    assert_code(&output, 0);
    "w52:p100"
}

/// A single recorded, done task dispatched under the legacy writing-role
/// key `fixer`, with no `branch=` (so `reclaim_lane_is_safe` finds no
/// `.multiverse/worktrees/<task>` lane and reads it clean) and its pane
/// still open — `reclaimable_panes`'s own shape for a "would close"
/// candidate. The pane is a real fake-herdr occupant, started with an
/// empty name, matching the dispatch line's own absent `agent=` field:
/// `occupant_matches` needs the live agent's `name` to equal
/// `task.agent.unwrap_or("")`, and only a real `agent start` leaves that
/// agent behind for `tasks --reclaimable`'s own `agent list` call to find
/// (a transcript stub, as conformance scenarios use, has no counterpart
/// here — this test drives the real built fake-herdr binary throughout).
fn seed_reclaimable_legacy_fixer_task(sandbox: &Sandbox) -> String {
    let workspace = sandbox.herdr(&[
        "workspace",
        "create",
        "--label",
        "legacy-reclaim",
        "--cwd",
        sandbox.project.to_str().unwrap(),
    ]);
    let workspace: Value = serde_json::from_slice(&workspace.stdout).expect("JSON reply");
    let pane_id = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .expect("workspace create returns a root pane id")
        .to_string();
    sandbox.herdr(&["agent", "start", "", "--pane", &pane_id, "--kind", "codex"]);

    let task_id = "20250101-fixer-aaaaaa".to_string();
    fs::create_dir_all(sandbox.project.join(".multiverse")).unwrap();
    fs::write(
        sandbox.project.join(".multiverse/routing.log"),
        format!(
            "2025-01-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\tproject=sandbox\trole=fixer\taccount=old-seat\tpool=codex:integration\toutcome=chain-primary\n\
             2025-01-01T00:01:00+00:00\tevent=started\ttask={task_id}\tpane={pane_id}\n\
             2025-01-01T00:02:00+00:00\tevent=result\ttask={task_id}\tstatus=done\tstate=done\trevisions=0\n"
        ),
    )
    .unwrap();
    task_id
}

// ---------------------------------------------------------------------
// board
// ---------------------------------------------------------------------

#[test]
fn board_all_read_only_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let legacy_tasks = seed_legacy_dispatch_history(&sandbox);
    let card = seed_legacy_backlog_card(&sandbox);

    let output = sandbox.cli(&["board", "--all", "--read-only"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert_no_retired_words("board --all --read-only (stdout)", &text);
    assert_no_retired_words("board --all --read-only (stderr)", &stderr(&output));

    // Translation proof, not just absence: the legacy `fixer`/`librarian`
    // dispatch lines and the legacy-role backlog card must still show up
    // as *something* current, not simply vanish from the board.
    assert!(
        text.contains("builder") || text.contains("archivist"),
        "legacy fixer/librarian history should render under its current \
         role name somewhere on the board: {text}"
    );
    let _ = (legacy_tasks, card);
}

#[test]
fn board_all_read_only_no_color_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_legacy_dispatch_history(&sandbox);
    seed_legacy_backlog_card(&sandbox);

    let output = sandbox.cli(&["board", "--all", "--read-only", "--no-color"]);
    assert_code(&output, 0);
    assert_surface_clean("board --all --read-only --no-color", &output);
}

#[test]
fn board_single_project_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_legacy_dispatch_history(&sandbox);
    seed_legacy_backlog_card(&sandbox);

    let output = sandbox.cli(&["board", "sandbox", "--read-only"]);
    assert_code(&output, 0);
    assert_surface_clean("board sandbox --read-only", &output);
}

// ---------------------------------------------------------------------
// check
// ---------------------------------------------------------------------

#[test]
fn check_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_legacy_dispatch_history(&sandbox);

    let output = sandbox.cli(&["check"]);
    assert_surface_clean("check", &output);
}

#[test]
fn check_verbose_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_legacy_dispatch_history(&sandbox);
    seed_stale_legacy_nexus_seed(&sandbox);

    let output = sandbox.cli(&["check", "--verbose"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert_no_retired_words("check --verbose (stdout)", &text);
    assert_no_retired_words("check --verbose (stderr)", &stderr(&output));
    assert!(
        text.contains("Nexus seed is stale"),
        "the legacy orchestrator.md fixture should still be read as a \
         stale seed, under the current name: {text}"
    );
}

// ---------------------------------------------------------------------
// start — drives a real Nexus launch against fake-herdr for both a
// project and the home hub in one batch, so both summary paragraphs
// `batch_summary` can print are exercised together.
// ---------------------------------------------------------------------

#[test]
fn start_project_and_hub_batch_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    enable_interactive_nexus_and_hub(&sandbox);

    let output = sandbox.cli(&["start", "sandbox", "multiverse"]);
    let text = stdout(&output);
    assert_no_retired_words("start sandbox multiverse (stdout)", &text);
    assert_no_retired_words("start sandbox multiverse (stderr)", &stderr(&output));
    assert_code(&output, 0);
    assert!(
        text.contains("opened 2 of 2"),
        "expected both the project and the hub to open against the fake: {text}"
    );
    assert!(text.contains("Nexus"), "{text}");
}

// ---------------------------------------------------------------------
// backlog done / reopen refused as a dispatched caller
// ---------------------------------------------------------------------

#[test]
fn backlog_done_refusal_as_a_dispatched_caller_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let pane = dispatch_live_scout(&sandbox);
    let added = sandbox.cli(&["backlog", "add", "Card a dispatched caller may not close"]);
    assert_code(&added, 0);
    let id = stdout(&added)
        .split_whitespace()
        .nth(1)
        .expect("added <id>")
        .to_string();

    let output = sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("HERDR_PANE_ID", pane)
        .args(["backlog", "done", &id])
        .output()
        .expect("run built multiverse binary");
    assert_code(&output, 1);
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("backlog done was refused"),
        "{diagnostic}"
    );
    assert_no_retired_words("backlog done refusal (stdout)", &stdout(&output));
    assert_no_retired_words("backlog done refusal (stderr)", &diagnostic);
    assert!(diagnostic.contains("Nexus"), "{diagnostic}");
}

#[test]
fn backlog_reopen_refusal_as_a_dispatched_caller_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let pane = dispatch_live_scout(&sandbox);
    let added = sandbox.cli(&["backlog", "add", "Card a dispatched caller may not reopen"]);
    assert_code(&added, 0);
    let id = stdout(&added)
        .split_whitespace()
        .nth(1)
        .expect("added <id>")
        .to_string();

    let output = sandbox
        .command(env!("CARGO_BIN_EXE_multiverse"))
        .env("HERDR_PANE_ID", pane)
        .args(["backlog", "reopen", &id])
        .output()
        .expect("run built multiverse binary");
    assert_code(&output, 1);
    let diagnostic = stderr(&output);
    assert!(
        diagnostic.contains("backlog reopen was refused"),
        "{diagnostic}"
    );
    assert_no_retired_words("backlog reopen refusal (stdout)", &stdout(&output));
    assert_no_retired_words("backlog reopen refusal (stderr)", &diagnostic);
    assert!(diagnostic.contains("Nexus"), "{diagnostic}");
}

// ---------------------------------------------------------------------
// --help, for every top-level and nested subcommand — enumerated from
// clap at runtime via the binary's own `--help` output, not a hand list
// that would go stale the next time a subcommand is added.
// ---------------------------------------------------------------------

/// Parses the subcommand names out of a clap `Commands:` section: a line
/// indented by exactly two spaces, name first. Stops at the first
/// less-indented (or blank) line, same as where clap's own section ends.
fn parse_subcommands(help_text: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_commands = false;
    for line in help_text.lines() {
        if line == "Commands:" {
            in_commands = true;
            continue;
        }
        if !in_commands {
            continue;
        }
        if !line.starts_with("  ") {
            in_commands = false;
            continue;
        }
        let rest = &line[2..];
        if rest.starts_with(' ') || rest.trim().is_empty() {
            continue; // a wrapped continuation line, not a new command
        }
        if let Some(name) = rest.split_whitespace().next() {
            names.push(name.to_string());
        }
    }
    names
}

/// Recursively drives `<path> --help`, collecting `(label, stdout)` for
/// every node clap's own `Commands:` section names — `help` (the
/// generated "print help of a subcommand" pseudo-entry) excluded, since it
/// has no help text of its own distinct from its parent's.
fn collect_help_surfaces(sandbox: &Sandbox, path: &[String], out: &mut Vec<(String, String)>) {
    let mut args: Vec<&str> = path.iter().map(String::as_str).collect();
    args.push("--help");
    let output = sandbox.cli(&args);
    assert_code(&output, 0);
    let text = stdout(&output);
    let label = if path.is_empty() {
        "multiverse --help".to_string()
    } else {
        format!("multiverse {} --help", path.join(" "))
    };
    for sub in parse_subcommands(&text) {
        if sub == "help" {
            continue;
        }
        let mut child = path.to_vec();
        child.push(sub);
        collect_help_surfaces(sandbox, &child, out);
    }
    out.push((label, text));
}

#[test]
fn help_text_for_every_command_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let mut surfaces = Vec::new();
    collect_help_surfaces(&sandbox, &[], &mut surfaces);
    assert!(
        surfaces.len() > 25,
        "expected the full top-level + nested command tree, got only {}: {surfaces:#?}",
        surfaces.len()
    );

    let mut failures = Vec::new();
    for (label, text) in &surfaces {
        // Exception 3 (this task's brief): `config migrate`'s own help may
        // document the old keys it rewrites — nowhere else is exempt.
        if label == "multiverse config migrate --help" {
            continue;
        }
        let hits = find_retired_hits(text);
        if !hits.is_empty() {
            failures.push(format!("{label}: {hits:#?}"));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

// ---------------------------------------------------------------------
// tasks / workers
// ---------------------------------------------------------------------

/// Regression coverage for the leak this guard found (card
/// `20261108-b-d51eab`, fixed by `20261109-b-2a3755`): `tasks`'s own `ROLE`
/// column used to print `TaskEvents::role` verbatim, never through its own
/// `display_role` (`commands::tasks.rs`, mirroring `commands::board_data`'s
/// translation of the same on-disk shape). A task dispatched under
/// `role=fixer`/`role=librarian`/`role=orchestrator` now prints its current
/// name instead of leaking the retired one.
#[test]
fn tasks_listing_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_legacy_dispatch_history(&sandbox);

    let output = sandbox.cli(&["tasks", "sandbox", "--no-reconcile", "--read-only"]);
    assert_code(&output, 0);
    assert_surface_clean("tasks sandbox --no-reconcile --read-only", &output);
}

/// Sibling regression coverage for `--reclaimable`'s own write-leg report
/// (card `20261109-b-252c53`): `reclaimable_and_report`'s two candidate
/// lines (`would close`/`stale bookkeeping`) used to print
/// `ReclaimCandidate::role` verbatim, same leak as the `ROLE` column
/// above, now routed through `display_role` too.
#[test]
fn tasks_reclaimable_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    seed_reclaimable_legacy_fixer_task(&sandbox);

    let output = sandbox.cli(&["tasks", "sandbox", "--no-reconcile", "--reclaimable"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert_surface_clean("tasks sandbox --no-reconcile --reclaimable", &output);
    assert!(
        text.contains("builder"),
        "the legacy fixer dispatch should render as builder in --reclaimable's \
         own candidate line, not leak the retired key: {text}"
    );
}

#[test]
fn workers_listing_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    dispatch_live_scout(&sandbox);

    let output = sandbox.cli(&["workers"]);
    assert_code(&output, 0);
    assert_surface_clean("workers", &output);
}

// ---------------------------------------------------------------------
// sweep --tick, project with no live Nexus (card `4b26a8`, fixed in
// 553600b): the base sandbox fixture configures no interactive account at
// all, which is exactly `tick_outcome`'s own "skipped (no Nexus)" guard.
// ---------------------------------------------------------------------

#[test]
fn sweep_tick_dry_run_skipped_no_nexus_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["sweep", "--tick", "--dry-run"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert_surface_clean("sweep --tick --dry-run", &output);
    assert!(text.contains("skipped (no Nexus)"), "{text}");
}

#[test]
fn sweep_tick_skipped_no_nexus_never_leaks_a_retired_role_name() {
    let sandbox = Sandbox::new();
    let output = sandbox.cli(&["sweep", "--tick"]);
    assert_code(&output, 0);
    let text = stdout(&output);
    assert_surface_clean("sweep --tick", &output);
    assert!(text.contains("skipped (no Nexus)"), "{text}");
}

// ---------------------------------------------------------------------
// Self-test: the scanner itself catches a planted leak and respects its
// declared exceptions. Not part of the guard's CLI coverage — a unit
// check on `find_retired_hits` so a future edit to the scanner cannot
// silently turn every assertion above into a no-op.
// ---------------------------------------------------------------------

#[test]
fn the_scanner_flags_a_bare_retired_word_and_allows_its_declared_exceptions() {
    assert_eq!(find_retired_hits("plain text, no issues here").len(), 0);
    assert_eq!(find_retired_hits("ORCHESTRATOR LAST PASS").len(), 1);
    assert_eq!(find_retired_hits("the reviewer declined").len(), 1);
    assert_eq!(find_retired_hits("explorers and explorer").len(), 1);
    assert_eq!(
        find_retired_hits("task 20250101-fixer-abcdef is done").len(),
        0,
        "a task id must not itself be flagged"
    );
    assert_eq!(
        find_retired_hits("ran orchestrator-pass just now").len(),
        0,
        "the marker file name must not itself be flagged"
    );
    assert_eq!(
        find_retired_hits("x20250101-fixer-abcdef").len(),
        1,
        "a word glued onto the date must still be flagged"
    );
}

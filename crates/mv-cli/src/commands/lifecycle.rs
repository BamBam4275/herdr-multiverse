//! `multiverse down`/`multiverse up`: fleet-wide pause and resume (card
//! `20261116-b-f0612d`, Lane B — see
//! `.multiverse/inputs/20261116-builder-6181a3/fleet-up-down-design.md` for
//! the full design this file is a bounded port of).
//!
//! Thin CLI orchestration over what already exists: `halt`
//! (`mv_state::halt`), the PASSDOWN sidecar contract Lane A added to
//! `mv_engine::nexus_seed` (`resolve_passdown`/`PASSDOWN_FILE`/
//! `WRAP_PROMPT`), and `start`'s own launch path (`commands::start::run`,
//! called in-process here — never a second `multiverse` process). This
//! file never touches docker/compose, never runs `sync-public`, never
//! commits or merges, never prunes a worktree, and never passes `--force`
//! to a workspace close. A Nexus does the project-specific wrap/bring-up
//! work — from the prompt `down` delivers, or from the seed `up`'s `start`
//! call re-renders once Lane A's resume hook sees a pending PASSDOWN.
//!
//! `down`: halt new dispatch with a `fleet down: ` reason, ask each open
//! project's Nexus to wrap (one prompt per project, delivered through the
//! same [`mv_engine::runner::deliver_prompt_to`] `start --reseed` already
//! uses), wait for `.multiverse/passdown.state` to read `wrapped` with a
//! non-empty `PASSDOWN.md` behind it, then close the workspace. A project
//! with no open workspace is skipped with a warning unless it is already
//! wrapped — `down` never starts an agent as a side effect of itself.
//!
//! `up`: `start` every targeted project in-process (so a pending PASSDOWN
//! is resumed exactly the way an ordinary `start`/`--reseed` already
//! would, once Lane A's seed hook lands), then wait on whichever of those
//! projects were pending or wrapped *before* this call until their
//! sidecar reads `up`, and clear the halt only when its reason still
//! carries the `fleet down: ` prefix this command writes — never a
//! `maintenance` halt or any other operator-set reason.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{fs, thread};

use clap::ArgMatches;
use mv_config::load::{Account, Config, Project};
use mv_engine::fleet_start::{close_workspace, FleetState};
use mv_engine::nexus_seed::{resolve_passdown, PASSDOWN_FILE, WRAP_PROMPT};
use mv_engine::runner::{deliver_prompt_to, Herdr, ProcessHerdr, Timing};
use mv_state::activity::HUB_PROJECT_NAME;
use mv_state::halt::{
    clear_halt_state, format_halt_line, read_halt_state, write_halt_state, HaltState,
};

use super::doctor::{project_path, resolve_config_path, state_dir_from_config, utc_timestamp};
use super::start::find_reseed_target;
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::{Failure, ERROR, NEEDS_CONFIG};

/// Must read exactly like `mv_engine::nexus_seed`'s own private
/// `PASSDOWN_STATE_FILE` — a cross-module file contract (the design's §2
/// sidecar table), not an internal of that module duplicated by accident.
/// Lane B's write scope does not include `nexus_seed.rs`, so this constant
/// cannot be imported and is restated here instead of guessed.
const PASSDOWN_STATE_FILE: &str = ".multiverse/passdown.state";

/// Design §6 Q1's recommended default: 30 minutes, `0` meaning forever.
const DEFAULT_WRAP_TIMEOUT_SECS: u64 = 1800;

/// The halt reason prefix `down` writes, and the only one `up` is allowed
/// to clear — a `halt on --reason maintenance` (or any other operator
/// reason) must survive `up` untouched (design decision 4 / Q3).
const HALT_REASON_PREFIX: &str = "fleet down: ";

/// Matches `stop.rs`'s own `LABEL_WIDTH`; duplicated rather than imported
/// since that one is private to its file.
const LABEL_WIDTH: usize = 21;

/// Same fix as `start.rs`/`stop.rs`'s own `print_config_load_error` (card
/// `20261102-b-4a5e3c`): each file in this crate keeps its own copy
/// rather than a shared helper, and this one does the same.
fn print_config_load_error(message: &str) {
    println!("config error: {message}");
    if is_retired_role_key_error(message) {
        println!("{MIGRATE_FIX_LINE}");
    } else {
        println!("Run `multiverse doctor` for a full readiness check.");
    }
}

fn load_config_or_report(args: &ArgMatches) -> Result<(PathBuf, Config), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        println!(
            "config error: no config file found at {}\nRun `multiverse doctor` for a full readiness check.",
            config_path.display()
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    let config = mv_config::load::load_config(&config_path).map_err(|error| {
        print_config_load_error(&error.to_string());
        Failure::Reported(NEEDS_CONFIG)
    })?;
    if config.projects.is_empty() {
        println!("No projects configured.");
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    Ok((config_path, config))
}

/// The positional `projects...`, defaulting to every configured project
/// sorted by name (including the hub) — `up`'s own "`start --all`'s
/// wanted list, without the name-the-projects refusal" default (design
/// decision 8), reused by `down` too.
fn select_projects<'a>(
    config: &'a Config,
    requested: &[String],
) -> Result<Vec<&'a Project>, Failure> {
    if requested.is_empty() {
        let mut projects: Vec<&Project> = config.projects.values().collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        return Ok(projects);
    }
    let unknown: Vec<&String> = requested
        .iter()
        .filter(|name| !config.projects.contains_key(*name))
        .collect();
    if !unknown.is_empty() {
        let mut configured: Vec<&str> = config.projects.keys().map(String::as_str).collect();
        configured.sort_unstable();
        let names: Vec<&str> = unknown.iter().map(|name| name.as_str()).collect();
        eprintln!(
            "no such project: {}; configured: {} (see `multiverse projects`)",
            names.join(", "),
            configured.join(", ")
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    Ok(requested
        .iter()
        .map(|name| &config.projects[name])
        .collect())
}

/// Projects first, hub last — design §4 "Home order": both the wrap
/// prompt and the post-wrap close want the hub seeing every project's
/// wrap before its own.
fn hub_last(mut projects: Vec<&Project>) -> Vec<&Project> {
    projects.sort_by_key(|project| (project.name == HUB_PROJECT_NAME, project.name.clone()));
    projects
}

/// `id`'s `--timeout SECONDS`, a finite non-negative number with `0`
/// meaning forever — deliberately a *different* arg id than the shared
/// `timeout()` helper's `"timeout"` (used by `dispatch`/`council`), whose
/// global `commands::validate` requires a strictly positive value. `down`/
/// `up` need `0` to mean "wait forever" (design §6 Q1), so each keeps its
/// own id (`down-timeout`/`up-timeout`) and validates it here instead.
fn parse_timeout(args: &ArgMatches, id: &str) -> Result<Duration, Failure> {
    match args.get_one::<String>(id) {
        None => Ok(Duration::from_secs(DEFAULT_WRAP_TIMEOUT_SECS)),
        Some(raw) => {
            let seconds: f64 = raw
                .parse()
                .ok()
                .filter(|n: &f64| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| {
                    Failure::Operation(
                        "--timeout must be a finite, non-negative number of seconds (0 means \
                         wait forever)"
                            .to_string(),
                    )
                })?;
            Ok(if seconds == 0.0 {
                Duration::MAX
            } else {
                Duration::from_secs_f64(seconds)
            })
        }
    }
}

fn describe_timeout(timeout: Duration) -> String {
    if timeout == Duration::MAX {
        "no timeout".to_string()
    } else {
        format!("{}s", timeout.as_secs())
    }
}

// --- the sidecar: read/write only, never parsed for its own prompt text
// (that vocabulary belongs to `nexus_seed.rs`; this file only checks the
// handful of values the design's §2 sidecar table gives a CLI-visible
// meaning). ---------------------------------------------------------------

fn sidecar_path(repository: &Path) -> PathBuf {
    repository.join(PASSDOWN_STATE_FILE)
}

fn sidecar_value(repository: &Path) -> Option<String> {
    fs::read_to_string(sidecar_path(repository))
        .ok()
        .map(|text| text.trim().to_string())
}

/// Refuses a pre-planted symlink at `path` before it is created, chmod'd,
/// or opened for write. `write_sidecar` calls this on both `.multiverse`
/// itself and the sidecar file: either can be swapped for a symlink by
/// anything that can plant content in the project tree ahead of a `down`
/// run, and `fs::set_permissions`/`OpenOptions::open` both follow a
/// symlink by default. The existing primitive for exactly this,
/// `guarded_path` (`crates/mv-state/src/worktrees.rs:447`), is
/// `pub(crate)` to `mv-state` and this lane's write scope excludes
/// `lib.rs`, so widening its visibility is out of reach here; this
/// restates the same `symlink_metadata` check rather than inventing a
/// different one. A `NotFound` is not a refusal — the sidecar's parent or
/// the sidecar itself is allowed to not exist yet.
fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing a symlink at {}", path.display()),
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Write `value` at 0600, creating `.multiverse/` at 0700 if needed — the
/// same durability posture `nexus_seed.rs`'s own seed writer uses (create
/// parent at 0700, write then `sync_all`; see that module's private
/// `write_rendered_seed`, which this restates rather than imports since
/// it is not `pub` and this lane's write scope does not reach it).
///
/// Refuses a symlinked `.multiverse` directory or a symlinked sidecar
/// file rather than writing through either (see [`refuse_symlink`]). The
/// lexical check right before `set_permissions` narrows, but cannot
/// close, the gap between that check and the chmod that follows it —
/// `O_NOFOLLOW` on the final file open closes the same gap for the write
/// itself, since the lexical check and the `open()` call are otherwise
/// two separate syscalls a swapped-in symlink could land between.
fn write_sidecar(repository: &Path, value: &str) -> io::Result<()> {
    let path = sidecar_path(repository);
    if let Some(parent) = path.parent() {
        refuse_symlink(parent)?;
        fs::create_dir_all(parent)?;
        refuse_symlink(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
    }
    refuse_symlink(&path)?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let mut file = options.open(&path)?;
    file.write_all(value.as_bytes())?;
    file.sync_all()
}

/// Idempotent per design §5: a repeat `down` on a project already
/// `pending` just rewrites the same value; one already `wrapped` is never
/// rewound back to `pending` — that would discard wrap progress a Nexus
/// already recorded.
fn ensure_pending_sidecar(repository: &Path) -> io::Result<()> {
    if sidecar_value(repository).as_deref() == Some("wrapped") {
        return Ok(());
    }
    write_sidecar(repository, "pending")
}

fn passdown_nonempty(repository: &Path) -> bool {
    fs::metadata(repository.join(PASSDOWN_FILE))
        .map(|metadata| metadata.len() > 0)
        .unwrap_or(false)
}

/// Design §2's acceptance rule: `wrapped` alone is not enough — the
/// document behind it must exist and be non-empty.
fn wrap_confirmed(repository: &Path) -> bool {
    sidecar_value(repository).as_deref() == Some("wrapped") && passdown_nonempty(repository)
}

/// `up`'s own "has this project's bring-up finished" check, built on
/// [`resolve_passdown`]: that function reads `None` exactly when the
/// sidecar is missing or reads `up` (and `Some` for `pending`/`wrapped`/
/// an unreadable-but-existing sidecar) — precisely the up/not-up split
/// `up` needs, already proven correct by Lane A's own tests.
fn bring_up_confirmed(repository: &Path) -> bool {
    resolve_passdown(repository).is_none()
}

/// Sleep for `poll`, clamped to whatever remains of `budget`. The same
/// shape as `mv_engine::fleet_start`'s own private `pause`, restated here
/// because that one is not `pub` and this lane's write scope does not
/// reach `fleet_start.rs`. Returns whether there was budget left to sleep
/// through; `budget == Duration::MAX` (the `--timeout 0` "forever" case)
/// never runs out, since `Instant::elapsed()` can never catch up to it.
fn pause(start: Instant, budget: Duration, poll: Duration) -> bool {
    let remaining = budget.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return false;
    }
    thread::sleep(poll.min(remaining));
    true
}

/// Poll `confirmed(repository)` every `poll`, up to `timeout`, checking
/// once more after the budget runs out so a flip that lands exactly at
/// the boundary is not missed.
fn wait_until(
    repository: &Path,
    confirmed: impl Fn(&Path) -> bool,
    timeout: Duration,
    poll: Duration,
) -> bool {
    let start = Instant::now();
    loop {
        if confirmed(repository) {
            return true;
        }
        if !pause(start, timeout, poll) {
            return confirmed(repository);
        }
    }
}

/// Design §4: "Poll 5s." Kept as a parameter on [`wait_until`] rather than
/// hard-coded there, so this module's own tests can drive the same
/// mechanism at a poll interval small enough to stay fast.
const WRAP_POLL: Duration = Duration::from_secs(5);

// --- talking to the live Nexus --------------------------------------------

/// `(seat, short)` for `naming::agent_name` — Python-style `naming_parts`
/// in `start.rs`, restated here since that one is a private helper this
/// lane's write scope does not reach.
fn naming_parts<'a>(project: &'a Project, seat: &'a Account) -> (&'a str, &'a str) {
    (seat.short.as_str(), project.short.as_deref().unwrap_or(""))
}

/// Deliver the fixed [`WRAP_PROMPT`] to `project`'s live Nexus, found by
/// identity the same two-step lookup `start --reseed` uses for its own
/// re-point target ([`find_reseed_target`]): the named roster entry first,
/// then the unnamed-pane fallback for a Nexus whose first seed never
/// reached `agent rename` even though `pane rename` already labelled the
/// pane (card `20261108-b-e3ca35`, shared with `down` for card
/// `20261116-b-457efa`). A blocked pane is reported, never typed into.
fn deliver_wrap_prompt<H: Herdr>(
    herdr: &mut H,
    fleet: &FleetState,
    project: &Project,
    path: &Path,
    seat_account: Option<&Account>,
    timing: &Timing,
) -> Result<(), String> {
    let account = seat_account.ok_or_else(|| "no interactive seat is configured".to_string())?;
    let (seat, short) = naming_parts(project, account);
    let (agent_name, live) = find_reseed_target(herdr, fleet, &project.name, path, seat, short)
        .ok_or_else(|| "its Nexus pane could not be found".to_string())?;
    if live.is_blocked() {
        return Err(format!(
            "{agent_name} is blocked and may never read the wrap prompt — clear it, then retry"
        ));
    }
    deliver_prompt_to(
        herdr,
        &agent_name,
        &live.pane_id,
        WRAP_PROMPT,
        account.kind.as_str(),
        live.state_change_seq,
        timing,
    )
    .map_err(|error| error.to_string())
}

/// Close `project`'s workspace once it is safe to: no lane holds
/// unrescued task output
/// ([`mv_state::tasks::lanes_holding_unrescued_output`], the same check
/// `stop` makes before destroying pane evidence), and *every* live agent
/// in the workspace — not only the Nexus — is idle or done, the exact
/// busy rule `stop` uses ([`super::stop::LiveAgent::is_safe_to_stop`]),
/// found the same way `stop` finds them
/// ([`super::stop::Fleet::agents_for`]): closing a workspace ends every
/// pane in it, so a worker lane still `working` when the Nexus writes
/// `wrapped` must keep the workspace open too, never typed into
/// mid-turn. `Ok(false)` means there was nothing left to close (the
/// workspace is already gone). `fleet` must be a snapshot taken *after*
/// the wrap wait, not the pre-prompt one: a project's workspace can close
/// or change id while this command is polling the sidecar, and a stale
/// snapshot would miss that here.
fn close_if_wrapped<H: Herdr>(
    herdr: &mut H,
    fleet: &FleetState,
    project: &Project,
    path: &Path,
) -> Result<bool, String> {
    let held = mv_state::tasks::lanes_holding_unrescued_output(path).map_err(|e| e.to_string())?;
    if !held.is_empty() {
        return Err(format!("holds unrescued output: {}", held.join(", ")));
    }
    let busy_fleet = super::stop::HerdrFleet {
        binary: super::stop::herdr_binary(),
    };
    let (_, agents) = super::stop::Fleet::agents_for(&busy_fleet, &project.name, path)?;
    if let Some(busy) = agents.iter().find(|agent| !agent.is_safe_to_stop()) {
        return Err(format!(
            "{} agent in {} is still working",
            busy.status, busy.pane_id
        ));
    }
    let Some(workspace_id) = fleet.workspace_id(&project.name, path) else {
        return Ok(false);
    };
    close_workspace(herdr, &workspace_id).map(|()| true)
}

// --- down ------------------------------------------------------------------

/// `multiverse down`'s command handler: halt new dispatch, ask each open
/// project's Nexus to wrap, wait for `passdown.state` to read `wrapped`,
/// then close the workspace — the sequence this module's own doc comment
/// describes in full. Reports `ERROR` if any project was skipped, timed
/// out, or could not close, after still acting on every other one.
pub(crate) fn run_down(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, config) = load_config_or_report(args)?;
    let state_dir = state_dir_from_config(&config_path);
    let requested: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let selected = hub_last(select_projects(&config, &requested)?);
    let reason = args
        .get_one::<String>("reason")
        .cloned()
        .unwrap_or_else(|| "operator requested".to_string());
    let timeout = parse_timeout(args, "down-timeout")?;
    let dry_run = args.get_flag("dry-run");
    let seat_account = config.accounts.values().find(|account| account.interactive);

    let mut herdr = ProcessHerdr;
    let fleet = FleetState::fetch(&mut herdr, true).map_err(|message| {
        eprintln!("{message}");
        Failure::Reported(ERROR)
    })?;

    let mut open_targets: Vec<(&Project, &Path)> = Vec::new();
    let mut problems: Vec<String> = Vec::new();
    for project in &selected {
        let Some(path) = project_path(project) else {
            println!(
                "  {:<width$}SKIPPED \u{2014} path is not configured",
                project.name,
                width = LABEL_WIDTH
            );
            problems.push(project.name.clone());
            continue;
        };
        if fleet.is_open(&project.name, path) {
            open_targets.push((project, path));
        } else if wrap_confirmed(path) {
            println!(
                "  {:<width$}already down",
                project.name,
                width = LABEL_WIDTH
            );
        } else {
            println!(
                "  {:<width$}SKIPPED \u{2014} no open Nexus; down never starts one",
                project.name,
                width = LABEL_WIDTH
            );
            problems.push(project.name.clone());
        }
    }

    if dry_run {
        println!("would halt: {HALT_REASON_PREFIX}{reason}");
        for (project, _) in &open_targets {
            println!(
                "  {:<width$}would wrap (wait up to {})",
                project.name,
                describe_timeout(timeout),
                width = LABEL_WIDTH
            );
        }
        return Ok(());
    }

    let halt_state = HaltState {
        halted: true,
        reason: Some(format!("{HALT_REASON_PREFIX}{reason}")),
        at: Some(utc_timestamp()),
    };
    write_halt_state(&state_dir, &halt_state)
        .map_err(|error| Failure::Operation(format!("could not write halt state: {error}")))?;
    println!("{}", format_halt_line(&halt_state));

    let timing = Timing::default();
    let mut waiting: Vec<(&Project, &Path)> = Vec::new();
    for (project, path) in &open_targets {
        let already_wrapped = sidecar_value(path).as_deref() == Some("wrapped");
        if let Err(error) = ensure_pending_sidecar(path) {
            println!(
                "  {:<width$}KEPT \u{2014} could not write passdown.state: {error}",
                project.name,
                width = LABEL_WIDTH
            );
            problems.push(project.name.clone());
            continue;
        }
        if already_wrapped {
            println!(
                "  {:<width$}already wrapped \u{2014} waiting to close",
                project.name,
                width = LABEL_WIDTH
            );
        } else {
            match deliver_wrap_prompt(&mut herdr, &fleet, project, path, seat_account, &timing) {
                Ok(()) => println!(
                    "  {:<width$}wrap prompt delivered",
                    project.name,
                    width = LABEL_WIDTH
                ),
                Err(message) => {
                    println!(
                        "  {:<width$}UNREACHABLE \u{2014} {message}",
                        project.name,
                        width = LABEL_WIDTH
                    );
                    problems.push(project.name.clone());
                    continue;
                }
            }
        }
        waiting.push((project, path));
    }

    let mut wrapped: Vec<(&Project, &Path)> = Vec::new();
    for (project, path) in &waiting {
        if wait_until(path, wrap_confirmed, timeout, WRAP_POLL) {
            wrapped.push((*project, *path));
        } else {
            println!(
                "  {:<width$}TIMEOUT \u{2014} PASSDOWN.md was not confirmed within {}",
                project.name,
                describe_timeout(timeout),
                width = LABEL_WIDTH
            );
            problems.push(project.name.clone());
        }
    }

    let fleet_after = FleetState::fetch(&mut herdr, true).map_err(|message| {
        eprintln!("{message}");
        Failure::Reported(ERROR)
    })?;
    let mut closed = 0usize;
    for (project, path) in &wrapped {
        match close_if_wrapped(&mut herdr, &fleet_after, project, path) {
            Ok(true) => {
                println!("  {:<width$}closed", project.name, width = LABEL_WIDTH);
                closed += 1;
            }
            Ok(false) => println!(
                "  {:<width$}already closed",
                project.name,
                width = LABEL_WIDTH
            ),
            Err(message) => {
                println!(
                    "  {:<width$}KEPT \u{2014} {message}",
                    project.name,
                    width = LABEL_WIDTH
                );
                problems.push(project.name.clone());
            }
        }
    }

    println!("\nclosed {closed} of {}", open_targets.len());
    if problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::Reported(ERROR))
    }
}

// --- up ----------------------------------------------------------------

/// Whether `state` is a halt `up` may clear: live, and its reason still
/// carries the exact `fleet down: ` prefix `down` writes. Design decision
/// 4 / Q3: a `maintenance` halt, or any other operator reason, is never
/// `up`'s to touch.
fn clears_our_halt(state: &HaltState) -> bool {
    state.halted
        && state
            .reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with(HALT_REASON_PREFIX))
}

/// `multiverse up`'s command handler: snapshot which selected projects
/// have a pending PASSDOWN, `start` every selected project in-process,
/// wait on only those pending/wrapped projects to confirm bring-up, then
/// clear the fleet halt if it is still the one `down` wrote
/// ([`clears_our_halt`]). Reports `ERROR` if any pending project timed out.
pub(crate) fn run_up(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, config) = load_config_or_report(args)?;
    let state_dir = state_dir_from_config(&config_path);
    let requested: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let selected = select_projects(&config, &requested)?;
    let timeout = parse_timeout(args, "up-timeout")?;
    let dry_run = args.get_flag("dry-run");

    // Snapshot *before* `start` runs — design §4: "wait only on projects
    // that were pending or wrapped at the start of this invocation."
    let mut pending: Vec<(String, PathBuf)> = Vec::new();
    for project in &selected {
        if let Some(path) = project_path(project) {
            if resolve_passdown(path).is_some() {
                pending.push((project.name.clone(), path.to_path_buf()));
            }
        }
    }

    if dry_run {
        let names: Vec<&str> = selected.iter().map(|p| p.name.as_str()).collect();
        println!("would start: {}", names.join(", "));
        for (name, _) in &pending {
            println!(
                "  {:<width$}would wait on its pending PASSDOWN",
                name,
                width = LABEL_WIDTH
            );
        }
        let halt = read_halt_state(&state_dir);
        if clears_our_halt(&halt) {
            println!(
                "would clear fleet halt ({})",
                halt.reason.as_deref().unwrap_or("")
            );
        }
        return Ok(());
    }

    // Reuse `start`'s own launch path in-process — never a second
    // `multiverse` process (design §6 Lane B / §1 step 2). Threading the
    // same `--config` through keeps this call resolving the identical
    // config file `up` itself just loaded.
    let mut argv: Vec<String> = vec!["multiverse".into(), "start".into()];
    if requested.is_empty() {
        argv.push("--all".into());
    } else {
        argv.extend(requested.iter().cloned());
    }
    if let Some(config_arg) = args.get_one::<String>("config") {
        argv.push("--config".into());
        argv.push(config_arg.clone());
    }
    let top = crate::args::command()
        .try_get_matches_from(&argv)
        .map_err(|error| {
            Failure::Operation(format!(
                "internal: could not build a start invocation: {error}"
            ))
        })?;
    let start_args = top
        .subcommand_matches("start")
        .expect("argv above always names the start subcommand");
    super::start::run(start_args)?;

    let mut problems: Vec<String> = Vec::new();
    for (name, path) in &pending {
        if wait_until(path, bring_up_confirmed, timeout, WRAP_POLL) {
            println!("  {:<width$}up", name, width = LABEL_WIDTH);
        } else {
            println!(
                "  {:<width$}TIMEOUT \u{2014} bring-up was not confirmed within {}",
                name,
                describe_timeout(timeout),
                width = LABEL_WIDTH
            );
            problems.push(name.clone());
        }
    }
    for project in &selected {
        if !pending.iter().any(|(name, _)| name == &project.name) {
            println!(
                "  {:<width$}no pending PASSDOWN",
                project.name,
                width = LABEL_WIDTH
            );
        }
    }

    if !problems.is_empty() {
        return Err(Failure::Reported(ERROR));
    }
    let halt = read_halt_state(&state_dir);
    if clears_our_halt(&halt) {
        clear_halt_state(&state_dir)
            .map_err(|error| Failure::Operation(format!("could not clear halt state: {error}")))?;
        println!("fleet halt cleared.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::placeholder::MaybePlaceholder;
    use std::collections::BTreeMap as Map;

    fn project(name: &str, path: &str) -> Project {
        Project {
            name: name.to_string(),
            path: MaybePlaceholder::parse(path),
            chains: Map::new(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        }
    }

    #[test]
    fn select_projects_defaults_to_every_configured_project_sorted() {
        let mut projects = Map::new();
        projects.insert("b".to_string(), project("b", "/b"));
        projects.insert("a".to_string(), project("a", "/a"));
        let config = Config {
            projects,
            ..test_config()
        };
        let selected = select_projects(&config, &[]).unwrap();
        assert_eq!(
            selected.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn select_projects_refuses_an_unknown_name() {
        let mut projects = Map::new();
        projects.insert("a".to_string(), project("a", "/a"));
        let config = Config {
            projects,
            ..test_config()
        };
        assert!(select_projects(&config, &["missing".to_string()]).is_err());
    }

    #[test]
    fn hub_last_sorts_the_hub_after_every_other_project() {
        let hub = project(HUB_PROJECT_NAME, "/hub");
        let a = project("a", "/a");
        let z = project("z", "/z");
        let ordered = hub_last(vec![&hub, &z, &a]);
        assert_eq!(
            ordered.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["a", "z", HUB_PROJECT_NAME]
        );
    }

    #[test]
    fn parse_timeout_zero_means_forever() {
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "down", "--timeout", "0"])
            .unwrap();
        let down = matches.subcommand_matches("down").unwrap();
        assert_eq!(parse_timeout(down, "down-timeout").unwrap(), Duration::MAX);
    }

    #[test]
    fn parse_timeout_defaults_to_thirty_minutes() {
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "down"])
            .unwrap();
        let down = matches.subcommand_matches("down").unwrap();
        assert_eq!(
            parse_timeout(down, "down-timeout").unwrap(),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn parse_timeout_rejects_a_negative_value() {
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "up", "--timeout", "-1"])
            .unwrap();
        let up = matches.subcommand_matches("up").unwrap();
        assert!(parse_timeout(up, "up-timeout").is_err());
    }

    #[test]
    fn clears_our_halt_only_matches_the_fleet_down_prefix() {
        assert!(clears_our_halt(&HaltState {
            halted: true,
            reason: Some("fleet down: operator requested".to_string()),
            at: None,
        }));
        assert!(!clears_our_halt(&HaltState {
            halted: true,
            reason: Some("maintenance".to_string()),
            at: None,
        }));
        assert!(!clears_our_halt(&HaltState {
            halted: false,
            reason: Some("fleet down: operator requested".to_string()),
            at: None,
        }));
    }

    #[test]
    fn sidecar_round_trips_and_ensure_pending_never_rewinds_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(sidecar_value(dir.path()), None);
        ensure_pending_sidecar(dir.path()).unwrap();
        assert_eq!(sidecar_value(dir.path()).as_deref(), Some("pending"));
        write_sidecar(dir.path(), "wrapped").unwrap();
        assert_eq!(sidecar_value(dir.path()).as_deref(), Some("wrapped"));
        ensure_pending_sidecar(dir.path()).unwrap();
        assert_eq!(
            sidecar_value(dir.path()).as_deref(),
            Some("wrapped"),
            "a repeat down must never rewind a wrapped sidecar back to pending"
        );
    }

    #[test]
    fn write_sidecar_refuses_a_symlinked_sidecar_file_and_leaves_the_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "do not touch").unwrap();
        fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, sidecar_path(dir.path())).unwrap();

        let error = write_sidecar(dir.path(), "pending").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(fs::read_to_string(&outside).unwrap(), "do not touch");
    }

    #[test]
    fn write_sidecar_refuses_a_symlinked_multiverse_directory_and_leaves_the_target_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("sentinel.txt"), "do not touch").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, dir.path().join(".multiverse")).unwrap();

        let error = write_sidecar(dir.path(), "pending").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            !outside.join("passdown.state").exists(),
            "the write must never land inside the symlink target"
        );
        assert_eq!(
            fs::read_to_string(outside.join("sentinel.txt")).unwrap(),
            "do not touch"
        );
    }

    #[test]
    fn wrap_confirmed_requires_both_wrapped_and_a_non_empty_passdown() {
        let dir = tempfile::tempdir().unwrap();
        write_sidecar(dir.path(), "wrapped").unwrap();
        assert!(!wrap_confirmed(dir.path()), "no PASSDOWN.md at all yet");
        fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        fs::write(dir.path().join(PASSDOWN_FILE), "").unwrap();
        assert!(!wrap_confirmed(dir.path()), "PASSDOWN.md is empty");
        fs::write(dir.path().join(PASSDOWN_FILE), "## STATE\n").unwrap();
        assert!(wrap_confirmed(dir.path()));
    }

    /// Proves the polling mechanism itself — design §4's "Poll 5s", at a
    /// poll interval small enough to stay fast: a background writer flips
    /// the sidecar to `wrapped` with a non-empty `PASSDOWN.md` partway
    /// through the wait budget, and `wait_until` notices on a later poll,
    /// not only on its very first check.
    #[test]
    fn wait_until_notices_a_later_write_after_several_polls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        assert!(!wrap_confirmed(&path));
        let writer_path = path.clone();
        let writer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            fs::create_dir_all(writer_path.join(".multiverse")).unwrap();
            fs::write(writer_path.join(PASSDOWN_FILE), "## STATE\n").unwrap();
            write_sidecar(&writer_path, "wrapped").unwrap();
        });
        let confirmed = wait_until(
            &path,
            wrap_confirmed,
            Duration::from_secs(2),
            Duration::from_millis(5),
        );
        writer.join().unwrap();
        assert!(confirmed, "wait_until must notice the later write");
    }

    #[test]
    fn wait_until_times_out_when_nothing_ever_changes() {
        let dir = tempfile::tempdir().unwrap();
        let confirmed = wait_until(
            dir.path(),
            wrap_confirmed,
            Duration::from_millis(20),
            Duration::from_millis(5),
        );
        assert!(!confirmed);
    }

    #[test]
    fn bring_up_confirmed_is_true_once_the_sidecar_is_not_pending_or_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        fs::write(dir.path().join(PASSDOWN_FILE), "## STATE\n").unwrap();
        write_sidecar(dir.path(), "pending").unwrap();
        assert!(!bring_up_confirmed(dir.path()));
        write_sidecar(dir.path(), "up").unwrap();
        assert!(bring_up_confirmed(dir.path()));
    }

    fn test_config() -> Config {
        mv_config::load::load_config_str("", Path::new("config.toml")).unwrap()
    }
}

//! `worktrees <path> [--prune|--rescue] [--force] [--yes] [--dry-run]
//! [--deep] [--restore TASK_ID]`: inspect, prune, rescue, or restore lanes.
//! `--deep` requires `--prune`. During prune, `--yes` or `--force` confirms
//! disposal of ignored files; all other removal guards still apply.
//! After pruning, `--deep` sweeps old finished-task leftovers and project
//! scratch, and named scratch caches in finished, clean surviving lanes, and
//! `.multiverse/worktree-state/`'s own quarantine copies (card
//! `20261106-b-3d2774`): one already byte-identical to what rescue copied
//! into the project's own `.multiverse/` is dropped regardless of age, and
//! the rest once past a 14-day retention window. Like the rest of `--deep`,
//! never part of a plain `--prune`.
//! Audit records, logs and small text survive the task sweep. A plain
//! `--prune` (no `--deep`) also frees a *kept* lane's own lane-root
//! `target/` once that lane's occupants are all finished and nothing is
//! still live in it, reporting the bytes freed. A lane that is actually
//! removed or archived, or kept under that same condition, also frees its
//! own per-worktree cargo build-dir under the shared build cache
//! `.cargo/config.toml` configures (card `20261115-b-47f043`), found
//! read-only with no `cargo`/`rustup` subprocess, reported the same way —
//! never a build-dir outside that configured root, or the main checkout's
//! own. `--prune` also reclaims every *other* shared-build hash dir under
//! that same root whose own evidence names only workspace roots gone from
//! disk (card `20261115-b-673528`) — independent of which lanes this
//! project's own state still lists — reporting the same bytes-freed line.
//! `--dry-run`
//! previews cleanup paths and byte totals without deleting them; with
//! `--prune` its removable set matches what `--yes` would remove, and with
//! `--rescue` it names what each lane's rescue would copy without copying
//! anything.
//!
//! Card `20261115-b-9815fe`: `--prune --reap` is an opt-in reaper, report by
//! default. A kept lane whose only refusal is "lane has a live process cwd"
//! is still only ever kept and named, never signaled, unless `--reap` is
//! also given — and never when `--force` was used, since `--force` may have
//! bypassed a live pane the process check can no longer trust. `--reap
//! --dry-run` previews with "would reap" and kills nothing; a real `--reap`
//! signals, waits, and re-checks pid identity before escalating, then
//! prunes once more so a now-clean lane is actually removed/archived in the
//! same run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_state::worktrees::{
    BranchOutcome, HerdrLaneObserver, MergeError, PruneOutcome, QuarantineOutcome,
    QuarantineReason, Worktrees,
};

use super::doctor::resolve_config_path;
use crate::exit::Failure;

fn fail(e: impl std::fmt::Display) -> Failure {
    Failure::Operation(e.to_string())
}

fn outcome_task(outcome: &PruneOutcome) -> &str {
    match outcome {
        PruneOutcome::Kept { task, .. }
        | PruneOutcome::WouldRemove { task, .. }
        | PruneOutcome::WouldArchive { task, .. }
        | PruneOutcome::Removed { task, .. }
        | PruneOutcome::Archived { task, .. } => task,
    }
}

fn action_line(outcome: &PruneOutcome) -> String {
    match outcome {
        PruneOutcome::Kept { reason, .. } => format!("kept — {reason}"),
        PruneOutcome::WouldRemove { .. } => "would remove".to_string(),
        PruneOutcome::WouldArchive { .. } => "would archive".to_string(),
        // Matches Python's own real-run wording (`cli.py`'s
        // `print("    removed (held nothing)", file=out)`).
        PruneOutcome::Removed { .. } => "removed (held nothing)".to_string(),
        PruneOutcome::Archived { tip, .. } => format!("archived at {tip}"),
    }
}

/// `lane_path`'s own lingering host processes, under `--reap` on
/// `worktrees --prune` (card `20261115-b-9815fe`). Pulled out of `prune`
/// as a pure function of one lane's own process snapshot so it is directly
/// unit-testable against a real spawned child, without capturing stdout or
/// driving the full CLI. Returns the notes to print under that lane's own
/// "kept" line, and whether anything was actually signaled (the caller's
/// cue to re-prune once more).
///
/// `reap` false previews "rerun with --reap"; `reap` true and `dry_run`
/// true previews "would reap" and signals nothing — both are the same
/// "report, never kill" shape `--reap --dry-run`/plain `--prune` already
/// promise. `reap` true and `dry_run` false is the one path that calls
/// [`mv_engine::dockerreport::reap`]. A process [`mv_engine::dockerreport::protected`]
/// names (a shell, Herdr itself, pid 1, or this process's own ancestor) is
/// never reaped regardless of `reap`/`dry_run`.
fn reap_lane_notes(
    lane_path: &Path,
    reap: bool,
    dry_run: bool,
    ancestors: &[u32],
) -> (Vec<String>, bool) {
    let Some(snapshot) = mv_engine::dockerreport::process_cwds() else {
        return (
            vec!["    warning: process-cwd probe unknown".to_string()],
            false,
        );
    };
    let mut notes = Vec::new();
    let mut any_reaped = false;
    for process in mv_engine::dockerreport::processes_under(lane_path, &snapshot) {
        let label = mv_engine::dockerreport::process_label(&process);
        if mv_engine::dockerreport::protected(&process, ancestors) {
            notes.push(format!(
                "    lane process {label} (protected, never reaped)"
            ));
        } else if !reap {
            notes.push(format!(
                "    lane process {label} (rerun with --reap to end it)"
            ));
        } else if dry_run {
            notes.push(format!("    would reap {label}"));
        } else {
            notes.push(format!("    reaping {label}"));
            mv_engine::dockerreport::reap(&process, mv_engine::dockerreport::DEFAULT_REAP_GRACE);
            any_reaped = true;
        }
    }
    (notes, any_reaped)
}

/// `worktrees`'s entry point: dispatches to `--restore`, `--merge`,
/// `--prune`, `--rescue`, or the plain inspect listing, in that order of
/// precedence. See the module doc for what each mode does.
pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    if args.get_flag("deep") && !args.get_flag("prune") {
        return Err(fail("--deep is only meaningful together with --prune"));
    }
    let path = args.get_one::<String>("path").expect("required");
    let worktrees = Worktrees::new(path).map_err(fail)?;

    if let Some(task) = args.get_one::<String>("restore") {
        let tip = worktrees.restore(task).map_err(fail)?;
        println!("restored multiverse/{task} at {tip}");
        return Ok(());
    }

    if let Some(task) = args.get_one::<String>("merge") {
        return merge(&worktrees, args, task);
    }

    if args.get_flag("prune") {
        return prune(&worktrees, args);
    }

    if args.get_flag("rescue") {
        return rescue(&worktrees, args.get_flag("dry-run"));
    }

    inspect(&worktrees, path)
}

fn prune(worktrees: &Worktrees, args: &ArgMatches) -> Result<(), Failure> {
    let force = args.get_flag("force");
    let dry_run = args.get_flag("dry-run");
    let reap = args.get_flag("reap");
    let confirm_ignored = force || args.get_flag("yes");

    for ignored in worktrees.ignored_entries().map_err(fail)? {
        println!("  {}: ignored: not a lane", ignored.display());
    }

    // Snapshotted before `prune()` can remove anything: a removed or
    // archived lane's own directory is gone by the time its outcome comes
    // back, and this label is purely descriptive, not safety-critical.
    let found = worktrees.lanes().map_err(fail)?;
    let summaries: HashMap<String, String> = found
        .iter()
        .map(|lane| {
            (
                lane.task.clone(),
                worktrees.lane_summary(lane, confirm_ignored),
            )
        })
        .collect();
    let lane_paths: HashMap<String, PathBuf> = found
        .iter()
        .map(|lane| (lane.task.clone(), lane.path.clone()))
        .collect();

    let mut observer = HerdrLaneObserver::new(worktrees.project());
    let mut outcomes = worktrees
        .prune_with_ignored(&mut observer, force, dry_run, confirm_ignored)
        .map_err(fail)?;

    // Card `20261115-b-9815fe`: the lane-exit reaper. Report-only by
    // default — the extra lines below are purely descriptive until
    // `--reap` is also given — and never when `--force` was used: a
    // live-pane keep-reason ordinarily implies `observer.check` already
    // passed, but `--force` can have bypassed exactly that check, so the
    // process this lane still shows may belong to a pane this run never
    // actually confirmed gone.
    let mut reap_notes: HashMap<String, Vec<String>> = HashMap::new();
    let mut reaped_tasks: Vec<String> = Vec::new();
    if !force {
        let ancestors = mv_engine::dockerreport::own_ancestor_pids();
        for outcome in &outcomes {
            let PruneOutcome::Kept { task, reason, .. } = outcome else {
                continue;
            };
            if !reason.contains("live process cwd") {
                continue;
            }
            let Some(lane_path) = lane_paths.get(task) else {
                continue;
            };
            let (notes, any_reaped) = reap_lane_notes(lane_path, reap, dry_run, &ancestors);
            reap_notes.insert(task.clone(), notes);
            if any_reaped {
                reaped_tasks.push(task.clone());
            }
        }
    }
    if !reaped_tasks.is_empty() {
        // The follow-through: re-prune now that the reaped lane(s) no
        // longer have a live process cwd, so a lane this run just reaped
        // is actually removed/archived in the same invocation rather than
        // needing a second `--prune` run. Lanes this pass already finished
        // (`Removed`/`Archived`/…) are gone from `lanes()` by now and are
        // simply absent from this second outcome set; only a reaped
        // task's own entry is spliced back into `outcomes` below.
        let mut follow_up_observer = HerdrLaneObserver::new(worktrees.project());
        if let Ok(follow_up) =
            worktrees.prune_with_ignored(&mut follow_up_observer, force, dry_run, confirm_ignored)
        {
            for outcome in follow_up {
                let task = outcome_task(&outcome).to_string();
                if reaped_tasks.contains(&task) {
                    if let Some(slot) = outcomes
                        .iter_mut()
                        .find(|existing| outcome_task(existing) == task)
                    {
                        *slot = outcome;
                    }
                }
            }
        }
    }

    if outcomes.is_empty() {
        // Matches Python's `cmd_worktrees`: not a return — the orphan
        // sweep below still runs even when there are no lanes at all —
        // and the same wording `inspect`'s own empty case prints, not
        // the unrelated "no lanes to prune" this used to say.
        println!("no worktrees under {}", project_name(worktrees));
    }

    let (mut removed, mut reclaimed, mut archived, mut kept) = (0u32, 0u32, 0u32, 0u32);
    for outcome in &outcomes {
        let task = outcome_task(outcome);
        if let Some(summary) = summaries.get(task) {
            println!("  {task}  {summary}");
        }
        // Matches Python's own `rescue(lane)` report: one line per artifact
        // actually copied, printed right after the lane's summary line and
        // before whatever `--prune` decided to do with the lane itself.
        if let PruneOutcome::Removed { rescued, .. } | PruneOutcome::Archived { rescued, .. } =
            outcome
        {
            for path in rescued {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let parent = path.parent().unwrap_or(path).display();
                println!("    rescued {name} -> {parent}");
            }
        }
        println!("    {}", action_line(outcome));
        for note in reap_notes.get(task).into_iter().flatten() {
            println!("{note}");
        }
        if let PruneOutcome::Kept {
            target_freed: Some(bytes),
            ..
        } = outcome
        {
            let verb = if dry_run { "would free" } else { "freed" };
            println!("    {verb} target/ ({bytes} bytes)");
        }
        if let PruneOutcome::Kept {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::WouldRemove {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::WouldArchive {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::Removed {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::Archived {
            build_dir_freed: Some(bytes),
            ..
        } = outcome
        {
            let verb = if dry_run { "would free" } else { "freed" };
            println!("    {verb} build-dir ({bytes} bytes)");
        }
        match outcome {
            PruneOutcome::Kept { .. } => kept += 1,
            PruneOutcome::WouldRemove { branch_refusal, .. }
            | PruneOutcome::Removed { branch_refusal, .. } => {
                removed += 1;
                if let Some(reason) = branch_refusal {
                    println!("    kept branch — {reason}");
                } else {
                    reclaimed += 1;
                    println!("    reclaimed branch multiverse/{task}");
                }
            }
            PruneOutcome::WouldArchive { .. } | PruneOutcome::Archived { .. } => archived += 1,
        }
    }
    let orphans = worktrees.prune_orphans(dry_run).map_err(fail)?;
    let orphaned = orphans.len();
    for outcome in &orphans {
        match outcome {
            BranchOutcome::Kept { task, reason } => {
                kept += 1;
                println!("  kept multiverse/{task} — {reason}");
            }
            BranchOutcome::Reclaimed { task } => {
                reclaimed += 1;
                println!("  reclaimed multiverse/{task}");
            }
            BranchOutcome::Archived { task, .. } => {
                archived += 1;
                println!("  archived multiverse/{task} -> refs/multiverse/archive/{task}");
            }
        }
    }
    // Card `20261115-b-673528`: every shared-build hash dir whose own
    // evidence names only vanished workspace roots, independent of which
    // lanes this project's own `lanes()` still lists — reported the same
    // way a lane's own `build_dir_freed` already is.
    let orphaned_build_dirs = worktrees.prune_orphaned_build_dirs(dry_run).map_err(fail)?;
    for build_dir in &orphaned_build_dirs {
        println!("  cargo shared-build {}", build_dir.path.display());
        let verb = if dry_run { "would free" } else { "freed" };
        println!("    {verb} build-dir ({} bytes)", build_dir.bytes);
    }
    let lanes = outcomes.len();

    println!();
    println!("removed {removed} of {lanes}");
    // Python's real (non-dry-run) path never prints this combined
    // breakdown, or the blank line ahead of it — only `removed N of M`
    // and, when there were orphans, `reclaimed N of M`. The blank line
    // belongs to `--dry-run`'s own summary string (`cli.py`'s leading
    // `f"\ndry run: ..."`), not a separator printed unconditionally; an
    // unconditional one here left a trailing blank line real-run output
    // never has. Found via `conformance/diff_fuzz.py` seed 50001.
    if dry_run {
        let breakdown = format!(
            "{removed} removed, {reclaimed} reclaimed, {archived} archived, {kept} kept \
             ({lanes} lane(s), {orphaned} orphaned branch(es))"
        );
        println!();
        println!("dry run: {breakdown} — nothing was changed");
    }
    if args.get_flag("deep") {
        worktrees
            .deep_prune(&found, dry_run, &mut std::io::stdout().lock())
            .map_err(fail)?;
        print_quarantine_outcomes(worktrees, dry_run)?;
    }
    Ok(())
}

/// `QuarantineReason`'s own report text, printed next to each
/// `.multiverse/worktree-state/` copy `--deep` removes or would remove.
fn quarantine_reason_text(reason: QuarantineReason) -> &'static str {
    match reason {
        QuarantineReason::Redundant => "already rescued; nothing here isn't saved elsewhere",
        QuarantineReason::RetentionExpired => "past the quarantine retention window",
    }
}

/// `--deep`'s own report for `Worktrees::prune_quarantine` (card
/// `20261106-b-3d2774`), in the same verbose "deep ..." per-path style
/// `deep_prune` already uses for its own accumulated housekeeping — never
/// part of a plain `--prune`, matching that convention, and so never
/// exercised by `conformance/diff_fuzz.py`'s own `worktrees` fuzz action,
/// which only ever drives plain `--prune`.
fn print_quarantine_outcomes(worktrees: &Worktrees, dry_run: bool) -> Result<(), Failure> {
    let outcomes = worktrees.prune_quarantine(dry_run).map_err(fail)?;
    let mut bytes = 0u64;
    let mut removed = 0u64;
    for outcome in &outcomes {
        match outcome {
            QuarantineOutcome::Removed {
                name,
                reason,
                bytes: size,
            } => {
                println!(
                    "  deep removed .multiverse/worktree-state/{name} ({size} bytes) — {}",
                    quarantine_reason_text(*reason)
                );
                bytes += size;
                removed += 1;
            }
            QuarantineOutcome::WouldRemove {
                name,
                reason,
                bytes: size,
            } => {
                println!(
                    "  deep would remove .multiverse/worktree-state/{name} ({size} bytes) — {}",
                    quarantine_reason_text(*reason)
                );
                bytes += size;
                removed += 1;
            }
            QuarantineOutcome::Kept { name, bytes } => {
                println!(
                    "  deep kept .multiverse/worktree-state/{name} ({bytes} bytes) — \
                     within quarantine retention"
                );
            }
        }
    }
    let action = if dry_run { "would free" } else { "freed" };
    let suffix = if dry_run {
        " — nothing was changed"
    } else {
        ""
    };
    println!("\nquarantine cleanup: {action} {bytes} bytes in {removed} path(s){suffix}");
    Ok(())
}

/// `--merge <task>`: the one enforced choke point card `20261114-b-069836`
/// (option (a)) asks for — scans `task`'s own commits for secrets before
/// landing them with `git merge --no-ff --no-edit`, so a lane cannot reach
/// this checkout's `HEAD` the way a raw `git merge` always could. Reads
/// `[secrets_scan]` from the resolved fleet config; every other `worktrees`
/// mode needs no config at all, so this is the one branch that loads one.
fn merge(worktrees: &Worktrees, args: &ArgMatches, task: &str) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;

    match worktrees.merge(task, &config.secrets_scan) {
        Ok(result) => {
            for warning in &result.warnings {
                println!("{warning}");
            }
            println!("merged multiverse/{task} at {}", result.sha);
            Ok(())
        }
        Err(MergeError::ScannerMissing(message)) => Err(Failure::Config(message)),
        Err(MergeError::Refused(message)) => Err(Failure::Operation(message)),
    }
}

/// `--rescue` takes no task argument, so it sweeps every discovered lane
/// — the bulk-maintenance reading, not a single-lane operation (the only
/// one consistent with `worktrees`'s own argument list, which has no
/// `--task` alongside `--rescue`). `dry_run` previews what each lane's
/// rescue would copy without copying anything.
fn rescue(worktrees: &Worktrees, dry_run: bool) -> Result<(), Failure> {
    let lanes = worktrees.lanes().map_err(fail)?;
    if lanes.is_empty() {
        println!("no lanes to rescue");
        return Ok(());
    }
    let verb = if dry_run { "would rescue" } else { "rescued" };
    let mut failures = 0;
    for lane in &lanes {
        match worktrees.rescue(&lane.task, dry_run) {
            Ok(rescued) => {
                for path in &rescued {
                    let name = path.file_name().unwrap_or_default().to_string_lossy();
                    let parent = path.parent().unwrap_or(path).display();
                    println!("  {verb} {name} -> {parent}");
                }
                println!("{verb} {}", lane.task);
            }
            Err(e) => {
                failures += 1;
                println!("rescue failed for {}: {e}", lane.task);
            }
        }
    }
    if dry_run {
        println!();
        println!("dry run: nothing was changed");
    }
    if failures == 0 {
        Ok(())
    } else {
        Err(Failure::Operation(format!(
            "{failures} of {} lane rescues failed",
            lanes.len()
        )))
    }
}

fn inspect(worktrees: &Worktrees, path: &str) -> Result<(), Failure> {
    let lanes = worktrees.lanes().map_err(fail)?;
    let mut printed_any = false;
    for lane in &lanes {
        println!("{}\t{}\t{}", lane.task, lane.branch, lane.path.display());
        printed_any = true;
    }
    for (task, sha) in worktrees.archived().map_err(fail)? {
        let short = &sha[..sha.len().min(12)];
        println!("archived {task} {short} (restore: multiverse worktrees {path} --restore {task})");
        printed_any = true;
    }
    if !printed_any {
        println!("no worktrees under {}", project_name(worktrees));
    }
    Ok(())
}

/// The last path component of the (canonicalized) project root, matching
/// Python's `project.name` (a `pathlib.Path.name`) for the "no worktrees
/// under {project.name}" message (card `20261102-b-dd1507`). Falls back
/// to the full path for the degenerate case `Path.name` also returns
/// empty for (root, or a path ending in `..`).
fn project_name(worktrees: &Worktrees) -> String {
    let project = worktrees.project();
    match project.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => project.display().to_string(),
    }
}

/// Process-global env lock shared by every `mv-cli` unit test that must
/// still mutate process env (card `20261102-b-f781e6`, extended so
/// `HERDR_BIN_PATH` / `MULTIVERSE_PMSET_BIN` / `MULTIVERSE_CODEX_BIN`
/// writers in this binary serialize together). Integration tests under
/// `tests/` pass the fake path per spawned `Command::env` and never touch
/// process env. One crate-wide lock: the previous per-module mutexes raced
/// under `cargo test`'s default thread pool, because
/// `accounts`/`dispatch`/`review_policy`/`worktrees`/`sweep` unit tests
/// share one binary. These tests call library code (`mv_herdr::run::run`,
/// `HerdrLaneObserver`, `live_open_count`, `pmset_binary`, `codex_binary`)
/// that reads the process environment, so `Command::env` on a test-owned
/// spawn cannot reach the child.
#[cfg(test)]
pub(crate) mod herdr_env {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the crate-wide `HERDR_BIN_PATH` env lock directly, for a
    /// test that needs to hold it without going through [`HerdrEnvGuard`].
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Holds the crate-wide env lock for its lifetime and restores
    /// `HERDR_BIN_PATH` to whatever it was when dropped. See the module
    /// doc above for why this is one lock shared by every test module.
    pub(crate) struct HerdrEnvGuard {
        previous: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl HerdrEnvGuard {
        /// Points `HERDR_BIN_PATH` at `path` for the guard's lifetime.
        pub(crate) fn set(path: &Path) -> Self {
            let held = lock();
            let previous = std::env::var("HERDR_BIN_PATH").ok();
            std::env::set_var("HERDR_BIN_PATH", path);
            Self {
                previous,
                _lock: held,
            }
        }

        /// A guaranteed-nonexistent binary: `mv_herdr::run::run` fails
        /// with a missing-binary error on the first spawn attempt, never
        /// reaching any real executable.
        pub(crate) fn unreachable() -> Self {
            Self::set(Path::new("/nonexistent/mv-cli-test-herdr-binary"))
        }
    }

    impl Drop for HerdrEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("HERDR_BIN_PATH", value),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::herdr_env::HerdrEnvGuard;
    use super::*;
    use mv_state::worktrees::{Lane, LaneObserver};
    use std::process::Command;

    /// Nanoseconds alone are not enough: this crate's test binary runs
    /// every `#[test]` fn as a thread of one process, and under heavy
    /// parallel load two threads have landed on the same tick before —
    /// observed as a flake in `herdr_lane_observer_keeps_an_unsettled_lane_with_a_live_pane_even_with_force`
    /// (card `20261102-b-9235c6`), where a path collision let one test's
    /// fake-herdr script or project dir get clobbered by another's
    /// concurrently-running write. An atomic counter alongside the
    /// timestamp — the same pairing `mv_state::worktrees`'s private
    /// `nonce()` already uses — makes every call unique regardless of
    /// clock resolution.
    fn unique_suffix() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            "{nanos}-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    /// A throwaway `#!/bin/sh` script replying to `agent list` with one
    /// agent occupying `pane` at `agent_status`. The literal values are
    /// test-controlled, never interpolated from untrusted input, so
    /// building the JSON by hand (rather than via `serde_json`) is safe.
    fn fake_herdr_agent_list(pane: &str, agent_status: &str) -> TempFile {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mv-cli-fake-herdr-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s' '{{\"result\": {{\"agents\": [{{\"pane_id\": \"{pane}\", \"agent_status\": \"{agent_status}\", \"name\": \"fixture\"}}]}}}}'\n"
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        TempFile(path)
    }

    /// A throwaway file under `TMPDIR`, removed on drop including on
    /// panic (card `20261115-b-c8065f`: the plain `PathBuf` this and
    /// `fake_scanner` returned only cleaned up on each test's own
    /// trailing `remove_file` call). `Deref`s to `Path` so every existing
    /// call site keeps reading `&script`/`script.to_string_lossy()`
    /// unchanged.
    struct TempFile(std::path::PathBuf);
    impl std::ops::Deref for TempFile {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempFile {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A throwaway project directory under `TMPDIR`, removed on drop
    /// including on panic (card `20261115-b-c8065f`: the plain `PathBuf`
    /// the `herdr_lane_observer_*` tests built here only cleaned up on
    /// their own trailing `remove_dir_all` call). `Deref`s to `Path` so
    /// every existing call site keeps reading `&project`/
    /// `project.join(...)` unchanged.
    struct TempDir(std::path::PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TempRepo(std::path::PathBuf);

    impl TempRepo {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mv-cli-worktrees-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let git = |args: &[&str]| {
                let status = Command::new("git")
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap();
                assert!(status.success(), "git {args:?} failed");
            };
            git(&["init", "-q", "-b", "main"]);
            git(&["config", "user.email", "test@example.com"]);
            git(&["config", "user.name", "test"]);
            std::fs::write(dir.join("README.md"), "hi\n").unwrap();
            git(&["add", "README.md"]);
            git(&["commit", "-q", "-m", "init"]);
            Self(dir)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inspect_lists_a_created_lane_and_archived_is_empty() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(inspect(&worktrees, repo.0.to_str().unwrap()).is_ok());
    }

    /// Card 20261102-b-dd1507: a project with zero lanes (none created,
    /// none archived, no orphan branches) used to fall straight through
    /// `inspect`'s three loops to `Ok(())` with no output at all. Python's
    /// `cmd_worktrees` always prints a "no worktrees under {project.name}"
    /// line for this case — nothing here captures stdout to check the
    /// exact text, but this at least pins that the empty path is reached
    /// and doesn't panic. `--deep` is validated before reaching this listing.
    #[test]
    fn inspect_on_a_project_with_no_lanes_does_not_panic() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        assert!(inspect(&worktrees, repo.0.to_str().unwrap()).is_ok());
    }

    #[test]
    fn project_name_is_the_last_path_component() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let expected = repo
            .0
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(project_name(&worktrees), expected);
    }

    #[test]
    fn prune_keeps_a_lane_with_no_recorded_pane_on_missing_status_alone() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let _herdr = HerdrEnvGuard::unreachable();
        let mut observer = HerdrLaneObserver::new(&repo.0);
        // No routing.log at all: HerdrLaneObserver finds nothing to
        // confirm live and steps aside; `terminal_statuses` is what
        // actually keeps this lane.
        let outcomes = worktrees.prune(&mut observer, false, true).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(&outcomes[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal"))
        );
    }

    #[test]
    fn rescue_sweeps_every_lane() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(rescue(&worktrees, false).is_ok());
    }

    /// Card `20261110-b-d3ba4f` defect 1: `worktrees <proj> --rescue
    /// --dry-run` used to perform the real rescue, because `run`'s dispatch
    /// never read the `--dry-run` flag for the `--rescue` branch at all.
    /// Drives the full CLI parse-and-dispatch path (`crate::args::command()`
    /// through `run`) rather than calling the internal `rescue` fn directly,
    /// so a regression in that wiring — not just in `Worktrees::rescue`
    /// itself — would be caught here.
    #[test]
    fn cli_rescue_dry_run_does_not_copy_task_artifacts() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let lane = worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/20261101-fixer-abcdef");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(task_dir.join("result.md"), "findings\n").unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "worktrees", path, "--rescue", "--dry-run"])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert!(run(sub).is_ok());

        assert!(
            !repo
                .0
                .join(".multiverse/tasks/20261101-fixer-abcdef/result.md")
                .exists(),
            "--rescue --dry-run must not actually copy anything into the project"
        );
    }

    /// A throwaway `#!/bin/sh` scanner script — these `--merge` CLI tests
    /// never touch the network or a real installed gitleaks/trufflehog.
    fn fake_scanner(body: &str) -> TempFile {
        let path = std::env::temp_dir().join(format!(
            "mv-cli-fake-scanner-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        TempFile(path)
    }

    /// A `--merge`-ready lane: created, recorded `done` with a non-empty
    /// `result.md`, and one committed change for the scanner to look at.
    fn done_lane(worktrees: &Worktrees, task: &str) -> Lane {
        let lane = worktrees.create(task, "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks").join(task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        std::fs::write(lane.path.join("change.txt"), "lane work\n").unwrap();
        for args in [["add", "change.txt"].as_slice(), &["commit", "-m", "work"]] {
            assert!(Command::new("git")
                .current_dir(&lane.path)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        lane
    }

    /// Proof item: `--merge` refuses on a planted-secret finding (exit
    /// code 1), names the evidence file in its own error, and leaves
    /// `HEAD` exactly where it was.
    #[test]
    fn cli_merge_refuses_on_a_gitleaks_finding_and_leaves_main_unchanged() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);
        let before = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();

        let script = fake_scanner("echo 'AKIA-REDACTED'\nexit 1");
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", script.display()),
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        assert!(error.to_string().contains("evidence"), "{error}");
        assert!(
            repo.0
                .join(format!(".multiverse/evidence/{task}-secrets.txt"))
                .is_file(),
            "evidence file must be written on a finding"
        );

        let after = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(before, after, "a refused merge must leave HEAD unchanged");
    }

    /// Proof item: a clean, done lane merges and `--merge` prints the
    /// merge sha, matching what `HEAD` now points at.
    #[test]
    fn cli_merge_merges_a_clean_lane_and_prints_the_merge_sha() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);

        let script = fake_scanner("exit 0");
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", script.display()),
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert!(run(sub).is_ok());

        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let message = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["log", "-1", "--format=%B"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(message.trim(), format!("Merge branch 'multiverse/{task}'"));
        let _ = sha;
    }

    /// Proof item: a missing required scanner exits 3 (needs config) with
    /// an install hint, and merges nothing.
    #[test]
    fn cli_merge_exits_needs_config_when_gitleaks_is_missing_and_required() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);

        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            "[secrets_scan]\ngitleaks = \"/nonexistent/mv-cli-test-gitleaks\"\n",
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::NEEDS_CONFIG);
        assert!(error.to_string().contains("install"), "{error}");
    }

    /// Proof item 1: a kept lane names the lingering pid without `--reap`,
    /// and kills nothing.
    #[test]
    fn reap_lane_notes_without_reap_names_the_pid_and_signals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let (notes, any_reaped) = reap_lane_notes(dir.path(), false, false, &[]);
        assert!(!any_reaped);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&format!("pid {}", child.id())),
            "{notes:?}"
        );
        assert!(notes[0].contains("rerun with --reap"), "{notes:?}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "no --reap must never signal the process"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Proof item 3: `--reap --dry-run` previews "would reap" and kills
    /// nothing.
    #[test]
    fn reap_lane_notes_dry_run_previews_and_signals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let (notes, any_reaped) = reap_lane_notes(dir.path(), true, true, &[]);
        assert!(!any_reaped);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("would reap"), "{notes:?}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "--reap --dry-run must never signal the process"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Proof item 2: `--reap` without `--dry-run` actually ends the
    /// process.
    #[test]
    fn reap_lane_notes_reaps_a_real_spawned_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let (notes, any_reaped) = reap_lane_notes(dir.path(), true, false, &[]);
        assert!(any_reaped);
        assert!(notes.iter().any(|n| n.contains("reaping")), "{notes:?}");
        let status = child.wait().unwrap();
        assert!(!status.success(), "{status:?}");
    }

    /// Proof item 4: a sibling directory's process is never matched, even
    /// when it shares the lane path as a string prefix.
    #[test]
    fn reap_lane_notes_never_matches_a_sibling_prefix_directory() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("lane");
        let sibling = dir.path().join("lane-other");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(&sibling)
            .spawn()
            .unwrap();
        let (notes, any_reaped) = reap_lane_notes(&lane, true, false, &[]);
        assert!(!any_reaped);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "a sibling directory's process must never be touched"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    // Decision 6's by-name denylist (shell/herdr/pid 1) is exercised
    // directly, without a live process, by `dockerreport`'s own
    // `protected_refuses_a_shell_or_herdr_by_name_even_with_a_matching_cwd`
    // (`crates/mv-engine/src/dockerreport.rs`) — `sh -c "sleep N"` execs
    // straight into `sleep` on both bash and dash (the tail-call
    // optimization), so there is no reliable way to spawn a real,
    // short-lived process whose `ps` comm is still a shell; forcing one to
    // stay resident (backgrounding and `wait`-ing) risks leaking the
    // grandchild if this test is ever interrupted before cleanup runs. The
    // ancestor-pid branch below exercises the same `protected()` call from
    // this file's own wiring, with a real spawned process.

    /// Decision 6: a candidate pid in the caller's own ancestor list is
    /// never reaped, even though its cwd and comm would otherwise qualify.
    #[test]
    fn reap_lane_notes_never_reaps_a_pid_in_the_ancestor_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new("sleep")
            .arg("30")
            .current_dir(dir.path())
            .spawn()
            .unwrap();
        let (notes, any_reaped) = reap_lane_notes(dir.path(), true, false, &[child.id()]);
        assert!(!any_reaped);
        assert!(notes.iter().any(|n| n.contains("protected")), "{notes:?}");
        assert!(
            child.try_wait().unwrap().is_none(),
            "a pid the caller named as an ancestor must never be reaped"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    /// Full `--prune --reap --yes` CLI path: a done, pane-clear lane with a
    /// spawned `sleep` is reported as kept on a plain `--prune`, then
    /// actually pruned (branch handled, lane directory gone) once `--reap`
    /// ends that process in the same invocation — proof items 1 and 2
    /// together, through the real CLI dispatch rather than the pure helper
    /// above.
    #[test]
    fn cli_prune_reap_ends_the_process_and_the_lane_is_no_longer_kept_for_it() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        let lane = done_lane(&worktrees, task);
        // `prune_with_ignored`'s own archiving check runs ahead of
        // `clean`'s own `has_process_cwd` refusal — an unmerged branch
        // would keep this lane for "not merged into HEAD" instead, never
        // reaching the live-process check this test is about. Merging the
        // lane's branch first makes the live process cwd the only
        // remaining reason to keep it.
        assert!(Command::new("git")
            .current_dir(&repo.0)
            .args(["merge", "--ff-only", &format!("multiverse/{task}")])
            .status()
            .unwrap()
            .success());
        let _herdr = HerdrEnvGuard::unreachable();

        let mut child = Command::new("sleep")
            .arg("60")
            .current_dir(&lane.path)
            .spawn()
            .unwrap();

        let path = repo.0.to_str().unwrap();
        let plain = crate::args::command()
            .try_get_matches_from(["multiverse", "worktrees", path, "--prune"])
            .unwrap();
        let (_, sub) = plain.subcommand().unwrap();
        assert!(run(sub).is_ok());
        assert!(
            lane.path.exists(),
            "a plain --prune must keep a lane with a live process cwd"
        );
        assert!(
            child.try_wait().unwrap().is_none(),
            "a plain --prune must never signal the process"
        );

        let reaping = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--prune",
                "--reap",
                "--yes",
            ])
            .unwrap();
        let (_, sub) = reaping.subcommand().unwrap();
        assert!(run(sub).is_ok());

        let status = child.wait().unwrap();
        assert!(!status.success(), "{status:?}");
        assert!(
            !lane.path.exists(),
            "--prune --reap must remove the lane once its process is gone"
        );
    }

    #[test]
    fn herdr_lane_observer_keeps_an_unsettled_lane_with_a_live_pane_even_with_force() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        std::fs::create_dir_all(project.join(".multiverse")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        let script = fake_herdr_agent_list("w1:p1", "working");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        for force in [false, true] {
            let error = observer.check(&lane, force).unwrap_err().to_string();
            assert!(
                error.contains("has no terminal status and still has a live agent pane"),
                "{error}"
            );
        }
    }

    #[test]
    fn herdr_lane_observer_keeps_a_settled_lane_whose_pane_is_still_live() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        let task_dir = project.join(".multiverse/tasks").join(&lane.task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        let script = fake_herdr_agent_list("w1:p1", "idle");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        let error = observer.check(&lane, false).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "{} still has a live agent pane (idle); rerun with --force to remove it anyway.",
                lane.task
            )
        );
    }

    #[test]
    fn herdr_lane_observer_allows_a_settled_lane_once_its_pane_is_gone() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        let task_dir = project.join(".multiverse/tasks").join(&lane.task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        // Herdr's own list names a different pane entirely: this lane's
        // pane has closed.
        let script = fake_herdr_agent_list("w2:p9", "idle");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        assert!(observer.check(&lane, false).is_ok());
    }
}

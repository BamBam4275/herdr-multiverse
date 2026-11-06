//! `worktrees <path> [--prune|--rescue] [--force] [--yes] [--dry-run]
//! [--deep] [--restore TASK_ID]`: inspect, prune, rescue, or restore lanes.
//! `--deep` requires `--prune`. During prune, `--yes` or `--force` confirms
//! disposal of ignored files; all other removal guards still apply.
//! After pruning, `--deep` sweeps old finished-task leftovers and project
//! scratch, and named scratch caches in finished, clean surviving lanes.
//! Audit records, logs and small text survive the task sweep. `--dry-run`
//! previews cleanup paths and byte totals without deleting them.

use std::collections::HashMap;

use clap::ArgMatches;
use mv_state::worktrees::{BranchOutcome, HerdrLaneObserver, PruneOutcome, Worktrees};

use crate::exit::Failure;

fn fail(e: impl std::fmt::Display) -> Failure {
    Failure::Operation(e.to_string())
}

fn outcome_task(outcome: &PruneOutcome) -> &str {
    match outcome {
        PruneOutcome::Kept { task, .. }
        | PruneOutcome::WouldRemove { task, .. }
        | PruneOutcome::WouldArchive { task }
        | PruneOutcome::Removed { task, .. }
        | PruneOutcome::Archived { task, .. } => task,
    }
}

fn action_line(outcome: &PruneOutcome) -> String {
    match outcome {
        PruneOutcome::Kept { reason, .. } => format!("kept — {reason}"),
        PruneOutcome::WouldRemove { .. } => "would remove".to_string(),
        PruneOutcome::WouldArchive { .. } => "would archive".to_string(),
        PruneOutcome::Removed { .. } => "removed".to_string(),
        PruneOutcome::Archived { tip, .. } => format!("archived at {tip}"),
    }
}

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

    if args.get_flag("prune") {
        return prune(&worktrees, args);
    }

    if args.get_flag("rescue") {
        return rescue(&worktrees);
    }

    inspect(&worktrees, path)
}

fn prune(worktrees: &Worktrees, args: &ArgMatches) -> Result<(), Failure> {
    let force = args.get_flag("force");
    let dry_run = args.get_flag("dry-run");

    for ignored in worktrees.ignored_entries().map_err(fail)? {
        println!("  {}: ignored: not a lane", ignored.display());
    }

    // Snapshotted before `prune()` can remove anything: a removed or
    // archived lane's own directory is gone by the time its outcome comes
    // back, and this label is purely descriptive, not safety-critical.
    let found = worktrees.lanes().map_err(fail)?;
    let summaries: HashMap<String, String> = found
        .iter()
        .map(|lane| (lane.task.clone(), worktrees.lane_summary(lane)))
        .collect();

    let mut observer = HerdrLaneObserver::new(worktrees.project());
    let outcomes = worktrees
        .prune_with_ignored(&mut observer, force, dry_run, force || args.get_flag("yes"))
        .map_err(fail)?;
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
        println!("    {}", action_line(outcome));
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
    }
    Ok(())
}

/// `--rescue` takes no task argument, so it sweeps every discovered lane
/// — the bulk-maintenance reading, not a single-lane operation (the only
/// one consistent with `worktrees`'s own argument list, which has no
/// `--task` alongside `--rescue`).
fn rescue(worktrees: &Worktrees) -> Result<(), Failure> {
    let lanes = worktrees.lanes().map_err(fail)?;
    if lanes.is_empty() {
        println!("no lanes to rescue");
        return Ok(());
    }
    let mut failures = 0;
    for lane in &lanes {
        match worktrees.rescue(&lane.task) {
            Ok(()) => println!("rescued {}", lane.task),
            Err(e) => {
                failures += 1;
                println!("rescue failed for {}: {e}", lane.task);
            }
        }
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

    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) struct HerdrEnvGuard {
        previous: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl HerdrEnvGuard {
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
    fn fake_herdr_agent_list(pane: &str, agent_status: &str) -> std::path::PathBuf {
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
        path
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
        assert!(rescue(&worktrees).is_ok());
    }

    #[test]
    fn herdr_lane_observer_keeps_an_unsettled_lane_with_a_live_pane_even_with_force() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
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
        let mut observer = HerdrLaneObserver::with_herdr_binary(&project, script.to_string_lossy());
        for force in [false, true] {
            let error = observer.check(&lane, force).unwrap_err().to_string();
            assert!(
                error.contains("has no terminal status and still has a live agent pane"),
                "{error}"
            );
        }

        std::fs::remove_file(&script).ok();
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn herdr_lane_observer_keeps_a_settled_lane_whose_pane_is_still_live() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
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
        let mut observer = HerdrLaneObserver::with_herdr_binary(&project, script.to_string_lossy());
        let error = observer.check(&lane, false).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "{} still has a live agent pane (idle); rerun with --force to remove it anyway.",
                lane.task
            )
        );

        std::fs::remove_file(&script).ok();
        std::fs::remove_dir_all(&project).ok();
    }

    #[test]
    fn herdr_lane_observer_allows_a_settled_lane_once_its_pane_is_gone() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
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
        let mut observer = HerdrLaneObserver::with_herdr_binary(&project, script.to_string_lossy());
        assert!(observer.check(&lane, false).is_ok());

        std::fs::remove_file(&script).ok();
        std::fs::remove_dir_all(&project).ok();
    }
}

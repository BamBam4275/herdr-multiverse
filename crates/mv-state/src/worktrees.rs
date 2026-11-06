//! Git lane lifecycle. Callers supply fresh occupant observations and record
//! outcomes through [`LaneObserver`]; this module never infers completion from
//! an idle pane. Removal retains quarantined state, including open-file writes.
//! A removed lane's branch is reclaimed with `-d` only after its outcome is
//! recorded and its tip is reachable from HEAD; forced unrecorded removal
//! retains the branch.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use mv_core::atomic::fsync_dir;
use mv_core::lock::{self, LockGuard};

#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("{0}")]
    Refused(String),
    #[error("git {args:?}: {message}")]
    Git { args: Vec<String>, message: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

type Result<T> = std::result::Result<T, WorktreeError>;

// Preserve this module's refusal diagnostic while sharing task-id validation.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    mv_core::id::TaskId::new(id)
        .map(|_| ())
        .map_err(|_| WorktreeError::Refused(format!("invalid task id {id:?}")))
}

pub fn branch_name(task: &str) -> Result<String> {
    validate_id(task)?;
    Ok(format!("multiverse/{task}"))
}

/// Mark `task_id`'s branch as reviewed, at `commit` if given, or at its
/// *current* tip otherwise — Python's `record_reviewed`
/// (`worktrees.py:1046-1072`). `commit` is an explicit `--commits sha...`
/// override's last entry (card `20261004-b-87b2ce`): a Critic handed a
/// commit list ending before the branch tip reviewed only that far, and
/// stamping the tip anyway would let the *next* round's "since last
/// review" window skip whatever landed after the list but before the next
/// real dispatch. Silently does nothing if `task_id` names no branch here
/// and no `commit` was given — the same "no evidence, no write" posture
/// this module's base-commit resolution already takes.
pub fn record_reviewed(project_root: &Path, task_id: &str, commit: Option<&str>) {
    let Ok(reviewed_ref) = reviewed_ref(task_id) else {
        return;
    };
    match commit {
        Some(sha) => {
            let _ = git(project_root, &["update-ref", &reviewed_ref, sha], None);
        }
        None => {
            let Ok(branch) = branch_name(task_id) else {
                return;
            };
            let Ok(resolved) = git(
                project_root,
                &["rev-parse", "--verify", "--quiet", &branch],
                None,
            ) else {
                return;
            };
            let resolved = resolved.trim();
            if resolved.is_empty() {
                return;
            }
            let _ = git(project_root, &["update-ref", &reviewed_ref, resolved], None);
        }
    }
}

/// `task_id`'s branch tip the last time it was reviewed, or `None` — a
/// first round, or a lane never reviewed under diff-scope policy. Python's
/// `last_reviewed_commit` (`worktrees.py:1077-1086`): the "since last
/// review" half of diff-scope re-review; [`record_reviewed`] is the other.
pub fn last_reviewed_commit(project_root: &Path, task_id: &str) -> Option<String> {
    let reviewed_ref = reviewed_ref(task_id).ok()?;
    let output = git(
        project_root,
        &["rev-parse", "--verify", "--quiet", &reviewed_ref],
        None,
    )
    .ok()?;
    let sha = output.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

fn reviewed_ref(task_id: &str) -> Result<String> {
    validate_id(task_id)?;
    Ok(format!("refs/multiverse/reviewed/{task_id}"))
}

/// Construct a new lane id. Historical role names remain valid only on reads.
pub fn lane_id(date: &str, role: &str, digest: &str) -> Result<String> {
    if date.len() != 8
        || !date.bytes().all(|b| b.is_ascii_digit())
        || !matches!(
            role,
            "nexus"
                | "scout"
                | "builder"
                | "archivist"
                | "critic"
                | "seer"
                | "council"
                | "artisan"
                | "watcher"
        )
        || digest.len() != 6
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(WorktreeError::Refused("invalid new lane identity".into()));
    }
    Ok(format!("{date}-{role}-{digest}"))
}

/// Whether a RECORDED task's lane is clean enough for `multiverse sweep`
/// to reclaim its still-open pane (`reclaimable_panes` condition 3):
/// dispatched straight to `main`, with no worktree of its own ever
/// created, is trivially clean — a role that never gets a lane cannot
/// leave one dirty. A task with an actual owned lane needs this module's
/// own prune-style ancestry/dirty check ([`Worktrees::prune`]); this
/// helper does not attempt that and reports "not clean" rather than
/// guess at it, the same fail-closed posture the rest of a sweep takes
/// for anything it cannot yet verify.
pub fn dispatched_lane_is_clean(project_root: &Path, task: &str, branch: &str) -> bool {
    branch == "main"
        && !project_root
            .join(".multiverse/worktrees")
            .join(task)
            .is_dir()
}

/// Every file `task`'s own lane has touched since its cut point —
/// `review-policy --lane`'s risk classification needs exactly this file
/// list to match against `[review_policy.review_risk_paths]` globs
/// (finding 5, critic-sweep-board.md). Mirrors the live-branch half of
/// Python's `worktrees.lane_diff`: the base is the lane's own recorded
/// cut point (`refs/multiverse/base/<task>`, the same ref
/// `tasks::branch_disposition` already reads) when one was recorded,
/// else a plain merge-base with `HEAD`.
///
/// Scoped to a branch still on disk. A merged or reclaimed lane's tip
/// recovery (Python's `_merged_lane_tip`: finding the merge commit or
/// `Task: <id>` trailer that subsumed it) is a materially larger, mostly
/// unrelated port and is refused here with a clear message rather than
/// silently guessed at — see this task's result.md.
pub fn lane_touched_files(project_root: &Path, task: &str) -> Result<Vec<String>> {
    let branch = branch_name(task)?;
    let branch_ref = format!("refs/heads/{branch}");
    let branch_exists = git_output(
        project_root,
        &["rev-parse", "--verify", "--quiet", &branch_ref],
        None,
    )
    .is_ok_and(|output| output.status.success());
    if !branch_exists {
        return Err(WorktreeError::Refused(format!(
            "no branch {branch:?} exists in this repository — a merged or \
             reclaimed lane's diff cannot be recovered yet"
        )));
    }

    let base_ref = format!("refs/multiverse/base/{task}");
    let base_recorded = git_output(
        project_root,
        &["rev-parse", "--verify", "--quiet", &base_ref],
        None,
    )
    .is_ok_and(|output| output.status.success());
    let base = if base_recorded {
        base_ref
    } else {
        git(project_root, &["merge-base", "HEAD", &branch], None)?
            .trim()
            .to_string()
    };

    let range = format!("{base}..{branch}");
    let diff = git(project_root, &["diff", "--name-only", &range], None)?;
    Ok(diff
        .lines()
        .map(str::to_owned)
        .filter(|line| !line.is_empty())
        .collect())
}

/// This lane's own recorded cut point (`refs/multiverse/base/<task>`), or
/// a `merge-base HEAD branch` fallback for a lane whose cut point predates
/// `_record_base` — the same two sources [`lane_touched_files`] reads, in
/// the same order, for [`has_commits`]/[`merge_readiness`]'s narrower need
/// (one commit, not a diff).
fn lane_base(project_root: &Path, task: &str, branch: &str) -> Result<String> {
    let base_ref = format!("refs/multiverse/base/{task}");
    let base_recorded = git_output(
        project_root,
        &["rev-parse", "--verify", "--quiet", &base_ref],
        None,
    )
    .is_ok_and(|output| output.status.success());
    if base_recorded {
        Ok(
            git(project_root, &["rev-parse", "--verify", &base_ref], None)?
                .trim()
                .to_string(),
        )
    } else {
        Ok(git(project_root, &["merge-base", "HEAD", branch], None)?
            .trim()
            .to_string())
    }
}

/// Porcelain status of `lane_root`'s own working tree, trimmed — tells a
/// human what an agent left behind in its lane (`--wait`'s own handback
/// report). Python's `uncommitted_changes` (`worktrees.py:2073-2109`)
/// raises rather than letting "could not tell" read as "clean"; this port
/// reaches for the same posture the rest of this module already uses for
/// an unreadable tree — `None`, never a false all-clear — rather than add
/// this module's first fallible-bool return.
pub fn uncommitted_changes(lane_root: &Path) -> Option<String> {
    if !lane_root.is_dir() {
        return None;
    }
    let output = git_output(lane_root, &["status", "--porcelain"], None).ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_string(),
    )
}

/// Whether the agent committed anything on this lane — Python's
/// `has_commits` (`worktrees.py:2005-2019`): measured from the lane's own
/// recorded cut point, never from the shared checkout's current `HEAD`,
/// so a human moving `HEAD` after dispatch cannot make a lane with real
/// work read as empty.
pub fn has_commits(project_root: &Path, task: &str) -> Result<bool> {
    let branch = branch_name(task)?;
    let base = lane_base(project_root, task, &branch)?;
    let tip = git(project_root, &["rev-parse", "--verify", &branch], None)?
        .trim()
        .to_string();
    Ok(tip != base)
}

/// Advisory warning for [`handback`]: whether `main` moved since this
/// lane's own cut point and touched any file the lane itself also
/// changed — Python's `merge_readiness` (`worktrees.py:2152-2182`). `None`
/// when the comparison cannot be made (no recorded base, no `main`) or
/// there is simply nothing to warn about; never acted on, only shown.
pub fn merge_readiness(project_root: &Path, task: &str) -> Option<String> {
    let branch = branch_name(task).ok()?;
    let base = lane_base(project_root, task, &branch).ok()?;
    let main_tip = git(
        project_root,
        &["rev-parse", "--verify", "--quiet", "main"],
        None,
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())?;
    if main_tip == base {
        return None;
    }
    let main_paths = git(
        project_root,
        &["diff", "--name-only", &format!("{base}..{main_tip}")],
        None,
    )
    .ok()?;
    let lane_paths = git(
        project_root,
        &["diff", "--name-only", &format!("{base}..{branch}")],
        None,
    )
    .ok()?;
    let main_set: HashSet<&str> = main_paths.lines().collect();
    let mut overlap: Vec<&str> = lane_paths
        .lines()
        .filter(|path| main_set.contains(path))
        .collect();
    overlap.sort_unstable();
    overlap.dedup();
    if overlap.is_empty() {
        return None;
    }
    Some(format!(
        "main moved since this lane's branch point and both changed: {}",
        overlap.join(", ")
    ))
}

/// Whether this lane's own commits (since its recorded cut point) touch
/// `.multiverse/` at all, aside from the one file a project genuinely
/// commits there (`roster.toml`) — Python's `multiverse_commit_refusal`
/// (`worktrees.py:3403-3447`). Card `20261015-b-fa41b8`: a lane that
/// `git add -f`s its own `result.md`/`status` defeats the `.gitignore`
/// exclusion those files rely on the moment it merges — [`handback`]'s own
/// "how to land it" must not recommend a merge that repeats this.
pub fn multiverse_commit_refusal(project_root: &Path, task: &str) -> Option<String> {
    let touched = lane_touched_files(project_root, task).ok()?;
    let mut offending: Vec<&str> = touched
        .iter()
        .map(String::as_str)
        .filter(|path| *path == ".multiverse" || path.starts_with(".multiverse/"))
        .filter(|path| *path != ".multiverse/roster.toml")
        .collect();
    offending.sort_unstable();
    if offending.is_empty() {
        return None;
    }
    Some(format!(
        "refused: lane commits .multiverse/ files: {}",
        offending.join(", ")
    ))
}

/// The commands a human needs to review, land, or discard this lane —
/// Python's `handback` (`worktrees.py:2022-2070`). §8 leaves merging to a
/// human deliberately, but "deliberately not automated" is not the same as
/// "undocumented": a writer's output nobody knows how to land is, in
/// practice, output nobody uses. `lane_root` is the lane's own worktree
/// checkout; `project_root` is the shared checkout `branch` merges into.
pub fn handback(project_root: &Path, task: &str, branch: &str, lane_root: &Path) -> Vec<String> {
    let dirty = uncommitted_changes(lane_root).is_some_and(|status| !status.is_empty());
    let committed = has_commits(project_root, task).unwrap_or(false);
    if dirty && !committed {
        return vec![
            "The agent left changes it never committed, so they exist in exactly".into(),
            "one place. Look before anything else removes them:".into(),
            format!("    git -C {} status", lane_root.display()),
            format!("    git -C {} diff", lane_root.display()),
            "To keep them:".into(),
            format!(
                "    git -C {} add -A && git -C {} commit -m 'agent work'",
                lane_root.display(),
                lane_root.display()
            ),
        ];
    }
    let mut lines = vec![
        "To review what the agent did:".into(),
        format!("    git -C {} diff HEAD...{branch}", project_root.display()),
    ];
    match multiverse_commit_refusal(project_root, task) {
        Some(refusal) => {
            lines.push("To land it: refused: .multiverse commit".into());
            lines.push(format!("    {refusal}"));
        }
        None => {
            lines.push("To land it:".into());
            lines.push(format!(
                "    git -C {} merge {branch}",
                project_root.display()
            ));
        }
    }
    if let Some(warning) = merge_readiness(project_root, task) {
        lines.push(format!("Warning: {warning}"));
    }
    lines.push("To discard it entirely:".into());
    lines.push(format!(
        "    git -C {} worktree remove {} && git -C {} branch -D {branch}",
        project_root.display(),
        lane_root.display(),
        project_root.display()
    ));
    if dirty {
        lines.push("Note: the lane also has uncommitted changes on top of those commits —".into());
        lines.push(format!("    git -C {} status", lane_root.display()));
    }
    lines
}

/// Walk without following symlinks, including the final component. Missing
/// components are allowed only when requested, for an ensuing create operation.
pub(crate) fn guarded_path(root: &Path, relative: &Path, missing: bool) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() {
        return Err(WorktreeError::Refused("empty relative path".into()));
    }
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(WorktreeError::Refused(
                "path must stay below its root".into(),
            ));
        };
        path.push(name);
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(WorktreeError::Refused(format!(
                    "symlink at {}",
                    path.display()
                )))
            }
            Ok(_) => (),
            Err(e) if missing && e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}

pub(crate) fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

/// Marker written into a fresh `.multiverse/.gitignore`: everything under
/// the directory is Multiverse's own scaffolding except `roster.toml`, the
/// one file a human is meant to commit (a project's own role chains).
const STATE_DIR_GITIGNORE: &str = "\
# Multiverse's own artifacts. Not the project's.
*

# ...except the one file a human writes: this project's own role
# chains (OPEN-QUESTIONS.md Q18). A routing decision that cannot be
# committed is one that vanishes with the checkout.
!roster.toml
";

/// Creates `root`'s own `.multiverse/` and makes git ignore it, before
/// anything writes there — a lane whose scaffolding shows in its own `git
/// status` can never be judged disposable, and an agent asked to commit its
/// work would reasonably commit ours too. Appends the `roster.toml`
/// exemption to a marker that predates it rather than overwriting, since a
/// human may have added rules of their own.
pub(crate) fn ensure_state_dir(root: &Path) -> io::Result<()> {
    let state = root.join(".multiverse");
    private_dir(&state)?;
    let marker = state.join(".gitignore");
    match fs::read_to_string(&marker) {
        Ok(body) if !body.contains("roster.toml") => {
            let mut body = body;
            if !body.ends_with('\n') {
                body.push('\n');
            }
            body.push_str(
                "\n# ...except the one file a human writes: this project's own role\n\
                 # chains (OPEN-QUESTIONS.md Q18). Added to an existing marker,\n\
                 # because a routing decision that cannot be committed vanishes\n\
                 # with the checkout.\n\
                 !roster.toml\n",
            );
            fs::write(&marker, body)
        }
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::write(&marker, STATE_DIR_GITIGNORE),
        Err(e) => Err(e),
    }
}

fn git_output(root: &Path, args: &[&str], input: Option<&str>) -> Result<Output> {
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(args)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("piped stdin")
            .write_all(input.as_bytes())?;
    }
    Ok(child.wait_with_output()?)
}

fn git(root: &Path, args: &[&str], input: Option<&str>) -> Result<String> {
    let output = git_output(root, args, input)?;
    if !output.status.success() {
        return Err(WorktreeError::Git {
            args: args.iter().map(|s| (*s).into()).collect(),
            message: String::from_utf8_lossy(&output.stderr).trim().into(),
        });
    }
    String::from_utf8(output.stdout)
        .map_err(|_| WorktreeError::Refused("non-UTF-8 git output".into()))
}

fn ancestor(root: &Path, tip: &str) -> Result<bool> {
    let output = git_output(root, &["merge-base", "--is-ancestor", tip, "HEAD"], None)?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(WorktreeError::Refused(
            "cannot determine branch ancestry".into(),
        )),
    }
}

fn utf8_path(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| WorktreeError::Refused("non-UTF-8 lane path".into()))
}

fn disposable_ignored(path: &str) -> bool {
    path.starts_with(".multiverse/")
        || [
            "target/",
            "bin/",
            ".venv/",
            ".pytest_cache/",
            "__pycache__/",
        ]
        .iter()
        .any(|prefix| path.starts_with(prefix))
}

fn lock(root: &Path) -> Result<LockGuard> {
    let path = guarded_path(root, Path::new(".multiverse/worktrees.lock"), true)?;
    private_dir(path.parent().expect("lock parent"))?;
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    let guard = lock::acquire_default(&path, "worktree").map_err(|error| match error {
        lock::LockError::Io(error) => WorktreeError::Io(error),
        error => WorktreeError::Refused(format!("worktree lock unavailable: {error}")),
    })?;
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(guard)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub task: String,
    pub branch: String,
    pub path: PathBuf,
}

/// Must inspect every task occupying an adopted lane, refuse live/unknown panes,
/// and append durable outcomes before removal. `force` may bypass missing
/// statuses only; never liveness. Called again immediately before mutation.
pub trait LaneObserver {
    fn check(&mut self, lane: &Lane, force: bool) -> Result<()>;
    fn record_prune(&mut self, lane: &Lane) -> Result<()>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    Kept {
        task: String,
        reason: String,
    },
    WouldRemove {
        task: String,
        branch_refusal: Option<String>,
    },
    WouldArchive {
        task: String,
    },
    Removed {
        task: String,
        branch_refusal: Option<String>,
        retained_state: Option<PathBuf>,
        rescued: Vec<PathBuf>,
    },
    Archived {
        task: String,
        tip: String,
        retained_state: Option<PathBuf>,
        rescued: Vec<PathBuf>,
    },
}

/// A branch with no registered checkout. Preview uses these same outcomes
/// without writing refs or creating the worktree lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchOutcome {
    Kept { task: String, reason: String },
    Reclaimed { task: String },
    Archived { task: String, tip: String },
}

#[derive(Debug, Clone)]
pub struct Worktrees {
    project: PathBuf,
}

fn deep_prune_size(path: &Path) -> u64 {
    let Ok(info) = fs::symlink_metadata(path) else {
        return 0;
    };
    let mut size = info.len();
    if info.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                size += deep_prune_size(&entry.path());
            }
        }
    }
    size
}

const DEEP_PRUNE_CACHE_NAMES: &[&str] = &[
    ".cache",
    ".lane-gocache",
    ".node-gyp",
    ".npm",
    ".pnpm-store",
    ".pytest_cache",
    ".yarn",
    "node_modules",
];

fn deep_prune_cache_candidate(path: &Path) -> bool {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    DEEP_PRUNE_CACHE_NAMES.contains(&name.as_ref())
        || name.starts_with("pytest-of-")
        || name.starts_with("pytest-tmp")
}

fn deep_prune_entries(path: &Path) -> io::Result<Vec<PathBuf>> {
    let mut entries = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    entries.sort();
    Ok(entries)
}

fn deep_prune_directories(path: &Path) -> Vec<PathBuf> {
    deep_prune_entries(path)
        .unwrap_or_default()
        .into_iter()
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|info| info.is_dir()))
        .collect()
}

fn deep_prune_newest_mtime(path: &Path) -> Option<SystemTime> {
    let info = fs::symlink_metadata(path).ok()?;
    let mut newest = info.modified().ok()?;
    if info.is_dir() {
        for entry in deep_prune_entries(path).ok()? {
            newest = newest.max(deep_prune_newest_mtime(&entry)?);
        }
    }
    Some(newest)
}

fn deep_prune_keep_reason(path: &Path) -> Option<&'static str> {
    let info = fs::symlink_metadata(path).ok()?;
    if info.is_symlink() || info.is_dir() {
        return None;
    }
    let name = path.file_name()?.to_string_lossy();
    if matches!(name.as_ref(), "brief.md" | "result.md" | "status") {
        Some("task audit record")
    } else if name.ends_with(".log") {
        Some("log file")
    } else if info.is_file() && info.len() <= 8192 && fs::read_to_string(path).is_ok() {
        Some("small text file")
    } else {
        None
    }
}

#[derive(Default)]
struct DeepPruneRecord {
    status: String,
    dispatch: u64,
    outcome: u64,
    lane: String,
}

fn deep_prune_terminal(status: &str) -> bool {
    matches!(status, "done" | "blocked" | "failed")
}

fn deep_prune_records(project: &Path) -> BTreeMap<String, DeepPruneRecord> {
    let mut tasks = BTreeMap::<String, DeepPruneRecord>::new();
    let Ok(records) = crate::index::records(
        &project.join(".multiverse/routing.log"),
        crate::index::Selector::Tasks,
    ) else {
        return tasks;
    };
    for record in records {
        let Ok(parsed) = record.parse() else { continue };
        let Some(task) = parsed.fields.get("task") else {
            continue;
        };
        let entry = tasks.entry(task.clone()).or_default();
        match parsed.event() {
            "dispatch" => {
                *entry = DeepPruneRecord {
                    dispatch: record.entry.ordinal + 1,
                    lane: parsed
                        .fields
                        .get("branch")
                        .and_then(|branch| branch.strip_prefix("multiverse/"))
                        .unwrap_or(task)
                        .to_string(),
                    ..Default::default()
                };
            }
            "result" | "supersede" => {
                let status = parsed
                    .fields
                    .get("status")
                    .map(String::as_str)
                    .unwrap_or("");
                if parsed.event() == "result" && status == "none" {
                    continue;
                }
                entry.status = status.to_string();
                entry.outcome = record.entry.ordinal + 1;
            }
            _ => {}
        }
    }
    tasks
}

fn deep_prune_finished(task: &Path, records: &BTreeMap<String, DeepPruneRecord>) -> bool {
    if records
        .get(
            task.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .as_ref(),
        )
        .is_some_and(|record| {
            deep_prune_terminal(&record.status) && record.outcome >= record.dispatch
        })
    {
        return true;
    }
    fs::read_to_string(task.join("status")).is_ok_and(|text| {
        text.lines().any(|line| {
            let line = line.trim();
            deep_prune_terminal(line)
                || line
                    .strip_prefix("status=")
                    .is_some_and(deep_prune_terminal)
        })
    })
}

/// Open every ancestor relative to a pinned directory, refusing symlinks.
/// Recursion also uses directory handles, so renaming/replacing an ancestor
/// cannot redirect an unlink into a symlink target outside the owner.
fn deep_prune_remove(path: &Path, root: &Path) -> Result<()> {
    use rustix::fs::{open, openat, Mode, OFlags};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let relative = path
        .strip_prefix(root)
        .map_err(|_| WorktreeError::Refused("cleanup path escapes root".into()))?;
    let mut directory = open(root, flags, Mode::empty()).map_err(io::Error::from)?;
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let Component::Normal(name) = component else {
            return Err(WorktreeError::Refused("cleanup path escapes root".into()));
        };
        if components.peek().is_none() {
            deep_prune_remove_at(&directory, name)?;
        } else {
            directory = openat(&directory, name, flags, Mode::empty()).map_err(io::Error::from)?;
        }
    }
    Ok(())
}

fn deep_prune_remove_at(parent: &std::os::fd::OwnedFd, name: &std::ffi::OsStr) -> io::Result<()> {
    use rustix::fs::{openat, statat, unlinkat, AtFlags, Dir, FileType, Mode, OFlags};
    use std::os::unix::ffi::OsStrExt;
    let info = statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
    if FileType::from_raw_mode(info.st_mode) == FileType::Directory {
        let child = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        for entry in Dir::read_from(&child)? {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name != b"." && name != b".." {
                deep_prune_remove_at(&child, std::ffi::OsStr::from_bytes(name))?;
            }
        }
        unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    } else {
        unlinkat(parent, name, AtFlags::empty())?;
    }
    Ok(())
}

struct DeepPrune<'a, W> {
    dry_run: bool,
    out: &'a mut W,
    bytes: u64,
    removed: u64,
}

impl<W: Write> DeepPrune<'_, W> {
    fn remove(&mut self, entry: &Path, root: &Path, suffix: &str) -> io::Result<()> {
        let size = deep_prune_size(entry);
        let action = if self.dry_run {
            "would remove"
        } else {
            if let Err(error) = deep_prune_remove(entry, root) {
                writeln!(
                    self.out,
                    "  deep kept {} — could not remove: {error}",
                    entry.display()
                )?;
                return Ok(());
            }
            "removed"
        };
        writeln!(
            self.out,
            "  deep {action} {} ({size} bytes){suffix}",
            entry.display()
        )?;
        self.bytes += size;
        self.removed += 1;
        Ok(())
    }
}

impl Worktrees {
    /// Sweep finished tasks older than two days, preserving audit records,
    /// `*.log` files and UTF-8 text files of at most 8192 bytes. Project
    /// scratch uses the newest descendant's age; finished, clean lanes'
    /// named scratch caches are eligible immediately. Preview only reports.
    /// `lanes` must be the snapshot taken before ordinary lane pruning.
    pub fn deep_prune(&self, lanes: &[Lane], dry_run: bool, out: &mut impl Write) -> Result<()> {
        let _lock = lock(&self.project)?;
        let cutoff = SystemTime::now() - std::time::Duration::from_secs(2 * 86400);
        let records = deep_prune_records(&self.project);
        let dirty_lanes: std::collections::BTreeSet<_> = lanes
            .iter()
            .filter(|lane| lane.path.exists())
            .filter(|lane| {
                git(
                    &lane.path,
                    &["status", "--porcelain=v1", "--untracked-files=all"],
                    None,
                )
                .map(|status| !status.trim().is_empty())
                .unwrap_or(true)
            })
            .map(|lane| lane.task.clone())
            .collect();
        let mut dirty_tasks = dirty_lanes.clone();
        for (task, record) in &records {
            if dirty_lanes.contains(&record.lane) {
                dirty_tasks.insert(task.clone());
            }
        }
        let mut owners = vec![self.project.clone()];
        owners.extend(
            lanes
                .iter()
                .filter(|lane| lane.path.exists())
                .map(|lane| lane.path.clone()),
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut cleanup = DeepPrune {
            dry_run,
            out,
            bytes: 0,
            removed: 0,
        };
        for owner in &owners {
            let Ok(tasks_root) = guarded_path(owner, Path::new(".multiverse/tasks"), false) else {
                continue;
            };
            let Ok(resolved) = fs::canonicalize(&tasks_root) else {
                continue;
            };
            if !seen.insert(resolved) {
                continue;
            }
            for task in deep_prune_directories(&tasks_root) {
                let name = task.file_name().unwrap().to_string_lossy();
                if !deep_prune_finished(&task, &records) {
                    writeln!(
                        cleanup.out,
                        "  deep kept {name} — task is in progress or has no terminal outcome"
                    )?;
                    continue;
                }
                if dirty_tasks.contains(name.as_ref()) {
                    writeln!(
                        cleanup.out,
                        "  deep kept {name} — worktree has uncommitted changes"
                    )?;
                    continue;
                }
                if !fs::metadata(&task)
                    .and_then(|info| info.modified())
                    .is_ok_and(|mtime| mtime < cutoff)
                {
                    continue;
                }
                for entry in deep_prune_entries(&task).unwrap_or_default() {
                    let size = deep_prune_size(&entry);
                    if let Some(reason) = deep_prune_keep_reason(&entry) {
                        writeln!(
                            cleanup.out,
                            "  deep kept {} ({size} bytes) — {reason}",
                            entry.display()
                        )?;
                    } else {
                        cleanup.remove(&entry, owner, "")?;
                    }
                }
            }
        }
        for lane in lanes {
            if !lane.path.exists() || dirty_lanes.contains(&lane.task) {
                continue;
            }
            let tasks = deep_prune_directories(&lane.path.join(".multiverse/tasks"));
            if tasks.is_empty() || !tasks.iter().all(|task| deep_prune_finished(task, &records)) {
                continue;
            }
            let Ok(scratch) = guarded_path(&lane.path, Path::new(".multiverse/scratch/tmp"), false)
            else {
                continue;
            };
            for entry in deep_prune_entries(&scratch).unwrap_or_default() {
                if deep_prune_cache_candidate(&entry) {
                    cleanup.remove(&entry, &lane.path, " — finished lane's scratch")?;
                }
            }
        }
        if let Ok(scratch) =
            guarded_path(&self.project, Path::new(".multiverse/scratch/tmp"), false)
        {
            for entry in deep_prune_entries(&scratch).unwrap_or_default() {
                if deep_prune_newest_mtime(&entry).is_some_and(|mtime| mtime < cutoff) {
                    cleanup.remove(&entry, &self.project, "")?;
                }
            }
        }
        let action = if dry_run { "would free" } else { "freed" };
        let suffix = if dry_run {
            " — nothing was changed"
        } else {
            ""
        };
        writeln!(
            cleanup.out,
            "\ndeep cleanup: {action} {} bytes in {} path(s){suffix}",
            cleanup.bytes, cleanup.removed
        )?;
        Ok(())
    }

    pub fn new(project: impl AsRef<Path>) -> Result<Self> {
        let project = fs::canonicalize(project)?;
        let top = git(&project, &["rev-parse", "--show-toplevel"], None)?;
        if fs::canonicalize(top.trim())? != project {
            return Err(WorktreeError::Refused(
                "expected the project repository root".into(),
            ));
        }
        Ok(Self { project })
    }

    fn lane(&self, task: &str, missing: bool) -> Result<Lane> {
        let branch = branch_name(task)?;
        let path = guarded_path(
            &self.project,
            &Path::new(".multiverse/worktrees").join(task),
            missing,
        )?;
        Ok(Lane {
            task: task.into(),
            branch,
            path,
        })
    }

    /// Foreign files, symlinks and dot-prefixed helpers are silent non-lanes.
    pub fn lanes(&self) -> Result<Vec<Lane>> {
        let root = guarded_path(&self.project, Path::new(".multiverse/worktrees"), true)?;
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut lanes = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let Some(task) = name.to_str() else { continue };
            if validate_id(task).is_ok() && entry.file_type()?.is_dir() {
                lanes.push(self.lane(task, false)?);
            }
        }
        lanes.sort_by(|a, b| a.task.cmp(&b.task));
        Ok(lanes)
    }

    fn registrations(&self) -> Result<BTreeMap<PathBuf, String>> {
        let output = git(
            &self.project,
            &["worktree", "list", "--porcelain", "-z"],
            None,
        )?;
        let mut result = BTreeMap::new();
        let mut path = None;
        for field in output.split('\0') {
            if let Some(value) = field.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            }
            if let Some(value) = field.strip_prefix("branch ") {
                if let Some(path) = path.take() {
                    result.insert(path, value.trim_start_matches("refs/heads/").into());
                }
            }
        }
        Ok(result)
    }

    fn registered(&self, lane: &Lane) -> Result<()> {
        if self.registrations()?.get(&lane.path) != Some(&lane.branch) {
            return Err(WorktreeError::Refused(
                "lane is not registered on its owner branch".into(),
            ));
        }
        Ok(())
    }

    pub fn create(&self, task: &str, base: &str) -> Result<Lane> {
        let _lock = lock(&self.project)?;
        let lane = self.lane(task, true)?;
        let parent = lane.path.parent().expect("lane parent");
        private_dir(parent)?;
        for entry in fs::read_dir(parent)? {
            if entry?
                .file_name()
                .to_string_lossy()
                .eq_ignore_ascii_case(task)
            {
                return Err(WorktreeError::Refused(
                    "lane path already exists (case folded)".into(),
                ));
            }
        }
        let base_sha = git(
            &self.project,
            &["rev-parse", "--verify", &format!("{base}^{{commit}}")],
            None,
        )?;
        if self.owner_branch_exists(&lane.branch)? {
            return Err(WorktreeError::Refused(
                "owner branch already exists; adopt or restore it explicitly".into(),
            ));
        }
        git(
            &self.project,
            &[
                "worktree",
                "add",
                "-b",
                &lane.branch,
                lane.path
                    .to_str()
                    .ok_or_else(|| WorktreeError::Refused("non-UTF-8 lane path".into()))?,
                base_sha.trim(),
            ],
            None,
        )?;
        git(
            &self.project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                base_sha.trim(),
            ],
            None,
        )?;
        // The lane's own copy, so no caller has to remember. A lane whose
        // scaffolding shows in its `git status` is a lane that can never be
        // judged disposable, and an agent asked to commit its work will
        // reasonably commit ours too.
        ensure_state_dir(&lane.path)?;
        Ok(lane)
    }

    fn owner_branch_exists(&self, branch: &str) -> Result<bool> {
        let full_ref = format!("refs/heads/{branch}");
        let existing = git_output(
            &self.project,
            &["show-ref", "--verify", "--quiet", &full_ref],
            None,
        )?;
        if existing.status.success() {
            return Ok(true);
        }
        if existing.status.code() != Some(1) {
            return Err(WorktreeError::Refused("cannot inspect owner branch".into()));
        }
        Ok(false)
    }

    /// Whether `task`'s owner branch already exists — the same collision
    /// [`create`](Self::create) itself refuses on, exposed so a caller
    /// minting a fresh task id (critic finding 5 /
    /// `crates/mv-cli/src/commands/dispatch.rs`'s `RealDispatchStore::
    /// prepare`) can detect a same-day collision and rebind to a free id
    /// instead of only ever finding out from a failed `create`.
    pub fn branch_exists(&self, task: &str) -> Result<bool> {
        let branch = branch_name(task)?;
        self.owner_branch_exists(&branch)
    }

    pub fn adopt(&self, task: &str, observer: &mut impl LaneObserver) -> Result<Lane> {
        let _lock = lock(&self.project)?;
        let lane = self.lane(task, false)?;
        self.registered(&lane)?;
        observer.check(&lane, false)?;
        // `observer.check` alone "steps aside" on an occupant with no
        // recorded pane at all (same gap
        // `prune_keeps_a_lane_with_no_recorded_pane_on_missing_status_alone`
        // documents for `HerdrLaneObserver`); `terminal_statuses` is
        // `prune`'s own backstop for that, and adoption needs the same one
        // — skipping only an occupant this lane never actually dispatched
        // (no task directory ever written for it), so a pristine,
        // never-used lane from a bare `create` stays freely adoptable.
        self.terminal_statuses(&lane, false, true)?;
        if has_process_cwd(&lane.path)? {
            return Err(WorktreeError::Refused("lane has a live process cwd".into()));
        }
        Ok(lane)
    }

    fn tip(&self, task: &str) -> Result<String> {
        let branch = branch_name(task)?;
        Ok(git(
            &self.project,
            &["rev-parse", "--verify", &format!("refs/heads/{branch}")],
            None,
        )?
        .trim()
        .into())
    }

    fn clean(&self, lane: &Lane, allow_state_commits: bool, allow_ignored: bool) -> Result<()> {
        self.registered(lane)?;
        let status = git(
            &lane.path,
            &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
            None,
        )?;
        if !status.is_empty() {
            return Err(WorktreeError::Refused(
                "lane has uncommitted changes".into(),
            ));
        }
        let committed = git(&lane.path, &["ls-files", "-z", "--", ".multiverse"], None)?;
        if !allow_state_commits
            && committed
                .split('\0')
                .any(|p| !p.is_empty() && p != ".multiverse/roster.toml")
        {
            return Err(WorktreeError::Refused(
                "lane contains committed task state".into(),
            ));
        }
        let ignored = git(
            &lane.path,
            &[
                "ls-files",
                "--others",
                "--ignored",
                "--exclude-standard",
                "-z",
            ],
            None,
        )?;
        for path in ignored.split('\0').filter(|p| !p.is_empty()) {
            if !allow_ignored && !disposable_ignored(path) {
                return Err(WorktreeError::Refused(format!(
                    "unrescued ignored file: {path}"
                )));
            }
        }
        if has_process_cwd(&lane.path)? {
            return Err(WorktreeError::Refused("lane has a live process cwd".into()));
        }
        Ok(())
    }

    /// `skip_never_dispatched`: when true, an occupant whose
    /// `.multiverse/tasks/<task>` directory was never written in either the
    /// project or the lane — [`Self::task_directory_exists`], i.e. no
    /// `write_brief` ever ran for it here — is skipped outright rather than
    /// treated as a missing status. `prune` always passes `false`: a
    /// pristine, never-dispatched lane still needs `force` (or a recorded
    /// terminal outcome) before it is destroyed. `adopt` passes `true`: a
    /// pristine lane is freely reusable, but one that was actually
    /// dispatched into and never settled is not.
    fn terminal_statuses(
        &self,
        lane: &Lane,
        force: bool,
        skip_never_dispatched: bool,
    ) -> Result<()> {
        for task in occupant_tasks(&self.project, lane)? {
            if skip_never_dispatched && !self.task_directory_exists(lane, &task)? {
                continue;
            }
            let relative = Path::new(".multiverse/tasks").join(&task).join("status");
            let root_path = guarded_path(&self.project, &relative, true)?;
            let root = if root_path.try_exists()? {
                &self.project
            } else {
                &lane.path
            };
            let path = guarded_path(root, &relative, true)?;
            if !path.try_exists()? {
                // Python's `record_prune_outcome`/`would_record_prune_outcome`
                // (`worktrees.py:3279-3401`) never gate on the status *file*;
                // they gate on `tasks.has_terminal_outcome`, which also
                // accepts a terminal `event=result` the routing log already
                // recorded for this task's latest dispatch — a status file
                // missing because `close_stranded` recovered the outcome
                // after a crash, never a reason to keep the lane.
                if force || has_terminal_outcome(&self.project, &task) {
                    continue;
                }
                return Err(WorktreeError::Refused(format!(
                    "missing terminal status for {task}"
                )));
            }
            let status = read_regular(root, &relative)?;
            if !matches!(status.trim(), "done" | "blocked" | "failed") {
                return Err(WorktreeError::Refused(format!(
                    "invalid terminal status for {task}"
                )));
            }
            if status.trim() == "done" {
                let result = read_regular(root, &relative.with_file_name("result.md"))?;
                if result.trim().is_empty() {
                    return Err(WorktreeError::Refused(format!("empty result for {task}")));
                }
            }
        }
        Ok(())
    }

    /// Whether `task`'s `.multiverse/tasks/<task>` directory exists in the
    /// project's shared checkout or in `lane`'s own checkout — the evidence
    /// `write_brief` (`mv-engine::runner`) leaves behind for any task a
    /// dispatch actually attempted, independent of whether a status or a
    /// pane ever followed it.
    fn task_directory_exists(&self, lane: &Lane, task: &str) -> Result<bool> {
        let relative = Path::new(".multiverse/tasks").join(task);
        Ok(guarded_path(&self.project, &relative, true)?.try_exists()?
            || guarded_path(&lane.path, &relative, true)?.try_exists()?)
    }

    /// Rescue never overwrites: different bytes, symlinks, and special files
    /// refuse removal. The source remains available after rescue.
    pub fn rescue(&self, task: &str) -> Result<()> {
        let _lock = lock(&self.project)?;
        let lane = self.lane(task, false)?;
        self.registered(&lane)?;
        self.rescue_state(&lane.path.join(".multiverse"))?;
        Ok(())
    }

    /// Returns every artifact path actually copied — see
    /// `copy_without_overwrite`, whose result this passes through unchanged.
    fn rescue_state(&self, state: &Path) -> Result<Vec<PathBuf>> {
        match fs::symlink_metadata(state) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                copy_without_overwrite(state, &self.project.join(".multiverse"))
            }
            Ok(_) => Err(WorktreeError::Refused("unsafe lane state".into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn prune(
        &self,
        observer: &mut impl LaneObserver,
        force: bool,
        dry_run: bool,
    ) -> Result<Vec<PruneOutcome>> {
        self.prune_with_ignored(observer, force, dry_run, false)
    }

    /// Explicit confirmation permits discarding ignored data. It never bypasses
    /// ancestry/archive authority, tracked changes, occupant or process guards.
    /// The confirmation is carried through the final removal recheck too.
    pub fn prune_with_ignored(
        &self,
        observer: &mut impl LaneObserver,
        force: bool,
        dry_run: bool,
        allow_ignored: bool,
    ) -> Result<Vec<PruneOutcome>> {
        // Read-only preview does not even create a lock file.
        let _lock = if dry_run {
            None
        } else {
            Some(lock(&self.project)?)
        };
        let mut outcomes = Vec::new();
        for lane in self.lanes()? {
            let check = (|| {
                observer.check(&lane, force)?;
                self.terminal_statuses(&lane, force, false)?;
                let tip = self.tip(&lane.task)?;
                let archiving = !ancestor(&self.project, &tip)?;
                if archiving
                    && !self.patch_landed(&tip)?
                    && !self.retirement_authorized(&lane.task, &tip)?
                {
                    return Err(WorktreeError::Refused(
                        "branch is not merged into HEAD and has no archive authority".into(),
                    ));
                }
                self.clean(&lane, archiving, allow_ignored)?;
                check_copy(
                    &lane.path.join(".multiverse"),
                    &self.project.join(".multiverse"),
                )?;
                Ok(archiving)
            })();
            let archiving = match check {
                Err(error) => {
                    outcomes.push(PruneOutcome::Kept {
                        task: lane.task,
                        reason: error.to_string(),
                    });
                    continue;
                }
                Ok(archiving) => archiving,
            };
            if dry_run {
                outcomes.push(if archiving {
                    PruneOutcome::WouldArchive { task: lane.task }
                } else {
                    let branch_refusal = self
                        .terminal_statuses(&lane, false, false)
                        .err()
                        .map(|_| format!("no recorded outcome for {}", lane.task));
                    PruneOutcome::WouldRemove {
                        task: lane.task,
                        branch_refusal,
                    }
                });
            } else {
                observer.check(&lane, force)?;
                self.terminal_statuses(&lane, force, false)?;
                observer.record_prune(&lane)?;
                // Rescue *before* removing, never after — see `remove_lane`'s
                // own doc comment. This is the one call whose copied-path
                // list a caller can report; `remove_lane`'s own internal
                // `rescue_state` (on the already-quarantined copy) exists
                // only to catch a late write in the swap race, and is never
                // itself surfaced as "rescued" — Python's own CLI reports
                // only its single `rescue()` call the same way.
                let mut rescued = self.rescue_state(&lane.path.join(".multiverse"))?;
                rescued.sort();
                let retained_state = self.remove_lane(&lane, archiving, allow_ignored)?;
                outcomes.push(if archiving {
                    PruneOutcome::Archived {
                        tip: self.archive_locked(&lane.task)?,
                        task: lane.task,
                        retained_state,
                        rescued,
                    }
                } else {
                    let branch_refusal = if has_terminal_outcome(&self.project, &lane.task) {
                        self.reclaim_locked(&lane.task).err().map(|e| e.to_string())
                    } else {
                        Some(format!("no recorded outcome for {}", lane.task))
                    };
                    PruneOutcome::Removed {
                        task: lane.task,
                        branch_refusal,
                        retained_state,
                        rescued,
                    }
                });
            }
        }
        Ok(outcomes)
    }

    fn remove_lane(
        &self,
        lane: &Lane,
        allow_state_commits: bool,
        allow_ignored: bool,
    ) -> Result<Option<PathBuf>> {
        self.clean(lane, allow_state_commits, allow_ignored)?;
        let state = lane.path.join(".multiverse");
        // Even a previously empty lane needs a redirect for a first late write.
        if !state.try_exists()? {
            private_dir(&state)?;
        }
        let snapshot = snapshot_state(&state)?;
        let retained = if state.exists() {
            let root = guarded_path(&self.project, Path::new(".multiverse/worktree-state"), true)?;
            private_dir(&root)?;
            let path = root.join(format!("{}-{}", lane.task, nonce()));
            fs::rename(&state, &path)?;
            // Redirect writers holding the old pathname while Git removes the
            // checkout. Git unlinks this symlink; it cannot traverse its target.
            if let Err(error) = std::os::unix::fs::symlink(&path, &state) {
                fs::rename(&path, &state)?;
                return Err(error.into());
            }
            // Rescue and quarantine are separate operations. A write between
            // them must refuse removal while the checkout still exists.
            if let Err(error) = check_copy(&path, &self.project.join(".multiverse")) {
                fs::remove_file(&state)?;
                fs::rename(&path, &state)?;
                return Err(error);
            }
            Some(path)
        } else {
            None
        };
        let removed = (|| {
            let status = git(
                &lane.path,
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                None,
            )?;
            if status.split('\0').filter(|v| !v.is_empty()).any(|record| {
                let path = record.get(3..).unwrap_or("");
                path != ".multiverse" && !path.starts_with(".multiverse/")
            }) {
                return Err(WorktreeError::Refused(
                    "late checkout write; removal refused".into(),
                ));
            }
            let ignored = git(
                &lane.path,
                &[
                    "ls-files",
                    "--others",
                    "--ignored",
                    "--exclude-standard",
                    "-z",
                ],
                None,
            )?;
            if !allow_ignored
                && ignored
                    .split('\0')
                    .any(|p| !p.is_empty() && !disposable_ignored(p))
            {
                return Err(WorktreeError::Refused(
                    "late ignored write; removal refused".into(),
                ));
            }
            // The redirect itself is untracked even with a .multiverse/ ignore
            // rule, which matches directories only. All other changes refuse.
            git(
                &self.project,
                &["worktree", "remove", "--force", utf8_path(&lane.path)?],
                None,
            )
        })();
        if removed.is_err() {
            if let Some(path) = &retained {
                if fs::symlink_metadata(&state).is_ok_and(|m| m.file_type().is_symlink()) {
                    fs::remove_file(&state)?;
                    fs::rename(path, &state)?;
                }
            }
            removed?;
        }
        // Never delete quarantine: an already open descriptor can still write
        // here after Git returns. Callers can surface this path for later rescue.
        if let Some(path) = &retained {
            self.restore_if_changed(lane, path, &snapshot)?;
            self.rescue_state(path)?;
        }
        Ok(retained)
    }

    fn restore_if_changed(
        &self,
        lane: &Lane,
        retained: &Path,
        before: &StateSnapshot,
    ) -> Result<()> {
        if snapshot_state(retained)? == *before {
            return Ok(());
        }
        let restored = git(
            &self.project,
            &["worktree", "add", utf8_path(&lane.path)?, &lane.branch],
            None,
        );
        if let Err(error) = restored {
            return Err(WorktreeError::Refused(format!(
                "late state write retained at {}; checkout restoration failed: {error}",
                retained.display()
            )));
        }
        let state = lane.path.join(".multiverse");
        if state.try_exists()? {
            // A checkout may recreate committed roster/task files. Keep those
            // separately rather than overwriting either version of the state.
            fs::rename(&state, retained.with_extension("checkout-state"))?;
        }
        fs::rename(retained, state)?;
        Err(WorktreeError::Refused(
            "late state write detected; lane and state restored".into(),
        ))
    }

    /// Ancestry is the only authority for deleting a branch with `-d`.
    pub fn reclaim(&self, task: &str) -> Result<()> {
        let _lock = lock(&self.project)?;
        self.reclaim_locked(task)
    }

    fn check_reclaim(&self, task: &str) -> Result<()> {
        let branch = branch_name(task)?;
        let tip = self.tip(task)?;
        if self.registrations()?.values().any(|value| value == &branch) {
            return Err(WorktreeError::Refused("branch is checked out".into()));
        }
        if !ancestor(&self.project, &tip)? {
            return Err(WorktreeError::Refused(
                "branch is not merged into HEAD".into(),
            ));
        }
        Ok(())
    }

    fn reclaim_locked(&self, task: &str) -> Result<()> {
        self.check_reclaim(task)?;
        let branch = branch_name(task)?;
        // -d can consult branch upstream. Explicit ancestry above still gates it.
        git(&self.project, &["branch", "-d", "--", &branch], None)?;
        Ok(())
    }

    /// Reclaim recorded ancestors; archive authorized non-ancestors; keep
    /// everything else. A status file alone never authorizes an orphan sweep.
    pub fn prune_orphans(&self, dry_run: bool) -> Result<Vec<BranchOutcome>> {
        let _lock = if dry_run {
            None
        } else {
            Some(lock(&self.project)?)
        };
        let mut outcomes = Vec::new();
        for task in self.orphan_branches()? {
            if !has_terminal_outcome(&self.project, &task) {
                outcomes.push(BranchOutcome::Kept {
                    reason: format!("no recorded outcome for {task}"),
                    task,
                });
                continue;
            }
            let reclaim = if dry_run {
                self.check_reclaim(&task)
            } else {
                self.reclaim_locked(&task)
            };
            if reclaim.is_ok() {
                outcomes.push(BranchOutcome::Reclaimed { task });
                continue;
            }
            let archive = if dry_run {
                self.tip(&task).and_then(|tip| {
                    self.check_archive(&task, &tip)?;
                    Ok(tip)
                })
            } else {
                self.archive_locked(&task)
            };
            outcomes.push(match archive {
                Ok(tip) => BranchOutcome::Archived { task, tip },
                Err(error) => BranchOutcome::Kept {
                    task,
                    reason: format!("{error} (no archive ref was written)"),
                },
            });
        }
        Ok(outcomes)
    }

    pub fn orphan_branches(&self) -> Result<Vec<String>> {
        let checked = self.registrations()?;
        let output = git(
            &self.project,
            &[
                "for-each-ref",
                "--format=%(refname:short)",
                "refs/heads/multiverse/",
            ],
            None,
        )?;
        Ok(output
            .lines()
            .filter(|branch| !checked.values().any(|b| b == branch))
            .filter_map(|branch| branch.strip_prefix("multiverse/"))
            .filter(|task| validate_id(task).is_ok())
            .map(str::to_owned)
            .collect())
    }

    /// Archive, never delete, a patch-equivalent or explicitly retired orphan.
    /// Move the branch to a pending ref before checking for a racing checkout;
    /// finalize only while the branch name is still absent. Every ref move is
    /// compare-and-swap, and a failed finalization retains the pending tip.
    pub fn archive(&self, task: &str) -> Result<String> {
        let _lock = lock(&self.project)?;
        self.archive_locked(task)
    }

    fn check_archive(&self, task: &str, tip: &str) -> Result<()> {
        let branch = branch_name(task)?;
        if self.registrations()?.values().any(|value| value == &branch)
            || self.lane(task, true)?.path.exists()
        {
            return Err(WorktreeError::Refused(
                "remove the lane before archiving its branch".into(),
            ));
        }
        if !self.patch_landed(tip)? && !self.retirement_authorized(task, tip)? {
            return Err(WorktreeError::Refused(
                "no patch-id or completed assessment retirement authority".into(),
            ));
        }
        Ok(())
    }

    fn archive_locked(&self, task: &str) -> Result<String> {
        let branch = branch_name(task)?;
        let tip = self.tip(task)?;
        self.check_archive(task, &tip)?;
        let reference = format!("refs/multiverse/archive/{task}");
        let pending = format!("refs/multiverse/archive-pending/{task}");
        let zero = "0".repeat(tip.len());
        let existing = git_output(
            &self.project,
            &["rev-parse", "--verify", "--quiet", &reference],
            None,
        )?;
        let old = if existing.status.success() {
            let sha = String::from_utf8_lossy(&existing.stdout).trim().to_owned();
            if sha != tip {
                return Err(WorktreeError::Refused(format!(
                    "{reference} already holds {}, a different commit than {branch}'s tip {}: \
                     {task} was reused for a second lane. Refusing to overwrite the first archive.",
                    &sha[..12],
                    &tip[..12]
                )));
            }
            sha
        } else {
            zero.clone()
        };
        let transaction = format!(
            "start\ncreate {pending} {tip}\ndelete refs/heads/{branch} {tip}\nprepare\ncommit\n"
        );
        git(
            &self.project,
            &["update-ref", "--stdin"],
            Some(&transaction),
        )?;
        // If inspection fails, restore rather than guessing that no checkout
        // raced with the move. CAS preserves a concurrently recreated branch.
        if !self
            .registrations()
            .is_ok_and(|checked| !checked.values().any(|b| b == &branch))
        {
            let restore = format!(
                "start\ncreate refs/heads/{branch} {tip}\ndelete {pending} {tip}\nprepare\ncommit\n"
            );
            git(&self.project, &["update-ref", "--stdin"], Some(&restore))?;
            return Err(WorktreeError::Refused(
                "checkout raced with archive; branch restored".into(),
            ));
        }
        self.finish_archive(task, &tip, &old)?;
        Ok(tip)
    }

    fn finish_archive(&self, task: &str, tip: &str, old: &str) -> Result<()> {
        let branch = branch_name(task)?;
        let reference = format!("refs/multiverse/archive/{task}");
        let pending = format!("refs/multiverse/archive-pending/{task}");
        let zero = "0".repeat(tip.len());
        let transaction = format!(
            "start\nverify refs/heads/{branch} {zero}\nupdate {reference} {tip} {old}\n\
             delete {pending} {tip}\nprepare\ncommit\n"
        );
        git(
            &self.project,
            &["update-ref", "--stdin"],
            Some(&transaction),
        )?;
        Ok(())
    }

    fn patch_landed(&self, tip: &str) -> Result<bool> {
        let output = git(&self.project, &["cherry", "HEAD", tip], None)?;
        if output.lines().any(|line| line.starts_with('+')) {
            return Ok(false);
        }
        if output.lines().any(|line| line.starts_with('-')) {
            return Ok(true);
        }
        // No unique patch may be an ancestor or a zero-diff commit.
        if ancestor(&self.project, tip)? {
            return Ok(true);
        }
        Ok(git(&self.project, &["diff", "--name-only", "HEAD", tip], None)?.is_empty())
    }

    fn retirement_authorized(&self, task: &str, tip: &str) -> Result<bool> {
        let root = guarded_path(&self.project, Path::new(".multiverse/tasks"), true)?;
        let entries = match fs::read_dir(&root) {
            Ok(v) => v,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        let target_time = git(&self.project, &["show", "-s", "--format=%ct", tip], None)?
            .trim()
            .parse::<u64>()
            .map_err(|_| WorktreeError::Refused("invalid commit time".into()))?;
        let recorded = recorded_task_outcomes(&self.project.join(".multiverse/routing.log"));
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == task || validate_id(&name).is_err() || !entry.file_type()?.is_dir() {
                continue;
            }
            // Python requires `role`/`status` from `routing.log`'s own
            // dispatch/result records, never the task-id spelling or the
            // task directory's own (unrecorded) status file — an
            // authorizing task's `done` must have actually been logged
            // (`critic-history-compat.md` finding 8).
            let Some(outcome) = recorded.get(&name) else {
                continue;
            };
            if !matches!(
                outcome.role.as_str(),
                "explorer" | "oracle" | "scout" | "seer"
            ) {
                continue;
            }
            if outcome.result_status.as_deref() != Some("done") {
                continue;
            }
            let result = match read_regular(&root, &Path::new(&name).join("result.md")) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if result.trim().is_empty() {
                continue;
            }
            let retire = match read_regular(&root, &Path::new(&name).join("retire")) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if fs::metadata(entry.path().join("retire"))?
                .modified()?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                <= target_time
            {
                continue;
            }
            for line in retire.lines() {
                let parts: Vec<_> = line.split_whitespace().collect();
                if parts.len() == 2
                    && parts[0].trim_start_matches("multiverse/") == task
                    && parts[1] == tip
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn archived(&self) -> Result<Vec<(String, String)>> {
        let output = git(
            &self.project,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/multiverse/archive/",
            ],
            None,
        )?;
        Ok(output
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter_map(|(r, sha)| {
                r.strip_prefix("refs/multiverse/archive/")
                    .map(|id| (id.to_owned(), sha.to_owned()))
            })
            .collect())
    }

    /// Restore only the branch. The archive remains, and no checkout is made.
    /// An existing branch is never overwritten; if it does not contain the
    /// archived tip, explain the checkout/archive race and name the saved ref.
    pub fn restore(&self, task: &str) -> Result<String> {
        let branch = branch_name(task)?;
        let reference = format!("refs/multiverse/archive/{task}");
        let existing = git_output(
            &self.project,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
            None,
        )?;
        let archived = git_output(
            &self.project,
            &["rev-parse", "--verify", "--quiet", &reference],
            None,
        )?;
        if existing.status.success() {
            if archived.status.success() {
                let sha = String::from_utf8_lossy(&archived.stdout).trim().to_owned();
                let contains = git_output(
                    &self.project,
                    &["merge-base", "--is-ancestor", &sha, &branch],
                    None,
                )?;
                if !contains.status.success() {
                    return Err(WorktreeError::Refused(format!(
                        "{branch} already exists but does NOT contain the archived \
                         commit {} — it was recreated at an unrelated root, \
                         which is what happens when a worktree still had the branch \
                         checked out at archive time and someone committed after. \
                         Nothing is lost: the archived tip is reachable at {reference}. \
                         Inspect it with `git log {reference}`, or check it out under a \
                         name of your choosing with `git branch <new-name> {reference}`. \
                         Refusing rather than overwriting {branch}, whose commits \
                         are someone's real work.",
                        &sha[..12]
                    )));
                }
            }
            return Err(WorktreeError::Refused(format!("{branch} already exists")));
        }
        if !archived.status.success() {
            return Err(WorktreeError::Refused(format!(
                "no archive ref for {task} ({reference})"
            )));
        }
        let tip = String::from_utf8_lossy(&archived.stdout).trim().to_owned();
        let _lock = lock(&self.project)?;
        git(
            &self.project,
            &[
                "update-ref",
                &format!("refs/heads/{branch}"),
                &tip,
                &"0".repeat(tip.len()),
            ],
            None,
        )?;
        Ok(tip)
    }

    /// The canonicalized project root a caller needs to build its own
    /// [`LaneObserver`] against (e.g. [`HerdrLaneObserver::new`]).
    pub fn project(&self) -> &Path {
        &self.project
    }

    /// Entries directly under `.multiverse/worktrees/` that `lanes()` does
    /// not count as a lane — a plain file, a foreign directory whose name
    /// doesn't validate as a task id, a symlink. `lanes()` stays silent
    /// about these (its own doc comment); this exists only so a human
    /// running `--prune`/inspect can see what was skipped and why.
    pub fn ignored_entries(&self) -> Result<Vec<PathBuf>> {
        let root = guarded_path(&self.project, Path::new(".multiverse/worktrees"), true)?;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut ignored = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let is_lane = name.to_str().is_some_and(|n| validate_id(n).is_ok())
                && entry.file_type()?.is_dir();
            if !is_lane {
                ignored.push(root.join(&name));
            }
        }
        ignored.sort();
        Ok(ignored)
    }

    /// A one-line "\[state\]  holds ..." label built from this lane's own git
    /// status, its non-disposable ignored files, and whatever artifacts
    /// under its `.multiverse/` the project does not already hold a copy
    /// of — Python's own per-lane summary line in `cmd_worktrees`
    /// (`cli.py:4388-4403`), built from `uncommitted_changes`,
    /// `ignored_changes`, `unrescued_artifacts`, and `prunable`
    /// (`worktrees.py:2073-3576`). `allow_ignored` is the same confirmation
    /// `prune_with_ignored`'s own caller passes through (`--force`/`--yes`).
    /// Purely descriptive — unlike `clean()`/`terminal_statuses()` this
    /// never gates prune, so a git failure (the lane directory isn't a real
    /// registered worktree, say, or its branch was never actually created —
    /// `Lane.branch` is always the canonical `multiverse/<task>` name here,
    /// unlike Python's own per-lane `symbolic-ref` read) degrades toward
    /// "clean"/"holds nothing" rather than aborting the caller's listing.
    pub fn lane_summary(&self, lane: &Lane, allow_ignored: bool) -> String {
        let dirty = uncommitted_changes(&lane.path).unwrap_or_default();
        let mut state = if dirty.is_empty() {
            "clean".to_string()
        } else {
            format!("{} uncommitted", dirty.lines().count())
        };
        let ignored = lane_ignored_files(&lane.path);
        if !ignored.is_empty() {
            state += &format!(", {} ignored file(s)", ignored.len());
        }
        let pending = lane_pending_artifacts(&self.project, lane);
        if pending > 0 {
            state += &format!(", {pending} artifact(s) only here");
        }
        let merged = self
            .tip(&lane.task)
            .and_then(|tip| ancestor(&self.project, &tip))
            .unwrap_or(true);
        let holds = if merged
            && dirty.is_empty()
            && (allow_ignored || ignored.is_empty())
            && pending == 0
            && multiverse_commit_refusal(&self.project, &lane.task).is_none()
        {
            "nothing"
        } else {
            "work"
        };
        format!("[{state}]  holds {holds}")
    }
}

/// Ignored paths in `lane_path` that are not known regenerable scaffolding
/// — Python's `ignored_changes` (`worktrees.py:2137-2149`), narrowed to
/// this module's own notion of safe-to-discard ([`disposable_ignored`])
/// rather than porting `_ignored_path_is_safe`'s own-`.gitignore` probe.
/// A git failure degrades to "no ignored files" rather than propagating,
/// matching [`Worktrees::lane_summary`]'s own never-gate posture.
fn lane_ignored_files(lane_path: &Path) -> Vec<String> {
    git(
        lane_path,
        &[
            "ls-files",
            "--others",
            "--ignored",
            "--exclude-standard",
            "-z",
        ],
        None,
    )
    .map(|out| {
        out.split('\0')
            .filter(|path| !path.is_empty() && !disposable_ignored(path))
            .map(str::to_owned)
            .collect()
    })
    .unwrap_or_default()
}

/// How many files under `lane`'s `.multiverse/` the project's own
/// `.multiverse/` does not already hold an identical copy of — Python's
/// `unrescued_artifacts` (`worktrees.py:2898-2945`), minus its
/// `_committed_and_clean` exemption for a tracked `roster.toml`: this
/// module has no equivalent tracked-and-clean lookup at this call site, so
/// a committed, byte-identical roster would be double-counted here if its
/// lane copy ever diverged from the project's. Walks the same exclusions
/// as [`copy_without_overwrite`]/`check_copy` ([`is_rescue_excluded`]), at
/// every depth, so a conflict this flags is the same one `--prune` itself
/// would refuse on.
fn lane_pending_artifacts(project: &Path, lane: &Lane) -> usize {
    fn count(source: &Path, destination: &Path) -> usize {
        let Ok(entries) = fs::read_dir(source) else {
            return 0;
        };
        let mut pending = 0;
        for entry in entries.flatten() {
            if is_rescue_excluded(&entry.file_name()) {
                continue;
            }
            let src = entry.path();
            let dst = destination.join(entry.file_name());
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                pending += count(&src, &dst);
            } else if file_type.is_file() {
                let identical = fs::read(&src)
                    .ok()
                    .zip(fs::read(&dst).ok())
                    .is_some_and(|(a, b)| a == b);
                if !identical {
                    pending += 1;
                }
            }
        }
        pending
    }
    count(&lane.path.join(".multiverse"), &project.join(".multiverse"))
}

/// Every task considered to occupy `lane`: the lane directory's own name
/// (`lane_occupants` "always includes the lane directory's own name"),
/// any task directories nested inside the lane's own `.multiverse/tasks/`,
/// and any task this project's routing log records dispatching onto the
/// lane's branch (an adopted occupant need not have written a local task
/// directory yet).
fn occupant_tasks(project: &Path, lane: &Lane) -> Result<Vec<String>> {
    let mut tasks = vec![lane.task.clone()];
    let local = guarded_path(&lane.path, Path::new(".multiverse/tasks"), true)?;
    if local.is_dir() {
        for entry in fs::read_dir(&local)? {
            let entry = entry?;
            let task = entry.file_name().to_string_lossy().into_owned();
            if validate_id(&task).is_ok() {
                tasks.push(task);
            }
        }
    }
    let log = project.join(".multiverse/routing.log");
    if log.try_exists()? {
        let history = read_regular(project, Path::new(".multiverse/routing.log"))?;
        for line in history.lines() {
            let fields: BTreeMap<_, _> =
                line.split('\t').filter_map(|v| v.split_once('=')).collect();
            if fields.get("event") == Some(&"dispatch")
                && fields.get("branch") == Some(&lane.branch.as_str())
            {
                if let Some(task) = fields.get("task") {
                    validate_id(task)?;
                    tasks.push((*task).into());
                }
            }
        }
    }
    tasks.sort();
    tasks.dedup();
    Ok(tasks)
}

/// `task`'s own recorded status text, preferring the project's copy of
/// `.multiverse/tasks/<task>/status` over the lane's own (`terminal_statuses`'
/// "project status wins"), or `None` when neither exists. Unlike
/// `terminal_statuses`, this never validates the value against
/// done/blocked/failed — callers that need that contract still go through
/// that method.
fn occupant_status_text(project: &Path, lane: &Lane, task: &str) -> Result<Option<String>> {
    let relative = Path::new(".multiverse/tasks").join(task).join("status");
    let root_path = guarded_path(project, &relative, true)?;
    let root: &Path = if root_path.try_exists()? {
        project
    } else {
        &lane.path
    };
    let path = guarded_path(root, &relative, true)?;
    if !path.try_exists()? {
        return Ok(None);
    }
    Ok(Some(read_regular(root, &relative)?.trim().to_string()))
}

/// The pane Herdr was told about at dispatch time for `task`
/// (`routing.log`'s `event=started ... pane=`), or `None` when the task
/// was never started or the log doesn't exist. The log's *last* matching
/// line wins, same as every other routing-log reader in this crate.
fn dispatch_pane(project: &Path, task: &str) -> Result<Option<String>> {
    let log = project.join(".multiverse/routing.log");
    if !log.try_exists()? {
        return Ok(None);
    }
    let history = read_regular(project, Path::new(".multiverse/routing.log"))?;
    let mut pane = None;
    for line in history.lines() {
        let fields: BTreeMap<_, _> = line.split('\t').filter_map(|v| v.split_once('=')).collect();
        if fields.get("event") == Some(&"started") && fields.get("task") == Some(&task) {
            pane = fields.get("pane").map(|p| (*p).to_string());
        }
    }
    Ok(pane)
}

/// A snapshot of Herdr's own `agent list` reply, or `None` when Herdr
/// could not answer at all (missing binary, timeout, malformed envelope).
/// `None` is deliberately indistinguishable from "Herdr has nothing to
/// say" at this layer — [`HerdrLaneObserver::check`] is the one that
/// decides whether that silence is safe to proceed past.
///
/// `herdr_binary`, when given, replaces `argv::agent_list()`'s own
/// `HERDR_BIN_PATH` resolution (`argv::herdr_binary`'s doc comment: "read
/// from the environment on every call") with the exact path the caller
/// pins — [`HerdrLaneObserver::with_herdr_binary`] uses this so its own
/// tests never touch that process-wide variable at all, rather than
/// serializing against every other test file's own, unrelated lock on it.
fn agent_list_snapshot(herdr_binary: Option<&str>) -> Option<serde_json::Value> {
    let mut command = mv_herdr::argv::agent_list();
    if let Some(binary) = herdr_binary {
        command.argv[0] = binary.to_string();
    }
    let result = mv_herdr::run::run(&command, None).ok()?;
    if !result.ok() {
        return None;
    }
    result.payload
}

/// The `agent_status` of whichever agent-list entry names `pane`, or
/// `None` when no entry matches. Pulled apart from `agent_list_snapshot`
/// so the parsing is testable without a subprocess.
fn pane_agent_status(payload: &serde_json::Value, pane: &str) -> Option<String> {
    let agents = payload.get("result")?.get("agents")?.as_array()?;
    agents.iter().find_map(|agent| {
        if agent.get("pane_id")?.as_str()? != pane {
            return None;
        }
        Some(
            agent
                .get("agent_status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        )
    })
}

/// Backs [`LaneObserver`] with Herdr's own `agent list` reply — the only
/// source of truth this crate has for whether an occupant's dispatch pane
/// is still alive. An occupant with no terminal status anywhere and a
/// still-live pane is refused outright (`live_unsettled_occupant`): we
/// don't know it finished and something is still holding the pane. An
/// occupant whose status *is* terminal but whose pane Herdr still reports
/// live is also kept (`live_guard_reason`/`live_occupant_pane_state`) —
/// the pane just hasn't closed yet — but unlike the unsettled case this
/// one names `--force` as the way past it, since the only thing blocking
/// removal is the lingering pane, not missing evidence. A `agent list`
/// call Herdr cannot answer at all is treated the same as "still live"
/// for an unsettled occupant: refuse rather than guess, same rationale as
/// the placeholder this replaces.
pub struct HerdrLaneObserver {
    project: PathBuf,
    /// An explicit Herdr binary this observer's own `agent list` probe
    /// always uses, bypassing `HERDR_BIN_PATH` — `None` (what [`Self::new`]
    /// sets) keeps the ordinary every-call environment resolution.
    herdr_binary: Option<String>,
}

impl HerdrLaneObserver {
    pub fn new(project: impl Into<PathBuf>) -> Self {
        Self {
            project: project.into(),
            herdr_binary: None,
        }
    }

    /// Same as [`Self::new`], but pins the exact Herdr binary this
    /// observer's `agent list` probe invokes, rather than reading
    /// `HERDR_BIN_PATH` — that variable is process-wide and `cargo test`
    /// runs every file's tests as threads of one process, so a test
    /// mutating it is only ever safe against other tests sharing its own
    /// lock, never against another file's own, separate one. Pinning the
    /// binary here removes this observer's tests from that shared global
    /// entirely instead of adding a bigger lock.
    pub fn with_herdr_binary(project: impl Into<PathBuf>, herdr_binary: impl Into<String>) -> Self {
        Self {
            project: project.into(),
            herdr_binary: Some(herdr_binary.into()),
        }
    }
}

/// The refusal [`HerdrLaneObserver::check`] would raise for one occupant,
/// or `None` to let it pass. Pulled apart from `check` for the same reason
/// [`pane_agent_status`] is — testable without a subprocess.
///
/// `force` matters only for a *settled* occupant whose pane Herdr still
/// lists as live: it may bypass that one case, and only when the pane's
/// own parsed state is ready for input (idle/done,
/// `mv_herdr::envelope::AgentState::is_ready_for_input`) — a working,
/// blocked, unknown, or unparseable state is refused regardless of
/// `force`, the same as an unsettled occupant's live pane or an
/// unconfirmable snapshot (`critic-engine-state.md` finding 5: `force`
/// previously did nothing here at all; `cli.py`'s own `reason_for`
/// decision table, `cli.py:4134-4186`).
fn occupant_refusal(
    task: &str,
    settled: bool,
    live_agent_status: Option<&str>,
    snapshot_is_none: bool,
    force: bool,
) -> Option<String> {
    match (settled, live_agent_status) {
        (true, Some(agent_status)) => {
            let ready_for_input = agent_status
                .parse::<mv_herdr::envelope::AgentState>()
                .is_ok_and(mv_herdr::envelope::AgentState::is_ready_for_input);
            if ready_for_input && force {
                return None;
            }
            Some(format!(
                "{task} still has a live agent pane ({agent_status}); rerun with \
                 --force to remove it anyway."
            ))
        }
        (false, Some(_)) => Some(format!(
            "{task} has no terminal status and still has a live agent pane"
        )),
        (false, None) if snapshot_is_none => Some(format!(
            "{task} has no terminal status and Herdr could not confirm its pane is closed"
        )),
        _ => None,
    }
}

impl LaneObserver for HerdrLaneObserver {
    fn check(&mut self, lane: &Lane, force: bool) -> Result<()> {
        let snapshot = agent_list_snapshot(self.herdr_binary.as_deref());
        for task in occupant_tasks(&self.project, lane)? {
            let status = occupant_status_text(&self.project, lane, &task)?;
            let settled = status
                .as_deref()
                .is_some_and(|s| matches!(s, "done" | "blocked" | "failed"));
            let Some(pane) = dispatch_pane(&self.project, &task)? else {
                continue;
            };
            let live = snapshot.as_ref().and_then(|p| pane_agent_status(p, &pane));
            if let Some(reason) =
                occupant_refusal(&task, settled, live.as_deref(), snapshot.is_none(), force)
            {
                return Err(WorktreeError::Refused(reason));
            }
        }
        Ok(())
    }

    fn record_prune(&mut self, lane: &Lane) -> Result<()> {
        record_prune_outcomes(&self.project, lane)
    }
}

/// `record_prune_outcome`/`would_record_prune_outcome` (`worktrees.py:3279-
/// 3401`): before a lane is removed, every occupant must have a terminal
/// `event=result` in the project's routing log, not merely a terminal
/// status *file* — [`Worktrees::rescue_state`] preserves the file, but a
/// log reader never learns the dispatch ended unless the log itself says
/// so (`critic-engine-state.md` finding 3: this was previously a no-op, so
/// `prune` removed lanes with no such record ever appended).
///
/// Idempotent per occupant via [`has_terminal_outcome`]: a task whose
/// latest dispatch already has a recorded `result` is left alone. A task
/// with neither a recorded outcome nor a valid terminal status file
/// refuses the whole prune rather than removing a lane no reader can later
/// account for — [`Worktrees::terminal_statuses`] already ran immediately
/// before this and should have refused first, so this is a backstop, not
/// the primary gate.
fn record_prune_outcomes(project: &Path, lane: &Lane) -> Result<()> {
    // Every occupant is checked before anything is appended — mirroring
    // `record_prune_outcome`'s own two passes (`worktrees.py:3346-3360`) —
    // so a lane that fails the check is not left half-logged
    // (`rereview-engine.md` "prune outcome recording can partially mutate
    // the ledger before refusing").
    let mut to_record = Vec::new();
    for task in occupant_tasks(project, lane)? {
        if has_terminal_outcome(project, &task) {
            continue;
        }
        let status = occupant_status_text(project, lane, &task)?;
        let Some(status) = status.filter(|s| matches!(s.as_str(), "done" | "blocked" | "failed"))
        else {
            return Err(WorktreeError::Refused(format!(
                "cannot record a prune outcome for {task}: no terminal status available"
            )));
        };
        to_record.push((task, status));
    }
    for (task, status) in to_record {
        let pane = dispatch_pane(project, &task)?.unwrap_or_default();
        let when = mv_core::time::now_stamp();
        let line = format!(
            "{when}\tevent=result\ttask={task}\tstatus={status}\tstate=reconciled\t\
             revisions=0\tpane={pane}"
        );
        crate::log::ProjectLog::new(project)
            .append(&line)
            .map_err(|error| {
                WorktreeError::Refused(format!(
                    "could not record a prune outcome for {task}: {error}"
                ))
            })?;
    }
    Ok(())
}

/// Whether `task` reached a terminal outcome in its *latest* dispatch —
/// mirrors Python's `tasks.has_terminal_outcome` (`tasks.py:564-619`).
/// Scanned from the log's end: a `dispatch` record for `task` reached
/// before any matching `result` means the run now occupying this task id
/// has not finished yet, even if an earlier run's result for the same id
/// sits further back in the log. Deliberately looser than
/// [`recorded_task_outcomes`]'s own per-task bookkeeping, which does not
/// reset `result_status` on a later `dispatch` and so cannot answer this
/// question on its own.
fn has_terminal_outcome(project: &Path, task: &str) -> bool {
    let log = project.join(".multiverse/routing.log");
    let Ok(records) = crate::index::records(&log, crate::index::Selector::Tasks) else {
        return false;
    };
    for record in records.iter().rev() {
        let Ok(parsed) = record.parse() else {
            continue;
        };
        if parsed.fields.get("task").map(String::as_str) != Some(task) {
            continue;
        }
        match parsed.event() {
            "dispatch" => return false,
            "result" => {
                if matches!(
                    parsed.fields.get("status").map(String::as_str),
                    Some("done" | "blocked" | "failed")
                ) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// What [`recorded_task_outcomes`] keeps per task: the role its most recent
/// `event=dispatch` record named, and the status its `event=result` record
/// (if any) recorded — mirrors `multiverse/worktrees.py:1747-1758`'s
/// `read_log`-based authorization, which [`Store::retirement_authorized`]
/// must use instead of the task-id spelling or an unrecorded on-disk status
/// file (`critic-history-compat.md` finding 8).
#[derive(Debug, Clone, Default)]
struct TaskOutcome {
    role: String,
    result_status: Option<String>,
}

/// One pass over `routing.log`'s task-lifecycle events, keyed by task id —
/// the same shape `mv-cli`'s `tasks.rs::gather_task_events` already reads
/// the log with. A missing or unreadable log yields no recorded outcomes,
/// which correctly authorizes nothing.
fn recorded_task_outcomes(log_path: &Path) -> BTreeMap<String, TaskOutcome> {
    let mut by_task: BTreeMap<String, TaskOutcome> = BTreeMap::new();
    let Ok(records) = crate::index::records(log_path, crate::index::Selector::Tasks) else {
        return by_task;
    };
    for record in &records {
        let Ok(parsed) = record.parse() else {
            continue;
        };
        let Some(task_id) = parsed.fields.get("task").cloned() else {
            continue;
        };
        let entry = by_task.entry(task_id).or_default();
        match parsed.event() {
            "dispatch" => {
                entry.role = parsed.fields.get("role").cloned().unwrap_or_default();
            }
            "result" => {
                entry.result_status = parsed.fields.get("status").cloned();
            }
            _ => {}
        }
    }
    by_task
}

pub(crate) fn read_regular(root: &Path, relative: &Path) -> Result<String> {
    let path = guarded_path(root, relative, false)?;
    let before = fs::symlink_metadata(&path)?;
    // Task artifacts are bounded; the append-only ledger grows with history.
    let limit = if relative == Path::new(".multiverse/routing.log") {
        u64::MAX
    } else {
        4 * 1024 * 1024
    };
    if !before.is_file() || before.nlink() != 1 || before.len() > limit {
        return Err(WorktreeError::Refused(
            "expected a regular, unlinked file".into(),
        ));
    }
    let file = File::open(&path)?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(&path)?;
    if opened.dev() != before.dev()
        || opened.ino() != before.ino()
        || after.ino() != before.ino()
        || after.file_type().is_symlink()
    {
        return Err(WorktreeError::Refused("file changed during open".into()));
    }
    use std::io::Read;
    let mut text = String::new();
    file.take(limit).read_to_string(&mut text)?;
    Ok(text)
}

fn nonce() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{nanos}-{}",
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

type StateSnapshot = BTreeMap<PathBuf, Option<Vec<u8>>>;

fn snapshot_state(root: &Path) -> Result<StateSnapshot> {
    fn collect(root: &Path, path: &Path, result: &mut StateSnapshot) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .expect("snapshot root")
                .to_path_buf();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                result.insert(relative, None);
                collect(root, &path, result)?;
            } else if kind.is_file() {
                result.insert(relative, Some(fs::read(path)?));
            } else {
                return Err(WorktreeError::Refused(
                    "unsafe artifact during state snapshot".into(),
                ));
            }
        }
        Ok(())
    }
    let mut result = BTreeMap::new();
    collect(root, root, &mut result)?;
    Ok(result)
}

/// Mirrors Python's `lane_artifacts` exclusion (`worktrees.py:2678-2713`):
/// the self-ignoring marker `ensure_state_dir` writes is not a writer's
/// output, a nested lane's own `worktrees/` is rescued with *that* lane,
/// not folded into this one, and `--input`'s copy of something the
/// project already handed the lane is not something the lane produced.
/// Checked at every recursion depth, same as Python's — a `worktrees`
/// directory several levels down is excluded exactly like a top-level one.
fn is_rescue_excluded(name: &std::ffi::OsStr) -> bool {
    matches!(name.to_str(), Some(".gitignore" | "worktrees" | "input"))
}

fn check_copy(source: &Path, destination: &Path) -> Result<()> {
    match fs::symlink_metadata(source) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(WorktreeError::Refused("unsafe state directory".into()))
        }
        Ok(_) => (),
    }
    match fs::symlink_metadata(destination) {
        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
            return Err(WorktreeError::Refused("unsafe rescue directory".into()))
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
        _ => (),
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if is_rescue_excluded(&entry.file_name()) {
            continue;
        }
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        let meta = entry.file_type()?;
        if meta.is_symlink() || (!meta.is_file() && !meta.is_dir()) {
            return Err(WorktreeError::Refused("unsafe state artifact".into()));
        }
        if let Ok(target) = fs::symlink_metadata(&dst) {
            if target.file_type().is_symlink() || target.is_dir() != meta.is_dir() {
                return Err(WorktreeError::Refused("unsafe rescue destination".into()));
            }
            if meta.is_file() && fs::read(&src)? != fs::read(&dst)? {
                return Err(WorktreeError::Refused(format!(
                    "rescue conflict: {}",
                    dst.display()
                )));
            }
        }
        if meta.is_dir() {
            check_copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// Returns every destination path actually written — mirrors Python's
/// `rescue`'s own `copied` list, which only names a freshly-created file,
/// never one already present with identical bytes (`rescue_plan`'s
/// `to_copy`/conflict split). Directories are never themselves reported,
/// matching `lane_artifacts`'s file-only `rglob`.
fn copy_without_overwrite(source: &Path, destination: &Path) -> Result<Vec<PathBuf>> {
    check_copy(source, destination)?;
    private_dir(destination)?;
    let mut copied = Vec::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if is_rescue_excluded(&entry.file_name()) {
            continue;
        }
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copied.extend(copy_without_overwrite(&src, &dst)?);
        } else {
            let mut input = File::open(&src)?;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&dst)
            {
                Ok(mut output) => {
                    io::copy(&mut input, &mut output)?;
                    output.set_permissions(input.metadata()?.permissions())?;
                    let meta = input.metadata()?;
                    output.set_times(
                        std::fs::FileTimes::new()
                            .set_accessed(meta.accessed()?)
                            .set_modified(meta.modified()?),
                    )?;
                    output.sync_all()?;
                    copied.push(dst);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if fs::read(&src)? != fs::read(&dst)? {
                        return Err(WorktreeError::Refused("rescue conflict".into()));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    fsync_dir(destination)?;
    Ok(copied)
}

/// Process inspection fails closed on an unavailable probe. Individual gone
/// processes are normal races; other processes with unreadable cwd are unknown.
fn has_process_cwd(lane: &Path) -> Result<bool> {
    #[cfg(target_os = "linux")]
    {
        // Our own effective uid: `/proc/<pid>/cwd` is only readable for a
        // process owned by the reader (or root), so a shared host's own
        // system daemons — sshd, cron, the container runtime, all root —
        // deny it unconditionally, every time, not as a race. That denial
        // is not "unknown"; a process we do not own could never be the one
        // whose cwd this check is guarding against, so it is skipped like
        // one already gone. A same-uid process still denies this rarely
        // enough that it stays the one case this still fails closed on.
        let own_uid = fs::metadata("/proc/self")?.uid();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            if !entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
            {
                continue;
            }
            match fs::metadata(entry.path()) {
                Ok(meta) if meta.uid() != own_uid => continue,
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            }
            match fs::read_link(entry.path().join("cwd")) {
                Ok(path) if path.starts_with(lane) => return Ok(true),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(false)
    }
    #[cfg(target_os = "macos")]
    {
        let output = Command::new("lsof")
            .args(["-a", "-d", "cwd", "-Fn"])
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            return Err(WorktreeError::Refused("cannot inspect process cwd".into()));
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| line.strip_prefix('n'))
            .any(|path| Path::new(path).starts_with(lane)))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = lane;
        Err(WorktreeError::Refused(
            "process cwd inspection unsupported".into(),
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Duration;

    pub(crate) struct TempRepo(pub PathBuf);
    impl TempRepo {
        pub(crate) fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mv-lanes-{}-{}", std::process::id(), nonce()));
            fs::create_dir(&path).unwrap();
            let repo = Self(fs::canonicalize(path).unwrap());
            git(&repo.0, &["init", "-b", "main"], None).unwrap();
            git(&repo.0, &["config", "user.name", "Lane Test"], None).unwrap();
            git(
                &repo.0,
                &["config", "user.email", "lane@example.invalid"],
                None,
            )
            .unwrap();
            git(&repo.0, &["config", "commit.gpgsign", "false"], None).unwrap();
            fs::write(repo.0.join(".gitignore"), ".multiverse/\nignored/\n").unwrap();
            fs::write(repo.0.join("README.md"), "initial\n").unwrap();
            git(&repo.0, &["add", "."], None).unwrap();
            git(&repo.0, &["commit", "-m", "initial"], None).unwrap();
            repo
        }
        fn store(&self) -> Worktrees {
            Worktrees::new(&self.0).unwrap()
        }
    }
    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Observer {
        live: bool,
        recorded: usize,
    }
    impl Observer {
        fn idle() -> Self {
            Self {
                live: false,
                recorded: 0,
            }
        }
    }
    impl LaneObserver for Observer {
        fn check(&mut self, _: &Lane, _: bool) -> Result<()> {
            if self.live {
                Err(WorktreeError::Refused("live occupant".into()))
            } else {
                Ok(())
            }
        }
        fn record_prune(&mut self, lane: &Lane) -> Result<()> {
            assert!(lane.path.exists(), "outcome must precede removal");
            self.recorded += 1;
            Ok(())
        }
    }

    fn deep_old(path: &Path) {
        File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - Duration::from_secs(3 * 86400))
            .unwrap();
    }

    fn deep_task(root: &Path, task: &str, status: &str, old: bool) -> PathBuf {
        let task = root.join(".multiverse/tasks").join(task);
        fs::create_dir_all(task.join("unknown-build-cache")).unwrap();
        fs::write(task.join("unknown-build-cache/data"), b"cache").unwrap();
        for (name, content) in [
            ("brief.md", "brief"),
            ("result.md", "result"),
            ("status", status),
            ("agent.log", "log"),
            ("notes", "small text"),
        ] {
            fs::write(task.join(name), content).unwrap();
        }
        fs::write(task.join("large.log"), vec![0xff; 9000]).unwrap();
        fs::write(task.join("binary"), [0xff]).unwrap();
        fs::write(task.join("large.txt"), vec![b'a'; 8193]).unwrap();
        if old {
            deep_old(&task);
        }
        task
    }

    #[test]
    fn deep_prune_keeps_audit_logs_small_text_unfinished_and_young_tasks() {
        let repo = TempRepo::new();
        let finished = deep_task(&repo.0, "finished", "status=done\n", true);
        // Audit names must survive independently of the small-text rule.
        for name in ["brief.md", "result.md"] {
            fs::write(finished.join(name), vec![0xff; 8193]).unwrap();
        }
        let status = format!("status=done\n{}", "x".repeat(8193));
        fs::write(finished.join("status"), &status).unwrap();
        let running = deep_task(&repo.0, "running", "working\n", true);
        let young = deep_task(&repo.0, "young", "blocked\n", false);
        let expected = deep_prune_size(&finished.join("unknown-build-cache")) + 1 + 8193;
        let mut out = Vec::new();
        repo.store().deep_prune(&[], false, &mut out).unwrap();
        for name in [
            "brief.md",
            "result.md",
            "status",
            "agent.log",
            "large.log",
            "notes",
        ] {
            assert!(finished.join(name).is_file(), "lost {name}");
        }
        assert_eq!(
            fs::read(finished.join("brief.md")).unwrap(),
            vec![0xff; 8193]
        );
        assert_eq!(
            fs::read(finished.join("result.md")).unwrap(),
            vec![0xff; 8193]
        );
        assert_eq!(fs::read_to_string(finished.join("status")).unwrap(), status);
        for name in ["unknown-build-cache", "binary", "large.txt"] {
            assert!(!finished.join(name).exists(), "retained {name}");
            assert!(running.join(name).exists(), "swept unfinished task");
            assert!(young.join(name).exists(), "swept young task");
        }
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("deep kept running — task is in progress or has no terminal outcome"));
        assert!(out.contains("— task audit record"));
        assert!(out.contains("— log file"));
        assert!(out.contains("— small text file"));
        assert!(out.ends_with(&format!(
            "\ndeep cleanup: freed {expected} bytes in 3 path(s)\n"
        )));
    }

    #[test]
    fn deep_prune_dry_run_preserves_every_candidate_and_counts_bytes() {
        let repo = TempRepo::new();
        let task = deep_task(&repo.0, "finished", "failed\n", true);
        let scratch = repo.0.join(".multiverse/scratch/tmp/old");
        fs::create_dir_all(&scratch).unwrap();
        fs::write(scratch.join("data"), b"scratch").unwrap();
        deep_old(&scratch.join("data"));
        deep_old(&scratch);
        let expected = deep_prune_size(&task.join("unknown-build-cache"))
            + 1
            + 8193
            + deep_prune_size(&scratch);
        let before = deep_prune_size(&task) + deep_prune_size(&scratch);
        let mut out = Vec::new();
        repo.store().deep_prune(&[], true, &mut out).unwrap();
        assert_eq!(before, deep_prune_size(&task) + deep_prune_size(&scratch));
        for path in [
            task.join("unknown-build-cache/data"),
            task.join("binary"),
            task.join("large.txt"),
            scratch.join("data"),
        ] {
            assert!(path.exists());
        }
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("deep would remove"));
        assert!(out.ends_with(&format!(
            "\ndeep cleanup: would free {expected} bytes in 4 path(s) — nothing was changed\n"
        )));
    }

    #[test]
    fn deep_prune_project_scratch_uses_newest_descendant_and_never_follows_links() {
        let repo = TempRepo::new();
        let scratch = repo.0.join(".multiverse/scratch/tmp");
        fs::create_dir_all(scratch.join("recent-child")).unwrap();
        fs::write(scratch.join("recent-child/data"), "new").unwrap();
        deep_old(&scratch.join("recent-child"));
        fs::write(scratch.join("old-file"), "old").unwrap();
        deep_old(&scratch.join("old-file"));
        fs::write(scratch.join("young-file"), "young").unwrap();
        let external = repo.0.join("precious");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("data"), "keep").unwrap();
        let task = deep_task(&repo.0, "finished", "done", true);
        std::os::unix::fs::symlink(&external, task.join("linked-cache")).unwrap();
        deep_old(&task);
        repo.store()
            .deep_prune(&[], false, &mut Vec::new())
            .unwrap();
        assert!(!scratch.join("old-file").exists());
        assert!(scratch.join("young-file").exists());
        assert!(scratch.join("recent-child/data").exists());
        assert!(external.join("data").exists());
        assert!(fs::symlink_metadata(task.join("linked-cache")).is_err());

        fs::remove_dir_all(repo.0.join(".multiverse/tasks")).unwrap();
        std::os::unix::fs::symlink(&external, repo.0.join(".multiverse/tasks")).unwrap();
        repo.store()
            .deep_prune(&[], false, &mut Vec::new())
            .unwrap();
        assert!(external.join("data").exists());
    }

    #[test]
    fn deep_prune_finished_clean_lane_scratch_ignores_age_but_dirty_and_unfinished_survive() {
        let repo = TempRepo::new();
        let store = repo.store();
        let clean = store.create("clean", "HEAD").unwrap();
        let dirty = store.create("dirty", "HEAD").unwrap();
        let running = store.create("running", "HEAD").unwrap();
        for lane in [&clean, &dirty, &running] {
            deep_task(
                &lane.path,
                &lane.task,
                if lane.task == "running" {
                    "working"
                } else {
                    "done"
                },
                true,
            );
            let tmp = lane.path.join(".multiverse/scratch/tmp");
            for cache in [
                ".cache",
                ".lane-gocache",
                ".node-gyp",
                ".npm",
                ".pnpm-store",
                ".pytest_cache",
                ".yarn",
                "node_modules",
                "pytest-of-test",
                "pytest-tmp-test",
                "other",
            ] {
                fs::create_dir_all(tmp.join(cache)).unwrap();
                fs::write(tmp.join(cache).join("data"), "young").unwrap();
            }
        }
        fs::write(dirty.path.join("README.md"), "dirty").unwrap();
        let adopted = deep_task(&repo.0, "adopted", "done", true);
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00Z\tevent=dispatch\ttask=adopted\tbranch=multiverse/dirty\n",
        )
        .unwrap();
        let lanes = [clean.clone(), dirty.clone(), running.clone()];
        store.deep_prune(&lanes, true, &mut Vec::new()).unwrap();
        assert!(clean
            .path
            .join(".multiverse/scratch/tmp/.cache/data")
            .exists());
        store.deep_prune(&lanes, false, &mut Vec::new()).unwrap();
        assert!(!clean.path.join(".multiverse/scratch/tmp/.cache").exists());
        assert!(!clean
            .path
            .join(".multiverse/scratch/tmp/pytest-tmp-test")
            .exists());
        assert!(clean
            .path
            .join(".multiverse/scratch/tmp/other/data")
            .exists());
        for lane in [&dirty, &running] {
            assert!(lane
                .path
                .join(".multiverse/scratch/tmp/.cache/data")
                .exists());
            assert!(lane
                .path
                .join(".multiverse/tasks")
                .join(&lane.task)
                .join("binary")
                .exists());
        }
        assert!(adopted.join("binary").exists());
    }

    #[test]
    fn deep_prune_log_outcomes_reset_on_redispatch_and_status_fallback_is_exact() {
        let repo = TempRepo::new();
        let recorded = deep_task(&repo.0, "recorded", "working", true);
        let redispatched = deep_task(&repo.0, "redispatched", "working", true);
        let fallback = deep_task(&repo.0, "fallback", " status=blocked \n", true);
        let malformed = deep_task(
            &repo.0,
            "malformed",
            "STATUS=done\nstatus= done\nDONE",
            true,
        );
        let missing = deep_task(&repo.0, "missing", "done", true);
        fs::remove_file(missing.join("status")).unwrap();
        deep_old(&missing);
        let history = [
            "event=dispatch\ttask=recorded",
            "event=result\ttask=recorded\tstatus=done",
            "event=dispatch\ttask=redispatched",
            "event=result\ttask=redispatched\tstatus=done",
            "event=dispatch\ttask=redispatched",
            "event=dispatch\ttask=fallback",
            "event=dispatch\ttask=malformed",
        ]
        .map(|line| format!("2026-01-01T00:00:00Z\t{line}\n"))
        .concat();
        fs::write(repo.0.join(".multiverse/routing.log"), history).unwrap();
        fs::write(recorded.join("boundary-text"), vec![b'a'; 8192]).unwrap();
        fs::create_dir(recorded.join("directory.log")).unwrap();
        fs::create_dir(recorded.join("brief.md-dir")).unwrap();
        deep_old(&recorded);
        repo.store()
            .deep_prune(&[], false, &mut Vec::new())
            .unwrap();
        assert!(!recorded.join("binary").exists());
        assert!(!fallback.join("binary").exists());
        assert!(recorded.join("boundary-text").exists());
        assert!(!recorded.join("directory.log").exists());
        assert!(!recorded.join("brief.md-dir").exists());
        for task in [&redispatched, &malformed, &missing] {
            assert!(task.join("binary").exists());
        }
    }

    #[test]
    fn deep_prune_refuses_symlinked_parents_and_counts_only_successful_removals() {
        let repo = TempRepo::new();
        let outside = repo.0.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("precious"), "keep").unwrap();
        fs::create_dir(repo.0.join(".multiverse")).unwrap();
        std::os::unix::fs::symlink(&outside, repo.0.join(".multiverse/scratch")).unwrap();
        assert!(deep_prune_remove(&repo.0.join(".multiverse/scratch/precious"), &repo.0).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("precious")).unwrap(),
            "keep"
        );
        let mut out = Vec::new();
        let mut cleanup = DeepPrune {
            dry_run: false,
            out: &mut out,
            bytes: 0,
            removed: 0,
        };
        cleanup
            .remove(&outside.join("missing"), &repo.0, "")
            .unwrap();
        assert_eq!((cleanup.bytes, cleanup.removed), (0, 0));
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("— could not remove:"));
    }

    #[test]
    fn deep_prune_empty_lane_does_not_authorize_young_scratch_cleanup() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("empty", "HEAD").unwrap();
        let tmp = lane.path.join(".multiverse/scratch/tmp/.cache");
        fs::create_dir_all(&tmp).unwrap();
        fs::write(tmp.join("data"), "keep").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks")).unwrap();
        store.deep_prune(&[lane], false, &mut Vec::new()).unwrap();
        assert!(tmp.join("data").exists());
    }

    #[test]
    fn branch_names_preserve_old_ids_but_new_roles_are_enforced() {
        assert_eq!(
            branch_name("20261101-fixer-abcdef").unwrap(),
            "multiverse/20261101-fixer-abcdef"
        );
        assert_eq!(
            lane_id("20261102", "builder", "012abc").unwrap(),
            "20261102-builder-012abc"
        );
        assert!(lane_id("20261102", "fixer", "012abc").is_err());
        for id in ["../escape", ".lane-venv-x", "-flag", "a/b", ""] {
            assert!(branch_name(id).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn utf8_path_refuses_a_non_utf8_lane_path() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/\xff"));
        let err = utf8_path(path).unwrap_err();
        assert!(err.to_string().contains("non-UTF-8 lane path"), "{err}");
    }

    #[test]
    fn lane_touched_files_lists_what_changed_since_the_lanes_own_cut_point() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("auth.rs"), "changed\n").unwrap();
        fs::write(lane.path.join("README.md"), "initial\nmore\n").unwrap();
        git(&lane.path, &["add", "."], None).unwrap();
        git(&lane.path, &["commit", "-m", "work"], None).unwrap();

        let mut touched = lane_touched_files(&repo.0, "20261101-fixer-abcdef").unwrap();
        touched.sort();
        assert_eq!(
            touched,
            vec!["README.md".to_string(), "auth.rs".to_string()]
        );
    }

    #[test]
    fn lane_touched_files_refuses_a_branch_that_does_not_exist() {
        let repo = TempRepo::new();
        let err = lane_touched_files(&repo.0, "20261101-fixer-abcdef").unwrap_err();
        assert!(matches!(err, WorktreeError::Refused(_)));
    }

    /// `--wait`'s handback report (scout-gaps #4 remainder, card
    /// `20261105-b-5e4d10`): `uncommitted_changes` must distinguish a clean
    /// lane (`Some("")`) from one git could not even read (`None`) — never
    /// let the latter pass for the former.
    #[test]
    fn uncommitted_changes_reports_dirty_and_clean_lanes() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert_eq!(uncommitted_changes(&lane.path), Some(String::new()));

        fs::write(lane.path.join("scratch.txt"), "dirty\n").unwrap();
        let status = uncommitted_changes(&lane.path).unwrap();
        assert!(status.contains("scratch.txt"), "{status}");

        assert_eq!(uncommitted_changes(&repo.0.join("no-such-lane")), None);
    }

    #[test]
    fn has_commits_is_false_until_the_agent_commits() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(!has_commits(&repo.0, "20261101-fixer-abcdef").unwrap());

        fs::write(lane.path.join("work.rs"), "done\n").unwrap();
        git(&lane.path, &["add", "."], None).unwrap();
        git(&lane.path, &["commit", "-m", "work"], None).unwrap();
        assert!(has_commits(&repo.0, "20261101-fixer-abcdef").unwrap());
    }

    #[test]
    fn handback_tells_a_human_to_commit_first_when_dirty_and_uncommitted() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("scratch.txt"), "dirty\n").unwrap();

        let lines = handback(
            &repo.0,
            "20261101-fixer-abcdef",
            "multiverse/20261101-fixer-abcdef",
            &lane.path,
        );
        assert!(lines[0].contains("never committed"), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("add -A")), "{lines:?}");
    }

    #[test]
    fn handback_recommends_merge_when_clean_and_committed() {
        let repo = TempRepo::new();
        let store = repo.store();
        let branch = "multiverse/20261101-fixer-abcdef";
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("work.rs"), "done\n").unwrap();
        git(&lane.path, &["add", "."], None).unwrap();
        git(&lane.path, &["commit", "-m", "work"], None).unwrap();

        let lines = handback(&repo.0, "20261101-fixer-abcdef", branch, &lane.path);
        assert!(
            lines.iter().any(|l| l.contains(&format!("merge {branch}"))),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("refused")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("discard")), "{lines:?}");
    }

    #[test]
    fn multiverse_commit_refusal_flags_a_force_added_result_file_but_exempts_roster() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/20261101-fixer-abcdef")).unwrap();
        fs::write(
            lane.path
                .join(".multiverse/tasks/20261101-fixer-abcdef/result.md"),
            "done\n",
        )
        .unwrap();
        fs::write(lane.path.join(".multiverse/roster.toml"), "x = 1\n").unwrap();
        git(
            &lane.path,
            &[
                "add",
                "-f",
                ".multiverse/tasks/20261101-fixer-abcdef/result.md",
                ".multiverse/roster.toml",
            ],
            None,
        )
        .unwrap();
        git(&lane.path, &["commit", "-m", "oops"], None).unwrap();

        let refusal = multiverse_commit_refusal(&repo.0, "20261101-fixer-abcdef").unwrap();
        assert!(refusal.contains("result.md"), "{refusal}");
        assert!(
            !refusal.contains("roster.toml"),
            "roster.toml is the one file a project genuinely commits: {refusal}"
        );

        let lines = handback(
            &repo.0,
            "20261101-fixer-abcdef",
            "multiverse/20261101-fixer-abcdef",
            &lane.path,
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("refused: .multiverse commit")),
            "{lines:?}"
        );
    }

    /// Scout-gaps #27 (card `20261105-b-5e4d10`): diff-scope re-review needs
    /// a durable "last reviewed at" marker per lane, independent of the
    /// lane's worktree ever existing on disk.
    #[test]
    fn record_reviewed_marks_the_branch_tip_and_last_reviewed_commit_reads_it_back() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert_eq!(last_reviewed_commit(&repo.0, "20261101-fixer-abcdef"), None);

        fs::write(lane.path.join("work.rs"), "v1\n").unwrap();
        git(&lane.path, &["add", "."], None).unwrap();
        git(&lane.path, &["commit", "-m", "v1"], None).unwrap();
        let v1 = git(&lane.path, &["rev-parse", "HEAD"], None)
            .unwrap()
            .trim()
            .to_string();

        record_reviewed(&repo.0, "20261101-fixer-abcdef", None);
        assert_eq!(
            last_reviewed_commit(&repo.0, "20261101-fixer-abcdef"),
            Some(v1.clone())
        );

        // A second round of commits moves the branch tip; the marker must
        // not move until `record_reviewed` is called again.
        fs::write(lane.path.join("work.rs"), "v2\n").unwrap();
        git(&lane.path, &["add", "."], None).unwrap();
        git(&lane.path, &["commit", "-m", "v2"], None).unwrap();
        assert_eq!(
            last_reviewed_commit(&repo.0, "20261101-fixer-abcdef"),
            Some(v1)
        );

        // An explicit `--commits` override stamps exactly that sha, not the
        // tip — a reviewer handed a commit list ending before the tip must
        // not have the marker silently jump past what it actually reviewed.
        let v2 = git(&lane.path, &["rev-parse", "HEAD"], None)
            .unwrap()
            .trim()
            .to_string();
        record_reviewed(&repo.0, "20261101-fixer-abcdef", Some(&v2));
        assert_eq!(
            last_reviewed_commit(&repo.0, "20261101-fixer-abcdef"),
            Some(v2)
        );
    }

    #[test]
    fn record_reviewed_is_silently_a_no_op_for_an_unknown_task_with_no_explicit_commit() {
        let repo = TempRepo::new();
        record_reviewed(&repo.0, "20261101-fixer-abcdef", None);
        assert_eq!(last_reviewed_commit(&repo.0, "20261101-fixer-abcdef"), None);
    }

    #[test]
    fn create_adopt_and_silent_non_lane_discovery() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let root = lane.path.parent().unwrap();
        fs::create_dir(root.join(".lane-venv-helper")).unwrap();
        fs::write(root.join("foreign-file"), "keep").unwrap();
        std::os::unix::fs::symlink(&lane.path, root.join("symlink-lane")).unwrap();
        assert_eq!(store.lanes().unwrap(), vec![lane.clone()]);
        assert_eq!(
            store.adopt(&lane.task, &mut Observer::idle()).unwrap(),
            lane
        );
        assert!(store.create("20261101-FIXER-ABCDEF", "HEAD").is_err());
        assert!(store
            .adopt(
                &lane.task,
                &mut Observer {
                    live: true,
                    recorded: 0
                }
            )
            .is_err());
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(root.join(".lane-venv-helper").is_dir());
        assert_eq!(
            fs::read_to_string(root.join("foreign-file")).unwrap(),
            "keep"
        );
    }

    /// `--into` must refuse a lane whose own task was actually dispatched
    /// (its `.multiverse/tasks/<task>` directory exists — `write_brief`'s
    /// own evidence of a real attempt) and never recorded a terminal
    /// status — the exact "no occupant task still unsettled" guarantee
    /// `RealDispatchStore::prepare`'s own doc comment claims `adopt` gives
    /// parity with `prune` on. Before this fix, `adopt` called only
    /// `observer.check`, never `terminal_statuses` — the same gap
    /// `prune_keeps_a_lane_with_no_recorded_pane_on_missing_status_alone`
    /// documents `HerdrLaneObserver::check` alone "stepping aside" on, with
    /// `terminal_statuses` as `prune`'s own backstop. A crash between
    /// `write_brief` and the first `event=started` routing-log record (no
    /// pane ever minted) leaves exactly this shape: a task directory with
    /// no status and no pane for `HerdrLaneObserver` to ever find live.
    #[test]
    fn adopt_refuses_a_lane_whose_own_task_was_dispatched_but_never_settled() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks").join(&lane.task);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("brief.md"), "scoped task\n").unwrap();

        let error = store.adopt(&lane.task, &mut Observer::idle()).unwrap_err();
        assert!(
            error.to_string().contains("missing terminal status"),
            "{error}"
        );

        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        assert_eq!(
            store.adopt(&lane.task, &mut Observer::idle()).unwrap(),
            lane
        );
    }

    /// Critic finding 5: `branch_exists` is what lets a caller detect a
    /// same-day task-id collision and rebind, instead of only ever
    /// learning about it from a failed `create`.
    #[test]
    fn branch_exists_reports_a_created_lanes_branch_and_nothing_else() {
        let repo = TempRepo::new();
        let store = repo.store();
        assert!(!store.branch_exists("20261101-fixer-abcdef").unwrap());
        store.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(store.branch_exists("20261101-fixer-abcdef").unwrap());
        assert!(!store.branch_exists("20261101-fixer-ffffff").unwrap());
    }

    #[test]
    fn live_dirty_unmerged_and_ignored_lanes_refuse_prune_even_with_force() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let result = store
            .prune(
                &mut Observer {
                    live: true,
                    recorded: 0,
                },
                true,
                false,
            )
            .unwrap();
        assert!(matches!(&result[0], PruneOutcome::Kept { reason, .. } if reason.contains("live")));
        fs::write(lane.path.join("README.md"), "dirty").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("uncommitted"))
        );
        git(&lane.path, &["add", "README.md"], None).unwrap();
        git(&lane.path, &["commit", "-m", "unique"], None).unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("not merged"))
        );
        assert!(store.reclaim("owner").is_err());
        git(&repo.0, &["merge", "--ff-only", &lane.branch], None).unwrap();
        fs::create_dir(lane.path.join("ignored")).unwrap();
        fs::write(lane.path.join("ignored/data"), "unrescued").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("ignored"))
        );
        assert!(lane.path.exists());
    }

    #[test]
    fn prune_reclaims_only_a_merged_branch_with_a_recorded_outcome() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let preview = store.prune(&mut Observer::idle(), true, true).unwrap();
        assert!(matches!(&preview[0], PruneOutcome::WouldRemove { .. }));
        assert!(lane.path.exists());
        assert!(store.branch_exists("recorded").unwrap());
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(!lane.path.exists());
        assert!(!store.branch_exists("recorded").unwrap());

        let unrecorded = store.create("unrecorded", "HEAD").unwrap();
        store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(!unrecorded.path.exists());
        assert!(store.branch_exists("unrecorded").unwrap());

        let unique = store.create("unique", "HEAD").unwrap();
        fs::write(unique.path.join("README.md"), "unique work\n").unwrap();
        git(&unique.path, &["commit", "-am", "unique"], None).unwrap();
        let tip = store.tip("unique").unwrap();
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Kept { .. }));
        assert_eq!(store.tip("unique").unwrap(), tip);
        assert!(unique.path.exists());
    }

    #[test]
    fn ignored_confirmation_keeps_unmerged_work_and_live_occupants() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        fs::create_dir(lane.path.join("ignored")).unwrap();
        fs::write(lane.path.join("ignored/data"), "confirmed disposal").unwrap();
        fs::write(lane.path.join("README.md"), "unique work\n").unwrap();
        git(&lane.path, &["commit", "-am", "unique"], None).unwrap();
        let tip = store.tip("owner").unwrap();
        let outcomes = store
            .prune_with_ignored(&mut Observer::idle(), true, false, true)
            .unwrap();
        assert!(
            matches!(&outcomes[0], PruneOutcome::Kept { reason, .. } if reason.contains("not merged"))
        );
        assert_eq!(store.tip("owner").unwrap(), tip);
        assert!(lane.path.exists());
        git(&repo.0, &["merge", "--ff-only", &lane.branch], None).unwrap();
        let outcomes = store
            .prune_with_ignored(
                &mut Observer {
                    live: true,
                    recorded: 0,
                },
                true,
                false,
                true,
            )
            .unwrap();
        assert!(
            matches!(&outcomes[0], PruneOutcome::Kept { reason, .. } if reason.contains("live"))
        );
        let mut child = Command::new("sleep")
            .arg("20")
            .current_dir(&lane.path)
            .spawn()
            .unwrap();
        let outcomes = store.prune_with_ignored(&mut Observer::idle(), true, false, true);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(&outcomes.unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("live process"))
        );
        assert!(store.branch_exists("owner").unwrap());
    }

    #[test]
    fn process_cwd_refuses_prune() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let mut child = Command::new("sleep")
            .arg("20")
            .current_dir(&lane.path)
            .spawn()
            .unwrap();
        let result = store.prune(&mut Observer::idle(), true, false);
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(
            matches!(&result.unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("live process"))
        );
    }

    #[test]
    fn ignored_build_outputs_are_regenerable_but_arbitrary_ignored_data_is_not() {
        let repo = TempRepo::new();
        fs::write(
            repo.0.join(".gitignore"),
            ".multiverse/\ntarget/\n.venv/\nignored/\n",
        )
        .unwrap();
        git(&repo.0, &["add", ".gitignore"], None).unwrap();
        git(&repo.0, &["commit", "-m", "build ignores"], None).unwrap();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        for directory in ["target", ".venv"] {
            fs::create_dir(lane.path.join(directory)).unwrap();
            fs::write(lane.path.join(directory).join("generated"), "regenerable\n").unwrap();
        }
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, true).unwrap()[0],
            PruneOutcome::WouldRemove { .. }
        ));
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Removed { .. }
        ));
        assert!(!lane.path.exists());
    }

    #[test]
    fn preview_is_read_only_rescue_keeps_metadata_and_removal_retains_late_writes() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let state = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&state).unwrap();
        let path = state.join("result.md");
        fs::write(&path, "initial result").unwrap();
        fs::write(state.join("status"), "done\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let mut writer = OpenOptions::new().append(true).open(&path).unwrap();
        let mut observer = Observer::idle();
        assert_eq!(
            store.prune(&mut observer, false, true).unwrap(),
            vec![PruneOutcome::WouldRemove {
                task: "owner".into(),
                branch_refusal: None,
            }]
        );
        assert_eq!(observer.recorded, 0);
        assert!(!repo.0.join(".multiverse/tasks").exists());
        let result = store.prune(&mut observer, false, false).unwrap();
        let PruneOutcome::Removed {
            retained_state: Some(retained),
            ..
        } = &result[0]
        else {
            panic!("{result:?}")
        };
        assert_eq!(observer.recorded, 1);
        assert!(!lane.path.exists());
        writeln!(writer, " late write").unwrap();
        writer.sync_all().unwrap();
        assert!(fs::read_to_string(retained.join("tasks/owner/result.md"))
            .unwrap()
            .contains("late write"));
        let rescued = repo.0.join(".multiverse/tasks/owner/result.md");
        assert_eq!(fs::read_to_string(&rescued).unwrap(), "initial result");
        assert_eq!(fs::metadata(&rescued).unwrap().mode() & 0o777, 0o640);
        store.reclaim("owner").unwrap();
        assert!(store.tip("owner").is_err());
    }

    #[test]
    fn rescue_conflict_and_symlink_state_keep_lane() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/owner")).unwrap();
        fs::write(lane.path.join(".multiverse/tasks/owner/result.md"), "lane").unwrap();
        fs::create_dir_all(repo.0.join(".multiverse/tasks/owner")).unwrap();
        fs::write(repo.0.join(".multiverse/tasks/owner/result.md"), "project").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("conflict"))
        );
        assert!(lane.path.exists());
        fs::remove_file(lane.path.join(".multiverse/tasks/owner/result.md")).unwrap();
        std::os::unix::fs::symlink(
            repo.0.join("README.md"),
            lane.path.join(".multiverse/tasks/owner/result.md"),
        )
        .unwrap();
        assert!(store.rescue("owner").is_err());
    }

    /// A lane's own self-ignoring `.multiverse/.gitignore`
    /// (`ensure_state_dir`) almost never matches the project's own
    /// `.multiverse/.gitignore` byte for byte — differing content is
    /// expected, not lost work, so rescue must never compare it. Mirrors
    /// Python's `lane_artifacts`, which excludes any `.gitignore` by name
    /// (`worktrees.py:2707`) regardless of where it appears in the tree.
    #[test]
    fn rescue_ignores_the_lanes_own_gitignore_even_when_it_differs_from_the_projects() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        fs::write(lane.path.join(".multiverse/.gitignore"), "lane\n").unwrap();
        fs::write(repo.0.join(".multiverse/.gitignore"), "project\n").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/owner")).unwrap();
        fs::write(lane.path.join(".multiverse/tasks/owner/result.md"), "lane").unwrap();
        store.rescue("owner").unwrap();
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/.gitignore")).unwrap(),
            "project\n"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/result.md")).unwrap(),
            "lane"
        );
    }

    #[test]
    fn all_occupants_need_terminal_status_and_project_status_wins() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal"))
        );
        let local = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("status"), "done\n").unwrap();
        fs::write(local.join("result.md"), "result\n").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask=adopted\tbranch=multiverse/owner\n",
        )
        .unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("adopted"))
        );
        let project_task = repo.0.join(".multiverse/tasks/adopted");
        fs::create_dir_all(&project_task).unwrap();
        fs::write(project_task.join("status"), "blocked\n").unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), false, true).unwrap()[0],
            PruneOutcome::WouldRemove { .. }
        ));
        fs::create_dir_all(repo.0.join(".multiverse/tasks/owner")).unwrap();
        fs::write(repo.0.join(".multiverse/tasks/owner/status"), "running\n").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("invalid terminal"))
        );
    }

    #[test]
    fn a_crash_recovered_result_in_the_routing_log_substitutes_for_a_missing_status_file() {
        // Mirrors Python's `has_terminal_outcome` (`tasks.py:564-619`): a
        // missing status file is not itself a refusal when the routing log
        // already carries a terminal `event=result` for this task's latest
        // dispatch — `tasks.close_stranded` can recover an outcome into the
        // log after an agent crashes before writing its own status file.
        let repo = TempRepo::new();
        let store = repo.store();
        let _lane = store.create("owner", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask=owner\trole=fixer\n\
             2026-11-02T00:05:00+00:00\tevent=result\ttask=owner\tstatus=done\n",
        )
        .unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), false, true).unwrap()[0],
            PruneOutcome::WouldRemove { .. }
        ));

        // A later redispatch with no result of its own reinstates the
        // refusal: an earlier run's result must never authorize removing
        // the lane out from under the run now occupying the same task id.
        let mut log = OpenOptions::new()
            .append(true)
            .open(repo.0.join(".multiverse/routing.log"))
            .unwrap();
        writeln!(
            log,
            "2026-11-02T01:00:00+00:00\tevent=dispatch\ttask=owner\trole=fixer"
        )
        .unwrap();
        drop(log);
        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status"))
        );
    }

    /// `critic-engine-state.md` finding 3: `record_prune` used to be a
    /// no-op, so a lane with only a terminal status *file* and no routing
    /// log `event=result` was pruned with the ledger never learning the
    /// dispatch ended. This exercises the replacement directly, matching
    /// Python's `record_prune_outcome` idempotence: an occupant missing a
    /// recorded outcome gets one appended; one that already has one is
    /// left untouched.
    #[test]
    fn record_prune_outcomes_appends_a_terminal_result_for_an_occupant_missing_one() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let local = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("status"), "done\n").unwrap();
        fs::write(local.join("result.md"), "result\n").unwrap();

        assert!(!has_terminal_outcome(&repo.0, "owner"));
        record_prune_outcomes(&repo.0, &lane).unwrap();
        assert!(has_terminal_outcome(&repo.0, "owner"));
        let log = fs::read_to_string(repo.0.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=result\ttask=owner\tstatus=done\tstate=reconciled"),
            "expected a reconciled result line, got: {log:?}"
        );

        // Idempotent: an occupant that already has a recorded outcome is
        // left alone, not appended a second time.
        record_prune_outcomes(&repo.0, &lane).unwrap();
        let log_again = fs::read_to_string(repo.0.join(".multiverse/routing.log")).unwrap();
        assert_eq!(
            log, log_again,
            "a second call must not duplicate the record"
        );
    }

    /// `rereview-engine.md` "prune outcome recording can partially mutate
    /// the ledger before refusing": Python's `record_prune_outcome`
    /// validates every occupant before appending any of them
    /// (`worktrees.py:3346-3349` then `:3351-3360`). A two-occupant lane
    /// where the first occupant is terminal-but-unrecorded and the second
    /// has no terminal status must refuse with *no* routing-log mutation
    /// at all, not a log entry for the first occupant followed by a
    /// refusal on the second.
    #[test]
    fn record_prune_outcomes_appends_nothing_when_any_occupant_is_unrecordable() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let tasks = lane.path.join(".multiverse/tasks");
        let first = tasks.join("owner");
        fs::create_dir_all(&first).unwrap();
        fs::write(first.join("status"), "done\n").unwrap();
        fs::write(first.join("result.md"), "result\n").unwrap();
        fs::create_dir_all(tasks.join("second")).unwrap();

        let err = record_prune_outcomes(&repo.0, &lane).unwrap_err();
        assert!(
            err.to_string().contains("no terminal status available"),
            "{err}"
        );
        assert!(
            !repo.0.join(".multiverse/routing.log").exists(),
            "a refused occupant must leave no other occupant's outcome recorded either"
        );
    }

    #[test]
    fn record_prune_outcomes_refuses_when_no_terminal_status_is_available() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let err = record_prune_outcomes(&repo.0, &lane).unwrap_err();
        assert!(
            err.to_string().contains("no terminal status available"),
            "{err}"
        );
        assert!(
            !repo.0.join(".multiverse/routing.log").exists(),
            "a refused occupant must not get a partial record"
        );
    }

    #[test]
    fn assessment_retirement_requires_done_other_assessment_and_exact_tip() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-builder-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("feature"), "unique\n").unwrap();
        git(&lane.path, &["add", "feature"], None).unwrap();
        git(&lane.path, &["commit", "-m", "unique feature"], None).unwrap();
        let tip = store.tip(&lane.task).unwrap();
        let assessment_id = "20261102-oracle-123456";
        let assessment = repo.0.join(".multiverse/tasks").join(assessment_id);
        fs::create_dir_all(&assessment).unwrap();
        fs::write(assessment.join("result.md"), "assessed\n").unwrap();
        fs::write(assessment.join("status"), "blocked\n").unwrap();
        fs::write(assessment.join("retire"), format!("{} {tip}\n", lane.task)).unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        File::options()
            .write(true)
            .open(assessment.join("retire"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        assert!(!store.retirement_authorized(&lane.task, &tip).unwrap());

        // The on-disk status file alone — even `done` — must never
        // authorize archival without a matching `event=result` record in
        // `routing.log`; an unrecorded `done` is not a logged outcome
        // (`critic-history-compat.md` finding 8).
        fs::write(assessment.join("status"), "done\n").unwrap();
        assert!(!store.retirement_authorized(&lane.task, &tip).unwrap());

        fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T10:00:00+00:00\tevent=dispatch\ttask={assessment_id}\trole=oracle\n\
                 2026-11-02T10:05:00+00:00\tevent=result\ttask={assessment_id}\tstatus=done\n"
            ),
        )
        .unwrap();
        assert!(store.retirement_authorized(&lane.task, &tip).unwrap());
        assert!(!store
            .retirement_authorized(&lane.task, &"0".repeat(40))
            .unwrap_or(false));
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, true).unwrap()[0],
            PruneOutcome::WouldArchive { .. }
        ));
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Archived { tip: actual, .. } if actual == &tip)
        );
        assert!(!lane.path.exists());
        assert_eq!(store.archived().unwrap(), vec![(lane.task, tip)]);
    }

    #[test]
    fn a_logged_dispatch_under_a_task_id_whose_spelling_lies_is_still_authoritative() {
        // The authorizing role must come from the log's own `dispatch`
        // record, never a `split('-').nth(1)` guess at the task id — a
        // `builder`-dispatched task id spelled `explorer` in its id string
        // must not authorize, and a genuinely scout/seer-dispatched task
        // whose id does not literally contain that word still should.
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-builder-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("feature"), "unique\n").unwrap();
        git(&lane.path, &["add", "feature"], None).unwrap();
        git(&lane.path, &["commit", "-m", "unique feature"], None).unwrap();
        let tip = store.tip(&lane.task).unwrap();

        let misleadingly_named = "20261102-explorer-abcdef";
        let assessment = repo.0.join(".multiverse/tasks").join(misleadingly_named);
        fs::create_dir_all(&assessment).unwrap();
        fs::write(assessment.join("result.md"), "assessed\n").unwrap();
        fs::write(assessment.join("status"), "done\n").unwrap();
        fs::write(assessment.join("retire"), format!("{} {tip}\n", lane.task)).unwrap();
        File::options()
            .write(true)
            .open(assessment.join("retire"))
            .unwrap()
            .set_times(
                std::fs::FileTimes::new().set_modified(SystemTime::now() + Duration::from_secs(10)),
            )
            .unwrap();
        // The id string says "explorer", but the log says this was a
        // builder dispatch (e.g. a reused/legacy-named id) — must not
        // authorize on the id's spelling alone.
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T10:00:00+00:00\tevent=dispatch\ttask={misleadingly_named}\trole=builder\n\
                 2026-11-02T10:05:00+00:00\tevent=result\ttask={misleadingly_named}\tstatus=done\n"
            ),
        )
        .unwrap();
        assert!(!store.retirement_authorized(&lane.task, &tip).unwrap());
    }

    #[test]
    fn ordinary_prune_refuses_committed_task_artifacts() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/owner")).unwrap();
        fs::write(lane.path.join(".multiverse/tasks/owner/status"), "failed\n").unwrap();
        git(
            &lane.path,
            &["add", "-f", ".multiverse/tasks/owner/status"],
            None,
        )
        .unwrap();
        git(
            &lane.path,
            &["commit", "-m", "historical committed state fixture"],
            None,
        )
        .unwrap();
        git(&repo.0, &["merge", "--ff-only", &lane.branch], None).unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("committed task state"))
        );
    }

    #[test]
    fn detected_late_write_restores_checkout_and_state() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let state = lane.path.join(".multiverse");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("note"), "before").unwrap();
        let before = snapshot_state(&state).unwrap();
        let retained = store.remove_lane(&lane, false, false).unwrap().unwrap();
        fs::write(retained.join("note"), "late content").unwrap();
        assert!(store
            .restore_if_changed(&lane, &retained, &before)
            .unwrap_err()
            .to_string()
            .contains("restored"));
        assert_eq!(
            fs::read_to_string(lane.path.join(".multiverse/note")).unwrap(),
            "late content"
        );
        store.registered(&lane).unwrap();
    }

    #[test]
    fn archive_uses_patch_evidence_and_restore_keeps_archive() {
        let repo = TempRepo::new();
        let store = repo.store();
        let branch = "multiverse/20261101-fixer-abcdef";
        git(&repo.0, &["checkout", "-b", branch], None).unwrap();
        fs::write(repo.0.join("feature"), "new feature\n").unwrap();
        git(&repo.0, &["add", "feature"], None).unwrap();
        git(&repo.0, &["commit", "-m", "lane feature"], None).unwrap();
        let tip = store.tip("20261101-fixer-abcdef").unwrap();
        git(&repo.0, &["checkout", "main"], None).unwrap();
        assert!(store.archive("20261101-fixer-abcdef").is_err());
        fs::write(repo.0.join("unrelated"), "other\n").unwrap();
        git(&repo.0, &["add", "unrelated"], None).unwrap();
        git(&repo.0, &["commit", "-m", "other work"], None).unwrap();
        git(&repo.0, &["cherry-pick", &tip], None).unwrap();
        assert!(!ancestor(&repo.0, &tip).unwrap());
        assert!(store.reclaim("20261101-fixer-abcdef").is_err());
        assert_eq!(store.archive("20261101-fixer-abcdef").unwrap(), tip);
        assert!(store.tip("20261101-fixer-abcdef").is_err());
        assert_eq!(store.restore("20261101-fixer-abcdef").unwrap(), tip);
        assert_eq!(
            store.archived().unwrap(),
            vec![("20261101-fixer-abcdef".into(), tip)]
        );
        assert!(store.restore("20261101-fixer-abcdef").is_err());
        assert!(store
            .restore("unknown")
            .unwrap_err()
            .to_string()
            .contains("refs/multiverse/archive/unknown"));
    }

    #[test]
    fn restore_explains_resurrected_branch_divergence_without_overwriting_refs() {
        let repo = TempRepo::new();
        let store = repo.store();
        let archive = "refs/multiverse/archive/resurrected";
        git(&repo.0, &["update-ref", archive, "HEAD"], None).unwrap();
        let tip = git(&repo.0, &["rev-parse", archive], None).unwrap();
        let tip = tip.trim();
        let root = git(
            &repo.0,
            &["commit-tree", "HEAD^{tree}", "-m", "resurrected root"],
            None,
        )
        .unwrap();
        let root = root.trim();
        git(
            &repo.0,
            &["update-ref", "refs/heads/multiverse/resurrected", root],
            None,
        )
        .unwrap();
        let refusal = store.restore("resurrected").unwrap_err().to_string();
        assert!(
            refusal.contains("already exists but does NOT contain the archived commit"),
            "{refusal}"
        );
        assert!(refusal.contains(&tip[..12]), "{refusal}");
        assert!(
            refusal.contains("recreated at an unrelated root"),
            "{refusal}"
        );
        assert!(refusal.contains(archive), "{refusal}");
        assert_eq!(store.tip("resurrected").unwrap(), root);
        assert_eq!(
            git(&repo.0, &["rev-parse", archive], None).unwrap().trim(),
            tip
        );
        assert_eq!(
            git(
                &repo.0,
                &["rev-list", "--count", "multiverse/resurrected"],
                None
            )
            .unwrap()
            .trim(),
            "1"
        );
        git(
            &repo.0,
            &["update-ref", "refs/heads/multiverse/resurrected", tip, root],
            None,
        )
        .unwrap();
        assert_eq!(
            store.restore("resurrected").unwrap_err().to_string(),
            "multiverse/resurrected already exists"
        );
        git(&repo.0, &["branch", "multiverse/unarchived"], None).unwrap();
        assert_eq!(
            store.restore("unarchived").unwrap_err().to_string(),
            "multiverse/unarchived already exists"
        );
        assert_eq!(
            store.restore("unknown").unwrap_err().to_string(),
            "no archive ref for unknown (refs/multiverse/archive/unknown)"
        );
    }

    #[test]
    fn orphan_archive_preserves_existing_archives_and_pending_race_tips() {
        let repo = TempRepo::new();
        let store = repo.store();
        let first = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let first = first.trim();
        git(
            &repo.0,
            &["update-ref", "refs/multiverse/archive/reused", first],
            None,
        )
        .unwrap();
        git(&repo.0, &["commit", "--allow-empty", "-m", "second"], None).unwrap();
        let second = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let second = second.trim();
        git(&repo.0, &["branch", "multiverse/reused"], None).unwrap();
        assert!(store
            .archive("reused")
            .unwrap_err()
            .to_string()
            .contains("already holds"));
        assert_eq!(store.tip("reused").unwrap(), second);
        assert_eq!(
            git(
                &repo.0,
                &["rev-parse", "refs/multiverse/archive/reused"],
                None
            )
            .unwrap()
            .trim(),
            first
        );

        git(&repo.0, &["branch", "multiverse/repeated"], None).unwrap();
        git(
            &repo.0,
            &["update-ref", "refs/multiverse/archive/repeated", second],
            None,
        )
        .unwrap();
        assert_eq!(store.archive("repeated").unwrap(), second);

        git(
            &repo.0,
            &[
                "update-ref",
                "refs/multiverse/archive-pending/racing",
                first,
            ],
            None,
        )
        .unwrap();
        git(&repo.0, &["branch", "multiverse/racing", second], None).unwrap();
        assert!(store
            .finish_archive("racing", first, &"0".repeat(first.len()))
            .is_err());
        assert_eq!(store.tip("racing").unwrap(), second);
        assert_eq!(
            git(
                &repo.0,
                &["rev-parse", "refs/multiverse/archive-pending/racing"],
                None
            )
            .unwrap()
            .trim(),
            first
        );
        assert!(!store
            .archived()
            .unwrap()
            .iter()
            .any(|(task, _)| task == "racing"));
    }

    #[test]
    fn unregistered_directory_and_symlink_roots_refuse_mutation() {
        let repo = TempRepo::new();
        let store = repo.store();
        fs::create_dir_all(repo.0.join(".multiverse/worktrees/foreign")).unwrap();
        assert!(store.adopt("foreign", &mut Observer::idle()).is_err());
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Kept { .. }
        ));
        fs::remove_dir_all(repo.0.join(".multiverse/worktrees")).unwrap();
        std::os::unix::fs::symlink(&repo.0, repo.0.join(".multiverse/worktrees")).unwrap();
        assert!(store.create("owner", "HEAD").is_err());
        assert!(store.lanes().is_err());
    }

    #[test]
    fn a_main_branch_dispatch_with_no_worktree_is_clean() {
        let repo = TempRepo::new();
        assert!(dispatched_lane_is_clean(
            &repo.0,
            "20250101-fixer-aaaaaa",
            "main"
        ));
    }

    #[test]
    fn a_main_branch_dispatch_with_a_leftover_worktree_dir_is_not_clean() {
        let repo = TempRepo::new();
        fs::create_dir_all(repo.0.join(".multiverse/worktrees/20250101-fixer-aaaaaa")).unwrap();
        assert!(!dispatched_lane_is_clean(
            &repo.0,
            "20250101-fixer-aaaaaa",
            "main"
        ));
    }

    #[test]
    fn an_owned_lane_branch_is_never_assumed_clean() {
        let repo = TempRepo::new();
        assert!(!dispatched_lane_is_clean(
            &repo.0,
            "20250101-fixer-aaaaaa",
            "multiverse/20250101-fixer-aaaaaa"
        ));
    }

    /// `critic-engine-state.md` finding 5: `HerdrLaneObserver::check` used
    /// to ignore `force` outright, so a settled, merely-idle occupant could
    /// never be removed even with `--force`. `force` may now bypass only
    /// that one case — idle/done — never an unsettled occupant, a working
    /// pane, an unrecognized status string, or an unconfirmable snapshot.
    #[test]
    fn occupant_refusal_lets_force_through_only_for_a_settled_idle_or_done_pane() {
        for status in ["idle", "done"] {
            assert_eq!(
                occupant_refusal("t1", true, Some(status), false, true),
                None,
                "force must clear a settled {status} pane"
            );
            assert!(
                occupant_refusal("t1", true, Some(status), false, false).is_some(),
                "without force a settled {status} pane is still refused"
            );
        }
    }

    #[test]
    fn occupant_refusal_never_lets_force_through_a_working_or_unknown_pane() {
        for status in ["working", "blocked", "something-herdr-never-reports"] {
            assert!(
                occupant_refusal("t1", true, Some(status), false, true).is_some(),
                "force must not clear a settled {status} pane"
            );
        }
    }

    #[test]
    fn occupant_refusal_never_lets_force_through_an_unsettled_occupant() {
        assert!(
            occupant_refusal("t1", false, Some("idle"), false, true).is_some(),
            "an unsettled occupant's live pane is never overridden, force or not"
        );
        assert!(
            occupant_refusal("t1", false, None, true, true).is_some(),
            "an unconfirmable snapshot is never overridden, force or not"
        );
    }

    #[test]
    fn occupant_refusal_passes_a_settled_occupant_with_no_live_pane() {
        assert_eq!(occupant_refusal("t1", true, None, false, false), None);
    }

    #[test]
    fn lane_summary_reports_clean_merged_lane_as_holding_nothing() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("clean", "HEAD").unwrap();
        assert_eq!(store.lane_summary(&lane, false), "[clean]  holds nothing");
    }

    #[test]
    fn lane_summary_counts_uncommitted_lines_and_holds_work() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("dirty", "HEAD").unwrap();
        fs::write(lane.path.join("one.txt"), "one").unwrap();
        fs::write(lane.path.join("two.txt"), "two").unwrap();
        assert_eq!(
            store.lane_summary(&lane, false),
            "[2 uncommitted]  holds work"
        );
    }

    #[test]
    fn lane_summary_lists_non_disposable_ignored_files_and_holds_work() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("ignored", "HEAD").unwrap();
        // `TempRepo::new` commits a top-level `.gitignore` with an `ignored/`
        // rule; a file under it is ignored but not one of `disposable_ignored`'s
        // known-regenerable prefixes.
        fs::create_dir_all(lane.path.join("ignored")).unwrap();
        fs::write(lane.path.join("ignored/secret"), "x").unwrap();
        assert_eq!(
            store.lane_summary(&lane, false),
            "[clean, 1 ignored file(s)]  holds work"
        );
        assert_eq!(
            store.lane_summary(&lane, true),
            "[clean, 1 ignored file(s)]  holds nothing"
        );
    }

    #[test]
    fn lane_summary_counts_artifacts_the_project_does_not_already_hold() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("artifacts", "HEAD").unwrap();
        let task = lane.path.join(".multiverse/tasks/artifacts");
        fs::create_dir_all(&task).unwrap();
        fs::write(task.join("brief.md"), "brief").unwrap();
        fs::write(task.join("result.md"), "result").unwrap();
        fs::write(task.join("status"), "done").unwrap();
        assert_eq!(
            store.lane_summary(&lane, false),
            "[clean, 3 artifact(s) only here]  holds work"
        );
        let rescued = repo.0.join(".multiverse/tasks/artifacts");
        fs::create_dir_all(&rescued).unwrap();
        fs::write(rescued.join("brief.md"), "brief").unwrap();
        fs::write(rescued.join("result.md"), "result").unwrap();
        fs::write(rescued.join("status"), "done").unwrap();
        assert_eq!(store.lane_summary(&lane, false), "[clean]  holds nothing");
    }

    #[test]
    fn lane_summary_holds_work_for_an_unmerged_commit_even_when_git_clean() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("unmerged", "HEAD").unwrap();
        fs::write(lane.path.join("new.txt"), "new").unwrap();
        git(&lane.path, &["add", "new.txt"], None).unwrap();
        git(&lane.path, &["commit", "-m", "lane work"], None).unwrap();
        assert_eq!(store.lane_summary(&lane, false), "[clean]  holds work");
    }

    #[test]
    fn lane_summary_degrades_to_holds_nothing_when_the_lane_branch_was_never_created() {
        // A plain directory presented as a lane (`worktrees-prune-live-lane-
        // and-non-lane-entry` in conformance/scenarios/): `Lane.branch` is
        // always the canonical `multiverse/<task>` name here, so `tip()`
        // fails outright rather than resolving to some other real branch.
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.lane("never-registered", true).unwrap();
        fs::create_dir_all(&lane.path).unwrap();
        assert_eq!(store.lane_summary(&lane, false), "[clean]  holds nothing");
    }
}

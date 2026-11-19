//! Git lane lifecycle. Callers supply fresh occupant observations and record
//! outcomes through [`LaneObserver`]; this module never infers completion from
//! an idle pane. Removal retains quarantined state, including open-file writes.
//! A removed lane's branch is reclaimed with `-d` only after its outcome is
//! recorded and its tip is reachable from HEAD; forced unrecorded removal
//! retains the branch.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File, OpenOptions};
use std::hash::{Hash, Hasher};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mv_config::load::{resolve_home_dir, SecretsScan};
use mv_core::atomic::fsync_dir;
use mv_core::lock::{self, LockGuard};
use mv_core::redact::redact_text;

/// This module's own error type: a deliberate refusal to act
/// (`Refused`, e.g. a dirty lane, a missing archive authority, an
/// invalid task id), a `git` subprocess that exited nonzero (`Git`,
/// carrying the exact args and stderr so a caller can show *which*
/// invocation failed), or an I/O failure neither of those cover.
/// Every public function in this module returns through this type so a
/// caller never has to guess which failure mode it is looking at.
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

/// [`Worktrees::merge`]'s own error, carrying the one exit-code split its
/// caller needs: a missing *required* `gitleaks` is a configuration
/// problem (exit 3, with an install hint), never the same refusal as every
/// other reason the merge did not happen (exit 1) — an unready lane, a
/// scan tool that ran and failed, or a scan that actually found secrets.
#[derive(Debug, thiserror::Error)]
pub enum MergeError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    ScannerMissing(String),
}

impl From<WorktreeError> for MergeError {
    fn from(error: WorktreeError) -> Self {
        Self::Refused(error.to_string())
    }
}

/// [`Worktrees::merge`]'s success case: the merge commit it made, plus any
/// warning lines a caller should print (`required = false` proceeding
/// unscanned past a missing `gitleaks`) even though nothing was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeResult {
    /// The merge commit's own sha, after `git merge --no-ff --no-edit`
    /// landed `task`'s branch onto `HEAD`.
    pub sha: String,
    /// Lines a caller should print even though nothing was refused — e.g.
    /// a non-required `gitleaks` that could not be run, so the merge went
    /// ahead unscanned (see [`Worktrees::merge`]'s own doc).
    pub warnings: Vec<String>,
}

/// A scanner subprocess outcome, narrowed to what [`Worktrees::merge`]
/// branches on: `Missing` only ever means "could not even run it"
/// (binary absent, or present but not executable) — never "ran and found
/// nothing", which is `Ok(None)` on the call site's own `Result`.
enum ScanError {
    Missing,
    Failed(String),
}

// Preserve this module's refusal diagnostic while sharing task-id validation.
pub(crate) fn validate_id(id: &str) -> Result<()> {
    mv_core::id::TaskId::new(id)
        .map(|_| ())
        .map_err(|_| WorktreeError::Refused(format!("invalid task id {id:?}")))
}

/// The canonical git branch name for `task`'s own lane — every lane this
/// module creates, adopts, or reclaims is identified by this one name, so
/// two different spellings of the same task id can never collide on, or
/// miss, the same branch.
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

/// Advisory warning for [`handback`]: whether the project's main checkout
/// (`HEAD` there, whatever the trunk is named) moved since this lane's
/// own cut point and touched any file the lane itself also changed —
/// Python's `merge_readiness` (`worktrees.py:2152-2182`). `None` when the
/// comparison cannot be made (no recorded base, no resolvable `HEAD`) or
/// there is simply nothing to warn about; never acted on, only shown.
pub fn merge_readiness(project_root: &Path, task: &str) -> Option<String> {
    let branch = branch_name(task).ok()?;
    let base = lane_base(project_root, task, &branch).ok()?;
    let trunk_tip = git(
        project_root,
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        None,
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())?;
    if trunk_tip == base {
        return None;
    }
    let trunk_paths = git(
        project_root,
        &["diff", "--name-only", &format!("{base}..{trunk_tip}")],
        None,
    )
    .ok()?;
    let lane_paths = git(
        project_root,
        &["diff", "--name-only", &format!("{base}..{branch}")],
        None,
    )
    .ok()?;
    let trunk_set: HashSet<&str> = trunk_paths.lines().collect();
    let mut overlap: Vec<&str> = lane_paths
        .lines()
        .filter(|path| trunk_set.contains(path))
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

/// Paths `git` currently reports as unmerged — conflicted stage `U` in
/// `git status --porcelain` — via `git diff --name-only --diff-filter=U`.
/// [`Worktrees::merge`] uses this both to refuse before starting when the
/// trunk checkout already has some left over from an earlier operation,
/// and to name them after aborting a merge its own `git merge --no-ff
/// --no-edit` just conflicted into.
fn conflicting_paths(root: &Path) -> Result<Vec<String>> {
    let output = git(root, &["diff", "--name-only", "--diff-filter=U"], None)?;
    Ok(output
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Runs a secrets-scanner subprocess (`gitleaks`/`trufflehog`), synchronously,
/// never through a shell. `program` may be a bare command (resolved against
/// `PATH`) or an explicit path, matching how `[secrets_scan]` documents both
/// fields. `Ok(None)`: ran, found nothing. `Ok(Some(output))`: ran, found
/// something — `output` is stdout+stderr, exactly as the scanner printed it.
/// `gitleaks`' own `--redact` flag (passed by [`Worktrees::run_gitleaks`])
/// makes that safe to write as-is; TruffleHog has no equivalent flag this
/// module could verify offline, so [`Worktrees::run_trufflehog`] instead
/// masks this function's raw return value itself, after the fact, before
/// any caller sees it. [`ScanError::Missing`] covers only "could not even
/// run it" (absent or not executable); any other nonzero exit is
/// [`ScanError::Failed`], since this module cannot tell a real tool error
/// from a findings exit code it was not told to expect.
fn run_scanner(
    cwd: &Path,
    program: &str,
    args: &[String],
    findings_code: i32,
) -> std::result::Result<Option<Vec<u8>>, ScanError> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = match command.output() {
        Ok(output) => output,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Err(ScanError::Missing)
        }
        Err(e) => return Err(ScanError::Failed(e.to_string())),
    };
    match output.status.code() {
        Some(0) => Ok(None),
        Some(code) if code == findings_code => {
            let mut combined = output.stdout;
            combined.extend_from_slice(&output.stderr);
            Ok(Some(combined))
        }
        _ => {
            let code = output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "by signal".to_string());
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(ScanError::Failed(format!(
                "{program} {}: exited {code}: {}",
                args.join(" "),
                stderr.trim()
            )))
        }
    }
}

/// TruffleHog's own `--json` field names that hold the raw secret value,
/// or close enough to it that leaving them be defeats the point —
/// `Redacted` is TruffleHog's own partial preview of the raw value, not a
/// safe replacement, so it is masked the same as `Raw`/`RawV2`.
const TRUFFLEHOG_SECRET_FIELDS: [&str; 3] = ["Raw", "RawV2", "Redacted"];

/// Masks TruffleHog's `--json` output (one finding object per line) before
/// any caller of [`Worktrees::run_trufflehog`] can see it: strips the
/// fields [`TRUFFLEHOG_SECRET_FIELDS`] names from every line that parses
/// as a JSON object, then runs the whole result through [`redact_text`] as
/// a second, pattern-based pass — belt-and-suspenders for a line that
/// fails to parse (stray stderr text, a TruffleHog version with different
/// field names) and for secret-shaped text JSON stripping alone would
/// miss.
fn redact_trufflehog_output(raw: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(raw);
    let mut lines = Vec::new();
    for line in text.lines() {
        let masked = match serde_json::from_str::<serde_json::Value>(line) {
            Ok(serde_json::Value::Object(mut fields)) => {
                for field in TRUFFLEHOG_SECRET_FIELDS {
                    if let Some(value) = fields.get_mut(field) {
                        if !value.is_null() {
                            *value = serde_json::Value::String("[redacted]".into());
                        }
                    }
                }
                serde_json::to_string(&serde_json::Value::Object(fields))
                    .unwrap_or_else(|_| line.to_string())
            }
            _ => line.to_string(),
        };
        lines.push(masked);
    }
    let mut joined = lines.join("\n");
    if raw.ends_with(b"\n") {
        joined.push('\n');
    }
    redact_text(&joined).into_bytes()
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
        || path.ends_with(".pyc")
        || path.split('/').any(|component| {
            component == "target" || component == "__pycache__" || component == "node_modules"
        })
        || ["bin/", ".venv/", ".pytest_cache/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

fn lock(root: &Path) -> Result<LockGuard> {
    let path = guarded_path(root, Path::new(".multiverse/worktrees.lock"), true)?;
    private_dir(path.parent().expect("lock parent"))?;
    // Open (never truncate yet) before acquiring: truncating ahead of the
    // lock would let a second, lock-losing process race the content a
    // first process is about to write.
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
    // Only now, holding the lock, overwrite any stale pid a crashed holder
    // left behind — this is diagnostic only (so a human can tell who holds
    // it), never itself part of the locking protocol.
    file.set_len(0)?;
    writeln!(file, "{}", std::process::id())?;
    Ok(guard)
}

/// One dispatched task's own git-worktree checkout: a branch, a directory
/// on disk, and the task id that owns both. This is the unit every
/// lifecycle operation in this module (`adopt`, `prune`, `archive`, …)
/// acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// The task id that owns this lane — also the lane directory's own
    /// name under `.multiverse/worktrees/`.
    pub task: String,
    /// This lane's own git branch, always [`branch_name`]`(task)`.
    pub branch: String,
    /// The lane's checkout directory on disk.
    pub path: PathBuf,
}

/// Must inspect every task occupying an adopted lane, refuse live/unknown panes,
/// and append durable outcomes before removal. `force` may bypass missing
/// statuses only; never liveness. Called again immediately before mutation.
pub trait LaneObserver {
    fn check(&mut self, lane: &Lane, force: bool) -> Result<()>;
    /// `task_records` is `Worktrees::task_records`'s output, passed in
    /// rather than fetched here so a caller pruning many lanes in one pass
    /// (`Worktrees::prune_with_ignored`) shares one scan of the routing log
    /// across every lane's own outcome recording, instead of each lane
    /// re-scanning it on its own.
    fn record_prune(
        &mut self,
        lane: &Lane,
        task_records: &[crate::index::IndexedRecord],
    ) -> Result<()>;
}

/// What `Worktrees::prune`/`prune_with_ignored` did with one lane. `Kept`
/// covers every refusal reason (dirty, unmerged with no archive
/// authority, a live occupant, …); `WouldRemove`/`WouldArchive` are
/// `dry_run`'s own preview of what a real pass would do next; `Removed`/
/// `Archived` are what an actual pass did. A lane whose branch was merged
/// into `HEAD` is reclaimed (`Removed`); one that is not is archived
/// instead of deleted outright, so its history is never lost to a prune.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PruneOutcome {
    Kept {
        task: String,
        reason: String,
        /// Bytes this lane's own lane-root `target/` freed — or, under
        /// `dry_run`, would free — because every occupant already has a
        /// real terminal status file and nothing is still live in the
        /// lane. `None` when none of that holds, there is no such
        /// directory, or it is a symlink: see `Worktrees::kept_target_bytes`.
        target_freed: Option<u64>,
        /// Bytes this lane's own per-worktree cargo build-dir freed — or,
        /// under `dry_run`, would free — under the same eligibility as
        /// `target_freed`: see `Worktrees::kept_build_dir_bytes`.
        build_dir_freed: Option<u64>,
        /// Why that same build-dir was left in place even though this lane
        /// qualified for its reclaim — see `BuildDirReclaim::kept`.
        build_dir_kept: Option<String>,
        /// The hash dir `build_dir_freed`/`build_dir_kept` describes, when
        /// one was resolved — see `BuildDirReclaim::path`.
        build_dir: Option<PathBuf>,
    },
    WouldRemove {
        task: String,
        branch_refusal: Option<String>,
        /// Bytes `lane_path`'s own cargo build-dir would free — see
        /// `scoped_lane_build_dir`. `None` when this workspace has no
        /// per-lane build-dir namespacing configured, or the lane's
        /// build-dir cannot be resolved or proven safe to delete.
        build_dir_freed: Option<u64>,
        /// Why `build_dir_freed` is `None` although this workspace does
        /// namespace build-dirs per lane — see `BuildDirReclaim::kept`.
        build_dir_kept: Option<String>,
        /// The hash dir those two fields describe — see `Kept`'s own.
        build_dir: Option<PathBuf>,
    },
    WouldArchive {
        task: String,
        build_dir_freed: Option<u64>,
        build_dir_kept: Option<String>,
        build_dir: Option<PathBuf>,
    },
    Removed {
        task: String,
        branch_refusal: Option<String>,
        retained_state: Option<PathBuf>,
        rescued: Vec<PathBuf>,
        /// Bytes this lane's own cargo build-dir actually freed. `None`
        /// under the same conditions as `WouldRemove`'s own field.
        build_dir_freed: Option<u64>,
        build_dir_kept: Option<String>,
        /// The hash dir those two fields describe. Under a measure-only
        /// pass it is still on disk with its workspace root now gone, so
        /// `prune_orphaned_build_dirs_skipping` must be told to pass it
        /// over (card `20261119-b-c3f9f5`).
        build_dir: Option<PathBuf>,
    },
    Archived {
        task: String,
        tip: String,
        retained_state: Option<PathBuf>,
        rescued: Vec<PathBuf>,
        build_dir_freed: Option<u64>,
        build_dir_kept: Option<String>,
        build_dir: Option<PathBuf>,
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

/// Why `Worktrees::prune_quarantine` considers a `.multiverse/worktree-state/`
/// copy safe to remove. `Redundant` holds regardless of age; `RetentionExpired`
/// only once `WORKTREE_STATE_RETENTION` has passed since the copy's own
/// newest write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuarantineReason {
    Redundant,
    RetentionExpired,
}

/// One `.multiverse/worktree-state/<task>-<nonce>/` quarantine copy's own
/// outcome under `Worktrees::prune_quarantine`. `name` is the copy's own
/// directory name, `bytes` the size `deep_prune_size` measured before any
/// removal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuarantineOutcome {
    Removed {
        name: String,
        reason: QuarantineReason,
        bytes: u64,
    },
    WouldRemove {
        name: String,
        reason: QuarantineReason,
        bytes: u64,
    },
    Kept {
        name: String,
        bytes: u64,
    },
}

/// A full workspace hash dir under the shared build root (see
/// `hash_dir_candidates`) that `prune_orphaned_build_dirs` reclaimed — or,
/// under `dry_run`, would reclaim — because every workspace root its own
/// build evidence names (see `hash_dir_is_orphaned`) no longer exists on
/// disk. Distinct from a lane's own `build_dir_freed` (see `PruneOutcome`):
/// this hash dir need not belong to any lane this project's own state ever
/// tracked — a worktree removed outside `--prune` entirely, or one this
/// project never owned, counts the same so long as the evidence proves it
/// gone (card `20261115-b-673528`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanedBuildDir {
    /// The full workspace hash directory under the shared build root that
    /// was reclaimed (or, under `dry_run`, would be).
    pub path: PathBuf,
    /// Bytes `deep_prune_size` measured under `path` before removal.
    pub bytes: u64,
    /// Why `path` is still on disk despite being proven orphaned — its
    /// removal failed (card `20261119-b-a9f95f`). `None` when it was
    /// freed, or under `dry_run`, would be.
    pub kept: Option<String>,
}

/// The entry point for every lane-lifecycle operation this module offers
/// (`create`, `adopt`, `prune`, `archive`, `restore`, `merge`, …), scoped
/// to one project's shared git checkout. `project` is always the
/// canonicalized repository root — see [`Self::new`].
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

/// The `[build] build-dir` value out of a `.cargo/config.toml`'s text
/// directly, rather than pulling in a TOML parser this crate has no other
/// use for: the one value this ever needs is a plain quoted string with no
/// escapes, which this repo's own config satisfies. Ignores every `#`
/// comment line, including ones that merely mention `build-dir` in prose.
fn build_dir_template(config_text: &str) -> Option<String> {
    for line in config_text.lines() {
        let Some(rest) = line.trim().strip_prefix("build-dir") else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(value) = rest.split('"').next() {
            return Some(value.to_string());
        }
    }
    None
}

/// `{cargo-cache-home}` resolved the same way cargo itself resolves a
/// `build-dir` template's one placeholder for it — `$CARGO_HOME` if set to
/// a non-empty value, else the user's home directory's own `.cargo` —
/// without ever invoking cargo to ask (card `20261115-b-47f043`, second
/// attempt: the first attempt asked via `cargo metadata`, which ran rustup
/// inside every lane during `--prune --dry-run` and was reverted in
/// `bb794a6`). `None` only when neither `$CARGO_HOME` nor a resolvable
/// home directory exists — propagated as a refusal by `shared_build_root`,
/// never silently defaulted, since this ultimately gates a delete
/// boundary (see `resolve_home_dir`'s own doc on that distinction).
fn cargo_cache_home() -> Option<PathBuf> {
    match std::env::var_os("CARGO_HOME") {
        Some(home) if !home.is_empty() => Some(PathBuf::from(home)),
        _ => resolve_home_dir().map(|home| home.join(".cargo")),
    }
}

/// `CARGO_BUILD_BUILD_DIR`, verbatim — cargo's own env override for the
/// `[build] build-dir` config key, which cargo gives higher precedence than
/// the config file (card `20261115-b-98b977`). Empty is treated the same
/// as unset, matching `cargo_cache_home`'s own `CARGO_HOME` handling just
/// above.
fn build_dir_env_override() -> Option<String> {
    match std::env::var_os("CARGO_BUILD_BUILD_DIR") {
        Some(value) if !value.is_empty() => value.into_string().ok(),
        _ => None,
    }
}

/// A `build-dir` template's prefix (everything before its
/// `{workspace-path-hash}` placeholder) resolved to a path: `{cargo-cache-home}`
/// expands the same way regardless of where the template came from, but a
/// plain relative prefix only joins onto `relative_to` when one is given —
/// `CARGO_BUILD_BUILD_DIR` resolves a relative path against the current
/// working directory the way cargo itself does (card `20261115-b-98b977`),
/// while a `.cargo/config.toml` prefix keeps its prior, unjoined behavior.
fn resolve_build_dir_prefix(prefix: &str, relative_to: Option<&Path>) -> Option<PathBuf> {
    let prefix = prefix.trim_end_matches('/');
    let resolved = match prefix.strip_prefix("{cargo-cache-home}") {
        Some(rest) => cargo_cache_home()?.join(rest.trim_start_matches('/')),
        None => {
            let path = PathBuf::from(prefix);
            match relative_to {
                Some(base) if path.is_relative() => base.join(path),
                _ => path,
            }
        }
    };
    if resolved.as_os_str().is_empty() {
        return None;
    }
    Some(resolved)
}

/// The shared root a `build-dir` template configures — resolved from the
/// template text alone, never by asking `cargo` to resolve some workspace's
/// build-dir and working backward from it the way the first attempt did
/// (see `cargo_cache_home`'s own doc for why that was reverted). Checks
/// `CARGO_BUILD_BUILD_DIR` first, cargo's own higher-precedence override of
/// `.cargo/config.toml`'s `[build] build-dir` (card `20261115-b-98b977`),
/// resolving a relative value against the current working directory the
/// way cargo does; falls back to the config file's own template, whose
/// relative prefixes (never seen in practice — this repo's template always
/// uses `{cargo-cache-home}`) resolve exactly as before. Either way, `None`
/// when neither source has a `build-dir` entry, it doesn't parse, or the
/// template has no `{workspace-path-hash}` placeholder — build-dirs here
/// are not namespaced per workspace, so nothing under such a root is a
/// disposable per-lane cache, and `scoped_lane_build_dir` must never be
/// reached for it.
pub fn shared_build_root(project_root: &Path) -> Option<PathBuf> {
    if let Some(value) = build_dir_env_override() {
        let (prefix, _hash_suffix) = value.split_once("{workspace-path-hash}")?;
        let cwd = std::env::current_dir().ok()?;
        return resolve_build_dir_prefix(prefix, Some(&cwd));
    }
    let config_text = fs::read_to_string(project_root.join(".cargo/config.toml")).ok()?;
    let template = build_dir_template(&config_text)?;
    let (prefix, _hash_suffix) = template.split_once("{workspace-path-hash}")?;
    resolve_build_dir_prefix(prefix, None)
}

/// Bounds a whole scan over every candidate hash dir under a shared build
/// root by wall-clock time alone — a whole-run safety net, never what
/// decides any one hash dir (see `PerDirFileBudget` for that). Scaled by
/// `for_candidates` to the number of hash dirs the scan must get through:
/// a normal shared root (about a dozen hash dirs, 0.4-4 GB each) is fully
/// decided well inside this, since `PerDirFileBudget` already bounds each
/// dir's own cost to `PerDirFileBudget::FILE_BUDGET` files; this deadline
/// exists only to bound a pathological case (a huge number of hash dirs,
/// or a slow filesystem), not a normal one.
///
/// Card `20261116-b-891676`: the single budget this replaces also carried
/// a hard file-count cap shared across the *whole* scan, so two big live
/// hash dirs sorted first (the main checkout, another live lane) could
/// exhaust it before three small vanished-lane dirs after them were ever
/// read — and a dir this scan never finished reading is always kept (see
/// `OrphanVerdict::cut_short`), never reclaimed. Splitting the file cap
/// out to `PerDirFileBudget`, reset fresh for every candidate, means one
/// big dir can only ever exhaust its own cap, never borrow against the
/// dirs sorted after it.
struct BuildDirScanBudget {
    deadline: Instant,
}

impl BuildDirScanBudget {
    /// Per hash dir the scan must get through, on top of the floor below.
    /// Generous relative to how long reading at most
    /// `PerDirFileBudget::FILE_BUDGET` (500) small text files actually
    /// takes, so a normal dozen-dir sweep sits nowhere near this ceiling;
    /// it only matters once a sweep spans far more hash dirs than a real
    /// shared build root ever holds.
    const TIME_PER_HASH_DIR: Duration = Duration::from_millis(500);
    /// Floor for a small scan (one hash dir, as every per-lane/main-
    /// checkout lookup is) — the original single-dir budget this replaces.
    const MIN_TIME_BUDGET: Duration = Duration::from_secs(2);

    /// Per computed hash dir a complete read (see `for_complete_reads`)
    /// must get through. Reading a real lane's full dep-info set (866 `.d`
    /// files after this repo's whole check suite) takes a fraction of a
    /// second; this leaves room for a host under load 20-30 and a slow
    /// filesystem, and still bounds a pathological dir.
    const COMPLETE_READ_TIME_PER_DIR: Duration = Duration::from_secs(30);

    fn for_candidates(candidate_count: usize) -> Self {
        let scaled = Self::TIME_PER_HASH_DIR.saturating_mul(candidate_count.max(1) as u32);
        Self {
            deadline: Instant::now() + scaled.max(Self::MIN_TIME_BUDGET),
        }
    }

    /// The deadline for reading `dir_count` computed hash dirs (see
    /// `computed_hash_dir`) in full — paired with
    /// `PerDirFileBudget::unbounded`, never with the per-dir file cap (card
    /// `20261119-b-a9f95f`). Each of those is one known dir, not a sweep
    /// over every dir under the shared root, so it gets a complete read:
    /// `COMPLETE_READ_TIME_PER_DIR` (30 s) per dir, shared across all of
    /// them.
    fn for_complete_reads(dir_count: usize) -> Self {
        Self {
            deadline: Instant::now()
                + Self::COMPLETE_READ_TIME_PER_DIR.saturating_mul(dir_count.max(1) as u32),
        }
    }

    fn exhausted(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

/// Bounds a single hash dir's own evidence scan: at most this many
/// build-evidence files opened before giving up on deciding *that* dir
/// alone (see `OrphanVerdict::cut_short`/`dir_names_marker`'s own
/// not-found-on-exhaustion rule). Reset fresh for every candidate a scan
/// visits — see `BuildDirScanBudget`'s own doc comment for why this is
/// the actual fix for card `20261116-b-891676`, not just a companion to
/// it. 500 is comfortably above a real workspace's own dep-info/
/// fingerprint count (dozens to a few hundred files for a multi-crate
/// workspace, almost always naming the workspace itself in the first few
/// read) and far below the "thousands" a pathological or long-lived hash
/// dir can accumulate. That count holds only because `is_build_evidence`
/// skips `.fingerprint/`: with it, a real lane's hash dir held 1703
/// candidate files, not a few hundred (card `20261116-b-891676`, 0.1.9
/// observation). That is enough for a live dir to prove itself live (one
/// matching line ends that dir's scan immediately) without reading
/// anywhere near its full evidence set, and small enough that a dir with
/// no usable evidence at all can never consume more than a sliver of a
/// whole run's time.
struct PerDirFileBudget {
    files_remaining: usize,
}

impl PerDirFileBudget {
    const FILE_BUDGET: usize = 500;

    fn new() -> Self {
        Self {
            files_remaining: Self::FILE_BUDGET,
        }
    }

    /// No file cap at all, for a computed hash dir's complete read (see
    /// `BuildDirScanBudget::for_complete_reads`): a lane that ran this
    /// repo's full check suite left 866 `.d` files outside `.fingerprint/`
    /// in its one hash dir, so `FILE_BUDGET` cut that read short and kept
    /// the dir forever (card `20261119-b-a9f95f`). Only the deadline
    /// bounds it.
    fn unbounded() -> Self {
        Self {
            files_remaining: usize::MAX,
        }
    }

    fn exhausted(&self) -> bool {
        self.files_remaining == 0
    }

    fn charge(&mut self) {
        self.files_remaining = self.files_remaining.saturating_sub(1);
    }
}

/// A file this scan treats as evidence of which workspace a cargo
/// build-dir hash belongs to: a dep-info (`.d`) file outside
/// `.fingerprint/` — the one place cargo's own build output names
/// absolute source paths. Never a compiled artifact (`.rlib`, `.o`, a
/// linked binary): those carry no path text worth reading, only cost to
/// open.
///
/// Never anything under `.fingerprint/` either (card `20261116-b-891676`,
/// 0.1.9 observation): cargo's fingerprint files hold hashes, JSON with
/// relative paths, and its own re-encoded copy of the `.d` files it
/// already keeps in `deps/` — on a real lane build-dir, none of them name
/// an absolute workspace path. They are also most of the hash dir by count
/// (1375 of 1703 candidate files on the lane that 0.1.9 left behind), and
/// `.fingerprint` sorts ahead of `build/` and `deps/`. Counting them used
/// up `PerDirFileBudget` before the scan reached the one `deps/*.d` file
/// that named the lane, so the lane's build-dir was never found and never
/// freed.
fn is_build_evidence(path: &Path) -> bool {
    path.extension().is_some_and(|ext| ext == "d")
        && !path.components().any(|c| c.as_os_str() == ".fingerprint")
}

/// Every path a cargo dep-info (`.d`) file's `content` names, in order:
/// each whitespace-separated token on each non-comment line, with
/// rustc's own `\ ` space escape undone and a rule's trailing `:`
/// dropped. `#` lines (`# env-dep:…`, `# checksum:…`) carry values, not
/// paths, and are skipped whole (card `20261119-b-087c6c`): read as
/// tokens, an env-dep value that merely mentions a path could be mistaken
/// for that path's own build evidence.
fn dep_info_paths(content: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in content.lines() {
        if line.starts_with('#') {
            continue;
        }
        let mut token = String::new();
        let mut chars = line.chars();
        loop {
            let next = chars.next();
            match next {
                Some('\\') if chars.as_str().starts_with(' ') => {
                    token.push(' ');
                    chars.next();
                    continue;
                }
                Some(ch) if !ch.is_ascii_whitespace() => {
                    token.push(ch);
                    continue;
                }
                _ => {}
            }
            let path = token.trim_end_matches(':');
            if !path.is_empty() {
                paths.push(path.to_owned());
            }
            token.clear();
            if next.is_none() {
                break;
            }
        }
    }
    paths
}

/// `content`'s own workspace evidence (see `dep_info_paths`): every
/// absolute path it names that is neither a cargo registry/git-checkout
/// source (see `is_cargo_cache_source`) nor under the shared build root
/// itself (see `is_shared_build_root_path`), then every relative one —
/// cargo hands rustc a workspace member's own sources relative to the
/// workspace root (`crates/mv-core/src/lib.rs`), so a relative path is a
/// source of whichever workspace this hash dir belongs to. The one filter
/// both the per-lane lookup (`dir_names_marker`) and the orphan sweep
/// (`scan_orphan_evidence`) read through, so neither can match evidence
/// the other would have discarded (card `20261119-b-087c6c`).
fn workspace_evidence(
    content: &str,
    cargo_home: Option<&Path>,
    shared_root: Option<&Path>,
) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut absolute = Vec::new();
    let mut relative = Vec::new();
    for token in dep_info_paths(content) {
        let path = PathBuf::from(token);
        if !path.is_absolute() {
            relative.push(path);
        } else if !is_cargo_cache_source(&path, cargo_home)
            && !is_shared_build_root_path(&path, shared_root)
        {
            absolute.push(path);
        }
    }
    (absolute, relative)
}

/// Walks `dir` read-only, depth-first, for a build-evidence file (see
/// `is_build_evidence`) whose own workspace evidence (see
/// `workspace_evidence`, read with `cargo_home` and `shared_root`) names a
/// whole path under `marker` — never one also under `exclude`, when given
/// (used only so a search for the main checkout's own hash dir never
/// matches a lane nested under it: see `find_main_checkout_build_dir`).
/// Matched path by path, component-wise, never as a raw substring of a
/// line (card `20261119-b-087c6c`): another hash dir's `.d` file that
/// merely mentions this lane's path — an env-dep value, a path that only
/// embeds it — must never read as this lane's own build-dir. Bounded by
/// the whole-scan `budget`'s own deadline and by `per_dir`'s own file
/// cap, scoped to this one `dir` alone (see `PerDirFileBudget`); either
/// exhausting first reads as `MarkerScan::CutShort`, never as `Found` —
/// a scan that ran out of budget must never be read as "nothing here to
/// protect" — and never as `NotFound` either: a scan that ran out of
/// budget must never be reported as "does not name this lane" (card
/// `20261119-b-a9f95f`).
fn dir_names_marker(
    dir: &Path,
    marker: &Path,
    exclude: Option<&Path>,
    cargo_home: Option<&Path>,
    shared_root: &Path,
    budget: &mut BuildDirScanBudget,
    per_dir: &mut PerDirFileBudget,
) -> MarkerScan {
    if budget.exhausted() || per_dir.exhausted() {
        return MarkerScan::CutShort;
    }
    let Ok(entries) = deep_prune_entries(dir) else {
        return MarkerScan::NotFound;
    };
    for entry in entries {
        if budget.exhausted() || per_dir.exhausted() {
            return MarkerScan::CutShort;
        }
        let Ok(file_type) = fs::symlink_metadata(&entry).map(|info| info.file_type()) else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            match dir_names_marker(
                &entry,
                marker,
                exclude,
                cargo_home,
                shared_root,
                budget,
                per_dir,
            ) {
                MarkerScan::NotFound => continue,
                found_or_cut_short => return found_or_cut_short,
            }
        }
        if !file_type.is_file() || !is_build_evidence(&entry) {
            continue;
        }
        per_dir.charge();
        let Ok(content) = fs::read_to_string(&entry) else {
            continue;
        };
        let (absolute, _) = workspace_evidence(&content, cargo_home, Some(shared_root));
        let matched = absolute.iter().any(|path| {
            path.starts_with(marker) && !exclude.is_some_and(|excl| path.starts_with(excl))
        });
        if matched {
            return MarkerScan::Found;
        }
    }
    MarkerScan::NotFound
}

/// What one `dir_names_marker` walk of a single hash dir concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerScan {
    /// Some evidence file names the marker.
    Found,
    /// Every evidence file was read and none names the marker.
    NotFound,
    /// A budget ran out first, so an unread file may still name it.
    CutShort,
}

/// cargo's own two-component shape for a `{workspace-path-hash}`
/// expansion — observed directly off a real shared build root
/// (`~/.cargo/shared-build/2a/45625e58230a70/…`), never assumed: a first
/// path component of exactly 2 lowercase hex digits (a bucket shared by
/// every hash with that prefix), then a second of exactly 14 (the
/// workspace's own hash) — 16 hex digits in all. Anything else at either
/// depth — `debug`, `release`, `tmp`, `CACHEDIR.TAG`, a target-triple
/// dir — is a legacy, un-namespaced tree or a bucket, never a full
/// workspace hash dir, and must never be treated as, or descended into
/// as, one. Fix round for card `20261115-b-47f043`: the prior scan
/// returned a *bucket* — `shared_root`'s own immediate child — as the
/// candidate, which is neither a full hash dir nor safe to delete: two
/// lanes whose hashes share a bucket would lose each other's build, and a
/// lane named only in legacy `debug/` evidence could resolve there and
/// delete it for every project.
const HASH_COMPONENT_LENS: [usize; 2] = [2, 14];

/// `name` is exactly `len` lowercase hex digits — the shape a single
/// `{workspace-path-hash}` path component takes (see
/// `HASH_COMPONENT_LENS`); never upper-case, never any other length.
fn is_hash_component(name: &str, len: usize) -> bool {
    name.len() == len
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Every full workspace hash directory under `shared_root` —
/// `HASH_COMPONENT_LENS.len()` path components down, each one matching
/// `is_hash_component` at its own depth — never a bucket one level up,
/// and never a legacy tree: a name that fails the shape at the very first
/// depth (`debug`, `release`, `tmp`, a target-triple dir) is excluded
/// before this ever descends into it, so its contents are never scanned
/// as hash-dir candidates at all.
fn hash_dir_candidates(shared_root: &Path) -> Vec<PathBuf> {
    let mut frontier = vec![shared_root.to_path_buf()];
    for len in HASH_COMPONENT_LENS {
        let mut next = Vec::new();
        for dir in frontier {
            for child in deep_prune_directories(&dir) {
                let matches = child
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| is_hash_component(name, len));
                if matches {
                    next.push(child);
                }
            }
        }
        frontier = next;
    }
    frontier
}

/// cargo's own `{workspace-path-hash}` expansion for the workspace whose
/// root manifest is `manifest` (card `20261119-b-4ec912`), as the relative
/// `HASH_COMPONENT_LENS` path it names under a shared build root
/// (`02/7179bbe4c308d8`). Mirrors cargo's `GlobalContext::build_dir`:
/// `short_hash` of the canonical manifest path — the path's own `Hash`
/// fed to `rustc_stable_hash::StableSipHasher128`, `Hasher::finish`'s u64
/// written as little-endian hex — split after its first two digits.
/// `manifest` must already be canonical (see
/// `canonical_workspace_manifest`); this never touches the filesystem.
fn workspace_path_hash(manifest: &Path) -> PathBuf {
    let mut hasher = rustc_stable_hash::StableSipHasher128::new();
    manifest.hash(&mut hasher);
    let hex: String = Hasher::finish(&hasher)
        .to_le_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let (bucket, leaf) = hex.split_at(HASH_COMPONENT_LENS[0]);
    Path::new(bucket).join(leaf)
}

/// `workspace_root`'s own `Cargo.toml`, canonicalized the way cargo
/// canonicalizes it before hashing — or, once that workspace is gone (a
/// lane an earlier `--prune` removed), its nearest still-existing
/// ancestor canonicalized with the vanished remainder joined back on: the
/// same path cargo saw while the workspace still existed, provided
/// nothing under that ancestor was itself a symlink.
fn canonical_workspace_manifest(workspace_root: &Path) -> PathBuf {
    let manifest = workspace_root.join("Cargo.toml");
    for ancestor in manifest.ancestors() {
        let Ok(real) = fs::canonicalize(ancestor) else {
            continue;
        };
        if let Ok(rest) = manifest.strip_prefix(ancestor) {
            return if rest.as_os_str().is_empty() {
                real
            } else {
                real.join(rest)
            };
        }
    }
    manifest
}

/// The one hash dir under `shared_root` cargo would build `workspace_root`
/// into (see `workspace_path_hash`) — computed, never scanned for, so it
/// costs nothing however many hash dirs the shared root holds. Says
/// nothing about whether that dir exists or what it holds; every caller
/// still proves it from the dir's own dep-info before freeing it.
fn computed_hash_dir(shared_root: &Path, workspace_root: &Path) -> PathBuf {
    shared_root.join(workspace_path_hash(&canonical_workspace_manifest(
        workspace_root,
    )))
}

/// Whether `dir` (a lane's `computed_hash_dir`, the lane at `lane_path`
/// already gone) holds a vanished lane's build and nothing else's: its own
/// workspace evidence, read in full, names at least one path under
/// `lane_path` and no absolute path outside it — so neither a live
/// workspace nor any other vanished one (card `20261119-b-4ec912`).
/// Relative sources do not count against it: the hash itself already
/// names the one workspace those belong to. Read through
/// `scan_orphan_evidence`, so a scan cut short is never a proof — and
/// read without the per-dir file cap (`PerDirFileBudget::unbounded`), so
/// a lane that left more `.d` files than `PerDirFileBudget::FILE_BUDGET`
/// is still read in full before `budget`'s deadline (card
/// `20261119-b-a9f95f`).
fn hash_dir_names_only_lane(
    dir: &Path,
    lane_path: &Path,
    cargo_home: Option<&Path>,
    shared_root: &Path,
    budget: &mut BuildDirScanBudget,
) -> bool {
    let mut verdict = OrphanVerdict::default();
    let mut per_dir = PerDirFileBudget::unbounded();
    scan_orphan_evidence(
        dir,
        cargo_home,
        Some(shared_root),
        budget,
        &mut per_dir,
        &mut verdict,
    );
    verdict.saw_dead
        && !verdict.saw_live
        && !verdict.cut_short
        && verdict.dead.iter().all(|path| path.starts_with(lane_path))
}

/// Sizes its own whole-scan `BuildDirScanBudget` off the full candidate
/// list up front (`BuildDirScanBudget::for_candidates`), then decides each
/// one off a fresh `PerDirFileBudget` — so a big hash dir sorted early
/// (the main checkout, another live lane) can only ever exhaust its own
/// per-dir cap, never the whole scan's, and the candidates after it are
/// still reached (card `20261116-b-891676`).
fn find_build_dir_evidence(
    shared_root: &Path,
    marker: &Path,
    exclude: Option<&Path>,
) -> BuildDirLookup {
    find_build_dir_evidence_within(
        shared_root,
        marker,
        exclude,
        BuildDirScanBudget::for_candidates,
    )
}

/// `find_build_dir_evidence` with its whole-scan budget sized by
/// `scan_budget` rather than always `BuildDirScanBudget::for_candidates`,
/// so a test can starve the scan outright.
fn find_build_dir_evidence_within(
    shared_root: &Path,
    marker: &Path,
    exclude: Option<&Path>,
    scan_budget: fn(usize) -> BuildDirScanBudget,
) -> BuildDirLookup {
    let cargo_home = cargo_cache_home();
    let candidates = hash_dir_candidates(shared_root);
    let total = candidates.len();
    let mut budget = scan_budget(total);
    let mut unread = 0;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if budget.exhausted() {
            return BuildDirLookup::Inconclusive {
                unread: unread + total - index,
            };
        }
        let mut per_dir = PerDirFileBudget::new();
        match dir_names_marker(
            &candidate,
            marker,
            exclude,
            cargo_home.as_deref(),
            shared_root,
            &mut budget,
            &mut per_dir,
        ) {
            MarkerScan::Found => return BuildDirLookup::Found(candidate),
            MarkerScan::CutShort => unread += 1,
            MarkerScan::NotFound => {}
        }
    }
    if unread > 0 {
        BuildDirLookup::Inconclusive { unread }
    } else {
        BuildDirLookup::NotFound
    }
}

/// What one `find_build_dir_evidence` scan concluded. A miss is split in
/// two so `scoped_lane_build_dir` can say which one kept a lane's
/// build-dir (card `20261116-b-891676`): on 0.1.9, a budget-starved scan
/// looked exactly like "no build-dir here" and `--prune` said nothing.
enum BuildDirLookup {
    Found(PathBuf),
    /// Every candidate was read in full and none names the workspace.
    NotFound,
    /// None matched, but `unread` candidates ran out of scan budget before
    /// being read in full, so one of them may still be the workspace's.
    Inconclusive {
        unread: usize,
    },
}

impl BuildDirLookup {
    fn found(self) -> Option<PathBuf> {
        match self {
            Self::Found(dir) => Some(dir),
            Self::NotFound | Self::Inconclusive { .. } => None,
        }
    }
}

/// The one full workspace hash directory under `shared_root` (see
/// `hash_dir_candidates` — never a bucket, never a legacy tree) whose own
/// build evidence names `workspace_root` (see `dir_names_marker`) —
/// `None` if no candidate matches, or the scan's own bounded budget runs
/// out first. A caller deciding what to delete still checks the result
/// explicitly against the shared root and the main checkout rather than
/// rely on any invariant of this resolution alone — see
/// `guard_build_dir_candidate`. Exposed for `mv_engine::hostdisk`'s own
/// orphaned-bytes notice, which needs this same per-workspace
/// resolution; `scoped_lane_build_dir` is the version `prune` itself
/// calls, which also applies the delete-safety guards this bare
/// resolution does not.
pub fn find_build_dir(shared_root: &Path, workspace_root: &Path) -> Option<PathBuf> {
    find_build_dir_evidence(shared_root, workspace_root, None).found()
}

/// `project_root`'s own build-dir hash, excluding any match that is also
/// under `project_root/.multiverse/worktrees/`: without that exclusion,
/// every lane path (itself nested under the project root) would also
/// satisfy a bare search for `project_root`, and the main checkout's own
/// hash dir could never be told apart from any one lane's. Exposed for
/// `mv_engine::hostdisk`'s own orphaned-bytes notice, which needs the same
/// project-vs-lane distinction `prune` itself relies on.
pub fn find_main_checkout_build_dir(shared_root: &Path, project_root: &Path) -> Option<PathBuf> {
    let worktrees = project_root.join(".multiverse/worktrees");
    find_build_dir_evidence(shared_root, project_root, Some(&worktrees)).found()
}

/// `find_build_dir`'s own two safety properties, checked explicitly here
/// rather than only relied upon as an invariant of that scan: a build-dir
/// this sure about deleting must prove, on the actual computed paths, that
/// it is neither the shared root itself nor the main checkout's own — the
/// same guard shape the first attempt's `scoped_lane_build_dir` had
/// (`bb794a6` reverted only that attempt's `cargo metadata` call, not
/// these checks).
fn guard_build_dir_candidate(
    candidate: PathBuf,
    shared_root: &Path,
    main_checkout_dir: Option<&Path>,
) -> Option<PathBuf> {
    let relative = candidate.strip_prefix(shared_root).ok()?;
    if relative.as_os_str().is_empty() {
        return None;
    }
    if main_checkout_dir == Some(candidate.as_path()) {
        return None;
    }
    Some(candidate)
}

/// `lane_path`'s own cargo build-dir, when it is provably safe to delete —
/// found read-only under `shared_root` (see `find_build_dir`) and checked
/// against `guard_build_dir_candidate`. `Err` is a `BuildDirReclaim::kept`
/// carrying the one-line reason for anything this cannot prove: no
/// matching hash dir, the scan's own budget running out first, or either
/// guard. `--prune` prints it as the reason it kept the lane's build-dir
/// (card `20261116-b-891676`). A guard refusal keeps the hash dir it
/// refused on `path`, so this same run's orphan sweep passes it over too;
/// a miss resolved none. No reason names the shared root or a hash dir
/// under it (card `20261119-b-087c6c`): that is a machine path under the
/// operator's home, and the `--prune` output it lands in is shared.
///
/// Card `20261119-b-4ec912`: the lane's own `computed_hash_dir` is the
/// first candidate, proven by its own dep-info naming the lane — one dir,
/// read in full under its own fresh budgets (no file cap — see
/// `BuildDirScanBudget::for_complete_reads`), so a shared root too big or
/// a host too loaded for the full scan below no longer leaves the lane's
/// build-dir behind. The scan runs only when that computed dir does not
/// exist (a lane built under another `build-dir`, or reached through
/// another spelling of its path); one that exists but cannot be proven is
/// kept, never traded for some other dir the scan might match. A read of
/// it that ran out of time first says so and resolves no dir (card
/// `20261119-b-a9f95f`), so this run's sweep is not told to skip a dir
/// nobody read in full.
fn scoped_lane_build_dir(
    shared_root: &Path,
    main_checkout_dir: Option<&Path>,
    lane_path: &Path,
) -> std::result::Result<PathBuf, BuildDirReclaim> {
    scoped_lane_build_dir_within(
        shared_root,
        main_checkout_dir,
        lane_path,
        BuildDirScanBudget::for_candidates,
        BuildDirScanBudget::for_complete_reads,
    )
}

/// `scoped_lane_build_dir` with the fallback scan's whole-scan budget
/// sized by `scan_budget` (see `find_build_dir_evidence_within`) and the
/// computed dir's complete read bounded by `complete_read`, so a test can
/// starve either one.
fn scoped_lane_build_dir_within(
    shared_root: &Path,
    main_checkout_dir: Option<&Path>,
    lane_path: &Path,
    scan_budget: fn(usize) -> BuildDirScanBudget,
    complete_read: fn(usize) -> BuildDirScanBudget,
) -> std::result::Result<PathBuf, BuildDirReclaim> {
    let computed = computed_hash_dir(shared_root, lane_path);
    let lookup = match fs::symlink_metadata(&computed) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            find_build_dir_evidence_within(shared_root, lane_path, None, scan_budget)
        }
        Ok(info) if info.is_dir() => {
            let cargo_home = cargo_cache_home();
            match dir_names_marker(
                &computed,
                lane_path,
                None,
                cargo_home.as_deref(),
                shared_root,
                &mut complete_read(1),
                &mut PerDirFileBudget::unbounded(),
            ) {
                MarkerScan::Found => BuildDirLookup::Found(computed),
                MarkerScan::NotFound => {
                    return Err(BuildDirReclaim::kept(
                        "its computed shared-build hash dir does not name this lane in its \
                         dep-info"
                            .into(),
                        Some(computed),
                    ))
                }
                MarkerScan::CutShort => {
                    return Err(BuildDirReclaim::kept(
                        "scan budget ran out before its computed shared-build hash dir was \
                         read in full"
                            .into(),
                        None,
                    ))
                }
            }
        }
        _ => {
            return Err(BuildDirReclaim::kept(
                "its computed shared-build hash dir is not a plain directory".into(),
                Some(computed),
            ))
        }
    };
    let candidate = match lookup {
        BuildDirLookup::Found(candidate) => candidate,
        BuildDirLookup::NotFound => {
            return Err(BuildDirReclaim::kept(
                "no shared-build hash dir names this lane in its dep-info".into(),
                None,
            ))
        }
        BuildDirLookup::Inconclusive { unread } => {
            return Err(BuildDirReclaim::kept(
                format!(
                    "scan budget ran out before {unread} shared-build hash dir(s) were read in full"
                ),
                None,
            ))
        }
    };
    guard_build_dir_candidate(candidate.clone(), shared_root, main_checkout_dir).ok_or_else(|| {
        BuildDirReclaim::kept(
            "its hash dir is the shared root itself or the main checkout's own build-dir".into(),
            Some(candidate),
        )
    })
}

/// The bytes freed, or the reason a lane's build-dir stayed: at most one
/// of the two is set, and neither when there was nothing to reclaim (no
/// per-lane namespacing, a lane not yet eligible, or an empty dir).
#[derive(Debug, Default)]
struct BuildDirReclaim {
    freed: Option<u64>,
    /// See `scoped_lane_build_dir`'s own `Err`, plus a failed delete.
    kept: Option<String>,
    /// The resolved hash dir `freed` or `kept` describes; `None` when none
    /// was resolved or it measured empty.
    path: Option<PathBuf>,
}

impl BuildDirReclaim {
    fn kept(reason: String, path: Option<PathBuf>) -> Self {
        Self {
            freed: None,
            kept: Some(reason),
            path,
        }
    }

    /// Measures `dir` and, unless `dry_run`, deletes it — confined to
    /// `root` (see `deep_prune_remove`). A failed delete is reported as
    /// kept rather than as freed bytes that are still on disk.
    fn free(root: &Path, dir: &Path, dry_run: bool) -> Self {
        let size = deep_prune_size(dir);
        if size == 0 {
            return Self::default();
        }
        if !dry_run {
            if let Err(error) = deep_prune_remove(dir, root) {
                return Self::kept(
                    format!("removing its hash dir failed: {error}"),
                    Some(dir.to_path_buf()),
                );
            }
        }
        Self {
            freed: Some(size),
            kept: None,
            path: Some(dir.to_path_buf()),
        }
    }

    /// `lane_path`'s own build-dir under `build_scope` (see
    /// `Worktrees::prune_with_ignored`), resolved and freed in one step.
    fn resolve_and_free(
        build_scope: Option<&(PathBuf, Option<PathBuf>)>,
        lane_path: &Path,
        dry_run: bool,
    ) -> Self {
        let Some((root, main_dir)) = build_scope else {
            return Self::default();
        };
        match scoped_lane_build_dir(root, main_dir.as_deref(), lane_path) {
            Ok(dir) => Self::free(root, &dir, dry_run),
            Err(kept) => kept,
        }
    }
}

/// `path` sits under `cargo_home`'s own registry or git-checkout trees —
/// where cargo extracts a downloaded dependency's source, never a
/// workspace a project builds from itself (card `20261115-b-673528`). A
/// build-evidence line naming one of these must never count as either live
/// or dead workspace evidence: unlike a workspace, a registry/git-checkout
/// source persists long after the workspace that depended on it is gone,
/// so treating it as proof of a still-existing workspace would read almost
/// every hash dir as permanently in use.
fn is_cargo_cache_source(path: &Path, cargo_home: Option<&Path>) -> bool {
    let Some(home) = cargo_home else {
        return false;
    };
    path.starts_with(home.join("registry")) || path.starts_with(home.join("git"))
}

/// `path` sits under the shared build root itself — cargo's own generated
/// output (an `OUT_DIR` a build script wrote into, say) rather than a
/// workspace source file. A build-evidence line naming one of these must
/// never count as workspace evidence either way: the shared root's own
/// hash dirs are exactly what this scan is walking, so a path under it is
/// always there for as long as that hash dir itself survives the scan —
/// treating it as proof of a "still-existing workspace" would make every
/// hash dir read as permanently live, including ones with no live
/// workspace evidence at all (card `20261115-b-673528`, fix round).
fn is_shared_build_root_path(path: &Path, shared_root: Option<&Path>) -> bool {
    shared_root.is_some_and(|root| path.starts_with(root))
}

/// Whether `file_path` — a source file a build-evidence line names — still
/// exists on disk (card `20261115-b-673528`, fix round). The prior rule
/// instead asked whether any *ancestor* of `file_path` currently held a
/// `Cargo.toml`, which is useless for this fleet's real layout: a lane
/// lives at `<project>/.multiverse/worktrees/<task>/`, nested inside the
/// main checkout whose own root holds `Cargo.toml` — so a deleted lane's
/// paths always had that same ancestor to find, and its hash dir was never
/// reclaimed. Checking the named path itself tells the two cases apart: a
/// deleted lane's own file is gone along with the rest of the lane, while
/// the main checkout's files remain. It also keeps a live workspace live
/// when just one of its source files is later renamed or deleted — that
/// one line reads as dead, but `scan_orphan_evidence` stops at the first
/// *other* line still naming a file that exists, so the hash dir as a
/// whole is only read as orphaned once every line it has evidence for
/// points at a vanished path.
fn names_an_existing_workspace(file_path: &Path) -> bool {
    file_path.exists()
}

/// Accumulates `hash_dir_is_orphaned`'s own verdict across a hash dir's
/// evidence files: `saw_live` once any line proves a still-existing
/// workspace root (at which point the scan stops — nothing further can
/// change the outcome), `saw_dead` once any line names one that no longer
/// exists, and `cut_short` if the scan's own bounded budget ran out before
/// every evidence file was read. `cut_short` must gate the final verdict
/// separately from `saw_dead`: a dead line seen before the budget ran out
/// says nothing about the evidence this scan never reached. `dead` and
/// `relative` keep every vanished absolute path and every relative source
/// seen, for `workspace_root_may_survive`'s own final check.
#[derive(Default)]
struct OrphanVerdict {
    saw_live: bool,
    saw_dead: bool,
    cut_short: bool,
    dead: BTreeSet<PathBuf>,
    relative: BTreeSet<PathBuf>,
}

/// Whether `dir` could still be the root of the workspace a hash dir's
/// relative sources belong to: it is a directory holding a `Cargo.toml`,
/// or one of `relative` resolves under it. Anything this cannot read
/// counts as "could be" — the caller deletes on `false` alone.
fn may_be_workspace_root(dir: &Path, relative: &BTreeSet<PathBuf>) -> bool {
    match fs::metadata(dir) {
        Ok(info) if info.is_dir() => {}
        Ok(_) => return false,
        Err(error) => {
            return !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
            )
        }
    }
    std::iter::once(Path::new("Cargo.toml"))
        .chain(relative.iter().map(PathBuf::as_path))
        .any(|entry| match dir.join(entry).try_exists() {
            Ok(found) => found,
            Err(error) => error.kind() != io::ErrorKind::NotADirectory,
        })
}

/// Whether a hash dir whose every absolute workspace path has vanished
/// (`verdict.dead`) may still belong to a live workspace through its
/// relative sources (`verdict.relative`) — card `20261119-b-087c6c`. One
/// hash dir is one workspace (`{workspace-path-hash}`), and cargo names
/// that workspace's own member sources relative to its root
/// (`crates/mv-core/src/lib.rs`), keeping absolute paths only for inputs
/// like an `include_str!` file. A vanished absolute path therefore says
/// only that *that file* is gone, not the workspace: the root is some
/// ancestor of it, and which one the evidence never says. So every
/// still-existing ancestor of every vanished path is a candidate root, and
/// the workspace counts as gone only when no candidate holds a
/// `Cargo.toml` or any of the relative sources (see
/// `may_be_workspace_root`). A hash dir with no relative sources at all
/// keeps the prior rule unchanged: its absolute paths are its sources.
fn workspace_root_may_survive(verdict: &OrphanVerdict) -> bool {
    if verdict.relative.is_empty() {
        return false;
    }
    let mut checked: HashSet<&Path> = HashSet::new();
    for path in &verdict.dead {
        for ancestor in path.ancestors().skip(1) {
            if !checked.insert(ancestor) {
                break;
            }
            if may_be_workspace_root(ancestor, &verdict.relative) {
                return true;
            }
        }
    }
    false
}

/// Walks `dir` read-only, depth-first, for every build-evidence file (see
/// `is_build_evidence`), classifying each absolute path its own workspace
/// evidence names (see `workspace_evidence`) against
/// `names_an_existing_workspace`, and keeping every relative one for
/// `workspace_root_may_survive`. Stops
/// the instant `verdict.saw_live` is set — no further evidence can change a
/// hash dir this proves still live — and marks `verdict.cut_short` rather
/// than guessing once either the whole-scan `budget`'s deadline or `dir`'s
/// own `per_dir` file cap runs out, the same "exhaustion is never found"
/// rule `dir_names_marker` already follows for its own budgets.
fn scan_orphan_evidence(
    dir: &Path,
    cargo_home: Option<&Path>,
    shared_root: Option<&Path>,
    budget: &mut BuildDirScanBudget,
    per_dir: &mut PerDirFileBudget,
    verdict: &mut OrphanVerdict,
) {
    if verdict.saw_live {
        return;
    }
    if budget.exhausted() || per_dir.exhausted() {
        verdict.cut_short = true;
        return;
    }
    let Ok(entries) = deep_prune_entries(dir) else {
        return;
    };
    for entry in entries {
        if verdict.saw_live {
            return;
        }
        if budget.exhausted() || per_dir.exhausted() {
            verdict.cut_short = true;
            return;
        }
        let Ok(file_type) = fs::symlink_metadata(&entry).map(|info| info.file_type()) else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            scan_orphan_evidence(&entry, cargo_home, shared_root, budget, per_dir, verdict);
            continue;
        }
        if !file_type.is_file() || !is_build_evidence(&entry) {
            continue;
        }
        per_dir.charge();
        let Ok(content) = fs::read_to_string(&entry) else {
            continue;
        };
        let (absolute, relative) = workspace_evidence(&content, cargo_home, shared_root);
        verdict.relative.extend(relative);
        for candidate in absolute {
            if names_an_existing_workspace(&candidate) {
                verdict.saw_live = true;
                return;
            }
            verdict.saw_dead = true;
            verdict.dead.insert(candidate);
        }
    }
}

/// Whether `candidate` (a full workspace hash dir — see
/// `hash_dir_candidates`) is safe to reclaim: its own build evidence names
/// at least one workspace root, and every one it names has vanished (card
/// `20261115-b-673528`). `false` the moment any line names a still-existing
/// workspace root, there is no readable (non-registry) evidence at all,
/// the scan was cut short before finishing, or its relative sources leave
/// a still-existing directory that could be the workspace's root (see
/// `workspace_root_may_survive`) — a scan this inconclusive must never
/// license a delete. `per_dir` bounds this one dir's own evidence
/// read, independent of every other candidate `budget`'s whole-scan
/// deadline also spans (card `20261116-b-891676`).
fn hash_dir_is_orphaned(
    dir: &Path,
    cargo_home: Option<&Path>,
    shared_root: Option<&Path>,
    budget: &mut BuildDirScanBudget,
    per_dir: &mut PerDirFileBudget,
) -> bool {
    let mut verdict = OrphanVerdict::default();
    scan_orphan_evidence(dir, cargo_home, shared_root, budget, per_dir, &mut verdict);
    verdict.saw_dead
        && !verdict.saw_live
        && !verdict.cut_short
        && !workspace_root_may_survive(&verdict)
}

/// Measures `dir`, a hash dir the orphan sweep already proved orphaned,
/// and, unless `dry_run`, deletes it — confined to `shared_root` (see
/// `deep_prune_remove`). `None` when it measured empty. A failed delete
/// is reported as kept with its reason, never as freed bytes that are
/// still on disk (card `20261119-b-a9f95f`) — the same rule
/// `BuildDirReclaim::free` follows for a lane's own build-dir.
fn free_orphaned_build_dir(
    shared_root: &Path,
    dir: PathBuf,
    dry_run: bool,
) -> Option<OrphanedBuildDir> {
    let bytes = deep_prune_size(&dir);
    if bytes == 0 {
        return None;
    }
    let kept = if dry_run {
        None
    } else {
        deep_prune_remove(&dir, shared_root)
            .err()
            .map(|error| format!("removing its hash dir failed: {error}"))
    };
    Some(OrphanedBuildDir {
        path: dir,
        bytes,
        kept,
    })
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

    /// Builds a [`Worktrees`] scoped to `project`, refusing anything that
    /// is not itself a git repository root — a lane's own checkout, say,
    /// or an arbitrary subdirectory — since every path this module
    /// resolves (`.multiverse/worktrees/<task>`, refs, the worktree lock)
    /// is anchored at the top level, never at whatever directory a caller
    /// happened to pass in.
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

    /// Creates a brand-new lane for `task`, branching from `base` — `git
    /// worktree add -b <branch> <path> <base>` — and records the branch
    /// point at `refs/multiverse/base/<task>` so later diffing/ancestry
    /// checks ([`lane_touched_files`], `has_commits`, …) always know this
    /// lane's own cut point even if `base`'s own tip moves afterward.
    /// Refuses outright if the owner branch already exists (use `adopt` or
    /// `restore` instead) or if a case-folded collision would make this
    /// lane's path ambiguous on a case-insensitive filesystem.
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

    /// Re-dispatches into `task`'s existing, already-registered lane
    /// rather than creating a fresh one — the owner branch and its
    /// checkout must already both exist (see `registered`). Refuses on a
    /// live or unknown occupant (`observer.check`), on an occupant that
    /// was actually dispatched into but never settled
    /// (`terminal_statuses`), and on a lane some other live process still
    /// has as its cwd; a pristine lane fresh out of `create` — never
    /// dispatched into at all — is freely adoptable.
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
        self.terminal_statuses(&lane, false, true, false, &self.task_records())?;
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

    /// Every task-lifecycle record in this project's routing log, decoded
    /// once. [`Self::terminal_statuses`] and [`Self::retirement_authorized`]
    /// (via [`has_terminal_outcome_in`]/[`recorded_task_outcomes_in`]) both
    /// read this same ledger; a caller that walks every lane in one pass
    /// (`prune_with_ignored`) fetches and validates it here exactly once for
    /// the whole call, not once per lane — see `prune_with_ignored`'s own
    /// `build_scope`, computed the same way for the same reason.
    fn task_records(&self) -> Vec<crate::index::IndexedRecord> {
        crate::index::records(
            &self.project.join(".multiverse/routing.log"),
            crate::index::Selector::Tasks,
        )
        .unwrap_or_default()
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
    ///
    /// `lane_task_abandoned`: when true, *only* the lane's own task
    /// (`lane.task`, never another occupant `occupant_tasks` turns up) is
    /// also skipped regardless of directory existence — [`Self::
    /// abandoned_before_dispatch`]'s own conclusion, already proven for
    /// `lane.task` specifically (age, no commits, and either no directory
    /// or, Scout 67a619 finding 2, a directory with no `event=dispatch`
    /// record for it at all — a brief written just before the host died,
    /// with the dispatch append that follows it in `runner.rs` never
    /// landing). `prune_with_ignored` is the only caller that ever passes
    /// `true` here, with the exact value `abandoned_before_dispatch`
    /// already computed for this lane; `adopt` always passes `false` — it
    /// has no age guard of its own, so a brief that exists at all, dispatch
    /// record or not, still means "touched", not "reusable".
    ///
    /// `task_records` is [`Self::task_records`]'s output, passed in rather
    /// than fetched here so a caller checking many lanes (or this same lane
    /// more than once) shares one scan of the ledger instead of paying for
    /// one per call — and is also `lane_task_abandoned`'s own evidence, via
    /// [`has_terminal_outcome_in`]'s sibling [`has_dispatch_record_in`].
    fn terminal_statuses(
        &self,
        lane: &Lane,
        force: bool,
        skip_never_dispatched: bool,
        lane_task_abandoned: bool,
        task_records: &[crate::index::IndexedRecord],
    ) -> Result<()> {
        for task in occupant_tasks(&self.project, lane)? {
            if skip_never_dispatched && !self.task_directory_exists(lane, &task)? {
                continue;
            }
            if lane_task_abandoned && task == lane.task {
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
                if force || has_terminal_outcome_in(task_records, &task) {
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

    /// Whether `task`'s lane is exactly the shape rank-7 (card
    /// 20261117-b-437eeb) names: `create` returned a checkout and a branch,
    /// but no brief was ever written for it (`task`'s own copy of
    /// `.multiverse/tasks/<task>` — the project's, or `lane_path`'s when the
    /// checkout still exists, matching [`Self::task_directory_exists`]) —
    /// **or** the brief *was* written but the host died before the
    /// routing-log `event=dispatch` append that follows it in
    /// `runner.rs::start_dispatch` (Scout 67a619 finding 2): a task
    /// directory with no matching `event=dispatch` record anywhere in
    /// `task_records` reads the same as no directory at all, since that is
    /// the one window where the directory exists but the dispatch itself
    /// never happened — and no commit was ever made past its recorded cut
    /// point ([`has_commits`]). `lane_path` is `None` once the checkout is
    /// already gone (`prune_orphans`'s own use, on a branch `prune` already
    /// removed the lane directory for) — only the project's copy can still
    /// be checked then, which is exactly what a never-dispatched task
    /// leaves nothing in either way. `task_records` is the caller's own
    /// routing-log fetch ([`Worktrees::task_records`]), shared across every
    /// lane/orphan one call checks rather than re-scanned per task.
    ///
    /// Age comes from `task`'s own branch reflog — the one timestamp
    /// `create`'s `git worktree add -b` always leaves, readable whether or
    /// not the checkout is still on disk — compared against
    /// [`WORKTREE_STATE_RETENTION`], the same age this module already
    /// waits out before treating other abandoned state as safe to reclaim
    /// (`prune_quarantine`'s `RetentionExpired`): this is the same
    /// question — "has enough time passed that this can no longer be the
    /// in-flight window between `create` returning and `write_brief`/
    /// `record_dispatch` finishing" — not a new kind of guard needing its
    /// own number. A branch with no reflog entry at all (expired, or
    /// `core.logAllRefUpdates` off) answers `false`: age can't be proven,
    /// so every existing guard — `terminal_statuses`, `has_terminal_outcome`
    /// — stays in force.
    fn abandoned_before_dispatch(
        &self,
        task: &str,
        lane_path: Option<&Path>,
        task_records: &[crate::index::IndexedRecord],
    ) -> Result<bool> {
        let relative = Path::new(".multiverse/tasks").join(task);
        let directory_exists = guarded_path(&self.project, &relative, true)?.try_exists()?
            || match lane_path {
                Some(lane_path) => guarded_path(lane_path, &relative, true)?.try_exists()?,
                None => false,
            };
        if directory_exists && has_dispatch_record_in(task_records, task) {
            return Ok(false);
        }
        if has_commits(&self.project, task)? {
            return Ok(false);
        }
        let branch_ref = format!("refs/heads/{}", branch_name(task)?);
        // `%gd`, not `%cd`: the reflog's own date (`--date=unix` applies to
        // the `@{...}` it prints), independent of the commit's own
        // committer date — `tip == base` means that commit's date is
        // whenever `base` was originally made, which can predate this
        // branch by months and says nothing about this lane's own age.
        let logged = git(
            &self.project,
            &[
                "log",
                "-g",
                "--format=%gd",
                "--date=unix",
                "-1",
                &branch_ref,
            ],
            None,
        )?;
        let Some(created) = logged
            .trim()
            .rsplit_once("@{")
            .and_then(|(_, rest)| rest.strip_suffix('}'))
            .and_then(|epoch| epoch.parse::<u64>().ok())
        else {
            return Ok(false);
        };
        let created = UNIX_EPOCH + Duration::from_secs(created);
        Ok(created < SystemTime::now() - WORKTREE_STATE_RETENTION)
    }

    /// Rescue never overwrites: different bytes, symlinks, and special files
    /// refuse removal. The source remains available after rescue. `dry_run`
    /// returns the same artifact list this would copy, without copying a
    /// byte, creating a directory, or taking the worktree lock — the lock
    /// file is itself a write, so acquiring it here would make `--rescue
    /// --dry-run` mutate the very state it claims only to preview.
    pub fn rescue(&self, task: &str, dry_run: bool) -> Result<Vec<PathBuf>> {
        let lane = self.lane(task, false)?;
        self.registered(&lane)?;
        if dry_run {
            return self.rescue_state_preview(&lane.path.join(".multiverse"));
        }
        let _lock = lock(&self.project)?;
        self.rescue_state(&lane.path.join(".multiverse"))
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

    /// [`rescue_state`]'s own preview: the same artifact list and the same
    /// conflict refusals, via [`would_copy_without_overwrite`], but no
    /// directory is created and no byte of `destination` is written.
    fn rescue_state_preview(&self, state: &Path) -> Result<Vec<PathBuf>> {
        match fs::symlink_metadata(state) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                would_copy_without_overwrite(state, &self.project.join(".multiverse"))
            }
            Ok(_) => Err(WorktreeError::Refused("unsafe lane state".into())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e.into()),
        }
    }

    /// `multiverse worktrees <path> --prune`'s own entry point: sweeps
    /// every lane under this project, reclaiming or archiving each one
    /// that is settled and clean, keeping the rest. Equivalent to
    /// [`Self::prune_with_ignored`] with `allow_ignored: false` — a
    /// non-disposable ignored file still blocks removal unless a caller
    /// explicitly opts into discarding it via that method instead — and
    /// with `free_caches: true`, so a qualifying lane's own `target/` and
    /// build-dir are still freed exactly as before card
    /// `20261119-b-73759c` gave the CLI a way to withhold that.
    pub fn prune(
        &self,
        observer: &mut impl LaneObserver,
        force: bool,
        dry_run: bool,
    ) -> Result<Vec<PruneOutcome>> {
        self.prune_with_ignored(observer, force, dry_run, false, true)
    }

    /// Explicit confirmation permits discarding ignored data. It never bypasses
    /// ancestry/archive authority, tracked changes, occupant or process guards.
    /// The confirmation is carried through the final removal recheck too.
    /// A lane this keeps still loses its own lane-root `target/` when the
    /// lane qualifies — see `Self::kept_target_bytes`.
    ///
    /// `free_caches` false (card `20261119-b-73759c`: `worktrees --prune`
    /// without `--yes`) still removes or archives each eligible lane, but
    /// only measures — never deletes — that lane-root `target/` and every
    /// lane's own shared-cache build-dir, reporting the same bytes a
    /// `dry_run` would. `dry_run` true never deletes either, regardless.
    pub fn prune_with_ignored(
        &self,
        observer: &mut impl LaneObserver,
        force: bool,
        dry_run: bool,
        allow_ignored: bool,
        free_caches: bool,
    ) -> Result<Vec<PruneOutcome>> {
        // The one switch every cache free below reads: a real removal
        // never implies permission to delete a build cache on its own.
        let measure_only = dry_run || !free_caches;
        // Read-only preview does not even create a lock file.
        let _lock = if dry_run {
            None
        } else {
            Some(lock(&self.project)?)
        };
        // Resolved once per call, not once per lane: every lane this pass
        // touches shares the same shared build root and the same main
        // checkout's own hash dir, each a read-only scan with its own
        // bounded budget. `None` means this workspace has no per-lane
        // build-dir namespacing (or none resolvable), so no lane below
        // gets one either — see `scoped_lane_build_dir`.
        let build_scope = shared_build_root(&self.project).map(|root| {
            let main_dir = find_main_checkout_build_dir(&root, &self.project);
            (root, main_dir)
        });
        // Likewise resolved once per call, not once per lane: every lane
        // below that needs a terminal-status or archive-authority decision
        // shares this one ledger scan (`task_records`) and this one
        // candidate listing (`retirement_candidates`) instead of each lane
        // re-scanning the whole routing log and re-walking every task
        // directory on its own.
        let task_records = self.task_records();
        let recorded_outcomes = recorded_task_outcomes_in(&task_records);
        let mut retirement_candidates = self.retirement_candidates()?;
        let mut outcomes = Vec::new();
        for lane in self.lanes()? {
            // Set the instant `observer.check` itself passes, before any
            // later step in this same closure can fail — recording that the
            // one Herdr probe `check` is allowed to make already found no
            // blocking pane, so a lane kept for some later reason (dirty,
            // unmerged, …) never needs a second `agent list` call just to
            // ask the same question `kept_target_bytes` cares about.
            let mut pane_clear = false;
            // "Created but never dispatched into" (rank-7, card
            // 20261117-b-437eeb): `occupant_tasks` always includes the
            // lane's own task name, so a lane `create` made but no brief
            // ever followed would otherwise need `force` forever — see
            // `abandoned_before_dispatch`'s own doc comment. Computed inside
            // the closure, after `observer.check`, not before it: a lane
            // whose branch does not even exist yet (an unregistered
            // directory `adopt` already refuses) must still fail through
            // this same closure's own error-to-`Kept` handling, not abort
            // `prune` outright on `?`.
            let mut skip_never_dispatched = false;
            let check = (|| {
                observer.check(&lane, force)?;
                pane_clear = true;
                // Fail safe: if abandonment cannot be proven (a git error,
                // e.g. no HEAD yet), keep every existing guard in force.
                skip_never_dispatched = self
                    .abandoned_before_dispatch(&lane.task, Some(&lane.path), &task_records)
                    .unwrap_or(false);
                self.terminal_statuses(
                    &lane,
                    force,
                    skip_never_dispatched,
                    skip_never_dispatched,
                    &task_records,
                )?;
                let tip = self.tip(&lane.task)?;
                let archiving = !ancestor(&self.project, &tip)?;
                if archiving
                    && !self.patch_landed(&tip)?
                    && !self.retirement_authorized_with(
                        &lane.task,
                        &tip,
                        &recorded_outcomes,
                        &retirement_candidates,
                    )?
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
                    let target_freed =
                        self.kept_target_bytes(&lane, pane_clear, measure_only, &task_records);
                    let build_dir = self.kept_build_dir_bytes(
                        &lane,
                        pane_clear,
                        measure_only,
                        build_scope.as_ref(),
                        &task_records,
                    );
                    outcomes.push(PruneOutcome::Kept {
                        task: lane.task,
                        reason: error.to_string(),
                        target_freed,
                        build_dir_freed: build_dir.freed,
                        build_dir_kept: build_dir.kept,
                        build_dir: build_dir.path,
                    });
                    continue;
                }
                Ok(archiving) => archiving,
            };
            if dry_run {
                let build_dir =
                    BuildDirReclaim::resolve_and_free(build_scope.as_ref(), &lane.path, true);
                outcomes.push(if archiving {
                    PruneOutcome::WouldArchive {
                        task: lane.task,
                        build_dir_freed: build_dir.freed,
                        build_dir_kept: build_dir.kept,
                        build_dir: build_dir.path,
                    }
                } else {
                    let branch_refusal = self
                        .terminal_statuses(
                            &lane,
                            false,
                            skip_never_dispatched,
                            skip_never_dispatched,
                            &task_records,
                        )
                        .err()
                        .map(|_| format!("no recorded outcome for {}", lane.task));
                    PruneOutcome::WouldRemove {
                        task: lane.task,
                        branch_refusal,
                        build_dir_freed: build_dir.freed,
                        build_dir_kept: build_dir.kept,
                        build_dir: build_dir.path,
                    }
                });
            } else {
                observer.check(&lane, force)?;
                self.terminal_statuses(
                    &lane,
                    force,
                    skip_never_dispatched,
                    skip_never_dispatched,
                    &task_records,
                )?;
                observer.record_prune(&lane, &task_records)?;
                // Rescue *before* removing, never after — see `remove_lane`'s
                // own doc comment. This is the one call whose copied-path
                // list a caller can report; `remove_lane`'s own internal
                // `rescue_state` (on the already-quarantined copy) exists
                // only to catch a late write in the swap race, and is never
                // itself surfaced as "rescued" — Python's own CLI reports
                // only its single `rescue()` call the same way.
                let mut rescued = self.rescue_state(&lane.path.join(".multiverse"))?;
                rescued.sort();
                let build_dir = build_scope.as_ref().map(|(root, main_dir)| {
                    (
                        root.clone(),
                        scoped_lane_build_dir(root, main_dir.as_deref(), &lane.path),
                    )
                });
                let retained_state = self.remove_lane(&lane, archiving, allow_ignored)?;
                // That lane's task dirs now live in the main checkout (the
                // rescue above), not in the lane just removed, so the
                // shared listing would point a later lane's authority check
                // at a path that is gone. Re-list only after a real removal.
                retirement_candidates = self.retirement_candidates()?;
                let build_dir = match build_dir {
                    None => BuildDirReclaim::default(),
                    Some((root, Ok(dir))) => BuildDirReclaim::free(&root, &dir, measure_only),
                    Some((_, Err(kept))) => kept,
                };
                outcomes.push(if archiving {
                    PruneOutcome::Archived {
                        tip: self.archive_locked(&lane.task)?,
                        task: lane.task,
                        retained_state,
                        rescued,
                        build_dir_freed: build_dir.freed,
                        build_dir_kept: build_dir.kept,
                        build_dir: build_dir.path,
                    }
                } else {
                    let branch_refusal = if has_terminal_outcome_in(&task_records, &lane.task)
                        || skip_never_dispatched
                    {
                        self.reclaim_locked(&lane.task).err().map(|e| e.to_string())
                    } else {
                        Some(format!("no recorded outcome for {}", lane.task))
                    };
                    PruneOutcome::Removed {
                        task: lane.task,
                        branch_refusal,
                        retained_state,
                        rescued,
                        build_dir_freed: build_dir.freed,
                        build_dir_kept: build_dir.kept,
                        build_dir: build_dir.path,
                    }
                });
            }
        }
        Ok(outcomes)
    }

    /// For a lane this `prune` is keeping, the lane-root `target/` it no
    /// longer needs — freed outright, or under `dry_run` only measured —
    /// once every occupant has a real terminal status file and nothing is
    /// still live in the lane. `None` leaves `target/` untouched.
    ///
    /// `pane_clear` is whatever the one `observer.check` call
    /// `prune_with_ignored` already made for this lane found: `false` means
    /// that call never even ran (`lanes()` for a different lane, or this
    /// lane's own pane is still genuinely live) or refused under the
    /// caller's actual `force`, either of which already says "running
    /// lane" on its own. A second `agent list` probe here would answer the
    /// same question at the cost of doubling every Herdr call `--prune`
    /// makes, which the fake-Herdr conformance fixtures count exactly.
    ///
    /// `has_process_cwd` is checked again regardless: `clean`'s own call to
    /// it never runs once an earlier check in that function (uncommitted
    /// changes, most often) has already returned, so a reason string of
    /// "uncommitted changes" says nothing about whether a process still has
    /// its cwd here. A task dispatched `--into` this lane and still
    /// unfinished is exactly the case `terminal_statuses` already refuses
    /// on, since `occupant_tasks` includes every task the project's routing
    /// log ever dispatched onto this lane's branch, not just the lane's
    /// own.
    ///
    /// Deliberately checks `terminal_statuses` with `force: false`
    /// regardless of `prune`'s own `force` argument: that argument confirms
    /// *removing* an otherwise-removable lane, not relaxing the evidence a
    /// kept lane's build cache gets judged by.
    fn kept_target_bytes(
        &self,
        lane: &Lane,
        pane_clear: bool,
        dry_run: bool,
        task_records: &[crate::index::IndexedRecord],
    ) -> Option<u64> {
        if !pane_clear {
            return None;
        }
        if self
            .terminal_statuses(lane, false, false, false, task_records)
            .is_err()
        {
            return None;
        }
        if has_process_cwd(&lane.path).unwrap_or(true) {
            return None;
        }
        let target = lane.path.join("target");
        let info = fs::symlink_metadata(&target).ok()?;
        if !info.is_dir() {
            return None;
        }
        let size = deep_prune_size(&target);
        if !dry_run {
            let _ = deep_prune_remove(&target, &lane.path);
        }
        Some(size)
    }

    /// `kept_target_bytes`'s own twin for `lane`'s per-worktree cargo
    /// build-dir (card `20261115-b-47f043`): the same `pane_clear`/
    /// terminal-status/no-live-process eligibility, but the disk this
    /// frees sits under the shared build cache, not inside the lane
    /// itself — see `scoped_lane_build_dir` for why it is safe to delete
    /// and `deep_prune_remove`'s own `root` parameter for why `build_scope`
    /// must name the shared root, not `lane.path`, as the confinement
    /// boundary. A lane not yet eligible gets neither bytes nor a kept
    /// reason: the lane itself is kept, and its own reason already says why.
    fn kept_build_dir_bytes(
        &self,
        lane: &Lane,
        pane_clear: bool,
        dry_run: bool,
        build_scope: Option<&(PathBuf, Option<PathBuf>)>,
        task_records: &[crate::index::IndexedRecord],
    ) -> BuildDirReclaim {
        if !pane_clear {
            return BuildDirReclaim::default();
        }
        if self
            .terminal_statuses(lane, false, false, false, task_records)
            .is_err()
        {
            return BuildDirReclaim::default();
        }
        if has_process_cwd(&lane.path).unwrap_or(true) {
            return BuildDirReclaim::default();
        }
        BuildDirReclaim::resolve_and_free(build_scope, &lane.path, dry_run)
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
        // Never delete quarantine here: an already open descriptor can still
        // write here after Git returns. Callers can surface this path for
        // later rescue; `prune_quarantine` is what eventually drops it, once
        // it is provably redundant or old enough that no such descriptor
        // could still be live.
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
        // Fetched once for the whole call, not once per task: only
        // `abandoned_before_dispatch` below needs the ledger itself, and it
        // checks every orphan candidate this loop walks.
        let task_records = self.task_records();
        let mut outcomes = Vec::new();
        for task in self.orphan_branches()? {
            if !has_terminal_outcome(&self.project, &task)
                && !self
                    .abandoned_before_dispatch(&task, None, &task_records)
                    .unwrap_or(false)
            {
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

    /// Sweeps `.multiverse/worktree-state/`'s own quarantine copies — each
    /// one `remove_lane` left behind as the one case it never deletes
    /// outright (card `20261106-b-3d2774`; see `remove_lane`'s own "never
    /// delete quarantine" comment). A copy is dropped once
    /// `quarantine_is_redundant` proves every file it holds is already
    /// saved, byte-identical, under this project's own live `.multiverse/`
    /// — regardless of the copy's age — or, failing that proof, once
    /// `WORKTREE_STATE_RETENTION` has passed since its own newest write
    /// *and* a rescue of it — the same `check_copy`/`copy_without_overwrite`
    /// a direct `rescue()` call would run — does not conflict with anything
    /// already there. That second clause is what keeps a crash inside
    /// `remove_lane`, between its own rename into quarantine and the
    /// redirect symlink right after it, from ever losing the copy: nothing
    /// had rescued it before the crash, so it is not yet redundant, but it
    /// is still rescued here, before retention age could otherwise excuse
    /// deleting the only copy of it. A copy whose rescue would conflict — a
    /// genuine byte mismatch against something already rescued under the
    /// same name — is kept rather than destroyed; task state with no
    /// rescued copy is never deleted. Neither guard fires early: a copy
    /// that is neither provably redundant nor past retention is always
    /// kept. `dry_run` reports what each copy would become without
    /// removing, rescuing, or writing anything, or taking the worktree
    /// lock, matching `prune_orphans`'s own preview behavior; a real run
    /// takes the lock once for the whole sweep.
    pub fn prune_quarantine(&self, dry_run: bool) -> Result<Vec<QuarantineOutcome>> {
        let _lock = if dry_run {
            None
        } else {
            Some(lock(&self.project)?)
        };
        let root = guarded_path(&self.project, Path::new(".multiverse/worktree-state"), true)?;
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut copies = entries
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<io::Result<Vec<_>>>()?;
        copies.sort();
        let project_state = self.project.join(".multiverse");
        let cutoff = SystemTime::now() - WORKTREE_STATE_RETENTION;
        let mut outcomes = Vec::new();
        for path in copies {
            let is_copy = fs::symlink_metadata(&path)
                .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
            if !is_copy {
                continue;
            }
            let name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let bytes = deep_prune_size(&path);
            let reason = if quarantine_is_redundant(&path, &project_state)? {
                Some(QuarantineReason::Redundant)
            } else if deep_prune_newest_mtime(&path).is_some_and(|mtime| mtime < cutoff) {
                // Not provably redundant, but old enough to otherwise drop:
                // rescue it first, so what this copy alone holds — whether
                // from a late write or from a `remove_lane` crash that never
                // reached its own `rescue_state` call — lands in the
                // project's own live state before the copy itself goes. A
                // genuine conflict there (`check_copy`/`copy_without_overwrite`
                // both refuse one the same way `rescue()` does) keeps this
                // copy instead of deleting task state no rescue ever saved.
                let rescued = if dry_run {
                    check_copy(&path, &project_state).is_ok()
                } else {
                    copy_without_overwrite(&path, &project_state).is_ok()
                };
                rescued.then_some(QuarantineReason::RetentionExpired)
            } else {
                None
            };
            outcomes.push(match reason {
                None => QuarantineOutcome::Kept { name, bytes },
                Some(reason) if dry_run => QuarantineOutcome::WouldRemove {
                    name,
                    reason,
                    bytes,
                },
                Some(reason) => {
                    deep_prune_remove(&path, &root)?;
                    QuarantineOutcome::Removed {
                        name,
                        reason,
                        bytes,
                    }
                }
            });
        }
        Ok(outcomes)
    }

    /// Every full workspace hash dir under the shared build root whose own
    /// build evidence names only workspace roots that no longer exist on
    /// disk (see `hash_dir_is_orphaned`) — card `20261115-b-673528`'s own
    /// global sweep, run independently of `prune_with_ignored`'s per-lane
    /// one: a hash dir here need not belong to any lane this project's own
    /// `lanes()` still lists, so it also reclaims a worktree removed
    /// outside `--prune` entirely, or one this project never tracked at
    /// all. A lane `prune_with_ignored` already removed earlier in this
    /// same `--prune` invocation counts too: its workspace root is gone
    /// from disk by the time this sweep's own fresh `fs::exists` check
    /// runs (see `names_an_existing_workspace`), the same as any other
    /// vanished workspace, so one real `--prune --yes` call frees the
    /// build dirs of the lanes it just finished removing rather than
    /// needing a second run. `[]` when this workspace has no shared
    /// build-dir namespacing configured (see `shared_build_root`).
    /// `dry_run` measures bytes without deleting anything, sizing one
    /// whole-sweep `BuildDirScanBudget` off the full candidate count and
    /// deciding each candidate off its own fresh `PerDirFileBudget` — see
    /// both types' own doc comments for why a single shared budget across
    /// every hash dir (card `20261116-b-891676`) let big dirs sorted first
    /// starve the small vanished ones after them.
    pub fn prune_orphaned_build_dirs(&self, dry_run: bool) -> Result<Vec<OrphanedBuildDir>> {
        self.prune_orphaned_build_dirs_skipping(dry_run, &[])
    }

    /// [`Self::prune_orphaned_build_dirs`], passing over every hash dir in
    /// `skip` untouched and unreported: the `PruneOutcome::build_dir` of
    /// each lane this same run already reported (card
    /// `20261119-b-c3f9f5`). A measure-only pass removes a lane but leaves
    /// its build-dir in place, now naming only a vanished workspace root,
    /// so without this the sweep reported those same bytes a second time.
    pub fn prune_orphaned_build_dirs_skipping(
        &self,
        dry_run: bool,
        skip: &[PathBuf],
    ) -> Result<Vec<OrphanedBuildDir>> {
        let Some(shared_root) = shared_build_root(&self.project) else {
            return Ok(Vec::new());
        };
        let _lock = if dry_run {
            None
        } else {
            Some(lock(&self.project)?)
        };
        let cargo_home = cargo_cache_home();
        let mut outcomes = Vec::new();
        let mut handled = skip.to_vec();
        for dir in self.vanished_lane_build_dirs(&shared_root, cargo_home.as_deref(), skip) {
            handled.push(dir.clone());
            if let Some(outcome) = free_orphaned_build_dir(&shared_root, dir, dry_run) {
                outcomes.push(outcome);
            }
        }
        let mut candidates = hash_dir_candidates(&shared_root);
        candidates.retain(|candidate| !handled.contains(candidate));
        let mut budget = BuildDirScanBudget::for_candidates(candidates.len());
        for candidate in candidates {
            if budget.exhausted() {
                break;
            }
            let mut per_dir = PerDirFileBudget::new();
            if !hash_dir_is_orphaned(
                &candidate,
                cargo_home.as_deref(),
                Some(&shared_root),
                &mut budget,
                &mut per_dir,
            ) {
                continue;
            }
            if let Some(outcome) = free_orphaned_build_dir(&shared_root, candidate, dry_run) {
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }

    /// Card `20261119-b-4ec912`: the `computed_hash_dir` of every lane this
    /// project's routing log records a task for whose worktree path no
    /// longer exists — a lane an earlier run removed, or one removed
    /// outside `--prune` — that `hash_dir_names_only_lane` proves held that
    /// vanished lane's build and nothing else's. The generic sweep below
    /// can never free these: a lane is nested inside the main checkout,
    /// whose own `Cargo.toml` and member sources keep the lane's relative
    /// sources reading as possibly live (`workspace_root_may_survive`).
    /// Identity here comes from the hash instead, so that rule is not
    /// loosened for any dir this does not compute. Never `skip`, never the
    /// main checkout's own computed dir, never a task id `validate_id`
    /// refuses (it becomes a path component).
    fn vanished_lane_build_dirs(
        &self,
        shared_root: &Path,
        cargo_home: Option<&Path>,
        skip: &[PathBuf],
    ) -> Vec<PathBuf> {
        let tasks: BTreeSet<String> = self
            .task_records()
            .iter()
            .filter_map(|record| record.parse().ok()?.fields.get("task").cloned())
            .filter(|task| validate_id(task).is_ok())
            .collect();
        let main_dir = computed_hash_dir(shared_root, &self.project);
        let worktrees = self.project.join(".multiverse/worktrees");
        let mut candidates = Vec::new();
        for task in tasks {
            let lane_path = worktrees.join(&task);
            match fs::symlink_metadata(&lane_path) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                _ => continue,
            }
            let dir = computed_hash_dir(shared_root, &lane_path);
            if dir == main_dir || skip.contains(&dir) {
                continue;
            }
            if fs::symlink_metadata(&dir).is_ok_and(|info| info.is_dir()) {
                candidates.push((lane_path, dir));
            }
        }
        let mut budget = BuildDirScanBudget::for_complete_reads(candidates.len());
        candidates
            .into_iter()
            .filter(|(lane_path, dir)| {
                hash_dir_names_only_lane(dir, lane_path, cargo_home, shared_root, &mut budget)
            })
            .map(|(_, dir)| dir)
            .collect()
    }

    /// Every `multiverse/<task>` branch this project's git has, that is
    /// not currently checked out in any registered worktree — a lane
    /// whose directory on disk was removed (or never recreated) while its
    /// branch was left behind. [`Self::prune_orphans`] is what decides
    /// whether each one gets reclaimed or archived; this only lists the
    /// candidates.
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
        // One atomic `update-ref` transaction, each line itself a
        // compare-and-swap against the old value it names: `verify`
        // refuses the whole transaction if the branch name has been
        // recreated since `archive_locked` last checked; `update` only
        // moves the archive ref if it still holds what this call read
        // before (`old`), so a concurrent archive of the same task cannot
        // be silently overwritten; `delete` only removes the pending ref
        // if it still points at `tip`, the exact value this call itself
        // wrote it to.
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

    /// Whether `tip`'s own change already landed on `HEAD` by content, not
    /// just by history — the check that lets a branch get archived (never
    /// deleted outright) even after its commits were rebased, squashed, or
    /// cherry-picked onto `HEAD` under a different sha, where a plain
    /// ancestry check (`ancestor`) would wrongly say "not merged" and
    /// refuse. `git cherry HEAD tip` compares each of `tip`'s own commits'
    /// patch-ids against `HEAD`'s history: a `+` line means at least one
    /// commit's patch has no equivalent anywhere on `HEAD`, so this is
    /// genuinely unlanded; a `-` line (and no `+`) means every commit found
    /// a patch-equivalent already on `HEAD`.
    fn patch_landed(&self, tip: &str) -> Result<bool> {
        let output = git(&self.project, &["cherry", "HEAD", tip], None)?;
        if output.lines().any(|line| line.starts_with('+')) {
            return Ok(false);
        }
        if output.lines().any(|line| line.starts_with('-')) {
            return Ok(true);
        }
        // `cherry` prints nothing when `tip` has no commits unique to it at
        // all (e.g. `tip` IS an ancestor of `HEAD`, or it is a no-op/empty
        // commit with nothing to compare) — fall back to a plain ancestry
        // check, then to a content-equal-tree check, rather than reading
        // silence as "not landed".
        if ancestor(&self.project, tip)? {
            return Ok(true);
        }
        Ok(git(&self.project, &["diff", "--name-only", "HEAD", tip], None)?.is_empty())
    }

    /// Every candidate authorizing task name this project currently knows
    /// about, paired with the `.multiverse/tasks` directory to read it
    /// from — the project's own copy when it has one, else the one lane
    /// (if any) still holding that task's own, not-yet-rescued
    /// `.multiverse/tasks/<name>` directory. Mirrors the project-then-lane
    /// fallback [`terminal_statuses`](Self::terminal_statuses)/
    /// [`occupant_status_text`] already use: an assessment's own `retire`
    /// sidecar lives in *its own* lane's task directory until that lane is
    /// pruned, same as its `result.md`/`status` — never copied to the
    /// project until rescue runs. Without this fallback, a dry-run preview
    /// (which never actually rescues anything) can never see an
    /// authorizing task's retirement artifacts that a same-batch real run
    /// would find, because that run rescues the authorizing task's own
    /// lane first — exactly the "model the rescue step in the preview"
    /// gap card `20261110-b-d3ba4f` names.
    ///
    /// Unfiltered: every lane's own task may itself be a candidate here, so
    /// a caller excludes its own task by name at use time
    /// ([`Self::retirement_authorized_with`]) rather than this listing
    /// recomputing itself once per excluded task — `prune_with_ignored`'s
    /// per-lane loop shares one listing across every lane this way instead
    /// of re-walking the project's and every lane's `.multiverse/tasks`
    /// directory for each one.
    fn retirement_candidates(&self) -> Result<Vec<(String, PathBuf)>> {
        let mut candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
        let project_tasks = guarded_path(&self.project, Path::new(".multiverse/tasks"), true)?;
        if let Ok(entries) = fs::read_dir(&project_tasks) {
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if validate_id(&name).is_err() || !entry.file_type()?.is_dir() {
                    continue;
                }
                candidates.insert(name, project_tasks.clone());
            }
        }
        for lane in self.lanes()? {
            let lane_tasks = guarded_path(&lane.path, Path::new(".multiverse/tasks"), true)?;
            let Ok(entries) = fs::read_dir(&lane_tasks) else {
                continue;
            };
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if validate_id(&name).is_err() || !entry.file_type()?.is_dir() {
                    continue;
                }
                candidates.entry(name).or_insert_with(|| lane_tasks.clone());
            }
        }
        Ok(candidates.into_iter().collect())
    }

    /// Single-task convenience wrapper: fetches its own ledger scan and
    /// candidate listing, for callers outside `prune_with_ignored`'s
    /// per-lane loop (`check_archive`, and this module's own tests) where
    /// there is only ever one task to check and sharing a cache across
    /// calls would not save anything.
    fn retirement_authorized(&self, task: &str, tip: &str) -> Result<bool> {
        let recorded = recorded_task_outcomes(&self.project.join(".multiverse/routing.log"));
        let candidates = self.retirement_candidates()?;
        self.retirement_authorized_with(task, tip, &recorded, &candidates)
    }

    /// [`Self::retirement_authorized`]'s own logic, run against a ledger
    /// scan and candidate listing the caller already fetched — see
    /// `prune_with_ignored`'s `recorded_outcomes`/`retirement_candidates`,
    /// shared across every lane in one pass instead of one fetch per lane.
    fn retirement_authorized_with(
        &self,
        task: &str,
        tip: &str,
        recorded: &BTreeMap<String, TaskOutcome>,
        candidates: &[(String, PathBuf)],
    ) -> Result<bool> {
        let target_time = git(&self.project, &["show", "-s", "--format=%ct", tip], None)?
            .trim()
            .parse::<u64>()
            .map_err(|_| WorktreeError::Refused("invalid commit time".into()))?;
        for (name, tasks_root) in candidates {
            if name == task {
                continue;
            }
            // Python requires `role`/`status` from `routing.log`'s own
            // dispatch/result records, never the task-id spelling or the
            // task directory's own (unrecorded) status file — an
            // authorizing task's `done` must have actually been logged
            // (`critic-history-compat.md` finding 8).
            let Some(outcome) = recorded.get(name) else {
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
            let result = match read_regular(tasks_root, &Path::new(name).join("result.md")) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if result.trim().is_empty() {
                continue;
            }
            let retire = match read_regular(tasks_root, &Path::new(name).join("retire")) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if fs::metadata(tasks_root.join(name).join("retire"))?
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

    /// Every task currently archived under `refs/multiverse/archive/` —
    /// each one's task id paired with the commit sha its branch pointed
    /// at when [`Self::archive`] moved it there. [`Self::restore`] is
    /// what turns one of these back into a live branch.
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

    /// `task`'s own terminal status is `done`, with a non-empty `result.md`
    /// — read from the project's copy if rescued there already, else the
    /// lane's own, the same project-then-lane fallback
    /// [`terminal_statuses`](Self::terminal_statuses) uses for every
    /// occupant of a lane. Unlike that method, this checks exactly one
    /// named task (the one `--merge <task>` was asked to land), not every
    /// task a lane has ever been dispatched into.
    fn task_terminal_done(&self, lane: &Lane, task: &str) -> Result<()> {
        let relative = Path::new(".multiverse/tasks").join(task).join("status");
        let root_path = guarded_path(&self.project, &relative, true)?;
        let root = if root_path.try_exists()? {
            &self.project
        } else {
            &lane.path
        };
        let path = guarded_path(root, &relative, true)?;
        if !path.try_exists()? {
            return Err(WorktreeError::Refused(format!(
                "{task} has no recorded terminal status; refusing to merge"
            )));
        }
        let status = read_regular(root, &relative)?;
        if status.trim() != "done" {
            return Err(WorktreeError::Refused(format!(
                "{task}'s status is {:?}, not done; refusing to merge",
                status.trim()
            )));
        }
        let result = read_regular(root, &relative.with_file_name("result.md"))?;
        if result.trim().is_empty() {
            return Err(WorktreeError::Refused(format!(
                "{task}'s result.md is empty; refusing to merge"
            )));
        }
        Ok(())
    }

    /// Writes `output` (already redacted by the caller — `gitleaks` via its
    /// own `--redact` flag, TruffleHog via [`redact_trufflehog_output`]) to
    /// `.multiverse/evidence/<task>-secrets.txt`, overwriting any evidence
    /// a previous attempt left — this merge's own run is what the file
    /// should reflect, not a stale failure. `0o600`, matching this
    /// module's other sensitive, project-root writes (e.g. the worktree
    /// lock file). The leaf path itself is checked for a symlink
    /// immediately before opening: `guarded_path` below only walks
    /// `.multiverse/evidence`'s own directory components, not the
    /// `<task>-secrets.txt` name joined on after it.
    fn write_secrets_evidence(&self, task: &str, tool: &str, output: &[u8]) -> Result<PathBuf> {
        let dir = guarded_path(&self.project, Path::new(".multiverse/evidence"), true)?;
        private_dir(&dir)?;
        let path = dir.join(format!("{task}-secrets.txt"));
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(WorktreeError::Refused(format!(
                    "symlink at {}",
                    path.display()
                )))
            }
            Ok(_) => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        writeln!(file, "{tool} findings for {task} (redacted):")?;
        file.write_all(output)?;
        if !output.ends_with(b"\n") {
            writeln!(file)?;
        }
        Ok(path)
    }

    /// `gitleaks git --log-opts=<trunk>..<lane-branch> --redact --exit-code 1
    /// [--config <allowlist>]`, in the project root — card `20261114-b-069836`
    /// option (a)'s own spelling of the command. `allowlist` is passed only
    /// when it actually exists under the project being merged; a configured
    /// path that is not there is silently not an error (same "use it if
    /// present" reading `[secrets_scan]`'s own doc comment gives it), never a
    /// reason to refuse the merge on its own.
    fn run_gitleaks(
        &self,
        secrets: &SecretsScan,
        log_opts: &str,
    ) -> std::result::Result<Option<Vec<u8>>, ScanError> {
        let mut args = vec![
            "git".to_string(),
            format!("--log-opts={log_opts}"),
            "--redact".to_string(),
            "--exit-code".to_string(),
            "1".to_string(),
        ];
        let allowlist = self.project.join(&secrets.allowlist);
        if allowlist.is_file() {
            args.push("--config".to_string());
            args.push(allowlist.display().to_string());
        }
        run_scanner(&self.project, &secrets.gitleaks, &args, 1)
    }

    /// `trufflehog git file://<root> --since-commit <trunk> --branch <lane>
    /// --only-verified --fail --json`. Advisory only — unlike `gitleaks`, a
    /// missing `trufflehog` never blocks the merge regardless of
    /// `[secrets_scan] required`, matching "if trufflehog is configured and
    /// present" in the card this implements. `--fail` exits TruffleHog with
    /// code 183 on a verified finding (verified against its own `--help`;
    /// it does not reuse exit code 1). `--json` switches output to one
    /// finding object per line so [`redact_trufflehog_output`] can reliably
    /// strip the raw-secret fields before anything downstream (an evidence
    /// file, routing.log, stdout) sees them — TruffleHog offers no
    /// equivalent to `gitleaks`' own `--redact` flag that this module could
    /// verify offline, so this masks the output after the fact instead of
    /// relying on a scanner flag.
    fn run_trufflehog(
        &self,
        trufflehog: &str,
        trunk: &str,
        branch: &str,
    ) -> std::result::Result<Option<Vec<u8>>, ScanError> {
        let args = vec![
            "git".to_string(),
            format!("file://{}", self.project.display()),
            "--since-commit".to_string(),
            trunk.to_string(),
            "--branch".to_string(),
            branch.to_string(),
            "--only-verified".to_string(),
            "--fail".to_string(),
            "--json".to_string(),
        ];
        run_scanner(&self.project, trufflehog, &args, 183)
            .map(|found| found.map(|raw| redact_trufflehog_output(&raw)))
    }

    /// `multiverse worktrees <path> --merge <task>`: the one enforced choke
    /// point card `20261114-b-069836` recommends (option (a)) — gate a
    /// lane's own commits through a secrets scan before landing them with
    /// `git merge --no-ff --no-edit`, so an agent-authored lane cannot reach
    /// `main` carrying a planted key the way raw `git merge` always could.
    ///
    /// Refuses (never merges) unless `task` has a recorded terminal `done`
    /// status with a non-empty `result.md`, and the lane itself is clean —
    /// the same bar `clean` already holds a lane to before
    /// `prune` will touch it, reused here rather than invented fresh: no
    /// uncommitted changes, no committed `.multiverse/` state, no
    /// non-disposable ignored file, no live process still using the lane as
    /// its cwd.
    ///
    /// A finding from either scanner refuses the merge outright (exit 1 at
    /// the `mv-cli` boundary) with its own redacted evidence file named in
    /// the error text; [`MergeError::ScannerMissing`] is the one case that
    /// maps to exit 3 instead, for a *required* `gitleaks` this could not
    /// even run.
    ///
    /// Also refuses outright, before anything else runs, when this
    /// checkout is already mid-merge (`MERGE_HEAD` present, or unmerged
    /// paths left over from an earlier crashed or conflicted merge); and
    /// if this call's own `git merge --no-ff --no-edit` conflicts, it runs
    /// `git merge --abort` before returning, so a conflicting lane never
    /// leaves the checkout mid-merge for the next caller to trip over.
    pub fn merge(
        &self,
        task: &str,
        secrets: &SecretsScan,
    ) -> std::result::Result<MergeResult, MergeError> {
        self.refuse_if_trunk_mid_merge()?;
        let lane = self.lane(task, false)?;
        self.registered(&lane)?;
        self.task_terminal_done(&lane, task)?;
        self.clean(&lane, false, false)?;

        let branch = branch_name(task)?;
        let trunk = git(&self.project, &["rev-parse", "--verify", "HEAD"], None)?
            .trim()
            .to_string();

        let mut warnings = Vec::new();
        let log_opts = format!("{trunk}..{branch}");
        match self.run_gitleaks(secrets, &log_opts) {
            Ok(Some(findings)) => {
                let evidence = self.write_secrets_evidence(task, "gitleaks", &findings)?;
                return Err(MergeError::Refused(format!(
                    "secrets scan (gitleaks) found a match in {task}'s commits; evidence \
                     written to {}; merge refused",
                    evidence.display()
                )));
            }
            Ok(None) => {}
            Err(ScanError::Missing) => {
                if secrets.required {
                    return Err(MergeError::ScannerMissing(format!(
                        "gitleaks ({}) was not found or could not be run; install it \
                         (https://github.com/gitleaks/gitleaks#installing) and retry, or set \
                         [secrets_scan] required = false to merge unscanned.",
                        secrets.gitleaks
                    )));
                }
                warnings.push(format!(
                    "WARNING: gitleaks ({}) was not found or could not be run; \
                     [secrets_scan] required = false, merging {task} unscanned.",
                    secrets.gitleaks
                ));
            }
            Err(ScanError::Failed(message)) => {
                return Err(MergeError::Refused(format!("gitleaks failed: {message}")));
            }
        }

        if let Some(trufflehog) = &secrets.trufflehog {
            match self.run_trufflehog(trufflehog, &trunk, &branch) {
                Ok(Some(findings)) => {
                    let evidence = self.write_secrets_evidence(task, "trufflehog", &findings)?;
                    return Err(MergeError::Refused(format!(
                        "secrets scan (trufflehog) found a verified match in {task}'s commits; \
                         evidence written to {}; merge refused",
                        evidence.display()
                    )));
                }
                Ok(None) => {}
                // Advisory: a configured-but-absent trufflehog never blocks.
                Err(ScanError::Missing) => {}
                Err(ScanError::Failed(message)) => {
                    return Err(MergeError::Refused(format!("trufflehog failed: {message}")));
                }
            }
        }

        let _lock = lock(&self.project)?;
        if let Err(error) = git(
            &self.project,
            &["merge", "--no-ff", "--no-edit", &branch],
            None,
        ) {
            let conflicts = conflicting_paths(&self.project).unwrap_or_default();
            let _ = git(&self.project, &["merge", "--abort"], None);
            return Err(MergeError::Refused(if conflicts.is_empty() {
                format!("merge of {task} failed and was aborted: {error}")
            } else {
                format!(
                    "merge of {task} conflicted on {} and was aborted; resolve the \
                     conflict in the lane and retry",
                    conflicts.join(", ")
                )
            }));
        }
        let sha = git(&self.project, &["rev-parse", "--verify", "HEAD"], None)?
            .trim()
            .to_string();
        Ok(MergeResult { sha, warnings })
    }

    /// Refuses before touching anything when this checkout is already
    /// mid-merge: `MERGE_HEAD` present from an earlier `git merge` that
    /// crashed or still needs resolving, or `git` reporting unmerged
    /// (conflicted) paths left over from it either way. Without this,
    /// [`Worktrees::merge`]'s own `git merge --no-ff --no-edit` would run
    /// straight into that state and fail with an opaque passthrough error
    /// ("you have not concluded your merge") that names no path and
    /// leaves the stuck state exactly as unresolved as it found it.
    fn refuse_if_trunk_mid_merge(&self) -> Result<()> {
        let merge_head = git(
            &self.project,
            &["rev-parse", "--git-path", "MERGE_HEAD"],
            None,
        )?;
        if self.project.join(merge_head.trim()).try_exists()? {
            return Err(WorktreeError::Refused(format!(
                "{proj} already has a merge in progress (MERGE_HEAD is present); \
                 run `git -C {proj} status` to inspect it, then resolve and commit \
                 it or run `git -C {proj} merge --abort` before retrying",
                proj = self.project.display(),
            )));
        }
        let conflicts = conflicting_paths(&self.project)?;
        if !conflicts.is_empty() {
            return Err(WorktreeError::Refused(format!(
                "{proj} has unmerged paths left over from an earlier operation: \
                 {paths}; run `git -C {proj} status` to inspect it, then resolve \
                 or discard them before retrying",
                proj = self.project.display(),
                paths = conflicts.join(", "),
            )));
        }
        Ok(())
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
    /// Builds an observer for `project` that resolves the Herdr binary to
    /// probe the ordinary way, from `HERDR_BIN_PATH` on every call — see
    /// [`Self::with_herdr_binary`] for the pinned alternative.
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

    fn record_prune(
        &mut self,
        lane: &Lane,
        task_records: &[crate::index::IndexedRecord],
    ) -> Result<()> {
        record_prune_outcomes(&self.project, lane, task_records)
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
/// Idempotent per occupant via [`has_terminal_outcome_in`]: a task whose
/// latest dispatch already has a recorded `result` is left alone. A task
/// with neither a recorded outcome nor a valid terminal status file
/// refuses the whole prune rather than removing a lane no reader can later
/// account for — [`Worktrees::terminal_statuses`] already ran immediately
/// before this and should have refused first, so this is a backstop, not
/// the primary gate.
///
/// `task_records` is [`Worktrees::task_records`]'s output, passed in rather
/// than fetched here — [`Worktrees::prune_with_ignored`] already scans the
/// routing log once for the whole call and shares it across every lane's
/// `terminal_statuses` check; this used to re-scan it again per occupant on
/// every real (non-dry-run) removal, undoing that sharing on the one path
/// that matters most.
fn record_prune_outcomes(
    project: &Path,
    lane: &Lane,
    task_records: &[crate::index::IndexedRecord],
) -> Result<()> {
    // Every occupant is checked before anything is appended — mirroring
    // `record_prune_outcome`'s own two passes (`worktrees.py:3346-3360`) —
    // so a lane that fails the check is not left half-logged
    // (`rereview-engine.md` "prune outcome recording can partially mutate
    // the ledger before refusing").
    let mut to_record = Vec::new();
    for task in occupant_tasks(project, lane)? {
        if has_terminal_outcome_in(task_records, &task) {
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
/// Fetches and validates the project's own routing log itself; a caller
/// checking more than one task against the same project (`prune_with_
/// ignored`'s per-lane loop) should fetch once via [`Worktrees::
/// task_records`] and call [`has_terminal_outcome_in`] directly instead, so
/// the ledger is scanned once for the whole call rather than once per task.
fn has_terminal_outcome(project: &Path, task: &str) -> bool {
    let log = project.join(".multiverse/routing.log");
    let Ok(records) = crate::index::records(&log, crate::index::Selector::Tasks) else {
        return false;
    };
    has_terminal_outcome_in(&records, task)
}

/// [`has_terminal_outcome`]'s own scan, run against records the caller
/// already fetched. Scanned from the log's end: a `dispatch` record for
/// `task` reached before any matching `result` means the run now occupying
/// this task id has not finished yet, even if an earlier run's result for
/// the same id sits further back in the log. Deliberately looser than
/// [`recorded_task_outcomes`]'s own per-task bookkeeping, which does not
/// reset `result_status` on a later `dispatch` and so cannot answer this
/// question on its own.
fn has_terminal_outcome_in(records: &[crate::index::IndexedRecord], task: &str) -> bool {
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

/// Whether `records` holds any `event=dispatch` line for `task`, at all —
/// [`abandoned_before_dispatch`](Worktrees::abandoned_before_dispatch)'s own
/// "was this ever genuinely dispatched" question, scanned over the same
/// already-fetched ledger [`has_terminal_outcome_in`] reads. Unlike that
/// function this does not need the log's end-to-start ordering: one
/// `dispatch` record anywhere for `task` is enough, since a task directory
/// existing with zero dispatch record is exactly the crash window (brief
/// written, host died before the append) this answers `false` for.
fn has_dispatch_record_in(records: &[crate::index::IndexedRecord], task: &str) -> bool {
    records.iter().any(|record| {
        record.parse().is_ok_and(|parsed| {
            parsed.event() == "dispatch"
                && parsed.fields.get("task").map(String::as_str) == Some(task)
        })
    })
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

/// Fetches and validates `log_path` itself, then runs
/// [`recorded_task_outcomes_in`] over the result. A missing or unreadable
/// log yields no recorded outcomes, which correctly authorizes nothing. A
/// caller that already has a records fetch to share (`prune_with_ignored`'s
/// per-lane loop, via [`Worktrees::task_records`]) should call
/// [`recorded_task_outcomes_in`] directly instead, so the ledger is scanned
/// once for the whole call rather than once per lane.
fn recorded_task_outcomes(log_path: &Path) -> BTreeMap<String, TaskOutcome> {
    let Ok(records) = crate::index::records(log_path, crate::index::Selector::Tasks) else {
        return BTreeMap::new();
    };
    recorded_task_outcomes_in(&records)
}

/// One pass over already-fetched task-lifecycle records, keyed by task id —
/// the same shape `mv-cli`'s `tasks.rs::gather_task_events` already reads
/// the log with.
fn recorded_task_outcomes_in(
    records: &[crate::index::IndexedRecord],
) -> BTreeMap<String, TaskOutcome> {
    let mut by_task: BTreeMap<String, TaskOutcome> = BTreeMap::new();
    for record in records {
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

/// Reads `relative` under `root` only if it is, start to finish, an
/// ordinary single-hardlink file never swapped for something else —
/// guards every status/result/log read in this module against a
/// stat-then-open race (a symlink dropped in between, or the path
/// replaced out from under this call) rather than trusting the first
/// `symlink_metadata` call alone.
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
    // Re-check identity after opening: the first stat and the open are two
    // separate syscalls, so the path could have been unlinked and
    // recreated (same name, different file) in between. Comparing the
    // opened fd's own (dev, ino) against the pre-open stat, and re-reading
    // the path's current (ino, file type) afterward, catches a swap on
    // either side of the open rather than trusting either stat alone.
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

/// How long `Worktrees::prune_quarantine` keeps a `.multiverse/worktree-state/`
/// copy it cannot prove redundant (card `20261106-b-3d2774`): `remove_lane`'s
/// own "never delete quarantine" guard protects against a write still
/// landing *during* the race Git's own removal runs, not forever, so a copy
/// this old has long outlived any descriptor that could still have been
/// writing to it. Two weeks comfortably outlasts the longest gap this
/// project has seen between a lane's removal and someone noticing a rescue
/// conflict worth investigating, while still bounding how long copies this
/// cannot otherwise account for keep accumulating unexamined.
const WORKTREE_STATE_RETENTION: Duration = Duration::from_secs(14 * 86400);

/// Whether every file under `quarantine` is byte-identical to its mirror at
/// the same relative path under `project_state` — meaning rescue already
/// placed everything this copy holds into the project's own live
/// `.multiverse/`, so the copy itself protects nothing further. A name
/// `is_rescue_excluded` would have skipped during rescue (a nested lane's
/// own `worktrees/`, `input`, `.gitignore`) is never provably redundant this
/// way: rescue never copied it anywhere, so this quarantine copy is its only
/// copy. Neither is a symlink, a special file, or any entry whose mirror is
/// missing or differs — every one of those falls through to
/// `prune_quarantine`'s own retention window instead of being proven safe
/// here.
fn quarantine_is_redundant(quarantine: &Path, project_state: &Path) -> Result<bool> {
    for entry in fs::read_dir(quarantine)? {
        let entry = entry?;
        if is_rescue_excluded(&entry.file_name()) {
            return Ok(false);
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Ok(false);
        }
        let mirror = project_state.join(entry.file_name());
        if kind.is_dir() {
            let mirror_is_dir = fs::symlink_metadata(&mirror)
                .is_ok_and(|meta| meta.is_dir() && !meta.file_type().is_symlink());
            if !mirror_is_dir || !quarantine_is_redundant(&entry.path(), &mirror)? {
                return Ok(false);
            }
        } else if kind.is_file() {
            match fs::read(&mirror) {
                Ok(mirror_bytes) if fs::read(entry.path())? == mirror_bytes => {}
                Ok(_) => return Ok(false),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
                Err(e) => return Err(e.into()),
            }
        } else {
            return Ok(false);
        }
    }
    Ok(true)
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

/// [`copy_without_overwrite`]'s own preview: the same traversal and the
/// same conflict refusal (via [`check_copy`]), reporting the paths that
/// would be freshly written without creating a directory or a byte of
/// `destination`.
fn would_copy_without_overwrite(source: &Path, destination: &Path) -> Result<Vec<PathBuf>> {
    check_copy(source, destination)?;
    let mut planned = Vec::new();
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if is_rescue_excluded(&entry.file_name()) {
            continue;
        }
        let src = entry.path();
        let dst = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            planned.extend(would_copy_without_overwrite(&src, &dst)?);
        } else if !dst.exists() {
            planned.push(dst);
        }
    }
    Ok(planned)
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
        //
        // Some hosts (hidepid-restricted procfs, a container's own pid
        // namespace) deny even `stat`-ing `/proc/<pid>` itself for a
        // process we do not own, as `EACCES`/`EPERM` rather than
        // `ENOENT` — both land on `PermissionDenied`. We cannot learn
        // that pid's uid at all in that case, but the same reasoning
        // applies: a process a denial like this hides from us could
        // never be a process we started, so it is skipped the same way.
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
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e.into()),
            }
            match fs::read_link(entry.path().join("cwd")) {
                Ok(path) if path.starts_with(lane) => return Ok(true),
                Ok(_) => (),
                Err(e) if e.kind() == io::ErrorKind::NotFound => (),
                // A same-uid process can still deny this: the kernel's
                // ptrace rules hide a non-dumpable process's `cwd` link
                // (`PR_SET_DUMPABLE` 0, e.g. a setuid binary or a runner
                // helper) behind `EACCES`, even from its own uid. The
                // agents multiverse starts are ordinary dumpable
                // processes, so a process hidden this way cannot be a
                // lane's agent, and is skipped the same as one already
                // gone.
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => (),
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
            Self::with_initial_branch("main")
        }

        fn with_initial_branch(branch: &str) -> Self {
            let path =
                std::env::temp_dir().join(format!("mv-lanes-{}-{}", std::process::id(), nonce()));
            fs::create_dir(&path).unwrap();
            let repo = Self(fs::canonicalize(path).unwrap());
            git(&repo.0, &["init", "-b", branch], None).unwrap();
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

    fn outcome_task(outcome: &PruneOutcome) -> &str {
        match outcome {
            PruneOutcome::Kept { task, .. }
            | PruneOutcome::WouldRemove { task, .. }
            | PruneOutcome::WouldArchive { task, .. }
            | PruneOutcome::Removed { task, .. }
            | PruneOutcome::Archived { task, .. } => task,
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
        fn record_prune(
            &mut self,
            lane: &Lane,
            _task_records: &[crate::index::IndexedRecord],
        ) -> Result<()> {
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

    /// Lane and the project's main checkout both edit the same file after
    /// the recorded cut point. `merge_readiness` must notice regardless of
    /// whether that checkout's branch is named `main`.
    fn merge_readiness_after_overlapping_trunk_and_lane_edits(repo: &TempRepo) -> Option<String> {
        let store = repo.store();
        let task = "20261101-fixer-abcdef";
        let lane = store.create(task, "HEAD").unwrap();
        fs::write(lane.path.join("README.md"), "lane\n").unwrap();
        git(&lane.path, &["add", "README.md"], None).unwrap();
        git(&lane.path, &["commit", "-m", "lane edit"], None).unwrap();
        fs::write(repo.0.join("README.md"), "trunk\n").unwrap();
        git(&repo.0, &["add", "README.md"], None).unwrap();
        git(&repo.0, &["commit", "-m", "trunk edit"], None).unwrap();
        merge_readiness(&repo.0, task)
    }

    #[test]
    fn merge_readiness_warns_when_main_moved_and_both_changed_a_file() {
        let repo = TempRepo::new();
        let warning = merge_readiness_after_overlapping_trunk_and_lane_edits(&repo).unwrap();
        assert!(warning.contains("README.md"), "{warning}");
    }

    #[test]
    fn merge_readiness_warns_when_master_moved_and_both_changed_a_file() {
        let repo = TempRepo::with_initial_branch("master");
        let warning = merge_readiness_after_overlapping_trunk_and_lane_edits(&repo);
        assert!(
            warning.is_some(),
            "a trunk named master must still report the moved-trunk overlap"
        );
        let warning = warning.unwrap();
        assert!(warning.contains("README.md"), "{warning}");
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

    /// Card `20261108-b-51a656`: a finished lane `prune` keeps anyway (here,
    /// for an uncommitted change) still loses its own lane-root `target/`,
    /// since nothing regenerable should survive a finished, idle lane —
    /// everything else about the lane, including files `prune` has no
    /// reason to touch, must survive untouched.
    #[test]
    fn prune_frees_lane_root_target_for_a_kept_terminal_lane() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        let target = lane.path.join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/build-artifact"), vec![0u8; 4096]).unwrap();
        let target_size = deep_prune_size(&target);
        assert!(target_size > 0);
        fs::write(lane.path.join("README.md"), "dirty").unwrap();

        let outcomes = store.prune(&mut Observer::idle(), false, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Kept {
                reason,
                target_freed,
                ..
            } => {
                assert!(reason.contains("uncommitted"));
                assert_eq!(*target_freed, Some(target_size));
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(lane.path.exists(), "the lane itself must survive");
        assert!(!target.exists(), "target/ must be gone");
        assert!(
            lane.path.join("README.md").exists(),
            "unrelated files must survive"
        );
    }

    /// A lane still doing live work must never lose its `target/`, even
    /// though it already has a terminal status file — the pane, not the
    /// status, is what decides this.
    #[test]
    fn prune_leaves_lane_root_target_untouched_for_a_live_lane() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::create_dir_all(lane.path.join("target/debug")).unwrap();
        fs::write(lane.path.join("target/debug/build-artifact"), b"x").unwrap();

        let outcomes = store
            .prune(
                &mut Observer {
                    live: true,
                    recorded: 0,
                },
                false,
                false,
            )
            .unwrap();
        assert!(matches!(
            &outcomes[0],
            PruneOutcome::Kept {
                target_freed: None,
                ..
            }
        ));
        assert!(
            lane.path.join("target").exists(),
            "a live lane's target/ must survive"
        );
    }

    /// `occupant_tasks` folds in every task the project's routing log ever
    /// dispatched onto this lane's branch, which is exactly how an
    /// unfinished `--into` follow-up is detected here too: the lane's own
    /// task is done, but the follow-up occupant has no terminal status, so
    /// `terminal_statuses` refuses and `target/` is left alone.
    #[test]
    fn prune_leaves_lane_root_target_untouched_for_an_unfinished_into_followup() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::create_dir_all(lane.path.join("target/debug")).unwrap();
        fs::write(lane.path.join("target/debug/build-artifact"), b"x").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=followup\tbranch={}\n",
                lane.branch
            ),
        )
        .unwrap();

        let outcomes = store.prune(&mut Observer::idle(), false, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Kept {
                reason,
                target_freed,
                ..
            } => {
                assert!(reason.contains("followup"), "reason was: {reason}");
                assert_eq!(*target_freed, None);
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(lane.path.join("target").exists());
    }

    /// `--dry-run` must report the exact bytes a same-flags real run would
    /// free, and delete nothing.
    #[test]
    fn prune_dry_run_reports_lane_root_target_bytes_without_deleting() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        let target = lane.path.join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/build-artifact"), vec![0u8; 2048]).unwrap();
        fs::write(lane.path.join("README.md"), "dirty").unwrap();
        let target_size = deep_prune_size(&target);

        let preview = store.prune(&mut Observer::idle(), false, true).unwrap();
        match &preview[0] {
            PruneOutcome::Kept { target_freed, .. } => {
                assert_eq!(*target_freed, Some(target_size))
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(target.exists(), "dry run must not delete target/");
        assert_eq!(
            deep_prune_size(&target),
            target_size,
            "dry run must not change target/'s bytes"
        );

        let real = store.prune(&mut Observer::idle(), false, false).unwrap();
        match &real[0] {
            PruneOutcome::Kept { target_freed, .. } => {
                assert_eq!(*target_freed, Some(target_size))
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(!target.exists(), "the real run must delete target/");
    }

    /// Writes `.cargo/config.toml` directly into `repo`'s own working tree
    /// (never committed — `shared_build_root` reads `self.project`'s own
    /// checkout, not any lane's, so this never needs to reach a lane's
    /// checked-out tree the way a real `cargo metadata` call would).
    fn write_build_dir_template(repo: &TempRepo, template: &str) {
        fs::create_dir_all(repo.0.join(".cargo")).unwrap();
        fs::write(
            repo.0.join(".cargo/config.toml"),
            format!("[build]\nbuild-dir = \"{template}\"\n"),
        )
        .unwrap();
    }

    /// A fake cargo dep-info (`.d`) file under `hash_dir`, naming each of
    /// `workspace_paths` on its own line — the same shape `find_build_dir`'s
    /// own scan looks for, standing in for a real `cargo build` here since
    /// this feature never runs cargo at all (card `20261115-b-47f043`,
    /// second attempt).
    fn write_build_evidence(hash_dir: &Path, workspace_paths: &[&Path]) {
        let deps = hash_dir.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let content: String = workspace_paths
            .iter()
            .map(|path| format!("{}/src/lib.rs\n", path.display()))
            .collect();
        fs::write(deps.join("fixture.d"), content).unwrap();
    }

    /// `shared_root`'s own real two-level hash-dir shape — a 2-hex-digit
    /// bucket, then a 14-hex-digit leaf — rather than the single flat
    /// level earlier fixtures here used, which no longer matches what
    /// `hash_dir_candidates` will ever consider a candidate. `leaf` is
    /// padded to 14 hex digits so callers can pass a short, readable
    /// literal (`"1"`, `"2"`, …) and still get a shape that passes
    /// `is_hash_component`.
    fn hash_dir(shared_root: &Path, bucket: &str, leaf: &str) -> PathBuf {
        shared_root.join(bucket).join(format!("{leaf:0>14}"))
    }

    /// Scopes `CARGO_HOME` to a throwaway directory for the duration of a
    /// `shared_build_root`/`cargo_cache_home` resolution a test makes, so
    /// `{cargo-cache-home}` never resolves into the real, shared `~/.cargo`.
    /// `std::env::set_var` is process-wide, so concurrent tests touching
    /// this still need the lock even though each uses its own directory —
    /// same save/restore-under-a-lock shape as `mv_config::load`'s own
    /// `StateDirEnvGuard`. Owns the throwaway directory itself (card
    /// `20261115-b-c8065f`: the directory used to outlive the guard and
    /// leak into `TMPDIR`), so dropping the guard also removes it.
    struct CargoHomeEnvGuard {
        dir: PathBuf,
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl CargoHomeEnvGuard {
        fn new() -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let dir = std::env::temp_dir().join(format!(
                "mv-cargo-home-{}-{}",
                std::process::id(),
                nonce()
            ));
            fs::create_dir_all(&dir).unwrap();
            let previous = std::env::var_os("CARGO_HOME");
            std::env::set_var("CARGO_HOME", &dir);
            Self {
                dir,
                previous,
                _lock: held,
            }
        }
    }
    impl Drop for CargoHomeEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("CARGO_HOME", value),
                None => std::env::remove_var("CARGO_HOME"),
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Scopes `CARGO_BUILD_BUILD_DIR` to a value a test supplies for its own
    /// duration, restoring it on drop — same save/restore-under-a-lock
    /// shape as `CargoHomeEnvGuard` just above, needed for the same reason:
    /// `std::env::set_var` is process-wide (card `20261115-b-98b977`).
    /// `lock()` is shared by both `set` and `cleared` below — a `static`
    /// local to just one of them would not serialize against the other,
    /// so a test that only clears the variable could still race a
    /// concurrent test that sets it to a different value (fix round,
    /// card `20261115-b-98b977`: the Nexus and every lane export
    /// `CARGO_BUILD_BUILD_DIR` to their own `$TMPDIR` path, so any test
    /// reaching `shared_build_root` without pinning this variable under
    /// the same lock inherits whichever value won the race).
    struct BuildDirEnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl BuildDirEnvGuard {
        fn lock() -> std::sync::MutexGuard<'static, ()> {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        fn set(value: &str) -> Self {
            let held = Self::lock();
            let previous = std::env::var_os("CARGO_BUILD_BUILD_DIR");
            std::env::set_var("CARGO_BUILD_BUILD_DIR", value);
            Self {
                previous,
                _lock: held,
            }
        }

        /// Clears any inherited `CARGO_BUILD_BUILD_DIR` for the duration of
        /// a test that means to exercise `shared_build_root`'s own
        /// `.cargo/config.toml` fallback: an inherited override (the Nexus
        /// and every lane export one to a `$TMPDIR` path with no
        /// `{workspace-path-hash}` placeholder) would otherwise shadow that
        /// fallback and resolve to `None` instead.
        fn cleared() -> Self {
            let held = Self::lock();
            let previous = std::env::var_os("CARGO_BUILD_BUILD_DIR");
            std::env::remove_var("CARGO_BUILD_BUILD_DIR");
            Self {
                previous,
                _lock: held,
            }
        }
    }
    impl Drop for BuildDirEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("CARGO_BUILD_BUILD_DIR", value),
                None => std::env::remove_var("CARGO_BUILD_BUILD_DIR"),
            }
        }
    }

    /// Prepends `dir` to `PATH` for the duration of a test, restoring the
    /// original value on drop — used only to put a fake, recording
    /// `cargo`/`rustup` ahead of the real ones so a test can prove neither
    /// is ever invoked (card `20261115-b-47f043`, second attempt: the first
    /// attempt's `cargo metadata` call ran rustup inside every lane).
    struct PathEnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }
    impl PathEnvGuard {
        fn prepend(dir: &Path) -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("PATH");
            let mut new_path = std::ffi::OsString::from(dir);
            if let Some(existing) = &previous {
                new_path.push(":");
                new_path.push(existing);
            }
            std::env::set_var("PATH", &new_path);
            Self {
                previous,
                _lock: held,
            }
        }
    }
    impl Drop for PathEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    /// A shell script at `path` that records its own invocation by
    /// creating `marker`, then exits non-zero — so a test using this as a
    /// fake `cargo`/`rustup` fails loudly (via the real command it stands
    /// in for erroring) if production code ever actually runs it, rather
    /// than silently succeeding and masking the call.
    fn write_fake_executable(path: &Path, marker: &Path) {
        let script = format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display());
        fs::write(path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// A throwaway directory for fake `cargo`/`rustup` stand-ins, removed
    /// on drop (card `20261115-b-c8065f`: it used to leak into `TMPDIR`).
    /// `Deref`s to `Path` so call sites read exactly like the plain
    /// `PathBuf` this replaced.
    struct TempBinDir(PathBuf);
    impl TempBinDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mv-fake-bin-{}-{}",
                std::process::id(),
                nonce()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl std::ops::Deref for TempBinDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempBinDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A `CARGO_BUILD_BUILD_DIR` override root a test points `{workspace-
    /// path-hash}` templates at — never created directly, only implied by
    /// `write_build_evidence`'s own `create_dir_all` underneath it, but
    /// still a `TMPDIR` entry that must not survive the test (card
    /// `20261115-b-c8065f`: `prune` only ever removes the leaf hash dir it
    /// resolved, never this root).
    struct TempOverrideRoot(PathBuf);
    impl TempOverrideRoot {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mv-build-override-{}-{}",
                std::process::id(),
                nonce()
            ));
            Self(dir)
        }
    }
    impl std::ops::Deref for TempOverrideRoot {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempOverrideRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// Card `20261115-b-47f043`, second attempt: a lane `--prune` actually
    /// removes must also lose its own per-worktree cargo build-dir, found
    /// read-only under the shared build root (no `cargo`/`rustup`
    /// subprocess — see `find_build_dir`) — the main checkout's own
    /// build-dir, and an unrelated sibling hash dir, both under the same
    /// shared root, must survive untouched. `--dry-run` must report the
    /// same bytes a real run would free, deleting nothing.
    #[test]
    fn prune_removes_a_removed_lanes_build_dir() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = hash_dir(&shared_root, "2a", "1");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let main_hash_dir = hash_dir(&shared_root, "5a", "2");
        write_build_evidence(&main_hash_dir, &[&repo.0]);
        let sibling_hash_dir = hash_dir(&shared_root, "8b", "3");
        write_build_evidence(
            &sibling_hash_dir,
            &[&repo.0.join(".multiverse/worktrees/other-task")],
        );
        let lane_build_size = deep_prune_size(&lane_hash_dir);

        let preview = store.prune(&mut Observer::idle(), true, true).unwrap();
        match &preview[0] {
            PruneOutcome::WouldRemove {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, Some(lane_build_size)),
            other => panic!("expected WouldRemove, got {other:?}"),
        }
        assert!(
            lane_hash_dir.exists(),
            "dry run must not delete the build-dir"
        );

        let real = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &real[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, Some(lane_build_size)),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(
            !lane_hash_dir.exists(),
            "the real run must delete the lane's build-dir"
        );
        assert!(
            main_hash_dir.exists(),
            "the main checkout's own build-dir must survive"
        );
        assert!(
            sibling_hash_dir.exists(),
            "an unrelated hash dir must survive"
        );
    }

    /// Card `20261119-b-73759c`: without the CLI's `--yes`
    /// (`free_caches: false`) a real prune still removes the lane, but its
    /// build-dir is only measured — the same bytes a dry run reports — and
    /// stays on disk.
    #[test]
    fn prune_without_free_caches_removes_the_lane_but_keeps_its_build_dir() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = hash_dir(&shared_root, "2a", "1");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let lane_build_size = deep_prune_size(&lane_hash_dir);

        let real = store
            .prune_with_ignored(&mut Observer::idle(), true, false, false, false)
            .unwrap();
        match &real[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, Some(lane_build_size)),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(!lane.path.exists(), "the lane itself is still removed");
        assert!(
            lane_hash_dir.exists(),
            "an unconfirmed prune must not delete the build-dir"
        );
    }

    /// Card `20261119-b-73759c`'s twin for a kept lane's lane-root
    /// `target/`: reported, never deleted, without `free_caches`.
    #[test]
    fn prune_without_free_caches_keeps_a_kept_lanes_target() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        let target = lane.path.join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(target.join("debug/build-artifact"), vec![0u8; 4096]).unwrap();
        let target_size = deep_prune_size(&target);
        fs::write(lane.path.join("README.md"), "dirty").unwrap();

        let outcomes = store
            .prune_with_ignored(&mut Observer::idle(), false, false, false, false)
            .unwrap();
        match &outcomes[0] {
            PruneOutcome::Kept { target_freed, .. } => {
                assert_eq!(*target_freed, Some(target_size))
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(target.exists(), "target/ must survive an unconfirmed prune");
    }

    /// The same eligibility `kept_target_bytes` already uses (finished,
    /// idle, pane clear) also frees a kept lane's own cargo build-dir —
    /// card `20261115-b-47f043` extends `--prune`'s existing lane-root
    /// `target/` reclaim to it, now resolved without any `cargo`
    /// subprocess.
    #[test]
    fn prune_frees_build_dir_for_a_kept_terminal_lane() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::write(lane.path.join("README.md"), "dirty").unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = hash_dir(&shared_root, "2a", "4");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let build_size = deep_prune_size(&lane_hash_dir);

        match &store.prune(&mut Observer::idle(), false, false).unwrap()[0] {
            PruneOutcome::Kept {
                reason,
                build_dir_freed,
                ..
            } => {
                assert!(reason.contains("uncommitted"), "reason was: {reason}");
                assert_eq!(*build_dir_freed, Some(build_size));
            }
            other => panic!("expected Kept, got {other:?}"),
        }
        assert!(lane.path.exists(), "the lane itself must survive");
        assert!(!lane_hash_dir.exists(), "build-dir must be gone");
    }

    /// A `.cargo/config.toml` whose `build-dir` has no
    /// `{workspace-path-hash}` placeholder — this workspace's own
    /// pre-`20261115-b-d9ef30` shape — resolves every workspace to the
    /// very same directory. `shared_build_root` must refuse to resolve a
    /// shared root at all in that case, so no lane's build-dir is ever
    /// scanned for, let alone deleted.
    #[test]
    fn prune_never_touches_an_unnamespaced_shared_build_dir() {
        let repo = TempRepo::new();
        write_build_dir_template(&repo, "{cargo-cache-home}/shared-build");
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        assert!(
            shared_build_root(&repo.0).is_none(),
            "an unnamespaced template must not resolve a shared root"
        );

        let store = repo.store();
        store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, None),
            other => panic!("expected Removed, got {other:?}"),
        }
    }

    /// Card `20261115-b-98b977`: `CARGO_BUILD_BUILD_DIR` is cargo's own
    /// higher-precedence override of `.cargo/config.toml`'s `build-dir` —
    /// no evidence is ever written under the config template's own root, so
    /// a lane's build-dir is found here only by actually honoring the
    /// override: a mutation that falls back to the config template instead
    /// resolves `shared_root` to a directory with no matching hash dir and
    /// reports nothing freed.
    #[test]
    fn prune_honors_cargo_build_build_dir_over_the_config_template() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();

        let override_root = TempOverrideRoot::new();
        let _build_dir = BuildDirEnvGuard::set(&format!(
            "{}/{{workspace-path-hash}}",
            override_root.display()
        ));

        assert_eq!(
            shared_build_root(&repo.0),
            Some(override_root.to_path_buf()),
            "CARGO_BUILD_BUILD_DIR must win over the config template"
        );

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let lane_hash_dir = hash_dir(&override_root, "2a", "1");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let lane_build_size = deep_prune_size(&lane_hash_dir);

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, Some(lane_build_size)),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(
            !lane_hash_dir.exists(),
            "the real run must delete the lane's build-dir under the override root"
        );
    }

    /// `guard_build_dir_candidate`'s own `main_checkout_dir` guard — not
    /// the shared-root structural guarantee `find_build_dir` already
    /// provides — is what this proves: a single hash dir's build evidence
    /// names both the main checkout's own source tree and this lane's, the
    /// way a stray `include!` or a build script reading a path outside its
    /// own workspace might produce in the wild. `find_build_dir` alone
    /// would happily return it as "this lane's own build-dir"; only the
    /// explicit `main_checkout_dir` comparison refuses it. Removing that
    /// comparison makes this test fail: the ambiguous hash dir would be
    /// deleted along with the lane.
    #[test]
    fn prune_never_deletes_the_main_checkouts_build_dir_when_a_lane_resolves_to_it() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let ambiguous_hash_dir = hash_dir(&shared_root, "2a", "5");
        write_build_evidence(&ambiguous_hash_dir, &[&repo.0, &lane.path]);
        fs::write(ambiguous_hash_dir.join("marker"), b"keep me").unwrap();

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, None),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(
            ambiguous_hash_dir.join("marker").exists(),
            "a hash dir that also names the main checkout's own sources must survive"
        );
    }

    /// `find_build_dir` can never itself return `shared_root` — every
    /// candidate it considers is a full two-level hash dir nested under it
    /// (see `hash_dir_candidates`), never the root itself — so this guard
    /// can only be exercised directly against
    /// `guard_build_dir_candidate`, not through a fixture `prune` call: it
    /// protects against a future change to that scan ever resolving to the
    /// root itself, the same guard shape the first attempt's
    /// `scoped_lane_build_dir` had for its own (differently sourced)
    /// candidate path.
    #[test]
    fn guard_build_dir_candidate_refuses_the_shared_root_itself() {
        let shared_root = std::env::temp_dir().join(format!(
            "mv-shared-root-guard-{}-{}",
            std::process::id(),
            nonce()
        ));
        assert_eq!(
            guard_build_dir_candidate(shared_root.clone(), &shared_root, None),
            None
        );
    }

    /// `guard_build_dir_candidate`'s own `strip_prefix` check — distinct
    /// from the "root itself" guard above, which covers an *empty*
    /// relative path once `strip_prefix` already succeeded. This covers
    /// `strip_prefix` failing outright: a candidate that is not nested
    /// under `shared_root` at all.
    #[test]
    fn guard_build_dir_candidate_refuses_a_path_outside_the_shared_root() {
        let shared_root = std::env::temp_dir().join(format!(
            "mv-shared-root-guard-{}-{}",
            std::process::id(),
            nonce()
        ));
        let outside = std::env::temp_dir().join(format!(
            "mv-outside-guard-{}-{}",
            std::process::id(),
            nonce()
        ));
        assert_eq!(guard_build_dir_candidate(outside, &shared_root, None), None);
    }

    /// Proof that this feature never shells out to `cargo` or `rustup` at
    /// all — not even the now-removed `cargo metadata` call the first
    /// attempt made, which ran rustup inside every lane during `--prune
    /// --dry-run` (reverted in `bb794a6`) — by putting fake, recording
    /// stand-ins for both at the front of `PATH` and asserting neither is
    /// ever invoked by a real or a `--dry-run` `--prune`.
    #[test]
    fn prune_never_invokes_cargo_or_rustup() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        write_build_evidence(&hash_dir(&shared_root, "2a", "6"), &[&lane.path]);

        let bin_dir = TempBinDir::new();
        let cargo_invoked = bin_dir.join("cargo-invoked");
        let rustup_invoked = bin_dir.join("rustup-invoked");
        write_fake_executable(&bin_dir.join("cargo"), &cargo_invoked);
        write_fake_executable(&bin_dir.join("rustup"), &rustup_invoked);
        let _path = PathEnvGuard::prepend(&bin_dir);

        store.prune(&mut Observer::idle(), true, true).unwrap();
        store.prune(&mut Observer::idle(), true, false).unwrap();

        assert!(!cargo_invoked.exists(), "prune must never invoke cargo");
        assert!(!rustup_invoked.exists(), "prune must never invoke rustup");
    }

    /// Fix round for card `20261115-b-47f043`: two lanes whose own hashes
    /// happen to share a first-level bucket (the real shape observed on
    /// disk — `~/.cargo/shared-build/2a/…`) must not share a fate.
    /// Against the pre-fix scan, which returned the bucket itself (a
    /// `shared_root` immediate child) as the candidate, this test fails:
    /// pruning `lane_hash_dir` would delete the whole `2a/` bucket,
    /// taking `other_hash_dir` with it even though nothing about the
    /// other lane changed.
    #[test]
    fn prune_in_a_shared_bucket_leaves_the_other_hash_dir_intact() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let bucket = shared_root.join("2a");
        let lane_hash_dir = hash_dir(&shared_root, "2a", "11111111111111");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let other_hash_dir = hash_dir(&shared_root, "2a", "22222222222222");
        write_build_evidence(
            &other_hash_dir,
            &[&repo.0.join(".multiverse/worktrees/other-task")],
        );

        store.prune(&mut Observer::idle(), true, false).unwrap();

        assert!(
            !lane_hash_dir.exists(),
            "the pruned lane's own hash dir must still be deleted"
        );
        assert!(
            other_hash_dir.exists(),
            "a sibling hash dir sharing only the bucket must survive"
        );
        assert!(
            bucket.exists(),
            "the shared bucket directory itself must never be the thing deleted"
        );
    }

    /// Fix round for card `20261115-b-47f043`: `debug/` is the operator's
    /// pre-namespacing shared tree (`f1b19f8` and earlier) — every project
    /// and every lane built before that card used it directly, so it can
    /// still carry dep-info naming any one of them. It fails cargo's own
    /// hash shape at the very first depth, so it must never be treated as
    /// a candidate even when its evidence happens to name this lane.
    /// Against the pre-fix scan, which accepted any `shared_root` child
    /// whose subtree named the marker, this test fails: `debug/` would
    /// resolve as the lane's own build-dir and be deleted — taking every
    /// other project's pre-namespacing cache with it.
    #[test]
    fn prune_never_resolves_a_lanes_legacy_debug_evidence_to_debug_itself() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let legacy_debug = shared_root.join("debug");
        write_build_evidence(&legacy_debug, &[&lane.path]);

        assert!(
            find_build_dir(&shared_root, &lane.path).is_none(),
            "a lane named only in legacy debug/ evidence must resolve to no candidate"
        );

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed, ..
            } => assert_eq!(*build_dir_freed, None),
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(
            legacy_debug.exists(),
            "legacy debug/ must survive a lane prune even though it names the lane"
        );
    }

    /// Fix round for card `20261115-b-47f043`: the main checkout's own
    /// hash dir can share a lane's bucket the same way two lanes can (see
    /// `prune_in_a_shared_bucket_leaves_the_other_hash_dir_intact`) —
    /// distinct from `guard_build_dir_candidate`'s own exact-path equality
    /// guard, which this test does not exercise, since the two hash dirs
    /// here are never equal, only bucket-siblings. Against the pre-fix
    /// scan this fails the same way: deleting the bucket takes the main
    /// checkout's own hash dir with it.
    #[test]
    fn prune_never_deletes_the_main_checkouts_hash_dir_sharing_a_lanes_bucket() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = hash_dir(&shared_root, "2a", "33333333333333");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let main_hash_dir = hash_dir(&shared_root, "2a", "44444444444444");
        write_build_evidence(&main_hash_dir, &[&repo.0]);

        store.prune(&mut Observer::idle(), true, false).unwrap();

        assert!(
            !lane_hash_dir.exists(),
            "the pruned lane's own hash dir must still be deleted"
        );
        assert!(
            main_hash_dir.exists(),
            "the main checkout's own hash dir, sharing the lane's bucket, must survive"
        );
    }

    /// Writes a `Cargo.toml` and a `src/lib.rs` at `path`, creating it
    /// first if needed — `src/lib.rs` is the exact path
    /// `write_build_evidence` names and `names_an_existing_workspace` now
    /// checks for (fix round, card `20261115-b-673528`: the prior rule
    /// walked ancestors for a `Cargo.toml`, so only that file needed to
    /// exist; the corrected rule checks the named path itself). Standing
    /// in for a real `cargo build`'s own workspace the same way
    /// `write_build_evidence` stands in for its dep-info.
    fn write_workspace_marker(path: &Path) {
        fs::create_dir_all(path.join("src")).unwrap();
        fs::write(path.join("Cargo.toml"), "[package]\nname = \"fixture\"\n").unwrap();
        fs::write(path.join("src/lib.rs"), "").unwrap();
    }

    /// Card `20261115-b-673528`: a hash dir whose own evidence names only a
    /// workspace root — a `Cargo.toml`-holding dir — that no longer exists
    /// on disk is reclaimed, independent of whether it was ever one of this
    /// project's own lanes. Dry-run first, matching the no-delete/real-
    /// delete pairing every other build-dir test in this module uses.
    #[test]
    fn prune_orphaned_build_dirs_reclaims_a_hash_dir_naming_only_a_deleted_workspace() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&vanished);
        let orphan_hash_dir = hash_dir(&shared_root, "2a", "9");
        write_build_evidence(&orphan_hash_dir, &[&vanished]);
        let size = deep_prune_size(&orphan_hash_dir);
        fs::remove_dir_all(&vanished).unwrap();

        let preview = store.prune_orphaned_build_dirs(true).unwrap();
        assert_eq!(
            preview,
            vec![OrphanedBuildDir {
                path: orphan_hash_dir.clone(),
                bytes: size,
                kept: None,
            }]
        );
        assert!(
            orphan_hash_dir.exists(),
            "dry run must not delete the orphaned build-dir"
        );

        let real = store.prune_orphaned_build_dirs(false).unwrap();
        assert_eq!(
            real,
            vec![OrphanedBuildDir {
                path: orphan_hash_dir.clone(),
                bytes: size,
                kept: None,
            }]
        );
        assert!(
            !orphan_hash_dir.exists(),
            "the real run must delete the orphaned build-dir"
        );
    }

    /// Card `20261115-b-673528`: a hash dir naming a workspace that still
    /// exists on disk — nothing about it says this project's own lanes
    /// track it — must never be reclaimed.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_hash_dir_naming_a_live_workspace() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let live = std::env::temp_dir().join(format!(
            "mv-live-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&live);
        let hash_dir_path = hash_dir(&shared_root, "2a", "a");
        write_build_evidence(&hash_dir_path, &[&live]);

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert!(
            outcomes.is_empty(),
            "a hash dir naming a live workspace must never be reclaimed"
        );
        assert!(hash_dir_path.exists());

        fs::remove_dir_all(&live).unwrap();
    }

    /// Card `20261115-b-673528`: mixed evidence — one still-existing
    /// workspace root alongside one vanished one — must keep the hash dir.
    /// One live line is enough to save it, same as the per-lane scan's own
    /// `prune_in_a_shared_bucket_leaves_the_other_hash_dir_intact` spirit:
    /// nothing here is deleted on a guess.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_hash_dir_naming_both_a_live_and_a_deleted_workspace() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let live = std::env::temp_dir().join(format!(
            "mv-live-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&live);
        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&vanished);
        let hash_dir_path = hash_dir(&shared_root, "2a", "b");
        write_build_evidence(&hash_dir_path, &[&vanished, &live]);
        fs::remove_dir_all(&vanished).unwrap();

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert!(
            outcomes.is_empty(),
            "mixed evidence naming one live workspace must keep the hash dir"
        );
        assert!(hash_dir_path.exists());

        fs::remove_dir_all(&live).unwrap();
    }

    /// Card `20261115-b-673528`: a hash dir with no readable build-evidence
    /// file at all — never a license to delete, the same rule a budget
    /// running out already follows.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_hash_dir_with_no_readable_evidence() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let hash_dir_path = hash_dir(&shared_root, "2a", "c");
        fs::create_dir_all(&hash_dir_path).unwrap();

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert!(outcomes.is_empty(), "no evidence must never be reclaimed");
        assert!(hash_dir_path.exists());
    }

    /// Card `20261115-b-673528`'s own registry/git-checkout exclusion: a
    /// real dep-info file for a deleted workspace almost always also names
    /// its dependencies' own source under `CARGO_HOME`'s registry, which
    /// persists — Cargo.toml and all — long after the workspace that built
    /// against it is gone. Without `is_cargo_cache_source` excluding it,
    /// that persistent Cargo.toml would read as "still live" forever and
    /// this hash dir would never be reclaimed at all.
    #[test]
    fn prune_orphaned_build_dirs_reclaims_a_deleted_workspace_even_with_a_live_registry_source_in_its_evidence(
    ) {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let registry_pkg = cargo_home
            .dir
            .join("registry/src/example-index/some-dep-1.0.0");
        write_workspace_marker(&registry_pkg);
        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&vanished);
        let hash_dir_path = hash_dir(&shared_root, "2a", "e");
        write_build_evidence(&hash_dir_path, &[&vanished, &registry_pkg]);
        let size = deep_prune_size(&hash_dir_path);
        fs::remove_dir_all(&vanished).unwrap();

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert_eq!(
            outcomes,
            vec![OrphanedBuildDir {
                path: hash_dir_path.clone(),
                bytes: size,
                kept: None,
            }]
        );
        assert!(
            !hash_dir_path.exists(),
            "a deleted workspace must still be reclaimed even with a live registry source in its evidence"
        );
    }

    /// Fix round, card `20261115-b-673528`: a lane lives nested inside the
    /// main checkout's own working tree
    /// (`<project>/.multiverse/worktrees/<task>/`), whose own root also
    /// holds a `Cargo.toml` — the real fleet layout the prior,
    /// ancestor-walking `names_an_existing_workspace` was useless against:
    /// every path inside a deleted lane still had that same `Cargo.toml`
    /// as an ancestor, so its hash dir was never reclaimed. Proves both
    /// halves of the fix at once, sharing one `shared_root`: the deleted
    /// lane's own hash dir is reclaimed, and the still-live main checkout's
    /// hash dir survives untouched.
    #[test]
    fn prune_orphaned_build_dirs_reclaims_a_nested_deleted_lane_while_keeping_the_live_main_checkout(
    ) {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        write_workspace_marker(&repo.0);
        let main_hash_dir = hash_dir(&shared_root, "2a", "f");
        write_build_evidence(&main_hash_dir, &[&repo.0]);

        let lane = repo.0.join(".multiverse/worktrees/deleted-task");
        write_workspace_marker(&lane);
        let lane_hash_dir = hash_dir(&shared_root, "5a", "10");
        write_build_evidence(&lane_hash_dir, &[&lane]);
        let lane_size = deep_prune_size(&lane_hash_dir);
        fs::remove_dir_all(&lane).unwrap();

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert_eq!(
            outcomes,
            vec![OrphanedBuildDir {
                path: lane_hash_dir.clone(),
                bytes: lane_size,
                kept: None,
            }]
        );
        assert!(
            !lane_hash_dir.exists(),
            "a deleted lane nested under a live main checkout must still be reclaimed"
        );
        assert!(
            main_hash_dir.exists(),
            "the main checkout's own hash dir must never be reclaimed"
        );
    }

    /// Fix round, card `20261115-b-673528`: a live workspace where one
    /// source file was later renamed or deleted must stay live — only one
    /// of its evidence lines now names a vanished path, but
    /// `scan_orphan_evidence` stops at the first *other* line still
    /// naming a file that exists, so the hash dir as a whole is kept.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_live_workspace_with_one_renamed_source_file() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let live = std::env::temp_dir().join(format!(
            "mv-live-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&live);
        fs::write(live.join("src/other.rs"), "").unwrap();

        let hash_dir_path = hash_dir(&shared_root, "2a", "11");
        let deps = hash_dir_path.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        fs::write(
            deps.join("renamed.d"),
            format!("{}/src/other.rs\n", live.display()),
        )
        .unwrap();
        fs::write(
            deps.join("surviving.d"),
            format!("{}/src/lib.rs\n", live.display()),
        )
        .unwrap();
        fs::remove_file(live.join("src/other.rs")).unwrap();

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert!(
            outcomes.is_empty(),
            "a live workspace missing one renamed source file must stay live"
        );
        assert!(hash_dir_path.exists());

        fs::remove_dir_all(&live).unwrap();
    }

    /// `count` junk dep-info files under `hash_dir`, each naming no
    /// absolute path at all — cheap filler that still counts as a
    /// build-evidence file `is_build_evidence` must open before it can
    /// tell there is nothing useful on that particular line. Named so a
    /// lexicographic walk reads every one of them before `marker_name`
    /// (chosen to sort last, e.g. `"zzz-live.d"`), the same way a real
    /// cargo build-dir's own fingerprint/dep-info files sort in no order
    /// that favors whichever one happens to name a still-live workspace.
    fn write_junk_evidence_files(hash_dir: &Path, count: usize) {
        let deps = hash_dir.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        for i in 0..count {
            fs::write(
                deps.join(format!("junk{i:05}.d")),
                "no-workspace-path-here\n",
            )
            .unwrap();
        }
    }

    /// Card `20261116-b-891676`: on the installed release, `--prune --yes`
    /// removed 5 merged lanes but froze none of their shared-build hash
    /// dirs in that same run — two big live hash dirs, sorted before the
    /// small vanished-lane ones, exhausted the single whole-root
    /// `BuildDirScanBudget` (4000 files total) before the vanished ones
    /// were ever read, and a dir this scan never finishes reading is
    /// always kept (`OrphanVerdict::cut_short`), never reclaimed. Rebuilds
    /// that exact shape: two hash dirs sorted first (`"00"`, `"01"`),
    /// each with 2200 junk evidence files (4400 total — over the old
    /// 4000-file cap) in front of one real live-workspace marker that
    /// sorts last within its own dir, then three small vanished-lane hash
    /// dirs (`"02"`..`"04"`) sorted after both. One reclaim pass must
    /// delete all three vanished dirs and keep both candidates sorted
    /// ahead of them — whether a candidate is kept because it is proven
    /// live or merely because its own per-dir cap cut the scan short
    /// before proving anything, either is the same safe "never deleted on
    /// a guess" outcome `OrphanVerdict::cut_short` already guarantees.
    #[test]
    fn prune_orphaned_build_dirs_reclaims_vanished_dirs_sorted_after_two_big_live_dirs() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        let mut live_dirs = Vec::new();
        for bucket in ["00", "01"] {
            let live = std::env::temp_dir().join(format!(
                "mv-live-workspace-{bucket}-{}-{}",
                std::process::id(),
                nonce()
            ));
            write_workspace_marker(&live);
            let big_hash_dir = hash_dir(&shared_root, bucket, "1");
            write_junk_evidence_files(&big_hash_dir, 2200);
            fs::write(
                big_hash_dir.join("debug/deps/zzz-live.d"),
                format!("{}/src/lib.rs\n", live.display()),
            )
            .unwrap();
            live_dirs.push((big_hash_dir, live));
        }

        let mut vanished_dirs = Vec::new();
        for (bucket, leaf) in [("02", "1"), ("03", "1"), ("04", "1")] {
            let vanished = std::env::temp_dir().join(format!(
                "mv-vanished-workspace-{bucket}-{}-{}",
                std::process::id(),
                nonce()
            ));
            write_workspace_marker(&vanished);
            let hash_dir_path = hash_dir(&shared_root, bucket, leaf);
            write_build_evidence(&hash_dir_path, &[&vanished]);
            let size = deep_prune_size(&hash_dir_path);
            fs::remove_dir_all(&vanished).unwrap();
            vanished_dirs.push((hash_dir_path, size));
        }

        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        let reclaimed_paths: std::collections::BTreeSet<_> =
            outcomes.iter().map(|o| o.path.clone()).collect();
        for (path, size) in &vanished_dirs {
            assert!(
                reclaimed_paths.contains(path),
                "{} must be reclaimed in the same pass as the two big dirs ahead of it",
                path.display()
            );
            assert!(!path.exists(), "a reclaimed vanished dir must be deleted");
            assert_eq!(
                outcomes.iter().find(|o| &o.path == path).unwrap().bytes,
                *size
            );
        }
        for (path, live) in &live_dirs {
            assert!(
                path.exists(),
                "a big dir sorted ahead of the vanished ones must survive"
            );
            fs::remove_dir_all(live).unwrap();
        }
    }

    /// Card `20261116-b-891676`'s second half: a lane `--prune --yes`
    /// removes in a run must have its own shared-build hash dir freed in
    /// that *same* run, not a later one — the combined `store.prune(...)`
    /// (removal) then `store.prune_orphaned_build_dirs(...)` (the global
    /// backstop sweep) call sequence the real CLI makes in one
    /// invocation. Two big junk-laden hash dirs sort ahead of the lane's
    /// own, the same shape as
    /// `prune_orphaned_build_dirs_reclaims_vanished_dirs_sorted_after_two_big_live_dirs`,
    /// so this also proves the per-dir cap does not regress the ordinary
    /// per-lane path (`scoped_lane_build_dir`, exercised inside
    /// `store.prune`) that a removed lane's own build-dir reclaim already
    /// relies on.
    #[test]
    fn prune_frees_a_just_removed_lanes_build_dir_in_the_same_run() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        for bucket in ["00", "01"] {
            let junk_hash_dir = hash_dir(&shared_root, bucket, "1");
            write_junk_evidence_files(&junk_hash_dir, 2200);
        }
        let lane_hash_dir = hash_dir(&shared_root, "5a", "2");
        write_build_evidence(&lane_hash_dir, &[&lane.path]);

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(!lane.path.exists(), "the lane itself must be removed");

        let orphaned = store.prune_orphaned_build_dirs(false).unwrap();
        assert!(
            !lane_hash_dir.exists(),
            "the lane's own build-dir must be freed in this same run, either by \
             `store.prune`'s own per-lane reclaim or by this backstop sweep"
        );
        for outcome in &orphaned {
            assert_ne!(
                outcome.path, lane_hash_dir,
                "the lane's build-dir was already freed by `store.prune` itself"
            );
        }
    }

    /// A hash dir laid out the way cargo really writes one, mirroring the
    /// lane 0.1.9 left behind (card `20261116-b-891676`): `CACHEDIR.TAG`
    /// and `tmp/` next to `debug/`, a `debug/.fingerprint/` tree of 1376
    /// files that name no absolute path (hashes and cargo's own relative
    /// JSON), a build-script dep-info under `build/`, a registry dep's
    /// dep-info, workspace members' dep-info with the *relative* source
    /// paths cargo gives rustc for them, and one `mv_engine` dep-info whose
    /// `include_str!` inputs are the only absolute `workspace` paths. Every
    /// file sorts so that `.fingerprint/` is read first, as it is on disk.
    /// The named `prompts/` files are created under `workspace` so they
    /// exist exactly as long as the workspace does.
    fn write_real_build_dir_layout(hash_dir: &Path, workspace: &Path, cargo_home: &Path) {
        let debug = hash_dir.join("debug");
        fs::create_dir_all(hash_dir.join("tmp")).unwrap();
        fs::write(
            hash_dir.join("CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n",
        )
        .unwrap();
        for i in 0..344 {
            let unit = debug.join(format!(".fingerprint/pkg{i:03}-{i:016x}"));
            fs::create_dir_all(&unit).unwrap();
            fs::write(unit.join(format!("dep-lib-pkg{i:03}")), [0u8, 1, 2, 0xff]).unwrap();
            fs::write(
                unit.join("invoked.timestamp"),
                "This file has an mtime of when this was started.",
            )
            .unwrap();
            fs::write(unit.join(format!("lib-pkg{i:03}")), format!("{i:016x}")).unwrap();
            fs::write(
                unit.join(format!("lib-pkg{i:03}.json")),
                format!(r#"{{"rustc":1,"path":{i},"local":[{{"CheckDepInfo":{{"dep_info":"debug/.fingerprint/pkg{i:03}-{i:016x}/dep-lib-pkg{i:03}"}}}}]}}"#),
            )
            .unwrap();
        }
        let out = |name: &str| debug.join(format!("deps/{name}")).display().to_string();
        let script = debug.join("build/serde-0123456789abcdef");
        fs::create_dir_all(&script).unwrap();
        let registry = cargo_home.join("registry/src/index.crates.io-0000000000000000/serde-1.0.0");
        fs::write(
            script.join("build_script_build-0123456789abcdef.d"),
            format!("{}: {}/build.rs\n", script.display(), registry.display()),
        )
        .unwrap();
        fs::create_dir_all(debug.join("deps")).unwrap();
        fs::write(
            debug.join("deps/serde-0123456789abcdef.d"),
            format!(
                "{}: {}/src/lib.rs\n",
                out("libserde.rmeta"),
                registry.display()
            ),
        )
        .unwrap();
        fs::write(
            debug.join("deps/mv_core-0123456789abcdef.d"),
            format!(
                "{}: crates/mv-core/src/lib.rs\n\ncrates/mv-core/src/lib.rs:\n",
                out("libmv_core.rmeta")
            ),
        )
        .unwrap();
        fs::create_dir_all(workspace.join("prompts")).unwrap();
        let prompt = workspace.join("prompts/role-builder.md");
        fs::write(&prompt, "builder\n").unwrap();
        fs::write(
            debug.join("deps/mv_engine-0123456789abcdef.d"),
            format!(
                "{}: crates/mv-engine/src/lib.rs {prompt}\n\ncrates/mv-engine/src/lib.rs:\n{prompt}:\n\n# env-dep:CARGO_PKG_NAME=mv-engine\n",
                out("libmv_engine.rmeta"),
                prompt = prompt.display()
            ),
        )
        .unwrap();
    }

    /// Card `20261116-b-891676`, observed on installed 0.1.9: `--prune
    /// --yes` removed a merged lane and printed nothing about its
    /// build-dir, which stayed. The home config sets an un-namespaced
    /// `build-dir`, the repo config overrides it with
    /// `{workspace-path-hash}`, and the lane's own hash dir sorts first
    /// (`02/…`). Its `.fingerprint/` tree used up the whole
    /// `PerDirFileBudget` before the scan reached the one `deps/*.d` file
    /// naming the lane, so both the per-lane reclaim and the orphan sweep
    /// gave up without a word. Runs the same `prune` then
    /// `prune_orphaned_build_dirs` sequence the CLI does.
    #[test]
    fn prune_frees_a_lane_build_dir_behind_a_real_fingerprint_tree() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        fs::write(
            cargo_home.dir.join("config.toml"),
            "[build]\nbuild-dir = \"{cargo-cache-home}/shared-build\"\n",
        )
        .unwrap();
        let _build_dir = BuildDirEnvGuard::cleared();
        // Tracked, so the lane checks it out clean and the layout below
        // rewrites it with identical content.
        fs::create_dir_all(repo.0.join("prompts")).unwrap();
        fs::write(repo.0.join("prompts/role-builder.md"), "builder\n").unwrap();
        git(&repo.0, &["add", "prompts"], None).unwrap();
        git(&repo.0, &["commit", "-m", "prompts"], None).unwrap();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        assert_eq!(shared_root, cargo_home.dir.join("shared-build"));
        let legacy = shared_root.join("debug/deps");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(
            legacy.join("legacy.d"),
            format!("{}/prompts/role-builder.md\n", lane.path.display()),
        )
        .unwrap();
        let lane_hash_dir = shared_root.join("02/7179bbe4c308d8");
        write_real_build_dir_layout(&lane_hash_dir, &lane.path, &cargo_home.dir);
        let main_hash_dir = shared_root.join("4b/be360cdd949358");
        write_real_build_dir_layout(&main_hash_dir, &repo.0, &cargo_home.dir);
        let lane_build_size = deep_prune_size(&lane_hash_dir);

        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed,
                build_dir_kept,
                ..
            } => {
                assert_eq!(*build_dir_kept, None);
                assert_eq!(*build_dir_freed, Some(lane_build_size));
            }
            other => panic!("expected Removed, got {other:?}"),
        }
        assert!(!lane.path.exists(), "the lane itself must be removed");
        assert!(
            !lane_hash_dir.exists(),
            "the lane's build-dir must be freed by the same prune that removed the lane"
        );
        assert!(store.prune_orphaned_build_dirs(false).unwrap().is_empty());
        assert!(
            main_hash_dir.exists(),
            "the main checkout's build-dir must survive"
        );
        assert!(
            legacy.exists(),
            "the legacy un-namespaced tree must survive"
        );
    }

    /// The orphan sweep's half of the same 0.1.9 layout: a workspace
    /// removed outside `--prune` leaves a real-shaped hash dir whose only
    /// absolute evidence sits behind a 1376-file `.fingerprint/` tree. It
    /// must be reclaimed, not kept as a scan cut short.
    #[test]
    fn prune_orphaned_build_dirs_reclaims_a_real_layout_behind_its_fingerprint_tree() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-lane-{}-{}",
            std::process::id(),
            nonce()
        ));
        let hash_dir_path = shared_root.join("02/7179bbe4c308d8");
        write_real_build_dir_layout(&hash_dir_path, &vanished, &cargo_home.dir);
        let size = deep_prune_size(&hash_dir_path);

        assert!(
            store.prune_orphaned_build_dirs(true).unwrap().is_empty(),
            "a live workspace's hash dir must be kept"
        );
        fs::remove_dir_all(&vanished).unwrap();
        let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
        assert_eq!(
            outcomes,
            vec![OrphanedBuildDir {
                path: hash_dir_path.clone(),
                bytes: size,
                kept: None,
            }]
        );
        assert!(!hash_dir_path.exists());
    }

    /// Critic `5a1612`'s High (card `20261119-b-087c6c`): the same real
    /// layout, but the workspace stays and only its `include_str!` input —
    /// the one absolute workspace path cargo wrote — is deleted. Its
    /// members' own dep-info names `crates/…` relative to the workspace
    /// root, so the vanished prompt never proved the workspace gone; on
    /// 0.1.11 the sweep read only that absolute path and deleted a live
    /// workspace's build. Kept both when the workspace root holds its
    /// `Cargo.toml` and, separately, when only its relative sources
    /// resolve.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_live_real_layout_whose_only_absolute_input_vanished() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        let store = repo.store();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");

        type MarkLive = fn(&Path);
        let live_marker: [(&str, MarkLive); 2] = [
            ("Cargo.toml", |workspace| {
                fs::write(workspace.join("Cargo.toml"), "[workspace]\n").unwrap();
            }),
            ("relative sources", |workspace| {
                for member in ["mv-core", "mv-engine"] {
                    let src = workspace.join(format!("crates/{member}/src"));
                    fs::create_dir_all(&src).unwrap();
                    fs::write(src.join("lib.rs"), "").unwrap();
                }
            }),
        ];
        for (label, mark_live) in live_marker {
            let workspace = std::env::temp_dir().join(format!(
                "mv-live-real-layout-{}-{}",
                std::process::id(),
                nonce()
            ));
            let hash_dir_path = shared_root.join("02/7179bbe4c308d8");
            write_real_build_dir_layout(&hash_dir_path, &workspace, &cargo_home.dir);
            mark_live(&workspace);
            fs::remove_file(workspace.join("prompts/role-builder.md")).unwrap();

            let outcomes = store.prune_orphaned_build_dirs(false).unwrap();
            let workspace_existed = workspace.exists();
            fs::remove_dir_all(&workspace).unwrap();
            assert!(workspace_existed);
            assert!(outcomes.is_empty(), "{label}: {outcomes:?}");
            assert!(
                hash_dir_path.exists(),
                "{label}: a live workspace's build-dir must be kept"
            );
            fs::remove_dir_all(&hash_dir_path).unwrap();
        }
    }

    /// Critic `5a1612`'s Medium (card `20261119-b-087c6c`): a lane whose
    /// dep-info spells its path another way (here through a symlink, as
    /// `/var` vs `/private/var` does on macOS) misses the per-lane lookup,
    /// and the 0.1.11 sweep then freed that same live lane's build-dir in
    /// the same run — `kept build-dir` followed by `freed`. The miss's own
    /// reason names no machine path, carries no hash dir, and the sweep now
    /// keeps the dir: the lane still holds its `Cargo.toml`.
    #[test]
    fn prune_never_reports_a_lane_build_dir_kept_while_the_sweep_frees_it() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        fs::write(repo.0.join("Cargo.toml"), "[workspace]\n").unwrap();
        git(&repo.0, &["add", "Cargo.toml"], None).unwrap();
        git(&repo.0, &["commit", "-m", "workspace"], None).unwrap();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let spelled = std::env::temp_dir().join(format!(
            "mv-lane-spelling-{}-{}",
            std::process::id(),
            nonce()
        ));
        std::os::unix::fs::symlink(&lane.path, &spelled).unwrap();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = shared_root.join("02/7179bbe4c308d8");
        write_real_build_dir_layout(&lane_hash_dir, &spelled, &cargo_home.dir);
        fs::remove_file(lane.path.join("prompts/role-builder.md")).unwrap();

        let outcomes = store.prune(&mut Observer::idle(), true, true).unwrap();
        let reported: Vec<PathBuf> = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                PruneOutcome::Kept { build_dir, .. }
                | PruneOutcome::WouldRemove { build_dir, .. }
                | PruneOutcome::WouldArchive { build_dir, .. }
                | PruneOutcome::Removed { build_dir, .. }
                | PruneOutcome::Archived { build_dir, .. } => build_dir.clone(),
            })
            .collect();
        let swept = store
            .prune_orphaned_build_dirs_skipping(false, &reported)
            .unwrap();
        fs::remove_file(&spelled).unwrap();
        assert!(swept.is_empty(), "{swept:?}");
        assert!(
            lane_hash_dir.exists(),
            "a live lane's build-dir must survive the sweep"
        );
        match &outcomes[0] {
            PruneOutcome::WouldRemove {
                build_dir_freed: None,
                build_dir_kept: Some(reason),
                build_dir: None,
                ..
            } => assert!(
                !reason.contains(&shared_root.display().to_string()),
                "reason was: {reason}"
            ),
            other => panic!("expected WouldRemove with a kept reason, got {other:?}"),
        }
    }

    /// Critic `5a1612`'s Low (card `20261119-b-087c6c`): the per-lane
    /// lookup matched any `.d` line containing `{lane}/` as a raw
    /// substring, so another hash dir sorted first whose dep-info only
    /// *mentions* the lane — an env-dep value, a path that embeds it —
    /// resolved as the lane's own build-dir, the one `--prune --yes`
    /// deletes. It now matches whole paths, filtered the way the sweep
    /// filters them.
    #[test]
    fn find_build_dir_ignores_dep_info_that_only_mentions_the_lane() {
        let _cargo_home = CargoHomeEnvGuard::new();
        let root = std::env::temp_dir().join(format!(
            "mv-lane-mention-{}-{}",
            std::process::id(),
            nonce()
        ));
        let lane = root.join("lane");
        let shared_root = root.join("shared-build");
        let mentions = hash_dir(&shared_root, "02", "1");
        fs::create_dir_all(mentions.join("debug/deps")).unwrap();
        fs::write(
            mentions.join("debug/deps/other.d"),
            format!(
                "{out}: /mirror{lane}/src/lib.rs\n\n/mirror{lane}/src/lib.rs:\n\n# env-dep:LANE_DIR={lane}/src\n",
                out = mentions.join("debug/deps/libother.rmeta").display(),
                lane = lane.display()
            ),
        )
        .unwrap();
        let owns = hash_dir(&shared_root, "03", "1");
        write_build_evidence(&owns, &[&lane]);

        let found = find_build_dir(&shared_root, &lane);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(found, Some(owns));
    }

    /// `workspace_path_hash` is cargo's own `{workspace-path-hash}` (card
    /// `20261119-b-4ec912`). The hasher vector is `rustc-stable-hash`'s own
    /// `test_hash_usize` (`[12037165114281468837, 3094087741167521712]`),
    /// read through the same `Hasher::finish` halves-combining cargo's
    /// `short_hash` reads. The `/private/tmp` manifest was cross-checked
    /// once against a real `cargo build`/`cargo metadata` of a tiny
    /// workspace there (`build_directory` `…/sb/84/49e87fc242ff5b`); the
    /// `/work` one is a neutral stand-in for a lane path, computed here.
    #[test]
    fn workspace_path_hash_matches_cargo() {
        let mut hasher = rustc_stable_hash::StableSipHasher128::new();
        0xABCD_EF01_usize.hash(&mut hasher);
        assert_eq!(
            Hasher::finish(&hasher),
            12037165114281468837_u64
                .wrapping_mul(3)
                .wrapping_add(3094087741167521712)
        );
        for (manifest, expected) in [
            (
                "/private/tmp/mv-hash-xcheck/ws/Cargo.toml",
                "84/49e87fc242ff5b",
            ),
            (
                "/work/project/.multiverse/worktrees/20261119-builder-4f2684/Cargo.toml",
                "a3/5b99de94ab95ff",
            ),
        ] {
            assert_eq!(
                workspace_path_hash(Path::new(manifest)),
                PathBuf::from(expected),
                "{manifest}"
            );
        }
    }

    /// A vanished workspace's manifest canonicalizes through its nearest
    /// still-existing ancestor, so a lane removed in an earlier run still
    /// computes the hash dir cargo built it into while it existed.
    #[test]
    fn computed_hash_dir_is_the_same_before_and_after_the_workspace_vanishes() {
        let root = std::env::temp_dir().join(format!(
            "mv-computed-vanish-{}-{}",
            std::process::id(),
            nonce()
        ));
        let lane = root.join("worktrees/lane");
        write_workspace_marker(&lane);
        let shared_root = Path::new("/shared-build");
        let before = computed_hash_dir(shared_root, &lane);
        let canonical = fs::canonicalize(lane.join("Cargo.toml")).unwrap();
        fs::remove_dir_all(root.join("worktrees")).unwrap();
        let after = computed_hash_dir(shared_root, &lane);
        fs::remove_dir_all(&root).unwrap();
        assert_eq!(before, after);
        assert_eq!(before, shared_root.join(workspace_path_hash(&canonical)));
    }

    /// Card `20261119-b-4ec912`, observed on 0.1.13 under host load ~20-30:
    /// the per-lane lookup printed "scan budget ran out" and kept a merged
    /// lane's 2 GB build-dir. The lane's own computed hash dir is now found
    /// without the scan at all — here with a scan budget already spent,
    /// which on its own leaves the scan inconclusive. A computed dir that
    /// exists but does not name the lane is kept, never traded for a scan.
    #[test]
    fn scoped_lane_build_dir_resolves_the_computed_hash_dir_with_a_zero_scan_budget() {
        let _cargo_home = CargoHomeEnvGuard::new();
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "mv-computed-hash-{}-{}",
                std::process::id(),
                nonce()
            ));
        let lane = root.join("lane");
        write_workspace_marker(&lane);
        let shared_root = root.join("shared-build");
        let other = hash_dir(&shared_root, "00", "1");
        write_build_evidence(&other, &[&root.join("other")]);
        let computed = computed_hash_dir(&shared_root, &lane);
        write_build_evidence(&computed, &[&lane]);
        let spent: fn(usize) -> BuildDirScanBudget = |_| BuildDirScanBudget {
            deadline: Instant::now(),
        };

        let scan = find_build_dir_evidence_within(&shared_root, &lane, None, spent);
        let resolved = scoped_lane_build_dir_within(
            &shared_root,
            None,
            &lane,
            spent,
            BuildDirScanBudget::for_complete_reads,
        );
        write_build_evidence(&computed, &[&root.join("other")]);
        let unproven = scoped_lane_build_dir_within(
            &shared_root,
            None,
            &lane,
            spent,
            BuildDirScanBudget::for_complete_reads,
        );
        fs::remove_dir_all(&root).unwrap();

        assert!(matches!(scan, BuildDirLookup::Inconclusive { .. }));
        assert_eq!(resolved.ok(), Some(computed.clone()));
        match unproven {
            Err(BuildDirReclaim {
                freed: None,
                kept: Some(reason),
                path: Some(path),
            }) => {
                assert_eq!(path, computed);
                assert!(reason.contains("does not name this lane"), "{reason}");
            }
            other => panic!("expected a kept computed dir, got {other:?}"),
        }
    }

    /// A repo laid out like this one: the main checkout holds `Cargo.toml`
    /// and the member sources a lane's dep-info names relative to its root,
    /// plus the `prompts/` file `write_real_build_dir_layout` names
    /// absolutely (tracked, so a lane checks it out clean).
    fn commit_nested_workspace(repo: &TempRepo) {
        fs::write(repo.0.join("Cargo.toml"), "[workspace]\n").unwrap();
        for member in ["mv-core", "mv-engine"] {
            let src = repo.0.join(format!("crates/{member}/src"));
            fs::create_dir_all(&src).unwrap();
            fs::write(src.join("lib.rs"), "").unwrap();
        }
        fs::create_dir_all(repo.0.join("prompts")).unwrap();
        fs::write(repo.0.join("prompts/role-builder.md"), "builder\n").unwrap();
        git(&repo.0, &["add", "."], None).unwrap();
        git(&repo.0, &["commit", "-m", "workspace"], None).unwrap();
    }

    /// Card `20261119-b-4ec912`: plain `--prune` (no `--yes`) removes a
    /// lane but only measures its build-dir, and that run's own sweep
    /// passes it over (card `20261119-b-c3f9f5`). Nested in the main
    /// checkout, the lane's relative sources then always read as possibly
    /// live to the generic sweep, so no later run freed it. The next run
    /// now frees it through the lane's computed hash dir, whose dep-info
    /// names only the vanished lane.
    #[test]
    fn prune_orphaned_build_dirs_frees_an_earlier_removed_lanes_computed_hash_dir() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        commit_nested_workspace(&repo);

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = computed_hash_dir(&shared_root, &lane.path);
        write_real_build_dir_layout(&lane_hash_dir, &lane.path, &cargo_home.dir);
        let main_hash_dir = computed_hash_dir(&shared_root, &repo.0);
        write_real_build_dir_layout(&main_hash_dir, &repo.0, &cargo_home.dir);
        let size = deep_prune_size(&lane_hash_dir);

        let outcomes = store
            .prune_with_ignored(&mut Observer::idle(), true, false, false, false)
            .unwrap();
        let reported = match &outcomes[0] {
            PruneOutcome::Removed {
                build_dir_freed: Some(freed),
                build_dir: Some(dir),
                ..
            } => {
                assert_eq!(*freed, size);
                dir.clone()
            }
            other => panic!("expected Removed with a measured build-dir, got {other:?}"),
        };
        assert_eq!(reported, lane_hash_dir);
        assert!(!lane.path.exists(), "the lane itself must be removed");
        assert!(store
            .prune_orphaned_build_dirs_skipping(false, &[reported])
            .unwrap()
            .is_empty());
        assert!(lane_hash_dir.exists(), "a measure-only run keeps it");

        let expected = vec![OrphanedBuildDir {
            path: lane_hash_dir.clone(),
            bytes: size,
            kept: None,
        }];
        assert_eq!(store.prune_orphaned_build_dirs(true).unwrap(), expected);
        assert!(lane_hash_dir.exists(), "a preview never deletes");
        assert_eq!(store.prune_orphaned_build_dirs(false).unwrap(), expected);
        assert!(!lane_hash_dir.exists(), "the next run frees it");
        assert!(
            main_hash_dir.exists(),
            "the main checkout's build-dir must survive"
        );
    }

    /// Card `20261119-b-4ec912`: a removed lane's computed hash dir whose
    /// dep-info also names a still-existing workspace is kept — the hash
    /// proves only where cargo would build the lane, not that nothing else
    /// was ever built there — while the same dir naming only the lane is
    /// freed.
    #[test]
    fn prune_orphaned_build_dirs_keeps_a_computed_hash_dir_that_also_names_a_live_workspace() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        commit_nested_workspace(&repo);

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(!lane.path.exists());

        let live = std::env::temp_dir().join(format!(
            "mv-live-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&live);
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = computed_hash_dir(&shared_root, &lane.path);
        write_build_evidence(&lane_hash_dir, &[&lane.path, &live]);

        let kept = store.prune_orphaned_build_dirs(false).unwrap();
        let kept_dir = lane_hash_dir.exists();
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let freed = store.prune_orphaned_build_dirs(false).unwrap();
        fs::remove_dir_all(&live).unwrap();

        assert!(kept.is_empty(), "{kept:?}");
        assert!(kept_dir, "a dir naming a live workspace must be kept");
        assert_eq!(freed.len(), 1, "{freed:?}");
        assert_eq!(freed[0].path, lane_hash_dir);
        assert!(!lane_hash_dir.exists());
    }

    /// Card `20261119-b-a9f95f`, item 1: a lane's computed hash dir whose
    /// read runs out of budget before reaching the dep-info that names the
    /// lane used to be reported as "does not name this lane", and its
    /// `Some(computed)` path put it on the same run's sweep skip list. A
    /// cut-short read now says the scan ran out and resolves no dir.
    #[test]
    fn scoped_lane_build_dir_reports_a_cut_short_computed_read_as_the_scan_running_out() {
        let cargo_home = CargoHomeEnvGuard::new();
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "mv-computed-cut-{}-{}",
                std::process::id(),
                nonce()
            ));
        let lane = root.join("lane");
        let shared_root = root.join("shared-build");
        let computed = computed_hash_dir(&shared_root, &lane);
        write_real_build_dir_layout(&computed, &lane, &cargo_home.dir);
        let spent: fn(usize) -> BuildDirScanBudget = |_| BuildDirScanBudget {
            deadline: Instant::now(),
        };

        let cut_short = scoped_lane_build_dir_within(&shared_root, None, &lane, spent, spent);
        let complete = scoped_lane_build_dir_within(
            &shared_root,
            None,
            &lane,
            spent,
            BuildDirScanBudget::for_complete_reads,
        );
        fs::remove_dir_all(&root).unwrap();

        match cut_short {
            Err(BuildDirReclaim {
                freed: None,
                kept: Some(reason),
                path,
            }) => {
                assert!(reason.contains("scan budget ran out"), "{reason}");
                assert!(!reason.contains("does not name"), "{reason}");
                assert_eq!(path, None, "an unread dir must not join the skip list");
            }
            other => panic!("expected a kept, unresolved build-dir, got {other:?}"),
        }
        assert_eq!(complete.ok(), Some(computed));
    }

    /// Card `20261119-b-a9f95f`, item 2: a lane that ran this repo's full
    /// check suite left 866 `.d` files outside `.fingerprint/` in its hash
    /// dir — more than `PerDirFileBudget::FILE_BUDGET`. Its computed dir is
    /// one known dir, so it is read in full: the dep-info naming the lane
    /// is found even sorted after 1000 other `.d` files.
    #[test]
    fn scoped_lane_build_dir_reads_a_computed_hash_dir_past_the_per_dir_file_cap() {
        let _cargo_home = CargoHomeEnvGuard::new();
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "mv-computed-big-{}-{}",
                std::process::id(),
                nonce()
            ));
        let lane = root.join("lane");
        write_workspace_marker(&lane);
        let shared_root = root.join("shared-build");
        let computed = computed_hash_dir(&shared_root, &lane);
        write_junk_evidence_files(&computed, PerDirFileBudget::FILE_BUDGET * 2);
        fs::write(
            computed.join("debug/deps/zzz.d"),
            format!("{}/src/lib.rs\n", lane.display()),
        )
        .unwrap();

        let resolved = scoped_lane_build_dir(&shared_root, None, &lane);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!(resolved.ok(), Some(computed));
    }

    /// Card `20261119-b-a9f95f`, item 2: a vanished lane's computed hash
    /// dir holding more `.d` files than `PerDirFileBudget::FILE_BUDGET`,
    /// every one naming only that lane, used to be cut short and kept by
    /// the vanished-lane sweep — and the generic sweep keeps it too, since
    /// its relative sources resolve under the main checkout. It is now
    /// read in full and freed.
    #[test]
    fn prune_orphaned_build_dirs_frees_a_vanished_lanes_computed_dir_past_the_file_cap() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();
        commit_nested_workspace(&repo);

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(!lane.path.exists());

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = computed_hash_dir(&shared_root, &lane.path);
        let deps = lane_hash_dir.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();
        let prompt = lane.path.join("prompts/role-builder.md");
        for i in 0..1100 {
            fs::write(
                deps.join(format!("unit{i:04}.d")),
                format!(
                    "{out}: crates/mv-core/src/lib.rs {prompt}\n",
                    out = deps.join(format!("libunit{i:04}.rmeta")).display(),
                    prompt = prompt.display()
                ),
            )
            .unwrap();
        }
        let size = deep_prune_size(&lane_hash_dir);

        let freed = store.prune_orphaned_build_dirs(false).unwrap();

        assert_eq!(
            freed,
            vec![OrphanedBuildDir {
                path: lane_hash_dir.clone(),
                bytes: size,
                kept: None,
            }]
        );
        assert!(!lane_hash_dir.exists(), "a dir read in full must be freed");
    }

    /// Restores `0o755` on each dir on drop, so a test that made one
    /// read-only still leaves nothing behind in `TMPDIR` when it fails.
    struct ReadOnlyDirs(Vec<PathBuf>);
    impl ReadOnlyDirs {
        fn new(dirs: Vec<PathBuf>) -> Self {
            for dir in &dirs {
                fs::set_permissions(dir, fs::Permissions::from_mode(0o555)).unwrap();
            }
            Self(dirs)
        }
    }
    impl Drop for ReadOnlyDirs {
        fn drop(&mut self) {
            for dir in &self.0 {
                let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o755));
            }
        }
    }

    /// Card `20261119-b-a9f95f`, item 4: both orphan-sweep loops — the
    /// vanished-lane one and the generic one — ignored a failed
    /// `deep_prune_remove` and reported the dir's bytes as freed while it
    /// was still on disk. A read-only subdir now makes each one report the
    /// dir kept, with the reason.
    #[test]
    fn prune_orphaned_build_dirs_reports_a_failed_delete_as_kept_in_both_loops() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let outcomes = store.prune(&mut Observer::idle(), true, false).unwrap();
        assert!(matches!(&outcomes[0], PruneOutcome::Removed { .. }));
        assert!(!lane.path.exists());

        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        let lane_hash_dir = computed_hash_dir(&shared_root, &lane.path);
        write_build_evidence(&lane_hash_dir, &[&lane.path]);
        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        let orphan_hash_dir = hash_dir(&shared_root, "2a", "9");
        write_build_evidence(&orphan_hash_dir, &[&vanished]);
        let lane_size = deep_prune_size(&lane_hash_dir);
        let orphan_size = deep_prune_size(&orphan_hash_dir);
        let _read_only = ReadOnlyDirs::new(vec![
            lane_hash_dir.join("debug/deps"),
            orphan_hash_dir.join("debug/deps"),
        ]);

        let swept = store.prune_orphaned_build_dirs(false).unwrap();

        let summary: Vec<_> = swept
            .iter()
            .map(|outcome| (outcome.path.clone(), outcome.bytes))
            .collect();
        assert_eq!(
            summary,
            [
                (lane_hash_dir.clone(), lane_size),
                (orphan_hash_dir.clone(), orphan_size)
            ]
        );
        for outcome in &swept {
            let reason = outcome.kept.as_deref().unwrap_or_default();
            assert!(
                reason.starts_with("removing its hash dir failed: "),
                "{outcome:?}"
            );
        }
        assert!(lane_hash_dir.join("debug/deps/fixture.d").exists());
        assert!(orphan_hash_dir.join("debug/deps/fixture.d").exists());
    }

    /// `dep_info_paths` reads rustc's own dep-info shape: rule targets and
    /// prerequisites alike, `\ `-escaped spaces kept inside one path, and
    /// `#` lines skipped whole.
    #[test]
    fn dep_info_paths_unescapes_spaces_and_skips_comment_lines() {
        let content = "/b/out.rmeta: crates/a/src/lib.rs /w/My\\ Project/x.md\n\n\
                       crates/a/src/lib.rs:\n/w/My\\ Project/x.md:\n\n\
                       # env-dep:HOME=/w/home\n";
        assert_eq!(
            dep_info_paths(content),
            [
                "/b/out.rmeta",
                "crates/a/src/lib.rs",
                "/w/My Project/x.md",
                "crates/a/src/lib.rs",
                "/w/My Project/x.md",
            ]
        );
    }

    /// A lane whose build-dir `--prune` cannot find or cannot prove safe
    /// now says so (card `20261116-b-891676`): a lane that never built
    /// reports the complete miss, and a lane whose dep-info sits behind
    /// more than `PerDirFileBudget::FILE_BUDGET` other `.d` files reports
    /// the scan running out — never a silent `None`.
    #[test]
    fn prune_reports_why_it_kept_a_lanes_build_dir() {
        let repo = TempRepo::new();
        write_build_dir_template(
            &repo,
            "{cargo-cache-home}/shared-build/{workspace-path-hash}",
        );
        let _cargo_home = CargoHomeEnvGuard::new();
        let _build_dir = BuildDirEnvGuard::cleared();

        let store = repo.store();
        let lane = store.create("recorded", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n",
        )
        .unwrap();
        let shared_root = shared_build_root(&repo.0).expect("shared root resolves");
        fs::create_dir_all(&shared_root).unwrap();

        match &store.prune(&mut Observer::idle(), true, true).unwrap()[0] {
            PruneOutcome::WouldRemove {
                build_dir_freed: None,
                build_dir_kept: Some(reason),
                ..
            } => {
                assert_eq!(
                    reason, "no shared-build hash dir names this lane in its dep-info",
                    "a kept reason never names a machine path"
                );
            }
            other => panic!("expected WouldRemove with a kept reason, got {other:?}"),
        }

        let lane_hash_dir = hash_dir(&shared_root, "02", "1");
        write_junk_evidence_files(&lane_hash_dir, PerDirFileBudget::FILE_BUDGET + 1);
        fs::write(
            lane_hash_dir.join("debug/deps/zzz.d"),
            format!("{}/src/lib.rs\n", lane.path.display()),
        )
        .unwrap();
        match &store.prune(&mut Observer::idle(), true, true).unwrap()[0] {
            PruneOutcome::WouldRemove {
                build_dir_freed: None,
                build_dir_kept: Some(reason),
                ..
            } => assert!(
                reason.contains("scan budget ran out"),
                "reason was: {reason}"
            ),
            other => panic!("expected WouldRemove with a kept reason, got {other:?}"),
        }
        assert!(lane_hash_dir.exists());
    }

    /// Nexus mutated `hash_dir_is_orphaned` to ignore `verdict.cut_short`
    /// entirely (`verdict.saw_dead && !verdict.saw_live`), and every
    /// existing test still passed — none of them ever exhausted the
    /// budget mid-scan. A per-dir cut-short scan is a real risk on a real,
    /// multi-GB shared root (card `20261115-b-673528`, fix round): a hash
    /// dir whose live evidence sorts after its dead evidence must stay
    /// kept rather than being judged on only the evidence this scan
    /// reached before running out. Injects a `PerDirFileBudget` with one
    /// file of headroom directly — `PerDirFileBudget::new`'s own limit
    /// (500) is far too generous to exhaust deterministically in a unit
    /// test — so the scan reads exactly the dead evidence file
    /// (`a-dead.d` sorts first) before running out, never reaching the
    /// live one (`b-live.d`) that sorts after it. The whole-scan `budget`
    /// is given generous headroom so only the per-dir cap is in play.
    #[test]
    fn hash_dir_is_orphaned_keeps_a_budget_cut_short_scan_even_with_dead_evidence_seen_first() {
        let dir = std::env::temp_dir().join(format!(
            "mv-cut-short-hash-dir-{}-{}",
            std::process::id(),
            nonce()
        ));
        let deps = dir.join("debug/deps");
        fs::create_dir_all(&deps).unwrap();

        let vanished = std::env::temp_dir().join(format!(
            "mv-vanished-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&vanished);
        fs::remove_dir_all(&vanished).unwrap();
        let live = std::env::temp_dir().join(format!(
            "mv-live-workspace-{}-{}",
            std::process::id(),
            nonce()
        ));
        write_workspace_marker(&live);

        fs::write(
            deps.join("a-dead.d"),
            format!("{}/src/lib.rs\n", vanished.display()),
        )
        .unwrap();
        fs::write(
            deps.join("b-live.d"),
            format!("{}/src/lib.rs\n", live.display()),
        )
        .unwrap();

        let mut budget = BuildDirScanBudget::for_candidates(1);
        let mut per_dir = PerDirFileBudget { files_remaining: 1 };
        assert!(
            !hash_dir_is_orphaned(&dir, None, None, &mut budget, &mut per_dir),
            "a scan cut short before reaching its live evidence must never read as orphaned"
        );

        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&live).unwrap();
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
            .prune_with_ignored(&mut Observer::idle(), true, false, true, true)
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
        let outcomes = store.prune_with_ignored(&mut Observer::idle(), true, false, true, true);
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

    // A same-uid process that has made itself non-dumpable (e.g. a runner
    // helper under `PR_SET_DUMPABLE` 0) hides its `/proc/<pid>/cwd` link
    // behind `EACCES` even from its own uid; only `root` is exempt, so this
    // only reproduces the gap when the test itself runs unprivileged, which
    // is how `cargo test --workspace` runs on `ubuntu-latest`.
    #[cfg(target_os = "linux")]
    #[test]
    fn process_cwd_skips_a_same_uid_non_dumpable_process() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let mut child = Command::new("python3")
            .args([
                "-c",
                "import ctypes,time;ctypes.CDLL(None).prctl(4, 0, 0, 0, 0);time.sleep(20)",
            ])
            .current_dir(&lane.path)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(500));
        let result = has_process_cwd(&lane.path);
        child.kill().unwrap();
        child.wait().unwrap();
        result.unwrap();
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

    // card 20261108-b-2416b2: `disposable_ignored` matched only root-level
    // prefixes, so nested Python bytecode and a nested cargo `target/` (as
    // under `conformance/history/runner/target/`) still read as "unrescued
    // ignored work" and blocked prune on an otherwise-clean, merged lane.
    #[test]
    fn nested_pycache_and_cargo_target_are_disposable_but_other_nested_ignored_files_still_hold_work(
    ) {
        let repo = TempRepo::new();
        fs::write(
            repo.0.join(".gitignore"),
            ".multiverse/\nignored/\n__pycache__/\n*.pyc\ntarget/\n",
        )
        .unwrap();
        git(&repo.0, &["add", ".gitignore"], None).unwrap();
        git(&repo.0, &["commit", "-m", "nested build ignores"], None).unwrap();
        let store = repo.store();

        let lane = store.create("pycache", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("conformance/__pycache__")).unwrap();
        fs::write(
            lane.path
                .join("conformance/__pycache__/normalize.cpython-315.pyc"),
            "bytecode",
        )
        .unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Removed { .. }
        ));
        assert!(!lane.path.exists());

        let lane = store.create("cargo-target", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("conformance/history/runner/target/debug")).unwrap();
        fs::write(
            lane.path
                .join("conformance/history/runner/target/debug/build-id"),
            "artifact",
        )
        .unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Removed { .. }
        ));
        assert!(!lane.path.exists());

        let lane = store.create("mixed", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("conformance/__pycache__")).unwrap();
        fs::write(
            lane.path
                .join("conformance/__pycache__/normalize.cpython-315.pyc"),
            "bytecode",
        )
        .unwrap();
        fs::create_dir_all(lane.path.join("ignored")).unwrap();
        fs::write(lane.path.join("ignored/secret"), "unrescued").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("unrescued ignored file"))
        );
        assert!(lane.path.exists());
    }

    // card 20261110-b-ef4bed: `node_modules` is installed by the project's
    // lane_setup and is regenerable, but it read as "unrescued ignored work"
    // the same way an un-rescued nested `target/` or `__pycache__` once did,
    // blocking prune on an otherwise-clean, merged lane.
    #[test]
    fn node_modules_is_disposable_but_other_nested_ignored_files_still_hold_work() {
        let repo = TempRepo::new();
        fs::write(
            repo.0.join(".gitignore"),
            ".multiverse/\nignored/\nnode_modules/\n",
        )
        .unwrap();
        git(&repo.0, &["add", ".gitignore"], None).unwrap();
        git(&repo.0, &["commit", "-m", "node_modules ignore"], None).unwrap();
        let store = repo.store();

        let lane = store.create("root-node-modules", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("node_modules/.bin")).unwrap();
        fs::write(
            lane.path.join("node_modules/.bin/baseline-browser-mapping"),
            "#!/bin/sh\n",
        )
        .unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Removed { .. }
        ));
        assert!(!lane.path.exists());

        let lane = store.create("nested-node-modules", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("web/node_modules/.bin")).unwrap();
        fs::write(
            lane.path
                .join("web/node_modules/.bin/baseline-browser-mapping"),
            "#!/bin/sh\n",
        )
        .unwrap();
        assert!(matches!(
            &store.prune(&mut Observer::idle(), true, false).unwrap()[0],
            PruneOutcome::Removed { .. }
        ));
        assert!(!lane.path.exists());

        let lane = store.create("mixed", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join("node_modules/.bin")).unwrap();
        fs::write(
            lane.path.join("node_modules/.bin/baseline-browser-mapping"),
            "#!/bin/sh\n",
        )
        .unwrap();
        fs::create_dir_all(lane.path.join("ignored")).unwrap();
        fs::write(lane.path.join("ignored/secret"), "unrescued").unwrap();
        assert!(
            matches!(&store.prune(&mut Observer::idle(), true, false).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("unrescued ignored file"))
        );
        assert!(lane.path.exists());
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
                build_dir_freed: None,
                build_dir_kept: None,
                build_dir: None,
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
        // Rescue keeps mtimes but never a source's permission bits — the
        // destination stays owner-only regardless of the lane file's mode.
        assert_eq!(fs::metadata(&rescued).unwrap().mode() & 0o777, 0o600);
        store.reclaim("owner").unwrap();
        assert!(store.tip("owner").is_err());
    }

    /// A lane file group/world-writable to the agent that wrote it must
    /// never carry those bits into the shared project tree: the rescued
    /// copy is owner-only no matter how open the source was.
    #[test]
    fn rescue_never_widens_the_destination_beyond_owner_only() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/owner")).unwrap();
        let path = lane.path.join(".multiverse/tasks/owner/result.md");
        fs::write(&path, "lane").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        store.rescue("owner", false).unwrap();
        let rescued = repo.0.join(".multiverse/tasks/owner/result.md");
        assert_eq!(fs::metadata(&rescued).unwrap().mode() & 0o777, 0o600);
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
        assert!(store.rescue("owner", false).is_err());
    }

    /// Recursively backdates every entry under `path`, including `path`
    /// itself, past `Worktrees::prune_quarantine`'s own retention window —
    /// `deep_old`'s own fixed 3-day offset is well inside that window, so
    /// quarantine aging needs its own helper rather than reusing it as-is.
    fn quarantine_old(path: &Path) {
        if fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()) {
            for entry in fs::read_dir(path).unwrap() {
                quarantine_old(&entry.unwrap().path());
            }
        }
        File::open(path)
            .unwrap()
            .set_modified(SystemTime::now() - WORKTREE_STATE_RETENTION - Duration::from_secs(86400))
            .unwrap();
    }

    /// Prunes a freshly created, already-`done` lane and returns its own
    /// `.multiverse/worktree-state/` quarantine copy — every test below
    /// starts from this same clean removal, then shapes the copy (or its
    /// age) to exercise one `prune_quarantine` guard at a time.
    fn quarantined_lane(store: &Worktrees, task: &str) -> PathBuf {
        let lane = store.create(task, "HEAD").unwrap();
        let state = lane.path.join(".multiverse/tasks").join(task);
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("status"), "done\n").unwrap();
        fs::write(state.join("result.md"), "result").unwrap();
        let result = store.prune(&mut Observer::idle(), false, false).unwrap();
        let PruneOutcome::Removed {
            retained_state: Some(retained),
            ..
        } = &result[0]
        else {
            panic!("{result:?}")
        };
        retained.clone()
    }

    #[test]
    fn quarantine_redundant_copy_is_removed_after_a_clean_prune() {
        let repo = TempRepo::new();
        let store = repo.store();
        let retained = quarantined_lane(&store, "owner");
        // The lane's own self-ignoring marker is never rescued (see
        // `rescue_ignores_the_lanes_own_gitignore...` below), so a copy
        // that still carries one can never be proven redundant; strip it
        // to prove the actual redundancy check, not that exclusion.
        fs::remove_file(retained.join(".gitignore")).unwrap();
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/status")).unwrap(),
            "done\n"
        );
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            QuarantineOutcome::Removed {
                reason: QuarantineReason::Redundant,
                ..
            }
        ));
        assert!(!retained.exists());
    }

    #[test]
    fn quarantine_non_redundant_copy_is_kept_inside_retention() {
        let repo = TempRepo::new();
        let store = repo.store();
        let retained = quarantined_lane(&store, "owner");
        // Left as-is: the lane's own `.gitignore` marker was never rescued,
        // so this copy can never be proven redundant, and it is fresh.
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], QuarantineOutcome::Kept { .. }));
        assert!(retained.exists());
    }

    #[test]
    fn quarantine_copy_with_differing_file_is_kept_not_redundant() {
        let repo = TempRepo::new();
        let store = repo.store();
        let retained = quarantined_lane(&store, "owner");
        // Same as the redundant-copy test above: strip the lane's own
        // unrescued `.gitignore` marker first, so the byte comparison in
        // `quarantine_is_redundant` is the only thing standing between this
        // copy and `Redundant`.
        fs::remove_file(retained.join(".gitignore")).unwrap();
        // Diverge the project's own rescued copy from what the quarantine
        // copy holds — the same shape as an older `result.md` left in place
        // by an earlier rescue. The byte check must keep this copy even
        // though a same-named mirror file exists.
        fs::write(
            repo.0.join(".multiverse/tasks/owner/result.md"),
            "an older result",
        )
        .unwrap();
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], QuarantineOutcome::Kept { .. }));
        assert!(retained.exists());
        assert_eq!(
            fs::read_to_string(retained.join("tasks/owner/result.md")).unwrap(),
            "result"
        );
    }

    #[test]
    fn quarantine_copy_with_differing_nested_file_is_kept_not_redundant() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let state = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(state.join("sub")).unwrap();
        fs::write(state.join("status"), "done\n").unwrap();
        fs::write(state.join("result.md"), "result").unwrap();
        fs::write(state.join("sub/note.md"), "nested").unwrap();
        let result = store.prune(&mut Observer::idle(), false, false).unwrap();
        let PruneOutcome::Removed {
            retained_state: Some(retained),
            ..
        } = &result[0]
        else {
            panic!("{result:?}")
        };
        let retained = retained.clone();
        fs::remove_file(retained.join(".gitignore")).unwrap();
        // Diverge the nested file one directory level below `tasks/owner`
        // itself, so the byte mismatch is only reachable through the
        // recursive directory branch rather than a top-level file.
        fs::write(
            repo.0.join(".multiverse/tasks/owner/sub/note.md"),
            "a different nested result",
        )
        .unwrap();
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], QuarantineOutcome::Kept { .. }));
        assert!(retained.exists());
    }

    #[test]
    fn quarantine_copy_past_retention_with_a_byte_conflict_is_kept_not_destroyed() {
        let repo = TempRepo::new();
        let store = repo.store();
        let retained = quarantined_lane(&store, "owner");
        // Diverge it from the already-rescued copy at the same relative
        // path, same as a late write would — this is not redundant, and
        // rescuing it now would conflict with what is already saved under
        // that name. Age alone must not excuse deleting it: the only copy
        // of this specific byte string lives here and nowhere else.
        fs::write(retained.join("tasks/owner/status"), "late write\n").unwrap();
        quarantine_old(&retained);
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(&outcomes[0], QuarantineOutcome::Kept { .. }));
        assert!(retained.exists());
        assert_eq!(
            fs::read_to_string(retained.join("tasks/owner/status")).unwrap(),
            "late write\n"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/status")).unwrap(),
            "done\n"
        );
    }

    #[test]
    fn quarantine_copy_past_retention_with_no_conflict_is_rescued_then_removed() {
        let repo = TempRepo::new();
        let store = repo.store();
        let retained = quarantined_lane(&store, "owner");
        // Add a file that was never part of the original rescue — the
        // shape a late write that only *adds* something new leaves behind.
        // It is not redundant (nothing mirrors it yet), but rescuing it
        // conflicts with nothing, so retention age may now excuse removing
        // this copy: what it alone held is no longer alone.
        fs::write(retained.join("tasks/owner/late.md"), "late addition").unwrap();
        quarantine_old(&retained);
        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            QuarantineOutcome::Removed {
                reason: QuarantineReason::RetentionExpired,
                ..
            }
        ));
        assert!(!retained.exists());
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/late.md")).unwrap(),
            "late addition"
        );
    }

    /// Proof for the crash window inside `remove_lane` itself (its own
    /// `fs::rename` into quarantine, immediately followed by the redirect
    /// `symlink`): stop right after the rename, as if the host died before
    /// the symlink — and before `remove_lane`'s own `rescue_state` call
    /// right after both, which never runs either. Nothing anywhere else
    /// has ever seen this task's state. Reverting the rescue-before-delete
    /// fix in `prune_quarantine` (deleting unconditionally once a copy is
    /// merely past retention) makes the final assertion here fail: the
    /// content would never have been rescued, so it would be gone, not
    /// reachable, once the quarantine copy itself is removed.
    #[test]
    fn a_remove_lane_crash_between_rename_and_symlink_is_rescued_before_prune_deletes_it() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let state = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("status"), "done\n").unwrap();
        fs::write(state.join("result.md"), "crash-only result").unwrap();

        let root = repo.0.join(".multiverse/worktree-state");
        fs::create_dir_all(&root).unwrap();
        let quarantine = root.join("owner-crash");
        fs::rename(lane.path.join(".multiverse"), &quarantine).unwrap();
        quarantine_old(&quarantine);

        let outcomes = store.prune_quarantine(false).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(
            &outcomes[0],
            QuarantineOutcome::Removed {
                reason: QuarantineReason::RetentionExpired,
                ..
            }
        ));
        assert!(!quarantine.exists());
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/result.md")).unwrap(),
            "crash-only result"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/status")).unwrap(),
            "done\n"
        );
    }

    #[test]
    fn quarantine_dry_run_removes_nothing_and_reports_what_it_would() {
        let repo = TempRepo::new();
        let store = repo.store();
        let redundant = quarantined_lane(&store, "redundant");
        fs::remove_file(redundant.join(".gitignore")).unwrap();
        let expired = quarantined_lane(&store, "expired");
        // A non-conflicting addition, not a byte-diverging overwrite: dry
        // run must predict this would be rescued cleanly (not kept for a
        // conflict) without ever writing it.
        fs::write(expired.join("tasks/expired/late.md"), "late addition").unwrap();
        quarantine_old(&expired);

        let outcomes = store.prune_quarantine(true).unwrap();
        assert_eq!(outcomes.len(), 2);
        assert!(matches!(
            &outcomes[0],
            QuarantineOutcome::WouldRemove {
                reason: QuarantineReason::RetentionExpired,
                ..
            }
        ));
        assert!(matches!(
            &outcomes[1],
            QuarantineOutcome::WouldRemove {
                reason: QuarantineReason::Redundant,
                ..
            }
        ));
        assert!(redundant.exists());
        assert!(expired.exists());
        assert_eq!(
            fs::read_to_string(redundant.join("tasks/redundant/status")).unwrap(),
            "done\n"
        );
        assert!(!repo.0.join(".multiverse/tasks/expired/late.md").exists());
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
        store.rescue("owner", false).unwrap();
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/.gitignore")).unwrap(),
            "project\n"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join(".multiverse/tasks/owner/result.md")).unwrap(),
            "lane"
        );
    }

    /// `(is_dir, file_contents, mtime)` for one [`fs_snapshot`] entry.
    type SnapshotEntry = (bool, Option<Vec<u8>>, Option<SystemTime>);

    /// Full recursive snapshot (paths, directory-ness, file contents, and
    /// mtimes) under `root`, or empty when `root` does not exist — strong
    /// enough that a write which happens to restore identical bytes still
    /// shows up as a changed mtime, unlike a plain content or file-count
    /// comparison.
    fn fs_snapshot(root: &Path) -> BTreeMap<PathBuf, SnapshotEntry> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, SnapshotEntry>) {
            let Ok(entries) = fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                let Ok(meta) = entry.metadata() else { continue };
                let mtime = meta.modified().ok();
                if meta.is_dir() {
                    out.insert(relative, (true, None, mtime));
                    walk(root, &path, out);
                } else {
                    out.insert(relative, (false, fs::read(&path).ok(), mtime));
                }
            }
        }
        let mut out = BTreeMap::new();
        if root.is_dir() {
            walk(root, root, &mut out);
        }
        out
    }

    /// Card `20261110-b-d3ba4f` defect 1: `rescue` used to call
    /// `rescue_state` unconditionally, performing the real copy despite
    /// `--dry-run`'s name. Snapshots the project's and the lane's own
    /// `.multiverse/` trees (paths, contents, mtimes) around `rescue(...,
    /// true)` and around `prune(..., true)`, asserting byte-for-byte,
    /// mtime-for-mtime equality — fails on the pre-fix `rescue`, which left
    /// the previewed artifacts actually copied into the project.
    #[test]
    fn rescue_and_prune_dry_run_write_nothing() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("owner", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/owner");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();

        let project_state = repo.0.join(".multiverse");
        let lane_state = lane.path.join(".multiverse");
        let before_project = fs_snapshot(&project_state);
        let before_lane = fs_snapshot(&lane_state);

        let mut previewed: Vec<PathBuf> = store
            .rescue("owner", true)
            .unwrap()
            .into_iter()
            .map(|p| p.strip_prefix(&project_state).unwrap().to_path_buf())
            .collect();
        previewed.sort();
        assert_eq!(
            previewed,
            vec![
                PathBuf::from("tasks/owner/result.md"),
                PathBuf::from("tasks/owner/status"),
            ],
            "a truthful preview still names what it would copy"
        );
        assert_eq!(
            fs_snapshot(&project_state),
            before_project,
            "rescue --dry-run wrote to the project"
        );
        assert_eq!(
            fs_snapshot(&lane_state),
            before_lane,
            "rescue --dry-run wrote to the lane"
        );
        assert!(
            !project_state.join("tasks/owner/result.md").exists(),
            "rescue --dry-run must not actually copy anything"
        );

        store.prune(&mut Observer::idle(), false, true).unwrap();
        assert_eq!(
            fs_snapshot(&project_state),
            before_project,
            "prune --dry-run wrote to the project"
        );
        assert_eq!(
            fs_snapshot(&lane_state),
            before_lane,
            "prune --dry-run wrote to the lane"
        );
        assert!(
            lane.path.exists(),
            "prune --dry-run must not remove the lane"
        );
    }

    /// Card `20261110-b-d3ba4f` defect 2: an authorizing assessment's own
    /// `result.md`/`retire` live in *its own* lane's task directory until
    /// that lane is pruned — the same place `rescue` copies them from —
    /// never in the project directly. Before this fix,
    /// `retirement_authorized` only ever scanned the project's own
    /// `.multiverse/tasks`, so a same-batch dry-run preview (which never
    /// actually rescues anything) marked an otherwise-archivable lane
    /// "Kept — ... no archive authority", while the very next real prune
    /// of the same lanes rescued the assessment's lane first and then
    /// archived it cleanly. A bare dry-run preview and a same-flags real
    /// run must agree.
    /// The shared candidate listing must not go stale within one pass.
    /// The assessment lane here sorts first, so a real prune rescues its
    /// task dir into the main checkout and removes the lane before it
    /// reaches the builder lane. The authority must still be found there,
    /// as it was when each lane rescanned the task directories.
    #[test]
    fn prune_finds_archive_authority_rescued_from_a_lane_removed_earlier_in_the_same_pass() {
        let repo = TempRepo::new();
        let store = repo.store();
        let assessment_id = "20261102-oracle-123456";
        let assessment_lane = store.create(assessment_id, "HEAD").unwrap();
        let lane = store.create("20261103-builder-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("feature"), "unique\n").unwrap();
        git(&lane.path, &["add", "feature"], None).unwrap();
        git(&lane.path, &["commit", "-m", "unique feature"], None).unwrap();
        let own_task_dir = lane.path.join(".multiverse/tasks").join(&lane.task);
        fs::create_dir_all(&own_task_dir).unwrap();
        fs::write(own_task_dir.join("status"), "done\n").unwrap();
        fs::write(own_task_dir.join("result.md"), "builder findings\n").unwrap();
        let tip = store.tip(&lane.task).unwrap();

        let assessment_dir = assessment_lane
            .path
            .join(".multiverse/tasks")
            .join(assessment_id);
        fs::create_dir_all(&assessment_dir).unwrap();
        fs::write(assessment_dir.join("result.md"), "assessed\n").unwrap();
        fs::write(assessment_dir.join("status"), "done\n").unwrap();
        fs::write(
            assessment_dir.join("retire"),
            format!("{} {tip}\n", lane.task),
        )
        .unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        File::options()
            .write(true)
            .open(assessment_dir.join("retire"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T10:00:00+00:00\tevent=dispatch\ttask={assessment_id}\trole=oracle\n\
                 2026-11-02T10:05:00+00:00\tevent=result\ttask={assessment_id}\tstatus=done\n"
            ),
        )
        .unwrap();

        let real = store.prune(&mut Observer::idle(), false, false).unwrap();
        let assessment_real = real
            .iter()
            .find(|outcome| outcome_task(outcome) == assessment_id)
            .unwrap();
        assert!(
            matches!(assessment_real, PruneOutcome::Removed { .. }),
            "the assessment lane must be removed first: {real:?}"
        );
        let builder_real = real
            .iter()
            .find(|outcome| outcome_task(outcome) == lane.task)
            .unwrap();
        assert!(
            matches!(builder_real, PruneOutcome::Archived { .. }),
            "{real:?}"
        );
    }

    #[test]
    fn prune_dry_run_preview_finds_archive_authority_in_the_assessments_own_unrescued_lane() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("20261101-builder-abcdef", "HEAD").unwrap();
        fs::write(lane.path.join("feature"), "unique\n").unwrap();
        git(&lane.path, &["add", "feature"], None).unwrap();
        git(&lane.path, &["commit", "-m", "unique feature"], None).unwrap();
        let own_task_dir = lane.path.join(".multiverse/tasks").join(&lane.task);
        fs::create_dir_all(&own_task_dir).unwrap();
        fs::write(own_task_dir.join("status"), "done\n").unwrap();
        fs::write(own_task_dir.join("result.md"), "builder findings\n").unwrap();
        let tip = store.tip(&lane.task).unwrap();

        // The authorizing assessment's own lane, never pruned: its
        // `result.md`/`retire` exist only here, not yet rescued anywhere.
        let assessment_id = "20261102-oracle-123456";
        let assessment_lane = store.create(assessment_id, "HEAD").unwrap();
        let assessment_dir = assessment_lane
            .path
            .join(".multiverse/tasks")
            .join(assessment_id);
        fs::create_dir_all(&assessment_dir).unwrap();
        fs::write(assessment_dir.join("result.md"), "assessed\n").unwrap();
        fs::write(assessment_dir.join("status"), "done\n").unwrap();
        fs::write(
            assessment_dir.join("retire"),
            format!("{} {tip}\n", lane.task),
        )
        .unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        File::options()
            .write(true)
            .open(assessment_dir.join("retire"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T10:00:00+00:00\tevent=dispatch\ttask={assessment_id}\trole=oracle\n\
                 2026-11-02T10:05:00+00:00\tevent=result\ttask={assessment_id}\tstatus=done\n"
            ),
        )
        .unwrap();
        assert!(
            !repo
                .0
                .join(".multiverse/tasks")
                .join(assessment_id)
                .exists(),
            "the project must not already have the assessment's artifacts"
        );
        assert!(store.retirement_authorized(&lane.task, &tip).unwrap());

        // A bare dry-run preview must already show what the same-flags
        // real run below actually does, even though neither lane has been
        // touched yet.
        let preview = store.prune(&mut Observer::idle(), false, true).unwrap();
        let builder_preview = preview
            .iter()
            .find(|outcome| outcome_task(outcome) == lane.task)
            .unwrap();
        assert!(
            matches!(builder_preview, PruneOutcome::WouldArchive { .. }),
            "{preview:?}"
        );
        assert!(lane.path.exists(), "a dry run must not remove anything");

        let real = store.prune(&mut Observer::idle(), false, false).unwrap();
        let builder_real = real
            .iter()
            .find(|outcome| outcome_task(outcome) == lane.task)
            .unwrap();
        assert!(
            matches!(builder_real, PruneOutcome::Archived { .. }),
            "{real:?}"
        );
        assert!(!lane.path.exists());
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

    /// Rewrites `task`'s own branch reflog so its one entry — the
    /// `branch: Created from ...` line `create`'s `git worktree add -b`
    /// always leaves — reads as `age` old, without sleeping or touching
    /// the lane's own files. Mirrors how other tests here backdate an
    /// mtime (`set_modified`) to exercise a retention cutoff without
    /// making the suite itself wait one out.
    fn backdate_branch_reflog(repo: &Path, task: &str, age: Duration) {
        let branch = branch_name(task).unwrap();
        let path = repo.join(".git/logs/refs/heads").join(&branch);
        let content = fs::read_to_string(&path).unwrap();
        let epoch = SystemTime::now()
            .checked_sub(age)
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .to_string();
        let rewritten: String = content
            .lines()
            .map(|line| {
                let (meta, msg) = line.split_once('\t').unwrap();
                let mut parts: Vec<&str> = meta.split(' ').collect();
                let len = parts.len();
                parts[len - 2] = &epoch;
                format!("{}\t{}\n", parts.join(" "), msg)
            })
            .collect();
        fs::write(&path, rewritten).unwrap();
    }

    /// Rank-7 (card `20261117-b-437eeb`, Scout `9c0832`): a lane
    /// `Worktrees::create` made but no `write_brief` ever followed used to
    /// be kept forever by plain `prune` — `occupant_tasks` always
    /// includes the lane's own task name, so the missing-status refusal
    /// fired on every call, with no age or ancestry ever mattering
    /// (`prune_shares_one_ledger_scan_and_candidate_listing_across_differently_shaped_lanes`'s
    /// own "missing" lane is this same shape, fresh). A fresh lane must
    /// still need `force` — the in-flight `create` → `write_brief` window
    /// is real — but once it is older than `WORKTREE_STATE_RETENTION`
    /// with no brief and no commit, `prune` must reclaim both the lane
    /// directory and its branch on its own.
    #[test]
    fn an_old_never_dispatched_lane_is_reclaimable_without_force() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("orphan", "HEAD").unwrap();

        // Fresh: still kept, same as any other never-dispatched lane —
        // the in-flight `create` → `write_brief` window must stay guarded.
        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status"))
        );

        backdate_branch_reflog(
            &repo.0,
            "orphan",
            WORKTREE_STATE_RETENTION + Duration::from_secs(86400),
        );

        let preview = store.prune(&mut Observer::idle(), false, true).unwrap();
        assert!(
            matches!(
                &preview[0],
                PruneOutcome::WouldRemove {
                    branch_refusal: None,
                    ..
                }
            ),
            "{preview:?}"
        );

        let outcomes = store.prune(&mut Observer::idle(), false, false).unwrap();
        assert!(
            matches!(
                &outcomes[0],
                PruneOutcome::Removed {
                    branch_refusal: None,
                    ..
                }
            ),
            "{outcomes:?}"
        );
        assert!(!lane.path.exists());
        assert!(!git_output(
            &repo.0,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "refs/heads/multiverse/orphan"
            ],
            None
        )
        .unwrap()
        .status
        .success());
    }

    /// The same age and missing-brief shape, but with a real commit on
    /// the lane — rank-7's own "keeping the existing guards for any lane
    /// with work in it". Age and a missing task directory are never
    /// enough alone; `has_commits` must also hold.
    #[test]
    fn an_old_lane_with_a_real_commit_still_needs_force() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("worked", "HEAD").unwrap();
        fs::write(lane.path.join("README.md"), "real work\n").unwrap();
        git(&lane.path, &["commit", "-am", "real work"], None).unwrap();

        backdate_branch_reflog(
            &repo.0,
            "worked",
            WORKTREE_STATE_RETENTION + Duration::from_secs(86400),
        );

        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status")),
            "a real commit must still require force even once the lane is old"
        );
        assert!(lane.path.exists());
    }

    /// Scout 67a619 finding 2: `write_brief` (`runner.rs::start_dispatch`)
    /// succeeds and returns before the project lock is dropped, but
    /// `event=dispatch` is not appended to the routing log until after —
    /// a hard kill in that exact span leaves a task directory and brief on
    /// disk with zero routing-log trace of the dispatch that wrote them.
    /// Before this fix, `abandoned_before_dispatch` keyed off the task
    /// directory's existence alone and refused this shape forever (the
    /// identical symptom rank-7/card `20261117-b-437eeb` already fixed for
    /// the sibling "no brief at all" case, just one step later in the same
    /// span) — only a routing-log dispatch record now counts as "genuinely
    /// dispatched"; the directory alone does not.
    #[test]
    fn an_old_lane_with_a_written_brief_but_no_dispatch_record_is_reclaimable_without_force() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("briefed", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/briefed")).unwrap();
        fs::write(
            lane.path.join(".multiverse/tasks/briefed/brief.md"),
            "# Task\n",
        )
        .unwrap();

        // Fresh: still kept, same in-flight-window guard as the no-brief
        // case — a brief that landed a second ago could still be about to
        // get its `event=dispatch` appended.
        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status"))
        );

        backdate_branch_reflog(
            &repo.0,
            "briefed",
            WORKTREE_STATE_RETENTION + Duration::from_secs(86400),
        );

        let preview = store.prune(&mut Observer::idle(), false, true).unwrap();
        assert!(
            matches!(
                &preview[0],
                PruneOutcome::WouldRemove {
                    branch_refusal: None,
                    ..
                }
            ),
            "{preview:?}"
        );

        let outcomes = store.prune(&mut Observer::idle(), false, false).unwrap();
        assert!(
            matches!(
                &outcomes[0],
                PruneOutcome::Removed {
                    branch_refusal: None,
                    ..
                }
            ),
            "{outcomes:?}"
        );
        assert!(!lane.path.exists());
    }

    /// The same brief-with-no-dispatch-record shape, but with a genuine
    /// `event=dispatch` for this task id also in the log — the fix must
    /// not treat *every* task directory as unproven just because it
    /// predates this change; one that really was dispatched still needs
    /// its existing terminal-outcome guard, age or no age.
    #[test]
    fn an_old_lane_with_a_real_dispatch_record_still_needs_force() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("dispatched", "HEAD").unwrap();
        fs::create_dir_all(lane.path.join(".multiverse/tasks/dispatched")).unwrap();
        fs::write(
            lane.path.join(".multiverse/tasks/dispatched/brief.md"),
            "# Task\n",
        )
        .unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask=dispatched\trole=fixer\tbranch=multiverse/dispatched\n",
        )
        .unwrap();

        backdate_branch_reflog(
            &repo.0,
            "dispatched",
            WORKTREE_STATE_RETENTION + Duration::from_secs(86400),
        );

        assert!(
            matches!(&store.prune(&mut Observer::idle(), false, true).unwrap()[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status")),
            "a genuinely dispatched task must still require force even once its lane is old"
        );
        assert!(lane.path.exists());
    }

    /// `prune_orphans`'s own half of rank-7: a never-dispatched lane
    /// whose checkout is already gone (simulated here with a direct `git
    /// worktree remove`, bypassing `remove_lane`, so only the orphan
    /// branch is left to reclaim) must not need a recorded terminal
    /// outcome either, once it is old enough — the same shape
    /// `abandoned_before_dispatch` already proves for `prune` itself.
    #[test]
    fn prune_orphans_reclaims_an_old_never_dispatched_orphan_branch() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("gone", "HEAD").unwrap();
        git(
            &repo.0,
            &[
                "worktree",
                "remove",
                "--force",
                utf8_path(&lane.path).unwrap(),
            ],
            None,
        )
        .unwrap();

        backdate_branch_reflog(
            &repo.0,
            "gone",
            WORKTREE_STATE_RETENTION + Duration::from_secs(86400),
        );

        assert_eq!(store.orphan_branches().unwrap(), vec!["gone".to_string()]);
        let outcomes = store.prune_orphans(false).unwrap();
        assert!(
            matches!(&outcomes[0], BranchOutcome::Reclaimed { task } if task == "gone"),
            "{outcomes:?}"
        );
        assert!(store.orphan_branches().unwrap().is_empty());
    }

    /// Card `20261117-b-18fd71` item 2: `prune_with_ignored` now fetches
    /// the routing log's task records and the retirement-candidate
    /// listing once per call via [`Worktrees::task_records`]/
    /// [`Worktrees::retirement_candidates`], not once per lane. Four lanes
    /// shaped to hit every branch that reads either input — missing
    /// status, a log-recovered terminal outcome, an unmerged branch with
    /// no archive authority, and an unmerged branch an assessment did
    /// authorize — must still each reach their own, independent decision
    /// from the one shared scan, in the same single `prune` call.
    #[test]
    fn prune_shares_one_ledger_scan_and_candidate_listing_across_differently_shaped_lanes() {
        let repo = TempRepo::new();
        let store = repo.store();

        let missing = store.create("missing", "HEAD").unwrap();

        let recorded = store.create("recorded", "HEAD").unwrap();

        let noauth = store.create("noauth", "HEAD").unwrap();
        fs::write(noauth.path.join("README.md"), "noauth unique\n").unwrap();
        git(&noauth.path, &["commit", "-am", "noauth unique"], None).unwrap();
        let noauth_dir = repo.0.join(".multiverse/tasks/noauth");
        fs::create_dir_all(&noauth_dir).unwrap();
        fs::write(noauth_dir.join("status"), "done\n").unwrap();
        fs::write(noauth_dir.join("result.md"), "noauth result\n").unwrap();

        let archive = store.create("archive", "HEAD").unwrap();
        fs::write(archive.path.join("README.md"), "archive unique\n").unwrap();
        git(&archive.path, &["commit", "-am", "archive unique"], None).unwrap();
        let archive_dir = repo.0.join(".multiverse/tasks/archive");
        fs::create_dir_all(&archive_dir).unwrap();
        fs::write(archive_dir.join("status"), "done\n").unwrap();
        fs::write(archive_dir.join("result.md"), "archive result\n").unwrap();
        let archive_tip = store.tip("archive").unwrap();

        let assessment_dir = repo.0.join(".multiverse/tasks/assess");
        fs::create_dir_all(&assessment_dir).unwrap();
        fs::write(assessment_dir.join("result.md"), "assessed\n").unwrap();
        fs::write(assessment_dir.join("status"), "done\n").unwrap();
        fs::write(
            assessment_dir.join("retire"),
            format!("archive {archive_tip}\n"),
        )
        .unwrap();
        let later = SystemTime::now() + Duration::from_secs(10);
        File::options()
            .write(true)
            .open(assessment_dir.join("retire"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();

        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=recorded\tstatus=done\n\
             2026-11-01T00:00:00+00:00\tevent=dispatch\ttask=assess\trole=oracle\n\
             2026-11-01T00:05:00+00:00\tevent=result\ttask=assess\tstatus=done\n",
        )
        .unwrap();

        let outcomes = store.prune(&mut Observer::idle(), false, true).unwrap();
        assert_eq!(outcomes.len(), 4, "{outcomes:?}");
        let by_task: BTreeMap<&str, &PruneOutcome> = outcomes
            .iter()
            .map(|outcome| (outcome_task(outcome), outcome))
            .collect();
        assert!(
            matches!(by_task["missing"], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal status")),
            "{:?}",
            by_task["missing"]
        );
        assert!(
            matches!(
                by_task["recorded"],
                PruneOutcome::WouldRemove {
                    branch_refusal: None,
                    ..
                }
            ),
            "{:?}",
            by_task["recorded"]
        );
        assert!(
            matches!(by_task["noauth"], PruneOutcome::Kept { reason, .. } if reason.contains("not merged")),
            "{:?}",
            by_task["noauth"]
        );
        assert!(
            matches!(by_task["archive"], PruneOutcome::WouldArchive { .. }),
            "{:?}",
            by_task["archive"]
        );
        for lane in [&missing, &recorded, &noauth, &archive] {
            assert!(lane.path.exists(), "dry run must not remove {}", lane.task);
        }
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
        record_prune_outcomes(&repo.0, &lane, &store.task_records()).unwrap();
        assert!(has_terminal_outcome(&repo.0, "owner"));
        let log = fs::read_to_string(repo.0.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=result\ttask=owner\tstatus=done\tstate=reconciled"),
            "expected a reconciled result line, got: {log:?}"
        );

        // Idempotent: an occupant that already has a recorded outcome is
        // left alone, not appended a second time.
        record_prune_outcomes(&repo.0, &lane, &store.task_records()).unwrap();
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

        let err = record_prune_outcomes(&repo.0, &lane, &store.task_records()).unwrap_err();
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
        let err = record_prune_outcomes(&repo.0, &lane, &store.task_records()).unwrap_err();
        assert!(
            err.to_string().contains("no terminal status available"),
            "{err}"
        );
        assert!(
            !repo.0.join(".multiverse/routing.log").exists(),
            "a refused occupant must not get a partial record"
        );
    }

    /// `prune_with_ignored` fetches `task_records` once per call and now
    /// hands that same slice to every lane's `observer.record_prune`
    /// (card `20261117-b-c9349a`), instead of `record_prune_outcomes`
    /// re-scanning the ledger per occupant. A real (non-dry-run) prune over
    /// several lanes in one call must still record exactly the same
    /// outcomes the old per-task scan did: leave an already-recorded
    /// occupant's single result line untouched, and append exactly one for
    /// an occupant that only has a terminal status file.
    #[test]
    fn real_prune_over_several_lanes_records_the_same_outcomes_as_a_per_task_scan() {
        let repo = TempRepo::new();
        let store = repo.store();

        let already_recorded = store.create("already-recorded", "HEAD").unwrap();
        let needs_recording = store.create("needs-recording", "HEAD").unwrap();
        fs::write(
            repo.0.join(".multiverse/routing.log"),
            "2026-11-01T00:00:00+00:00\tevent=result\ttask=already-recorded\tstatus=done\n",
        )
        .unwrap();
        let local = needs_recording
            .path
            .join(".multiverse/tasks/needs-recording");
        fs::create_dir_all(&local).unwrap();
        fs::write(local.join("status"), "done\n").unwrap();
        fs::write(local.join("result.md"), "result\n").unwrap();

        let before = fs::read_to_string(repo.0.join(".multiverse/routing.log")).unwrap();
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&repo.0, "/nonexistent/fake-herdr-unused");
        let outcomes = store.prune(&mut observer, true, false).unwrap();
        assert_eq!(outcomes.len(), 2);
        for outcome in &outcomes {
            assert!(
                matches!(outcome, PruneOutcome::Removed { .. }),
                "{outcome:?}"
            );
        }
        assert!(!already_recorded.path.exists());
        assert!(!needs_recording.path.exists());

        let after = fs::read_to_string(repo.0.join(".multiverse/routing.log")).unwrap();
        assert!(
            after.starts_with(&before),
            "an occupant that already had a recorded outcome must keep its exact existing line"
        );
        let appended = &after[before.len()..];
        assert_eq!(
            appended.matches("task=needs-recording").count(),
            1,
            "the unrecorded occupant must get exactly one appended result line: {appended:?}"
        );
        assert_eq!(
            after.matches("task=already-recorded").count(),
            1,
            "the already-recorded occupant's outcome must not be duplicated: {after:?}"
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

    // -- `Worktrees::merge` (card `20261114-b-069836`, option (a)) ----------

    /// A throwaway `#!/bin/sh` scanner script, standing in for a real
    /// `gitleaks`/`trufflehog` binary — tests never touch the network or a
    /// real installed scanner. `body` is the script's own exit behavior
    /// (`"exit 0"` clean, `"echo ...; exit 1"` a gitleaks finding, etc.).
    fn fake_scanner(body: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "mv-state-fake-scanner-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    fn secrets_scan_with(gitleaks: &Path) -> SecretsScan {
        SecretsScan {
            gitleaks: gitleaks.display().to_string(),
            trufflehog: None,
            required: true,
            allowlist: ".gitleaks.toml".to_string(),
        }
    }

    /// A lane with a recorded `done` status, a non-empty `result.md`, and
    /// one committed change (the planted-secret proof needs real content
    /// in the diff gitleaks scans) — the shape `merge` requires before it
    /// will even invoke a scanner.
    fn done_lane(store: &Worktrees, task: &str) -> Lane {
        let lane = store.create(task, "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks").join(task);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::write(lane.path.join("change.txt"), "lane work\n").unwrap();
        git(&lane.path, &["add", "change.txt"], None).unwrap();
        git(&lane.path, &["commit", "-m", "lane work"], None).unwrap();
        lane
    }

    #[test]
    fn merge_refuses_a_task_with_no_recorded_terminal_status() {
        let repo = TempRepo::new();
        let store = repo.store();
        store.create("task", "HEAD").unwrap();
        let secrets = secrets_scan_with(Path::new("gitleaks"));
        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::Refused(m) if m.contains("no recorded terminal status")),
            "{error}"
        );
    }

    #[test]
    fn merge_refuses_a_task_whose_status_is_not_done() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("task", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/task");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "blocked\n").unwrap();
        let secrets = secrets_scan_with(Path::new("gitleaks"));
        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::Refused(m) if m.contains("not done")),
            "{error}"
        );
    }

    #[test]
    fn merge_refuses_a_dirty_lane() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("task", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/task");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::write(lane.path.join("dirty.txt"), "oops").unwrap();
        let secrets = secrets_scan_with(Path::new("gitleaks"));
        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::Refused(m) if m.contains("uncommitted")),
            "{error}"
        );
    }

    /// Proof item: a lane whose diff adds a planted secret is refused (its
    /// own error names the evidence file), the evidence file holds the
    /// (fake) scanner's redacted output, and main is left exactly where it
    /// was — nothing merges on a finding.
    #[test]
    fn merge_refuses_on_a_gitleaks_finding_writes_evidence_and_leaves_main_unchanged() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = done_lane(&store, "secret-task");
        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let script = fake_scanner("echo 'README.md:3: AKIA-REDACTED'\nexit 1");
        let secrets = secrets_scan_with(&script);

        let error = store.merge("secret-task", &secrets).unwrap_err();
        match &error {
            MergeError::Refused(message) => {
                assert!(
                    message.contains("secrets scan (gitleaks) found"),
                    "{message}"
                );
                assert!(message.contains("secret-task-secrets.txt"), "{message}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        let evidence = repo.0.join(".multiverse/evidence/secret-task-secrets.txt");
        let contents = fs::read_to_string(&evidence).unwrap();
        assert!(contents.contains("AKIA-REDACTED"), "{contents}");
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(before, after, "a refused merge must leave main unchanged");
        assert!(
            lane.path.exists(),
            "a refused merge must not touch the lane"
        );
        fs::remove_file(&script).ok();
    }

    /// Proof item: a clean, done lane merges with git's own default no-ff
    /// message — `Merge branch 'multiverse/<task>'` — and `merge`'s own
    /// returned sha matches what main now points at.
    #[test]
    fn merge_merges_a_clean_lane_with_the_default_no_ff_message() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "clean-task");
        let script = fake_scanner("exit 0");
        let secrets = secrets_scan_with(&script);

        let result = store.merge("clean-task", &secrets).unwrap();
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        let message = git(&repo.0, &["log", "-1", "--format=%B"], None).unwrap();
        assert_eq!(message.trim(), "Merge branch 'multiverse/clean-task'");
        let sha = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(sha.trim(), result.sha);
        fs::remove_file(&script).ok();
    }

    /// Proof item: gitleaks missing + `required = true` (the default)
    /// exits non-silently (an install hint) and merges nothing.
    #[test]
    fn merge_blocks_when_gitleaks_is_missing_and_required() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let missing = std::env::temp_dir().join(format!(
            "mv-state-missing-gitleaks-{}-{}",
            std::process::id(),
            nonce()
        ));
        let secrets = secrets_scan_with(&missing);

        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::ScannerMissing(m) if m.contains("install")),
            "{error}"
        );
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(
            before, after,
            "a missing required scanner must merge nothing"
        );
    }

    /// Proof item: `required = false` only warns on a missing gitleaks and
    /// still lands the merge.
    #[test]
    fn merge_warns_and_merges_when_gitleaks_is_missing_and_not_required() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let missing = std::env::temp_dir().join(format!(
            "mv-state-missing-gitleaks-{}-{}",
            std::process::id(),
            nonce()
        ));
        let mut secrets = secrets_scan_with(&missing);
        secrets.required = false;

        let result = store.merge("task", &secrets).unwrap();
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].starts_with("WARNING:"),
            "{:?}",
            result.warnings
        );
        assert!(
            result.warnings[0].contains("required = false"),
            "{:?}",
            result.warnings
        );
    }

    /// trufflehog only runs after gitleaks passes, and a verified finding
    /// (exit 183 — TruffleHog's own `--fail` code, not 1) blocks the merge
    /// the same way a gitleaks finding does.
    #[test]
    fn merge_blocks_on_a_trufflehog_finding_after_gitleaks_passes() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let gitleaks = fake_scanner("exit 0");
        let trufflehog = fake_scanner("echo 'verified secret'\nexit 183");
        let mut secrets = secrets_scan_with(&gitleaks);
        secrets.trufflehog = Some(trufflehog.display().to_string());

        let error = store.merge("task", &secrets).unwrap_err();
        match &error {
            MergeError::Refused(message) => {
                assert!(message.contains("trufflehog"), "{message}");
                assert!(message.contains("task-secrets.txt"), "{message}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        fs::remove_file(&gitleaks).ok();
        fs::remove_file(&trufflehog).ok();
    }

    /// A configured trufflehog that cannot be run is advisory only — unlike
    /// gitleaks, it never blocks the merge regardless of `required`.
    #[test]
    fn merge_proceeds_when_a_configured_trufflehog_cannot_be_run() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let gitleaks = fake_scanner("exit 0");
        let mut secrets = secrets_scan_with(&gitleaks);
        secrets.trufflehog = Some(
            std::env::temp_dir()
                .join(format!("mv-state-missing-trufflehog-{}", nonce()))
                .display()
                .to_string(),
        );

        let result = store.merge("task", &secrets).unwrap();
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
        fs::remove_file(&gitleaks).ok();
    }

    /// Point 1 of card `20261116-b-9a5c0c`: a verified TruffleHog finding
    /// must never leave its raw secret in the evidence file, or in the
    /// merge's own error text — `--json`'s `Raw`/`RawV2`/`Redacted` fields
    /// are stripped before either sees it, and the merge is still refused
    /// exactly as before. Revert check (not run by CI, done by hand):
    /// removing `run_trufflehog`'s `.map(|found| found.map(|raw|
    /// redact_trufflehog_output(&raw)))` makes this test fail, since the
    /// stub's fake secret then reaches the evidence file verbatim —
    /// confirmed locally before this change was committed.
    #[test]
    fn merge_redacts_a_verified_trufflehog_finding_before_writing_evidence() {
        const FAKE_SECRET: &str = "EXAMPLE-NOT-A-REAL-SECRET-0123456789";
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let gitleaks = fake_scanner("exit 0");
        let finding = format!(
            "echo '{{\"DetectorName\":\"Generic\",\"Verified\":true,\"Raw\":\"{FAKE_SECRET}\",\"RawV2\":\"{FAKE_SECRET}\",\"Redacted\":\"{FAKE_SECRET}\"}}'\nexit 183"
        );
        let trufflehog = fake_scanner(&finding);
        let mut secrets = secrets_scan_with(&gitleaks);
        secrets.trufflehog = Some(trufflehog.display().to_string());

        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let error = store.merge("task", &secrets).unwrap_err();
        let message = match &error {
            MergeError::Refused(message) => message.clone(),
            other => panic!("expected Refused, got {other:?}"),
        };
        assert!(
            !message.contains(FAKE_SECRET),
            "the fake secret leaked into the merge error text: {message}"
        );
        let evidence = repo.0.join(".multiverse/evidence/task-secrets.txt");
        let contents = fs::read_to_string(&evidence).unwrap();
        assert!(
            !contents.contains(FAKE_SECRET),
            "the fake secret leaked into the evidence file: {contents}"
        );
        assert!(contents.contains("[redacted]"), "{contents}");
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(before, after, "a refused merge must leave main unchanged");

        fs::remove_file(&gitleaks).ok();
        fs::remove_file(&trufflehog).ok();
    }

    /// Point 2 of card `20261116-b-9a5c0c`: the evidence leaf itself must
    /// be checked for a symlink immediately before opening, not only the
    /// `.multiverse/evidence` directory components `guarded_path` already
    /// walks.
    #[test]
    fn merge_refuses_when_the_evidence_leaf_is_a_symlink() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");
        let script = fake_scanner("echo 'README.md:3: AKIA-REDACTED'\nexit 1");
        let secrets = secrets_scan_with(&script);

        let evidence_dir = repo.0.join(".multiverse/evidence");
        fs::create_dir_all(&evidence_dir).unwrap();
        let sentinel = std::env::temp_dir().join(format!(
            "mv-state-evidence-sentinel-{}-{}",
            std::process::id(),
            nonce()
        ));
        fs::write(&sentinel, "untouched\n").unwrap();
        let leaf = evidence_dir.join("task-secrets.txt");
        std::os::unix::fs::symlink(&sentinel, &leaf).unwrap();

        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::Refused(m) if m.contains("symlink")),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(&sentinel).unwrap(),
            "untouched\n",
            "a symlinked evidence leaf must not be written through"
        );
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(before, after, "a refused merge must leave main unchanged");

        fs::remove_file(&script).ok();
        fs::remove_file(&sentinel).ok();
        fs::remove_file(&leaf).ok();
    }

    /// Proof item (a) for the half-finished-merge fix: a `MERGE_HEAD` left
    /// over from an earlier crashed or conflicted merge — produced here
    /// with a raw conflicting `git merge`, independent of `merge`'s own
    /// code — makes `merge` refuse outright, naming the stuck state and a
    /// git command to inspect it, and changes nothing: the `MERGE_HEAD`,
    /// the conflict markers, and `HEAD` are all exactly as it found them.
    #[test]
    fn merge_refuses_when_trunk_already_has_a_merge_in_progress() {
        let repo = TempRepo::new();
        let store = repo.store();
        done_lane(&store, "task");

        git(&repo.0, &["checkout", "-b", "other"], None).unwrap();
        fs::write(repo.0.join("README.md"), "other\n").unwrap();
        git(&repo.0, &["commit", "-am", "other change"], None).unwrap();
        git(&repo.0, &["checkout", "main"], None).unwrap();
        fs::write(repo.0.join("README.md"), "main\n").unwrap();
        git(&repo.0, &["commit", "-am", "main change"], None).unwrap();
        git(&repo.0, &["merge", "--no-ff", "--no-edit", "other"], None).unwrap_err();
        assert!(
            repo.0.join(".git/MERGE_HEAD").try_exists().unwrap(),
            "test setup must leave a real MERGE_HEAD"
        );
        let conflicted = fs::read_to_string(repo.0.join("README.md")).unwrap();

        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let secrets = secrets_scan_with(Path::new("gitleaks"));
        let error = store.merge("task", &secrets).unwrap_err();
        assert!(
            matches!(&error, MergeError::Refused(m) if m.contains("MERGE_HEAD") && m.contains("git -C")),
            "{error}"
        );
        assert!(
            repo.0.join(".git/MERGE_HEAD").try_exists().unwrap(),
            "a pre-existing merge-in-progress state must be left exactly as found"
        );
        assert_eq!(
            fs::read_to_string(repo.0.join("README.md")).unwrap(),
            conflicted,
            "the refusal must not touch the conflict markers"
        );
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(before, after, "the refusal must change nothing on trunk");

        // Clean up by hand so `TempRepo`'s own teardown doesn't trip on it.
        let _ = git(&repo.0, &["merge", "--abort"], None);
    }

    /// Proof item (b): a lane whose own merge genuinely conflicts is
    /// aborted cleanly (`git merge --abort`) rather than left mid-merge,
    /// the refusal names the conflicting path, and trunk ends up exactly
    /// where it started — no `MERGE_HEAD`, no unmerged paths, same `HEAD`.
    #[test]
    fn merge_aborts_cleanly_and_reports_conflicting_paths_when_a_lane_conflicts() {
        let repo = TempRepo::new();
        let store = repo.store();
        let lane = store.create("task", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/task");
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        fs::write(lane.path.join("README.md"), "lane version\n").unwrap();
        git(&lane.path, &["commit", "-am", "lane edits README"], None).unwrap();

        fs::write(repo.0.join("README.md"), "trunk version\n").unwrap();
        git(&repo.0, &["commit", "-am", "trunk edits README"], None).unwrap();

        let before = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        let script = fake_scanner("exit 0");
        let secrets = secrets_scan_with(&script);

        let error = store.merge("task", &secrets).unwrap_err();
        match &error {
            MergeError::Refused(message) => {
                assert!(message.contains("README.md"), "{message}");
                assert!(message.contains("conflicted"), "{message}");
                assert!(message.contains("aborted"), "{message}");
            }
            other => panic!("expected Refused, got {other:?}"),
        }
        assert!(
            !repo.0.join(".git/MERGE_HEAD").try_exists().unwrap(),
            "a conflicted merge must abort cleanly, leaving no MERGE_HEAD"
        );
        assert!(
            conflicting_paths(&repo.0).unwrap().is_empty(),
            "an aborted merge must leave no unmerged paths"
        );
        let after = git(&repo.0, &["rev-parse", "HEAD"], None).unwrap();
        assert_eq!(
            before, after,
            "an aborted conflicted merge must leave main unchanged"
        );

        fs::remove_file(&script).ok();
    }
}

//! Recovering a lane's diff for `review-policy --lane`'s risk
//! classification (card `20260930-b-eb20d3`), including a merged or
//! reclaimed lane's tip (card `20261015-fixer-ea95a9`), and for the
//! `dispatch --dry-run` reviewer card's own diff metadata (card
//! `20261102-b-82436e`) — port of Python's `worktrees.lane_diff` /
//! `worktrees._merged_lane_tip` (`multiverse/worktrees.py`). `LaneDiff`
//! now carries Python's full shape — `base`, `stat_text`, `truncated`,
//! alongside `touched_files`/`body_text` — rather than only the subset
//! `mv_cli::commands::review_policy::format_lane_risk` needed before this
//! card; that caller still only reads `touched_files`/`body_text`, via
//! [`crate::risk_paths::risk_areas_touched`].
//!
//! This task's write scope is `mv-cli/src/commands/review_policy.rs`,
//! `mv-cli/src/commands/dispatch.rs`, and the `mv-engine` code they call —
//! not `mv-state`, whose own `worktrees::lane_touched_files` already
//! covers the live-branch case and explicitly defers merged-lane recovery
//! as a separate, larger port (see that function's own doc comment).
//! Rather than edit `mv-state`, this module duplicates its minimal
//! branch/base-ref git plumbing and extends it with the two-step tip
//! recovery Python's `_merged_lane_tip` performs.

use std::path::Path;
use std::process::Command;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum LaneDiffError {
    /// `task_id` names no branch this repo can diff, and no merged-lane
    /// tip recovers one either — either it was never dispatched as a
    /// writer, or its lane's branch is gone with nothing left to recover.
    #[error(
        "{task_id:?} has no branch to diff against — it may never have been \
         dispatched as a writer, or its lane was already reclaimed"
    )]
    NoLaneDiff { task_id: String },
}

/// Port of Python's `worktrees.LaneDiff` — see this module's doc comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneDiff {
    /// The commit this diff is measured from, as `git rev-parse --short`
    /// renders it — a short sha, for a brief a human might also read, even
    /// when `base` resolved from a ref name (`refs/multiverse/base/<task>`)
    /// rather than a bare sha.
    pub base: String,
    /// `git diff --stat`'s own output, never truncated — it is already a
    /// one-line-per-file summary, not the thing `max_lines` caps.
    pub stat_text: String,
    pub touched_files: Vec<String>,
    /// The unified diff, truncated to at most `max_lines` lines.
    pub body_text: String,
    /// Whether `body_text` is missing lines the real diff had.
    pub truncated: bool,
}

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn ref_exists(root: &Path, reference: &str) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "--quiet", reference])
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Port of Python's `_merged_lane_tip`: recovers a lane's last commit once
/// its own branch is gone, searching `HEAD`'s own history only (never
/// `--all` — a ref this lane's commits never actually merged into isn't
/// this lane's history just because some commit on it happens to match),
/// in the same two-step order Python tries.
///
/// 1. The merge that brought it in, if there is one: a merge commit on
///    `HEAD` whose *subject* is exactly git's own default message for
///    merging `branch` — `"Merge branch '<branch>'"`, optionally followed
///    by `" into <x>"` — compared against the whole subject, never
///    grepped as a substring (a substring match on the branch name alone
///    would also catch a merge of some other branch whose name contains
///    it). Its second parent is, by how a merge commit is built, the
///    branch tip as it stood at merge time — still live in the object
///    database as long as the merge commit itself is reachable, even
///    though no ref points at it directly any more.
/// 2. A lane that landed by fast-forward or cherry-pick leaves no such
///    merge — a fast-forward just moves the branch pointer, and a
///    cherry-pick never creates one. Every writer lane is told to stamp
///    its own commits with a `Task: <task_id>` trailer, so the newest
///    `HEAD`-reachable commit carrying that exact line (a fixed-string
///    match against the whole line, not a substring search) stands in as
///    the tip instead — skipping any commit whose subject is a `git
///    revert` of another commit (`Revert "..."`): a revert on `HEAD`
///    carries the original's trailers right along with it, and is the one
///    commit that must never be mistaken for the lane tip it undoes.
fn merged_lane_tip(root: &Path, task_id: &str, branch: &str) -> Option<String> {
    let merge_subject = format!("Merge branch '{branch}'");
    let merge_subject_into = format!("{merge_subject} into ");
    let merges = git(root, &["log", "HEAD", "--merges", "--format=%H%x1f%s"])?;
    for line in merges.lines() {
        let Some((sha, subject)) = line.split_once('\u{1f}') else {
            continue;
        };
        if subject == merge_subject || subject.starts_with(&merge_subject_into) {
            let tip = git(
                root,
                &["rev-parse", "--verify", "--quiet", &format!("{sha}^2")],
            )?;
            let tip = tip.trim();
            return (!tip.is_empty()).then(|| tip.to_owned());
        }
    }

    let needle = format!("Task: {task_id}");
    let log = git(root, &["log", "HEAD", "--format=%H%x1f%B%x1e"])?;
    for record in log.split('\u{1e}') {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        let Some((sha, body)) = record.split_once('\u{1f}') else {
            continue;
        };
        let subject = body.lines().next().unwrap_or("");
        if subject.starts_with("Revert \"") {
            continue;
        }
        if body.lines().any(|line| line == needle) {
            return Some(sha.to_owned());
        }
    }
    None
}

/// `task_id`'s lane, diffed against its own cut point — the base/tip
/// resolution half of Python's `worktrees.lane_diff`. Reads directly off
/// the branch and the recorded base ref (`refs/multiverse/base/<task_id>`),
/// same as [`mv_state::worktrees::lane_touched_files`]; unlike that
/// function, this one also falls back to `merged_lane_tip` once the
/// branch itself is gone, so a lane `--prune` already reclaimed because it
/// landed cleanly still classifies instead of refusing outright. Still
/// refuses if either half is unavailable — no recorded base, or neither
/// recovery path finds a tip — same as a lane that was never dispatched as
/// a writer.
///
/// `max_lines` truncates `body_text` to at most that many lines, same cap
/// `[review_policy].max_diff_lines` already applies to the no-`--lane`
/// report's own diff reading.
pub fn lane_diff(root: &Path, task_id: &str, max_lines: u32) -> Result<LaneDiff, LaneDiffError> {
    let no_lane_diff = || LaneDiffError::NoLaneDiff {
        task_id: task_id.to_owned(),
    };
    let branch = mv_state::worktrees::branch_name(task_id).map_err(|_| no_lane_diff())?;
    let branch_ref = format!("refs/heads/{branch}");
    let base_ref = format!("refs/multiverse/base/{task_id}");
    let base_recorded = ref_exists(root, &base_ref);

    let (base, tip) = if ref_exists(root, &branch_ref) {
        let base = if base_recorded {
            base_ref
        } else {
            git(root, &["merge-base", "HEAD", &branch])
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "HEAD".to_owned())
        };
        (base, branch.clone())
    } else {
        if !base_recorded {
            return Err(no_lane_diff());
        }
        let tip = merged_lane_tip(root, task_id, &branch).ok_or_else(no_lane_diff)?;
        (base_ref, tip)
    };

    let range = format!("{base}..{tip}");
    let stat_text = git(root, &["diff", "--stat", &range])
        .unwrap_or_default()
        .trim()
        .to_owned();
    let touched_files = git(root, &["diff", "--name-only", &range])
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let body_full = git(root, &["diff", &range]).unwrap_or_default();
    let body_lines: Vec<&str> = body_full.lines().collect();
    let truncated = body_lines.len() > max_lines as usize;
    let body_text = body_lines[..body_lines.len().min(max_lines as usize)].join("\n");

    let base_display = git(root, &["rev-parse", "--short", &base])
        .map(|s| s.trim().to_owned())
        .unwrap_or(base);

    Ok(LaneDiff {
        base: base_display,
        stat_text,
        touched_files,
        body_text,
        truncated,
    })
}

/// Every path any of `commits` touches, sorted and de-duplicated — port of
/// Python's `worktrees._touched_paths_for_commits` (`worktrees.py:1089-1095`).
fn touched_paths_for_commits(root: &Path, commits: &[String]) -> Vec<String> {
    let mut paths: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for sha in commits {
        if let Some(output) = git(
            root,
            &[
                "diff-tree",
                "--no-commit-id",
                "--name-only",
                "-r",
                sha.as_str(),
            ],
        ) {
            paths.extend(
                output
                    .lines()
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned),
            );
        }
    }
    paths.into_iter().collect()
}

/// The diff for exactly `commits` — a fix round's own commits
/// ([`fix_round_commits`]), or an explicit `--commits sha...` override
/// (`mv_cli::commands::dispatch`'s `RealDispatchStore::review_scope`) —
/// restricted to the paths those commits actually touch. Port of Python's
/// `worktrees.commit_set_diff` (`worktrees.py:1098-1154`).
///
/// Built as `git diff <first's parent>..<last> -- <touched paths>` rather
/// than a plain `since..branch` range: the point of naming an exact commit
/// set is to leave out whatever else happened on the branch in between (a
/// `git merge main` brought in to resolve a conflict, say), and restricting
/// the diff to only the files these commits touched is what keeps an
/// unrelated merge's own changes to other files from sweeping back in.
///
/// `commits` is trusted to be given oldest-first, on the lane's own
/// branch — the caller is responsible for that ordering; this does not
/// re-sort or validate it. Never fails: an unreadable `commits[0]^` or an
/// empty touched set falls back to `commits[0]` itself / an empty diff,
/// same as Python's own best-effort git calls.
pub fn commit_set_diff(root: &Path, commits: &[String], max_lines: u32) -> LaneDiff {
    if commits.is_empty() {
        return LaneDiff {
            base: "(none)".to_string(),
            stat_text: String::new(),
            touched_files: Vec::new(),
            body_text: String::new(),
            truncated: false,
        };
    }

    let touched = touched_paths_for_commits(root, commits);
    let since = git(root, &["rev-parse", &format!("{}^", commits[0])])
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| commits[0].clone());
    let until = &commits[commits.len() - 1];

    let (stat_text, body_text, truncated) = if touched.is_empty() {
        (String::new(), String::new(), false)
    } else {
        let mut stat_args = vec!["diff", "--stat", since.as_str(), until.as_str(), "--"];
        stat_args.extend(touched.iter().map(String::as_str));
        let stat_text = git(root, &stat_args).unwrap_or_default().trim().to_owned();

        let mut diff_args = vec!["diff", since.as_str(), until.as_str(), "--"];
        diff_args.extend(touched.iter().map(String::as_str));
        let body_full = git(root, &diff_args).unwrap_or_default();
        let body_lines: Vec<&str> = body_full.lines().collect();
        let truncated = body_lines.len() > max_lines as usize;
        let body_text = body_lines[..body_lines.len().min(max_lines as usize)].join("\n");
        (stat_text, body_text, truncated)
    };

    let base_display = git(root, &["rev-parse", "--short", &since])
        .map(|s| s.trim().to_owned())
        .unwrap_or(since);

    LaneDiff {
        base: base_display,
        stat_text,
        touched_files: touched,
        body_text,
        truncated,
    }
}

/// `task_id`'s own commits after `since`, oldest first, excluding merges —
/// what [`commit_set_diff`] needs to build the auto-detected fix-round
/// diff. Port of Python's `worktrees.fix_round_commits` (`worktrees.py:1157-1180`).
///
/// `--first-parent` walks only the lane's own mainline, skipping commits
/// `main` itself made in between that a plain `since..branch` range would
/// otherwise sweep in the moment the lane merges `main` in to catch up;
/// `--no-merges` on top of that then drops the merge commit itself too,
/// leaving exactly the commits the fixer authored directly on the lane
/// after `since`.
pub fn fix_round_commits(
    root: &Path,
    task_id: &str,
    since: &str,
) -> Result<Vec<String>, LaneDiffError> {
    let branch =
        mv_state::worktrees::branch_name(task_id).map_err(|_| LaneDiffError::NoLaneDiff {
            task_id: task_id.to_owned(),
        })?;
    let range = format!("{since}..{branch}");
    Ok(git(
        root,
        &[
            "rev-list",
            "--first-parent",
            "--no-merges",
            "--reverse",
            &range,
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter(|line| !line.is_empty())
    .map(str::to_owned)
    .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_ok(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    struct Repo(tempfile::TempDir);
    impl Repo {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            git_ok(dir.path(), &["init", "-q", "-b", "main"]);
            git_ok(dir.path(), &["config", "user.email", "t@example.invalid"]);
            git_ok(dir.path(), &["config", "user.name", "Test"]);
            git_ok(dir.path(), &["config", "commit.gpgsign", "false"]);
            fs::write(dir.path().join("README.md"), "hi\n").unwrap();
            git_ok(dir.path(), &["add", "."]);
            git_ok(dir.path(), &["commit", "-q", "-m", "initial"]);
            Self(dir)
        }

        fn path(&self) -> &Path {
            self.0.path()
        }

        fn record_base(&self, task: &str) {
            let sha = String::from_utf8(
                Command::new("git")
                    .arg("-C")
                    .arg(self.path())
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap();
            git_ok(
                self.path(),
                &[
                    "update-ref",
                    &format!("refs/multiverse/base/{task}"),
                    sha.trim(),
                ],
            );
        }

        fn write_commit(&self, path: &str, contents: &str, message: &str) {
            let full = self.path().join(path);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(&full, contents).unwrap();
            git_ok(self.path(), &["add", "."]);
            git_ok(self.path(), &["commit", "-q", "-m", message]);
        }

        fn head(&self) -> String {
            String::from_utf8(
                Command::new("git")
                    .arg("-C")
                    .arg(self.path())
                    .args(["rev-parse", "HEAD"])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .trim()
            .to_owned()
        }
    }

    #[test]
    fn a_live_branch_diffs_against_its_recorded_base() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        git_ok(
            repo.path(),
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        repo.write_commit("src/auth/login.rs", "fn login() {}\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert_eq!(diff.touched_files, vec!["src/auth/login.rs".to_string()]);
        assert!(diff.body_text.contains("+fn login() {}"));
    }

    #[test]
    fn no_branch_and_no_recorded_base_refuses() {
        let repo = Repo::new();
        let err = lane_diff(repo.path(), "20261101-fixer-abcdef", 1500).unwrap_err();
        assert_eq!(
            err,
            LaneDiffError::NoLaneDiff {
                task_id: "20261101-fixer-abcdef".into()
            }
        );
    }

    #[test]
    fn a_merged_lane_recovers_its_tip_from_the_merge_commit() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        let branch = format!("multiverse/{task}");
        repo.record_base(task);
        git_ok(repo.path(), &["checkout", "-q", "-b", &branch]);
        repo.write_commit("src/auth/login.rs", "fn login() {}\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);
        git_ok(
            repo.path(),
            &[
                "merge",
                "--no-ff",
                "-m",
                &format!("Merge branch '{branch}'"),
                &branch,
            ],
        );
        // The lane's own branch is reclaimed once merged — exactly the
        // state `--prune` leaves behind.
        git_ok(repo.path(), &["branch", "-D", &branch]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert_eq!(diff.touched_files, vec!["src/auth/login.rs".to_string()]);
        assert!(diff.body_text.contains("+fn login() {}"));
    }

    #[test]
    fn a_fast_forwarded_lane_recovers_its_tip_from_the_task_trailer() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        let branch = format!("multiverse/{task}");
        repo.record_base(task);
        git_ok(repo.path(), &["checkout", "-q", "-b", &branch]);
        fs::write(repo.path().join("src.rs"), "fn login() {}\n").unwrap();
        git_ok(repo.path(), &["add", "."]);
        git_ok(
            repo.path(),
            &[
                "commit",
                "-q",
                "-m",
                &format!("fix(cli): port login\n\nTask: {task}\n"),
            ],
        );
        git_ok(repo.path(), &["checkout", "-q", "main"]);
        git_ok(repo.path(), &["merge", "--ff-only", &branch]);
        git_ok(repo.path(), &["branch", "-D", &branch]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert_eq!(diff.touched_files, vec!["src.rs".to_string()]);
        assert!(diff.body_text.contains("+fn login() {}"));
    }

    #[test]
    fn a_revert_carrying_the_same_trailer_does_not_shadow_the_original_tip() {
        // A `git revert` commit can carry the very same `Task:` trailer the
        // lane's own commit does — e.g. a correction round that stamps
        // every commit it writes, including a revert of its own mistake —
        // so without explicitly skipping a `Revert "..."` subject, the
        // revert (newer than the lane's own commit) would be picked as the
        // tip, and the recovered diff would show the lane's change undone
        // instead of the change itself. `git revert` does not add the
        // trailer on its own, so this constructs that commit explicitly,
        // same as the Python conformance fixture does.
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        let branch = format!("multiverse/{task}");
        repo.record_base(task);
        git_ok(repo.path(), &["checkout", "-q", "-b", &branch]);
        fs::write(repo.path().join("src.rs"), "fn login() {}\n").unwrap();
        git_ok(repo.path(), &["add", "."]);
        git_ok(
            repo.path(),
            &[
                "commit",
                "-q",
                "-m",
                &format!("work for {task}\n\nTask: {task}\n"),
            ],
        );
        let trailer_sha = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let trailer_sha = trailer_sha.trim();
        git_ok(repo.path(), &["checkout", "-q", "main"]);
        git_ok(repo.path(), &["merge", "--ff-only", &branch]);
        git_ok(repo.path(), &["branch", "-D", &branch]);

        git_ok(repo.path(), &["revert", "--no-edit", "-n", trailer_sha]);
        git_ok(
            repo.path(),
            &[
                "commit",
                "-q",
                "-m",
                &format!(
                    "Revert \"work for {task}\"\n\nThis reverts commit {trailer_sha}.\n\nTask: {task}\n"
                ),
            ],
        );

        // The revert is newer but skipped on its `Revert "..."` subject;
        // the lane's own trailer-carrying commit — still reachable from
        // `HEAD` since the revert only undoes its *content*, not its
        // ancestry — is recovered instead, so the diff still shows the
        // lane's real change rather than nothing (or the revert undone).
        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert_eq!(diff.touched_files, vec!["src.rs".to_string()]);
        assert!(diff.body_text.contains("+fn login() {}"));
    }

    #[test]
    fn body_text_truncates_to_max_lines() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        git_ok(
            repo.path(),
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        repo.write_commit("many.txt", "a\nb\nc\nd\ne\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let diff = lane_diff(repo.path(), task, 2).unwrap();
        assert_eq!(diff.body_text.lines().count(), 2);
        assert!(diff.truncated);
    }

    #[test]
    fn truncated_is_false_when_the_diff_fits_under_max_lines() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        git_ok(
            repo.path(),
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        repo.write_commit("few.txt", "a\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert!(!diff.truncated);
    }

    #[test]
    fn base_is_the_recorded_base_ref_rendered_as_a_short_sha() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        let expected_base = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["rev-parse", "--short", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        git_ok(
            repo.path(),
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        repo.write_commit("src/auth/login.rs", "fn login() {}\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert_eq!(diff.base, expected_base.trim());
    }

    #[test]
    fn stat_text_carries_git_diff_stats_own_summary_line() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        git_ok(
            repo.path(),
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        repo.write_commit("src/auth/login.rs", "fn login() {}\n", "work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let diff = lane_diff(repo.path(), task, 1500).unwrap();
        assert!(diff.stat_text.contains("src/auth/login.rs"));
        assert!(diff.stat_text.contains("1 file changed"));
    }

    // -- task 20261106-fixer-fbf55c: `commit_set_diff` / `fix_round_commits` --

    #[test]
    fn commit_set_diff_restricts_to_the_touched_paths_of_exactly_the_named_commits() {
        let repo = Repo::new();
        repo.write_commit("a.rs", "fn a() {}\n", "commit A");
        let commit_a = repo.head();
        repo.write_commit("b.rs", "fn b() {}\n", "commit B");
        let commit_b = repo.head();

        let diff = commit_set_diff(repo.path(), &[commit_b], 1500);
        assert_eq!(diff.touched_files, vec!["b.rs".to_string()]);
        assert!(diff.body_text.contains("+fn b() {}"));
        assert!(!diff.body_text.contains("+fn a() {}"));
        let expected_base = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(repo.path())
                .args(["rev-parse", "--short", &commit_a])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(diff.base, expected_base.trim());
    }

    #[test]
    fn commit_set_diff_leaves_out_an_intervening_merges_own_unrelated_file_changes() {
        // The whole point of naming an exact commit set rather than a
        // plain `since..until` range: a `git merge main` the lane pulled in
        // between two of its own commits (to resolve a conflict, say)
        // brings main's own file changes into that range too — restricting
        // the diff to the named commits' own touched paths (`-- touched`)
        // is what keeps that unrelated file out, even though an unscoped
        // `git diff since until` would show it.
        let repo = Repo::new();
        let initial = repo.head();
        repo.write_commit("fixer1.rs", "fn one() {}\n", "fixer commit 1");
        let commit_1 = repo.head();
        git_ok(repo.path(), &["branch", "other-work", &initial]);
        git_ok(repo.path(), &["checkout", "-q", "other-work"]);
        repo.write_commit("main.rs", "fn main_work() {}\n", "unrelated main work");
        git_ok(repo.path(), &["checkout", "-q", "main"]);
        git_ok(
            repo.path(),
            &[
                "merge",
                "--no-ff",
                "-m",
                "Merge branch 'other-work'",
                "other-work",
            ],
        );
        repo.write_commit("fixer2.rs", "fn two() {}\n", "fixer commit 2");
        let commit_2 = repo.head();

        // Sanity check: an unscoped diff over the same range does sweep in
        // the merge's own file, which is exactly what `commit_set_diff`
        // must avoid.
        let unscoped = git(
            repo.path(),
            &["diff", "--name-only", &format!("{initial}..{commit_2}")],
        )
        .unwrap();
        assert!(unscoped.contains("main.rs"));

        let diff = commit_set_diff(repo.path(), &[commit_1, commit_2], 1500);
        assert_eq!(
            diff.touched_files,
            vec!["fixer1.rs".to_string(), "fixer2.rs".to_string()]
        );
        assert!(diff.body_text.contains("+fn one() {}"));
        assert!(diff.body_text.contains("+fn two() {}"));
        assert!(!diff.body_text.contains("main_work"));
    }

    #[test]
    fn commit_set_diff_is_empty_for_an_empty_commit_list() {
        let repo = Repo::new();
        let diff = commit_set_diff(repo.path(), &[], 1500);
        assert_eq!(diff.base, "(none)");
        assert!(diff.touched_files.is_empty());
        assert!(diff.body_text.is_empty());
        assert!(!diff.truncated);
    }

    #[test]
    fn fix_round_commits_lists_only_the_lanes_own_commits_after_since_oldest_first() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        let branch = format!("multiverse/{task}");
        git_ok(repo.path(), &["checkout", "-q", "-b", &branch]);
        repo.write_commit("a.rs", "fn a() {}\n", "commit A");
        let since = repo.head();
        repo.write_commit("b.rs", "fn b() {}\n", "commit B");
        let commit_b = repo.head();
        repo.write_commit("c.rs", "fn c() {}\n", "commit C");
        let commit_c = repo.head();
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let commits = fix_round_commits(repo.path(), task, &since).unwrap();
        assert_eq!(commits, vec![commit_b, commit_c]);
    }

    #[test]
    fn fix_round_commits_excludes_a_main_merge_brought_in_between() {
        let repo = Repo::new();
        let task = "20261101-fixer-abcdef";
        repo.record_base(task);
        let branch = format!("multiverse/{task}");
        git_ok(repo.path(), &["checkout", "-q", "-b", &branch]);
        repo.write_commit("a.rs", "fn a() {}\n", "commit A");
        let since = repo.head();
        git_ok(repo.path(), &["checkout", "-q", "main"]);
        repo.write_commit("main.rs", "fn main_work() {}\n", "main work");
        git_ok(repo.path(), &["checkout", "-q", &branch]);
        git_ok(
            repo.path(),
            &[
                "merge",
                "--no-ff",
                "-m",
                "Merge branch 'main' into lane",
                "main",
            ],
        );
        repo.write_commit("b.rs", "fn b() {}\n", "commit B");
        let commit_b = repo.head();
        git_ok(repo.path(), &["checkout", "-q", "main"]);

        let commits = fix_round_commits(repo.path(), task, &since).unwrap();
        assert_eq!(commits, vec![commit_b]);
    }
}

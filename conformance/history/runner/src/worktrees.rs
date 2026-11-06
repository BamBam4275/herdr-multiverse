//! Worktree directory checks.
//!
//! `mv-state::worktrees` (crates/mv-state/src/worktrees.rs, 2109 lines)
//! has since landed, but as git-worktree lifecycle machinery (branch
//! naming, `dispatched_lane_is_clean`, pruning, `HerdrLaneObserver`) that
//! operates on a live checkout. Evaluated for the swap; decision is **no
//! swap**: there is no function there for "given a directory name and a
//! nested `multiverse-state/tasks/`, check the shape `collect.sh`
//! captured" — this module's job has no live-checkout equivalent to call
//! into, because `collect.sh` deliberately never copies the git checkout
//! itself (see its own header and this lane's result.md "what I did not
//! cover"). Branch-naming and git-ref checks stay a real gap, not
//! implemented here: closing it needs `collect.sh` to capture branch
//! metadata first, independent of this swap question.

use std::path::Path;

use crate::finding::Report;
use crate::tasks::{check_tasks_root, looks_like_task_id};

pub fn check_worktrees_root(worktrees_root: &Path, report: &mut Report) {
    let Ok(entries) = std::fs::read_dir(worktrees_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();

        if looks_like_task_id(&name) {
            report.note("worktrees.dir_name_task_id_shaped");
        } else {
            report.fail(
                &path,
                None,
                format!("worktree directory name is not task-id-shaped: {name:?}"),
            );
        }

        let nested_tasks = path.join("multiverse-state").join("tasks");
        if nested_tasks.is_dir() {
            report.note("worktrees.has_nested_task_dir");
            check_tasks_root(&nested_tasks, report);

            // A nested task dir whose id differs from the worktree it
            // lives in is *not* drift: sampled fleet data shows the normal
            // shape of worktree adoption (`mv-state::worktrees`'s doc comment
            // names "adopt" as a lifecycle verb) — a later
            // Explorer/Librarian/Reviewer follow-up in the same lane runs
            // in the already-checked-out worktree and gets its own
            // nested task dir alongside the original. An earlier version
            // of this check flagged every one of those as a finding (45
            // false positives against the real corpus before this fix);
            // counted as a variant instead.
            if let Ok(entries) = std::fs::read_dir(&nested_tasks) {
                for nested in entries.flatten() {
                    let nested_name = nested.file_name().to_str().unwrap_or_default().to_string();
                    if nested.path().is_dir() {
                        report.note(if nested_name == name {
                            "worktrees.nested_task_dir_matches_worktree"
                        } else {
                            "worktrees.nested_task_dir_is_adopted_followup"
                        });
                    }
                }
            }
        }
    }
}

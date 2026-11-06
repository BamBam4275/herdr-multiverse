//! `result <task_id>`: read and print a terminal task's redacted result.

use std::io::{self, BufRead};
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_engine::gateway::{self, SecureFiles};
use mv_state::tasks::{read_status_verbose, StatusRead};

use crate::exit::Failure;

use super::{doctor::project_path, load_config};

/// `gateway::read_result`'s own refusal text collapses "never wrote a
/// status file" and "wrote something we don't recognize" into one
/// generic "no terminal status" message — right for the nonterminal
/// (`running`) case, but not what a history reader needs for these two:
/// spec-on-disk-contract.md calls out `donedone`/`donedonedone`/
/// `doneblocked` from the live fleet's partial writes, and callers need
/// to tell "ambiguous, might still be running" from "refusing to guess".
/// Checked before handing off to `gateway::read_result`, which still owns
/// the lane-fallback, nonterminal, and result-reading rules unchanged.
fn classify_status(root: &Path, task_id: &str) -> StatusRead {
    let root_status = root.join(".multiverse/tasks").join(task_id).join("status");
    match read_status_verbose(&root_status) {
        StatusRead::Absent => {
            let lane_status = root
                .join(".multiverse/worktrees")
                .join(task_id)
                .join(".multiverse/tasks")
                .join(task_id)
                .join("status");
            read_status_verbose(&lane_status)
        }
        other => other,
    }
}

/// Opens relative to `root` through `mv_core::secure_open::open_regular`
/// (card `20261102-b-f25484`) — the shared `openat`/`O_NOFOLLOW` chain also
/// used by `commands::inspect`'s `RealFiles`, rather than this command
/// carrying its own copy of the same walk (what this struct used to do,
/// back when `inspect.rs` belonged to another lane's scope).
struct CheckedFiles {
    root: PathBuf,
}

impl SecureFiles for CheckedFiles {
    fn open_regular(&self, relative: &Path) -> io::Result<Box<dyn BufRead>> {
        mv_core::secure_open::open_regular(&self.root, relative, "result")
    }
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    let task_id = args.get_one::<String>("task_id").expect("required");
    let config = load_config(args)?;
    let cwd = std::env::current_dir().map_err(|error| Failure::Operation(error.to_string()))?;
    let root = config
        .projects
        .values()
        .filter_map(|project| {
            project_path(project).and_then(|path| std::fs::canonicalize(path).ok())
        })
        .filter(|root| cwd.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| {
            Failure::Config(format!("no configured project contains {}", cwd.display()))
        })?;
    if mv_state::paths::is_valid_task_id(task_id) {
        match classify_status(&root, task_id) {
            StatusRead::Absent => {
                return Err(Failure::Operation(format!(
                    "task '{task_id}' has no status file. A running task has no status file \
                     at all, so this absence is ambiguous — it may still be running, or the \
                     task id may not exist"
                )));
            }
            StatusRead::Unrecognized(raw) => {
                return Err(Failure::Operation(format!(
                    "task '{task_id}' has an unrecognized status ('{raw}'); refusing to guess \
                     whether it is finished"
                )));
            }
            StatusRead::Terminal(_) => {}
        }
    }
    let files = CheckedFiles { root };
    let result = gateway::read_result(&files, task_id, &mv_core::redact::redact)
        .map_err(|refusal| Failure::Operation(refusal.0))?;
    print!("{}", mv_board::layout::sanitize_multiline(&result.text));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mv-cli-result-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const TASK: &str = "20261101-fixer-abcdef";

    #[test]
    fn reads_and_redacts_a_done_task_result() {
        let root = temp_dir("done");
        let task_dir = root.join(".multiverse/tasks").join(TASK);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(
            task_dir.join("result.md"),
            "token: SECRET_TOKEN=abcdef1234567890\n",
        )
        .unwrap();

        let files = CheckedFiles { root: root.clone() };
        let result = gateway::read_result(&files, TASK, &mv_core::redact::redact).unwrap();
        assert!(!result.text.contains("abcdef1234567890"));
        assert_eq!(result.status, gateway::TaskStatus::Done);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_result_file_is_refused_not_followed() {
        let root = temp_dir("symlink");
        let task_dir = root.join(".multiverse/tasks").join(TASK);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        let outside = root.join("outside.md");
        fs::write(&outside, "secret elsewhere").unwrap();
        std::os::unix::fs::symlink(&outside, task_dir.join("result.md")).unwrap();

        let files = CheckedFiles { root: root.clone() };
        assert!(gateway::read_result(&files, TASK, &mv_core::redact::redact).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symlinked_intermediate_task_directory_is_refused_not_followed() {
        let root = temp_dir("symlink-dir");
        let outside_task = root.join("outside/.multiverse/tasks").join(TASK);
        fs::create_dir_all(&outside_task).unwrap();
        fs::write(outside_task.join("status"), "done\n").unwrap();
        fs::write(outside_task.join("result.md"), "secret elsewhere").unwrap();

        fs::create_dir_all(root.join(".multiverse/tasks")).unwrap();
        std::os::unix::fs::symlink(&outside_task, root.join(".multiverse/tasks").join(TASK))
            .unwrap();

        let files = CheckedFiles { root: root.clone() };
        assert!(gateway::read_result(&files, TASK, &mv_core::redact::redact).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_nonterminal_status_is_refused() {
        let root = temp_dir("nonterminal");
        let task_dir = root.join(".multiverse/tasks").join(TASK);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "running\n").unwrap();

        let files = CheckedFiles { root: root.clone() };
        assert!(gateway::read_result(&files, TASK, &mv_core::redact::redact).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn result_text_strips_osc_52_and_keeps_markdown_newlines() {
        let dirty = "# findings\n\u{1b}]52;c;QUJDRA==\u{07}body\n";
        let clean = mv_board::layout::sanitize_multiline(dirty);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{07}'));
        assert_eq!(clean, "# findings\nbody\n");
    }
}

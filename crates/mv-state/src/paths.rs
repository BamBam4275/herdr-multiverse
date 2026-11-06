//! `TaskPaths`: containment rules so a task id can never traverse
//! above its own task directory.

use std::io;
use std::path::{Path, PathBuf, StripPrefixError};

/// Mirrors `multiverse.config.PROJECT_STATE_DIR` (`config.py:78`). A
/// one-line private copy:
/// `mv-config` owns the real constant but has not landed it yet.
const PROJECT_STATE_DIR: &str = ".multiverse";

/// Whether `value` matches the shared identifier grammar used for task
/// ids: 1-64 characters, starting with a letter or digit, the rest
/// letters, digits, `.`, `_`, or `-`.
///
/// A character-class rule, not a role or kind enum — a history-era id
/// like `20261031-fixer-ab12cd` validates exactly the same as a
/// current one minted under the new role names, so old role names
/// embedded in historical task ids keep reading fine without this
/// function knowing anything about role names at all.
pub fn is_valid_task_id(value: &str) -> bool {
    mv_core::id::is_valid_identifier(value)
}

/// A task id that failed [`is_valid_task_id`], or whose task directory
/// did not resolve strictly inside the tasks root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskPathError {
    #[error("invalid task id {0:?}: must match ^[A-Za-z0-9][A-Za-z0-9._-]{{0,63}}$")]
    InvalidTaskId(String),
    #[error("task {0:?} resolves outside its task root or cannot be resolved")]
    OutsideTaskRoot(String),
}

/// Resolve `path` as far as an existing ancestor allows, then rejoin
/// the non-existing tail literally.
///
/// `Path::canonicalize` refuses outright when any component is
/// missing, but a task directory is resolved *before* it is created.
/// This mirrors Python's `Path.resolve()` (`strict=False` by default),
/// which resolves symlinks on every existing prefix and leaves the
/// rest exactly as written — `task_dir` (`brief.py:91-106`) relies on
/// exactly that behaviour to catch a symlink planted inside an
/// already-existing `tasks/` directory before the candidate directory
/// itself exists.
fn resolve_best_effort(path: &Path) -> io::Result<PathBuf> {
    if let Ok(resolved) = path.canonicalize() {
        return Ok(resolved);
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    let resolved_parent = resolve_best_effort(parent)?;
    Ok(resolved_parent.join(file_name))
}

/// Directory holding one task's artifacts, strictly inside the task root.
///
/// Mirrors `task_dir` (`brief.py:91-106`): validates `task_id` against
/// the identifier grammar, then confirms the resolved directory is a
/// strict descendant of `<project_root>/.multiverse/tasks` — not just
/// lexically, but after resolving symlinks on whatever already exists,
/// so a symlink planted under a task id a caller is about to use
/// cannot redirect a write outside the tasks root.
pub fn task_dir(project_root: &Path, task_id: &str) -> Result<PathBuf, TaskPathError> {
    if !is_valid_task_id(task_id) {
        return Err(TaskPathError::InvalidTaskId(task_id.to_string()));
    }
    let tasks_root = project_root.join(PROJECT_STATE_DIR).join("tasks");
    let root = tasks_root.join(task_id);

    let resolved_tasks_root = resolve_best_effort(&tasks_root)
        .map_err(|_| TaskPathError::OutsideTaskRoot(task_id.to_string()))?;
    let resolved_root = resolve_best_effort(&root)
        .map_err(|_| TaskPathError::OutsideTaskRoot(task_id.to_string()))?;

    if resolved_root == resolved_tasks_root || !resolved_root.starts_with(&resolved_tasks_root) {
        return Err(TaskPathError::OutsideTaskRoot(task_id.to_string()));
    }

    Ok(root)
}

/// The artifact paths for one task: `brief.md`, `result.md`, `status`,
/// and the optional `revise`/`retire` sidecars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskPaths {
    root: PathBuf,
}

impl TaskPaths {
    /// Build directly from an already-resolved task directory.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Build the paths rooted at `task_id`'s task directory, validating
    /// and containment-checking it first.
    pub fn for_task(project_root: &Path, task_id: &str) -> Result<Self, TaskPathError> {
        Ok(Self::new(task_dir(project_root, task_id)?))
    }

    /// The task's own directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The assignment file the specialist reads before acting.
    pub fn brief(&self) -> PathBuf {
        self.root.join("brief.md")
    }

    /// The findings file the specialist writes after doing the work.
    pub fn result(&self) -> PathBuf {
        self.root.join("result.md")
    }

    /// The outcome file: exactly one of `done`, `blocked`, `failed`.
    /// Absence means the task is still running — see
    /// [`crate::tasks::read_status`].
    pub fn status(&self) -> PathBuf {
        self.root.join("status")
    }

    /// Artifacts this task's result asks someone else to change, one
    /// per line. Absence means nothing to act on; **never written
    /// empty** — an empty file reads as a finding somebody lost
    /// (Q21, `brief.py:148-166`).
    pub fn revise(&self) -> PathBuf {
        self.root.join("revise")
    }

    /// Branches a completed assessment authorises archiving, one
    /// `task-id full-sha` pair per line. Only a finished Scout/Seer
    /// assessment writes this. Absence authorises nothing to archive;
    /// **never written empty**, for the same reason as [`Self::revise`]
    /// (`brief.py:169-193`).
    pub fn retire(&self) -> PathBuf {
        self.root.join("retire")
    }

    /// The same paths, expressed relative to `project_root` — how they
    /// read in a prompt. An absolute path is for anything the agent
    /// must resolve without a working directory.
    pub fn relative_to_project(&self, project_root: &Path) -> Result<Self, StripPrefixError> {
        Ok(Self::new(
            self.root.strip_prefix(project_root)?.to_path_buf(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "mv-state-paths-test-{tag}-{nanos}-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rejects_noncanonical_task_ids() {
        for task_id in [
            "",
            ".",
            "..",
            "../../outside",
            "/tmp/escape",
            "a\\b",
            "a\x00b",
            "a\nb",
            "a b",
            "a=b",
            "-task",
            &"x".repeat(65),
        ] {
            assert!(!is_valid_task_id(task_id), "{task_id:?} should be rejected");
            let dir = temp_dir("reject");
            assert!(
                task_dir(&dir, task_id).is_err(),
                "{task_id:?} should be rejected"
            );
            assert!(
                TaskPaths::for_task(&dir, task_id).is_err(),
                "{task_id:?} should be rejected"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn accepts_canonical_ids_strictly_inside_the_task_root() {
        for task_id in ["t1", "20261101-fixer-abc123", "A.b_c-2", &"x".repeat(64)] {
            let dir = temp_dir("accept");
            let paths = TaskPaths::for_task(&dir, task_id).unwrap();
            assert_eq!(
                paths.root().strip_prefix(&dir).unwrap(),
                Path::new(".multiverse").join("tasks").join(task_id)
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn rejects_a_task_symlink_outside_the_tasks_root() {
        #[cfg(unix)]
        {
            let dir = temp_dir("symlink");
            let tasks_root = dir.join(".multiverse").join("tasks");
            std::fs::create_dir_all(&tasks_root).unwrap();
            let outside = dir.join("outside");
            std::fs::create_dir_all(&outside).unwrap();
            std::os::unix::fs::symlink(&outside, tasks_root.join("20261101-explorer-abc123"))
                .unwrap();

            let err = task_dir(&dir, "20261101-explorer-abc123").unwrap_err();
            assert!(matches!(err, TaskPathError::OutsideTaskRoot(_)));

            let err = TaskPaths::for_task(&dir, "20261101-explorer-abc123").unwrap_err();
            assert!(matches!(err, TaskPathError::OutsideTaskRoot(_)));

            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn task_dir_is_under_the_state_directory() {
        let dir = temp_dir("state-dir");
        let resolved = task_dir(&dir, "t1").unwrap();
        let components: Vec<_> = resolved
            .strip_prefix(&dir)
            .unwrap()
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        assert_eq!(components, vec![".multiverse", "tasks", "t1"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn task_paths_layout_matches_the_spec() {
        let paths = TaskPaths::new(PathBuf::from("/repo/.multiverse/tasks/t1"));

        assert_eq!(
            paths.brief(),
            PathBuf::from("/repo/.multiverse/tasks/t1/brief.md")
        );
        assert_eq!(
            paths.result(),
            PathBuf::from("/repo/.multiverse/tasks/t1/result.md")
        );
        assert_eq!(
            paths.status(),
            PathBuf::from("/repo/.multiverse/tasks/t1/status")
        );
    }

    #[test]
    fn retire_sits_next_to_result_like_revise() {
        let paths = TaskPaths::new(PathBuf::from("/repo/.multiverse/tasks/t1"));

        assert_eq!(
            paths.retire(),
            PathBuf::from("/repo/.multiverse/tasks/t1/retire")
        );
        assert_eq!(paths.retire().parent(), paths.result().parent());
    }

    #[test]
    fn task_paths_can_be_made_relative() {
        let paths = TaskPaths::new(PathBuf::from("/repo/.multiverse/tasks/t1"));
        let relative = paths.relative_to_project(Path::new("/repo")).unwrap();
        assert_eq!(
            relative.brief(),
            PathBuf::from(".multiverse/tasks/t1/brief.md")
        );
    }
}

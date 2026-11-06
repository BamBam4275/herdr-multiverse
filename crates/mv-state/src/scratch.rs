//! Owned scratch-pane records below `.multiverse/tasks`. Pane creation belongs
//! to the command adapter, which reserves intent under the project lock before
//! splitting, then completes or removes the record.
//! Close verifies the current pane label before closing, then removes the record.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use mv_herdr::{argv, run};
use serde::{Deserialize, Serialize};

use crate::worktrees::{guarded_path, private_dir, read_regular, validate_id, WorktreeError};

#[derive(Debug, thiserror::Error)]
pub enum ScratchError {
    #[error(transparent)]
    Path(#[from] WorktreeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Refused(String),
}

type Result<T> = std::result::Result<T, ScratchError>;

// Field order is Python's json.dumps(sort_keys=True) order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScratchPane {
    pub name: String,
    pub pane: String,
    pub task: String,
}

impl ScratchPane {
    pub fn new(task: &str, pane: &str) -> Result<Self> {
        validate_id(task)?;
        if pane.trim().is_empty() || pane.chars().any(char::is_control) {
            return Err(ScratchError::Refused("invalid scratch pane id".into()));
        }
        Ok(Self {
            name: format!("scratch-{task}"),
            pane: pane.into(),
            task: task.into(),
        })
    }

    fn opening(task: &str) -> Result<Self> {
        validate_id(task)?;
        Ok(Self {
            name: format!("scratch-{task}"),
            pane: String::new(),
            task: task.into(),
        })
    }

    fn validate(&self, task: &str) -> Result<()> {
        let expected = if self.pane.is_empty() {
            Self::opening(task)?
        } else {
            Self::new(task, &self.pane)?
        };
        if self != &expected {
            return Err(ScratchError::Refused(
                "scratch record does not belong to this task".into(),
            ));
        }
        Ok(())
    }

    /// Python's sorted, ASCII JSON, including its spaces and trailing newline.
    pub fn encode(&self) -> Result<String> {
        self.validate(&self.task)?;
        Ok(format!(
            "{{\"name\": {}, \"pane\": {}, \"task\": {}}}\n",
            ascii_json(&self.name)?,
            ascii_json(&self.pane)?,
            ascii_json(&self.task)?
        ))
    }
}

fn ascii_json(value: &str) -> Result<String> {
    let encoded = serde_json::to_string(value)?;
    let mut result = String::new();
    for c in encoded.chars() {
        if c.is_ascii() {
            result.push(c);
        } else {
            for unit in c.encode_utf16(&mut [0; 2]) {
                result.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    Ok(result)
}

/// Identity shared by every scratch operation: the registered lane owns
/// the record, and the routing-log occupant owns the task.
#[derive(Debug)]
pub struct ScratchContext {
    pub project_root: PathBuf,
    pub lane_root: PathBuf,
    pub task: String,
}

pub fn resolve_context(pane: Option<&str>, cwd: &Path) -> Result<ScratchContext> {
    let pane = pane
        .map(str::trim)
        .filter(|pane| !pane.is_empty())
        .ok_or_else(|| {
            ScratchError::Refused(
                "refusing scratch outside a dispatched lane: HERDR_PANE_ID is unset".into(),
            )
        })?;
    let cwd = fs::canonicalize(cwd)?;
    let lane = cwd
        .ancestors()
        .find(|candidate| {
            candidate.parent().is_some_and(|parent| {
                parent.file_name().is_some_and(|name| name == "worktrees")
                    && parent
                        .parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == ".multiverse")
            })
        })
        .ok_or_else(|| {
            ScratchError::Refused("refusing scratch outside a dispatched lane".into())
        })?;
    let project = lane
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("lane ancestry checked");
    let lane_id = lane
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ScratchError::Refused("invalid lane id".into()))?;
    let branch = crate::worktrees::branch_name(lane_id)?;
    let listing = std::process::Command::new("git")
        .args(["worktree", "list", "--porcelain", "-z"])
        .current_dir(project)
        .output()?;
    let output = String::from_utf8_lossy(&listing.stdout);
    let mut registered_path = None;
    let mut registered = false;
    for field in output.split('\0') {
        if let Some(path) = field.strip_prefix("worktree ") {
            registered_path = Some(PathBuf::from(path));
        } else if let Some(found_branch) = field.strip_prefix("branch refs/heads/") {
            if registered_path.as_deref() == Some(lane) && found_branch == branch {
                registered = true;
            }
        }
    }
    if !listing.status.success() || !registered {
        return Err(ScratchError::Refused(
            "refusing scratch: not a registered dispatched lane".into(),
        ));
    }
    let task = crate::tasks::sweep_tasks(project)
        .into_iter()
        .filter(|task| task.pane.as_deref() == Some(pane) && task.branch == branch)
        .max_by_key(|task| task.dispatch_ordinal)
        .ok_or_else(|| {
            ScratchError::Refused(format!(
                "refusing scratch: pane {pane} is not the dispatched occupant of lane {lane_id}"
            ))
        })?;
    validate_id(&task.task_id)?;
    let relative = Path::new(".multiverse/tasks").join(&task.task_id);
    let task_root = guarded_path(lane, &relative, false)?;
    if !task_root.is_dir() {
        return Err(ScratchError::Refused(
            "refusing scratch: task directory is missing".into(),
        ));
    }
    // Reject a symlinked record even before opening or contacting Herdr.
    ScratchStore::new(lane)?.record_path(&task.task_id)?;
    Ok(ScratchContext {
        project_root: project.to_owned(),
        lane_root: lane.to_owned(),
        task: task.task_id,
    })
}

/// Injectable for isolated tests; the production adapter uses only mv-herdr.
pub trait ScratchPanes {
    fn label(&mut self, pane: &str) -> Result<String>;
    fn read(&mut self, pane: &str) -> Result<String>;
    fn close(&mut self, pane: &str) -> Result<()>;
}

pub struct HerdrPanes;

impl ScratchPanes for HerdrPanes {
    fn label(&mut self, pane: &str) -> Result<String> {
        let result = run::run(&argv::pane_get(pane), None)
            .map_err(|e| ScratchError::Refused(e.to_string()))?;
        if !result.ok() {
            return Err(ScratchError::Refused(result.message()));
        }
        result
            .payload
            .as_ref()
            .and_then(|v| v.pointer("/result/pane/label"))
            .and_then(|v| v.as_str())
            .map(str::to_owned)
            .ok_or_else(|| ScratchError::Refused("pane reply has no label".into()))
    }
    fn read(&mut self, pane: &str) -> Result<String> {
        let result = run::run(&argv::pane_read(pane, Some("visible"), None), None)
            .map_err(|e| ScratchError::Refused(e.to_string()))?;
        if !result.ok() {
            return Err(ScratchError::Refused(result.message()));
        }
        Ok(result.stdout)
    }
    fn close(&mut self, pane: &str) -> Result<()> {
        let result = run::run(&argv::pane_close(pane), None)
            .map_err(|e| ScratchError::Refused(e.to_string()))?;
        if !result.ok() {
            return Err(ScratchError::Refused(result.message()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct ScratchStore {
    project: PathBuf,
}

impl ScratchStore {
    pub fn new(project: impl AsRef<Path>) -> Result<Self> {
        let project = fs::canonicalize(project)?;
        guarded_path(&project, Path::new(".multiverse/tasks"), true)?;
        Ok(Self { project })
    }

    pub fn record_path(&self, task: &str) -> Result<PathBuf> {
        validate_id(task)?;
        Ok(guarded_path(
            &self.project,
            &Path::new(".multiverse/tasks")
                .join(task)
                .join("scratch-pane"),
            true,
        )?)
    }

    /// A record is created exclusively: an existing owner must close first.
    pub fn record(&self, record: &ScratchPane) -> Result<()> {
        record.validate(&record.task)?;
        let path = self.record_path(&record.task)?;
        private_dir(path.parent().expect("record parent"))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        file.write_all(record.encode()?.as_bytes())?;
        file.sync_all()?;
        mv_core::atomic::fsync_dir(path.parent().expect("record parent"))?;
        Ok(())
    }

    /// Caller holds the project lock through split, rename, and completion.
    /// A stale empty intent can be retried; a recorded pane must be closed.
    pub fn reserve(&self, task: &str) -> Result<()> {
        if let Some(saved) = self.load(task)? {
            if !saved.pane.is_empty() {
                return Err(ScratchError::Refused(format!(
                    "scratch pane {} is already recorded; close it first",
                    saved.pane
                )));
            }
            self.unlink(task)?;
        }
        self.record(&ScratchPane::opening(task)?)
    }

    pub fn complete(&self, record: &ScratchPane) -> Result<()> {
        record.validate(&record.task)?;
        if record.pane.is_empty()
            || self.load(&record.task)? != Some(ScratchPane::opening(&record.task)?)
        {
            return Err(ScratchError::Refused(
                "scratch opening record changed".into(),
            ));
        }
        mv_core::atomic::write_atomic(
            &self.record_path(&record.task)?,
            record.encode()?.as_bytes(),
        )?;
        Ok(())
    }

    pub fn unlink(&self, task: &str) -> Result<()> {
        match fs::remove_file(self.record_path(task)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn load(&self, task: &str) -> Result<Option<ScratchPane>> {
        let path = self.record_path(task)?;
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
            Ok(_) => (),
        }
        let relative = path.strip_prefix(&self.project).expect("guarded path");
        let record: ScratchPane = serde_json::from_str(&read_regular(&self.project, relative)?)?;
        record.validate(task)?;
        Ok(Some(record))
    }

    pub fn read(&self, task: &str, panes: &mut impl ScratchPanes) -> Result<String> {
        let record = self.owned(task, panes)?;
        panes.read(&record.pane)
    }

    fn owned(&self, task: &str, panes: &mut impl ScratchPanes) -> Result<ScratchPane> {
        let record = self
            .load(task)?
            .ok_or_else(|| ScratchError::Refused("no scratch pane for task".into()))?;
        if record.pane.is_empty() {
            return Err(ScratchError::Refused(
                "refusing scratch: pane is still being opened".into(),
            ));
        }
        if panes.label(&record.pane)? != record.name {
            return Err(ScratchError::Refused(
                "pane no longer has this task's scratch label".into(),
            ));
        }
        Ok(record)
    }

    pub fn close(&self, task: &str, panes: &mut impl ScratchPanes) -> Result<bool> {
        if self.load(task)?.is_none() {
            return Ok(false);
        }
        let record = self.owned(task, panes)?;
        panes.close(&record.pane)?;
        // A new record written while Herdr ran belongs to the new pane.
        if self.load(task)?.as_ref() != Some(&record) {
            return Err(ScratchError::Refused(
                "scratch record changed during close".into(),
            ));
        }
        fs::remove_file(self.record_path(task)?)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worktrees::tests::TempRepo;

    struct Panes {
        label: String,
        fail: bool,
        closed: bool,
    }
    impl ScratchPanes for Panes {
        fn label(&mut self, _: &str) -> Result<String> {
            Ok(self.label.clone())
        }
        fn read(&mut self, _: &str) -> Result<String> {
            Ok("visible text".into())
        }
        fn close(&mut self, _: &str) -> Result<()> {
            if self.fail {
                return Err(ScratchError::Refused("close failed".into()));
            }
            self.closed = true;
            Ok(())
        }
    }

    #[test]
    fn pending_intent_is_private_retryable_and_cannot_drive_a_pane() {
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        store.reserve("task").unwrap();
        assert_eq!(store.load("task").unwrap().unwrap().pane, "");
        assert_eq!(
            fs::metadata(store.record_path("task").unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let mut panes = Panes {
            label: "scratch-task".into(),
            fail: false,
            closed: false,
        };
        assert!(store
            .read("task", &mut panes)
            .unwrap_err()
            .to_string()
            .contains("still being opened"));
        assert!(store.close("task", &mut panes).is_err());
        assert!(!panes.closed);
        store.reserve("task").unwrap();
        store
            .complete(&ScratchPane::new("task", "p1").unwrap())
            .unwrap();
        assert!(store.reserve("task").is_err());
        assert_eq!(store.load("task").unwrap().unwrap().pane, "p1");
    }

    #[test]
    fn scratch_path_outside_root_and_symlinks_are_refused() {
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        for task in ["../escape", "a/b", ".hidden", ""] {
            assert!(store.record_path(task).is_err());
        }
        let dir = repo.0.join(".multiverse/tasks");
        fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink(&repo.0, dir.join("owner")).unwrap();
        assert!(store.record_path("owner").is_err());
        assert!(store
            .record(&ScratchPane::new("owner", "p1").unwrap())
            .is_err());
    }

    #[test]
    fn sorted_ascii_json_matches_python_and_legacy_ids_read() {
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        let record = ScratchPane::new("20261101-fixer-abcdef", "p1").unwrap();
        assert_eq!(record.encode().unwrap(), "{\"name\": \"scratch-20261101-fixer-abcdef\", \"pane\": \"p1\", \"task\": \"20261101-fixer-abcdef\"}\n");
        assert_eq!(ascii_json("é😀").unwrap(), "\"\\u00e9\\ud83d\\ude00\"");
        store.record(&record).unwrap();
        assert_eq!(store.load(&record.task).unwrap(), Some(record.clone()));
        assert!(store.record(&record).is_err());
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(
                &fs::metadata(store.record_path(&record.task).unwrap())
                    .unwrap()
                    .permissions()
            ) & 0o777,
            0o600
        );
    }

    #[test]
    fn close_checks_fresh_ownership_and_preserves_record_on_failure() {
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        let record = ScratchPane::new("owner", "p1").unwrap();
        store.record(&record).unwrap();
        let mut panes = Panes {
            label: "foreign".into(),
            fail: false,
            closed: false,
        };
        assert!(store.close("owner", &mut panes).is_err());
        assert!(!panes.closed);
        panes.label = record.name;
        panes.fail = true;
        assert!(store.close("owner", &mut panes).is_err());
        assert!(store.load("owner").unwrap().is_some());
        panes.fail = false;
        assert_eq!(store.read("owner", &mut panes).unwrap(), "visible text");
        assert!(store.close("owner", &mut panes).unwrap());
        assert!(panes.closed);
        assert!(!store.close("owner", &mut panes).unwrap());
    }

    #[test]
    fn record_ownership_and_symlink_file_are_checked() {
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        let record = ScratchPane::new("owner", "p1").unwrap();
        store.record(&record).unwrap();
        let path = store.record_path("owner").unwrap();
        fs::write(
            &path,
            "{\"task\":\"other\",\"name\":\"scratch-owner\",\"pane\":\"p1\"}\n",
        )
        .unwrap();
        assert!(store.load("owner").is_err());
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(repo.0.join("README.md"), &path).unwrap();
        assert!(store.load("owner").is_err());
    }

    #[test]
    fn replacement_record_survives_a_concurrent_close() {
        struct ReplacingPanes(PathBuf);
        impl ScratchPanes for ReplacingPanes {
            fn label(&mut self, _: &str) -> Result<String> {
                Ok("scratch-owner".into())
            }
            fn read(&mut self, _: &str) -> Result<String> {
                unreachable!()
            }
            fn close(&mut self, _: &str) -> Result<()> {
                fs::write(&self.0, ScratchPane::new("owner", "p2")?.encode()?)?;
                Ok(())
            }
        }
        let repo = TempRepo::new();
        let store = ScratchStore::new(&repo.0).unwrap();
        store
            .record(&ScratchPane::new("owner", "p1").unwrap())
            .unwrap();
        let mut panes = ReplacingPanes(store.record_path("owner").unwrap());
        assert!(store.close("owner", &mut panes).is_err());
        assert_eq!(store.load("owner").unwrap().unwrap().pane, "p2");
    }
}

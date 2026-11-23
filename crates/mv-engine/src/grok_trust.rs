//! Grok's per-directory folder-trust store, pre-written by
//! [`crate::runner::dispatch`] immediately before a Grok seat is launched
//! — the Grok counterpart of [`crate::codex_trust`] (card
//! `20261122-b-1961a6`).
//!
//! Grok Build asks "Do you trust the contents of this directory?" the first
//! time it opens a folder that has project instructions, hooks, or other
//! project config, and parks there until a human answers. The answer lives
//! in `$GROK_HOME/trusted_folders.toml`, one table per canonical folder
//! path:
//!
//! ```toml
//! [folders."/canonical/path/to/project"]
//! trusted = true
//! decided_at = 1795347480
//! ```
//!
//! That shape was read off a throwaway `GROK_HOME` after `grok --trust
//! inspect` (Grok Build 1.0.92), and a hand-written entry of the same shape
//! was confirmed to flip `grok inspect`'s "Project trusted" line from `no`
//! to `yes` — `decided_at` is optional to Grok, but written anyway so an
//! entry this module adds reads like one Grok wrote itself. A nested git
//! checkout is a separate workspace to Grok, so the lane worktree gets its
//! own entry beside the project root's, exactly like Codex.
//!
//! Existing trusted entries are left alone, a missing folder table is
//! appended, and an explicit `trusted = false` refuses the launch rather
//! than silently overriding a human's prior decision — the same contract
//! [`crate::codex_trust::ensure_codex_trusted`] keeps. Edits go through
//! `toml_edit::DocumentMut` so every other entry and comment in the file
//! survives byte-for-byte.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use toml_edit::{DocumentMut, Item, Table};

/// Everything that can go wrong persisting Grok folder trust.
#[derive(Debug, thiserror::Error)]
pub enum GrokTrustError {
    /// `trusted_folders.toml` exists but is not valid TOML.
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    /// The `folders` key (or one folder's own entry) exists but is not a
    /// TOML table.
    #[error("Grok folders table in {0} is not a table")]
    NotATable(PathBuf),
    /// An existing entry for `path` says `trusted = false` (or anything
    /// other than `true`) — refused rather than silently overridden.
    #[error("Grok folder {path:?} is explicitly not trusted")]
    ExplicitlyUntrusted { path: PathBuf },
    /// Could not acquire the exclusive lock on `trusted_folders.toml`.
    #[error(transparent)]
    Lock(#[from] mv_core::lock::LockError),
    /// Any other filesystem failure reading or writing the store.
    #[error(transparent)]
    Io(#[from] io::Error),
}

const STORE_FILENAME: &str = "trusted_folders.toml";
/// The lock file Grok itself leaves beside the store (an empty file it
/// keeps after the write). Taken here as an exclusive `flock` so two
/// concurrent Grok launches sharing a seat never race each other's
/// read-modify-write.
const LOCK_FILENAME: &str = "trusted_folders.toml.lock";

/// Persist `trusted = true` for `worktree` and `project_root` in the Grok
/// `trusted_folders.toml` under `grok_home`, creating `grok_home` if it
/// does not already exist.
///
/// Both paths are canonicalized before use (Grok keys the store by the
/// resolved path — `/private/var/...` on macOS, not `/var/...`), so the
/// same worktree reached two different ways collapses to one entry, and a
/// reader-mode dispatch, where `worktree == project_root`, writes only one.
pub fn ensure_grok_trusted(
    grok_home: &Path,
    worktree: &Path,
    project_root: &Path,
) -> Result<(), GrokTrustError> {
    fs::create_dir_all(grok_home)?;
    let store_path = grok_home.join(STORE_FILENAME);
    let lock_path = grok_home.join(LOCK_FILENAME);

    let _guard = mv_core::lock::acquire_default(&lock_path, "grok-trust")?;

    let mut paths: Vec<PathBuf> = Vec::new();
    for path in [project_root, worktree] {
        let resolved = fs::canonicalize(path)?;
        if !paths.contains(&resolved) {
            paths.push(resolved);
        }
    }

    let text = match fs::read_to_string(&store_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: DocumentMut = text.parse().map_err(|source| GrokTrustError::Parse {
        path: store_path.clone(),
        source,
    })?;

    let folders = doc
        .entry("folders")
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
        .ok_or_else(|| GrokTrustError::NotATable(store_path.clone()))?;

    let decided_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let decided_at = i64::try_from(decided_at).unwrap_or(i64::MAX);
    let mut changed = false;
    for path in &paths {
        let key = path.to_string_lossy().into_owned();
        match folders.get_mut(&key) {
            Some(item) => {
                let folder = item
                    .as_table_like_mut()
                    .ok_or_else(|| GrokTrustError::NotATable(store_path.clone()))?;
                match folder.get("trusted") {
                    Some(value) if value.as_bool() == Some(true) => {}
                    Some(_) => {
                        return Err(GrokTrustError::ExplicitlyUntrusted { path: path.clone() })
                    }
                    None => {
                        folder.insert("trusted", Item::Value(true.into()));
                        if folder.get("decided_at").is_none() {
                            folder.insert("decided_at", Item::Value(decided_at.into()));
                        }
                        changed = true;
                    }
                }
            }
            None => {
                let mut table = Table::new();
                table.insert("trusted", Item::Value(true.into()));
                table.insert("decided_at", Item::Value(decided_at.into()));
                folders.insert(&key, Item::Table(table));
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(());
    }

    // `0600`, the same mode Grok itself creates this file with.
    mv_core::atomic::write_atomic(&store_path, doc.to_string().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(home: &Path) -> (String, DocumentMut) {
        let rendered = fs::read_to_string(home.join(STORE_FILENAME)).unwrap();
        let doc = rendered.parse().unwrap();
        (rendered, doc)
    }

    #[test]
    fn adds_entries_for_both_paths_without_disturbing_unrelated_content() {
        let home = tempfile::tempdir().unwrap();
        let existing = tempfile::tempdir().unwrap();
        let existing_resolved = fs::canonicalize(existing.path()).unwrap();
        fs::write(
            home.path().join(STORE_FILENAME),
            format!(
                "# kept\n[folders.\"{}\"]\ntrusted = true\ndecided_at = 1\n",
                existing_resolved.display()
            ),
        )
        .unwrap();

        let project = tempfile::tempdir().unwrap();
        let lane = tempfile::tempdir().unwrap();

        ensure_grok_trusted(home.path(), lane.path(), project.path()).unwrap();

        let (rendered, doc) = folders(home.path());
        assert!(rendered.starts_with(&format!(
            "# kept\n[folders.\"{}\"]\ntrusted = true\ndecided_at = 1\n",
            existing_resolved.display()
        )));
        let table = doc["folders"].as_table().unwrap();
        for path in [existing.path(), project.path(), lane.path()] {
            let resolved = fs::canonicalize(path).unwrap();
            let key = resolved.to_string_lossy().into_owned();
            assert_eq!(
                table[key.as_str()]["trusted"].as_bool(),
                Some(true),
                "{key} missing or untrusted in {rendered}"
            );
            assert!(table[key.as_str()]["decided_at"].as_integer().is_some());
        }
    }

    #[test]
    fn writes_the_shape_grok_itself_writes() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        ensure_grok_trusted(home.path(), project.path(), project.path()).unwrap();

        let (rendered, _) = folders(home.path());
        let resolved = fs::canonicalize(project.path()).unwrap();
        let header = format!(
            "[folders.\"{}\"]\ntrusted = true\ndecided_at = ",
            resolved.display()
        );
        assert!(rendered.starts_with(&header), "{rendered}");
        assert!(!rendered.contains("[folders]\n"), "{rendered}");
    }

    #[test]
    fn a_second_call_is_a_byte_identical_no_op() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let lane = tempfile::tempdir().unwrap();

        ensure_grok_trusted(home.path(), lane.path(), project.path()).unwrap();
        let first = fs::read(home.path().join(STORE_FILENAME)).unwrap();
        ensure_grok_trusted(home.path(), lane.path(), project.path()).unwrap();
        let second = fs::read(home.path().join(STORE_FILENAME)).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn a_reader_dispatch_with_one_path_adds_exactly_one_entry() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        ensure_grok_trusted(home.path(), project.path(), project.path()).unwrap();

        let (_, doc) = folders(home.path());
        assert_eq!(doc["folders"].as_table().unwrap().len(), 1);
    }

    #[test]
    fn refuses_an_explicit_untrusted_entry_rather_than_overriding_it() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join(STORE_FILENAME),
            format!(
                "[folders.\"{}\"]\ntrusted = false\ndecided_at = 1\n",
                fs::canonicalize(project.path()).unwrap().display()
            ),
        )
        .unwrap();

        let err = ensure_grok_trusted(home.path(), project.path(), project.path()).unwrap_err();
        assert!(matches!(err, GrokTrustError::ExplicitlyUntrusted { .. }));
        let (rendered, _) = folders(home.path());
        assert!(rendered.contains("trusted = false"));
    }

    #[test]
    fn creates_grok_home_and_store_when_neither_exists() {
        let home = tempfile::tempdir().unwrap();
        let grok_home = home.path().join("nested").join("grok-home");
        let project = tempfile::tempdir().unwrap();

        ensure_grok_trusted(&grok_home, project.path(), project.path()).unwrap();

        assert!(grok_home.join(STORE_FILENAME).exists());
    }
}

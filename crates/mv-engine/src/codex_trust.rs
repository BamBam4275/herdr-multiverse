//! Port of Python's `herdr.ensure_codex_trusted` (`herdr.py:234-316`),
//! called from [`crate::runner::dispatch`] immediately before a Codex
//! seat is launched.
//!
//! Codex has no launch flag that skips its own interactive
//! directory-trust prompt, so the trust entries it would otherwise ask
//! about are written straight into that seat's `CODEX_HOME/config.toml`
//! first. Existing trusted entries are left alone, a missing project
//! table is appended, and an explicit, conflicting `trust_level` refuses
//! the launch rather than silently overriding a human's prior decision.
//!
//! Python hand-splices new `[projects."<path>"]` blocks into the file's
//! text to avoid disturbing anything else in it. This port uses
//! `toml_edit::DocumentMut` instead — the same format-preserving approach
//! `mv_config::edit` already uses for `config.toml`'s own targeted
//! edits — which keeps every comment and existing table untouched and
//! only appends what is missing, without hand-rolling TOML string
//! escaping or line-splicing.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

/// Everything that can go wrong persisting Codex trust.
#[derive(Debug, thiserror::Error)]
pub enum CodexTrustError {
    #[error("cannot parse {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml_edit::TomlError,
    },
    #[error("Codex projects table in {0} is not a table")]
    NotATable(PathBuf),
    #[error("Codex project {path:?} is explicitly not trusted")]
    ExplicitlyUntrusted { path: PathBuf },
    #[error(transparent)]
    Lock(#[from] mv_core::lock::LockError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

const CONFIG_FILENAME: &str = "config.toml";
const LOCK_FILENAME: &str = ".config.toml.lock";

/// Persist `trust_level = "trusted"` for `worktree` and `project_root` in
/// the Codex `config.toml` under `config_dir`, creating `config_dir` if
/// it does not already exist.
///
/// Both paths are canonicalized before use, so the same worktree reached
/// two different ways (a symlinked `/tmp`, say) collapses to one entry —
/// and a reader-mode dispatch, where `worktree == project_root`, writes
/// only one. Holds an exclusive file lock on `config_dir`'s own
/// `.config.toml.lock` for the duration, the same lock file Python's
/// `fcntl.flock` uses, so two concurrent Codex launches sharing a seat
/// never race each other's read-modify-write.
pub fn ensure_codex_trusted(
    config_dir: &Path,
    worktree: &Path,
    project_root: &Path,
) -> Result<(), CodexTrustError> {
    fs::create_dir_all(config_dir)?;
    let config_path = config_dir.join(CONFIG_FILENAME);
    let lock_path = config_dir.join(LOCK_FILENAME);

    let _guard = mv_core::lock::acquire_default(&lock_path, "codex-trust")?;

    let mut paths: Vec<PathBuf> = Vec::new();
    for path in [project_root, worktree] {
        let resolved = fs::canonicalize(path)?;
        if !paths.contains(&resolved) {
            paths.push(resolved);
        }
    }

    let text = match fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.into()),
    };
    let mut doc: DocumentMut = text.parse().map_err(|source| CodexTrustError::Parse {
        path: config_path.clone(),
        source,
    })?;

    let projects = doc
        .entry("projects")
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| CodexTrustError::NotATable(config_path.clone()))?;

    let mut changed = false;
    for path in &paths {
        let key = path.to_string_lossy().into_owned();
        match projects.get_mut(&key) {
            Some(item) => {
                let project = item
                    .as_table_like_mut()
                    .ok_or_else(|| CodexTrustError::NotATable(config_path.clone()))?;
                match project.get("trust_level").and_then(Item::as_str) {
                    Some("trusted") => {}
                    Some(_) => {
                        return Err(CodexTrustError::ExplicitlyUntrusted { path: path.clone() })
                    }
                    None => {
                        project.insert("trust_level", Item::Value("trusted".into()));
                        changed = true;
                    }
                }
            }
            None => {
                let mut table = Table::new();
                table.insert("trust_level", Item::Value("trusted".into()));
                projects.insert(&key, Item::Table(table));
                changed = true;
            }
        }
    }

    if !changed {
        return Ok(());
    }

    // Unlike Python (which preserves an existing `config.toml`'s own
    // mode, falling back to `mkstemp`'s default of `0600` for a brand
    // new one), this always writes at `0600`: `mv_core::atomic::
    // write_atomic` is the one atomic-write helper every other store in
    // this workspace already uses, and a seat's Codex config is exactly
    // the kind of file that should not be group/world-readable regardless
    // of what mode it happened to have before. Not exercised by Python's
    // own `ensure_codex_trusted` test, which only covers content/
    // idempotency — see this task's `result.md`.
    mv_core::atomic::write_atomic(&config_path, doc.to_string().as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_entries_for_both_paths_without_disturbing_unrelated_content() {
        let home = tempfile::tempdir().unwrap();
        let config_path = home.path().join("config.toml");
        let existing = tempfile::tempdir().unwrap();
        let existing_resolved = fs::canonicalize(existing.path()).unwrap();
        fs::write(
            &config_path,
            format!(
                "model = \"preserve-me\"\n\n[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                existing_resolved.display()
            ),
        )
        .unwrap();

        let project = tempfile::tempdir().unwrap();
        let lane = tempfile::tempdir().unwrap();

        ensure_codex_trusted(home.path(), lane.path(), project.path()).unwrap();

        let rendered = fs::read_to_string(&config_path).unwrap();
        assert!(rendered.contains("model = \"preserve-me\""));
        let doc: DocumentMut = rendered.parse().unwrap();
        let projects = doc["projects"].as_table().unwrap();
        for path in [existing.path(), project.path(), lane.path()] {
            let resolved = fs::canonicalize(path).unwrap();
            let key = resolved.to_string_lossy().into_owned();
            assert_eq!(
                projects[key.as_str()]["trust_level"].as_str(),
                Some("trusted"),
                "{key} missing or untrusted in {rendered}"
            );
        }
    }

    #[test]
    fn a_second_call_is_a_byte_identical_no_op() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let lane = tempfile::tempdir().unwrap();

        ensure_codex_trusted(home.path(), lane.path(), project.path()).unwrap();
        let first = fs::read(home.path().join("config.toml")).unwrap();
        ensure_codex_trusted(home.path(), lane.path(), project.path()).unwrap();
        let second = fs::read(home.path().join("config.toml")).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn a_reader_dispatch_with_one_path_adds_exactly_one_entry() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();

        ensure_codex_trusted(home.path(), project.path(), project.path()).unwrap();

        let rendered = fs::read_to_string(home.path().join("config.toml")).unwrap();
        let doc: DocumentMut = rendered.parse().unwrap();
        assert_eq!(doc["projects"].as_table().unwrap().len(), 1);
    }

    #[test]
    fn refuses_an_explicit_untrusted_entry_rather_than_overriding_it() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("config.toml"),
            format!(
                "[projects.\"{}\"]\ntrust_level = \"untrusted\"\n",
                fs::canonicalize(project.path()).unwrap().display()
            ),
        )
        .unwrap();

        let err = ensure_codex_trusted(home.path(), project.path(), project.path()).unwrap_err();
        assert!(matches!(err, CodexTrustError::ExplicitlyUntrusted { .. }));
        // Refused before any write — the file on disk is untouched.
        let rendered = fs::read_to_string(home.path().join("config.toml")).unwrap();
        assert!(rendered.contains("\"untrusted\""));
    }

    #[test]
    fn creates_config_dir_and_file_when_neither_exists() {
        let home = tempfile::tempdir().unwrap();
        let config_dir = home.path().join("nested").join("codex-home");
        let project = tempfile::tempdir().unwrap();

        ensure_codex_trusted(&config_dir, project.path(), project.path()).unwrap();

        assert!(config_dir.join("config.toml").exists());
    }
}

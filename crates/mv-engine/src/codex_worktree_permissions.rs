//! Port of Python's `herdr.codex_worktree_permissions` (`herdr.py:141-189`),
//! called from [`crate::runner::dispatch`] alongside
//! [`crate::codex_trust::ensure_codex_trusted`] in the same Codex launch
//! path, once the writer worktree [`crate::runner::DispatchStore::prepare`]
//! creates actually exists.
//!
//! Codex's automatic workspace deny includes the worktree's own private
//! gitdir, so the common `.git` entry a main-checkout launch relies on
//! cannot by itself permit a commit from a writer lane. Codex's buildx
//! integration also writes activity beneath the user's Docker config, and
//! Go's build/module caches are shared in the user's home, so a writer
//! lane gets those exact directories in the same narrow override. A
//! reader dispatch (`worktree == project_root`) and plan time (before the
//! worktree exists) both get `None` — nothing to widen for either.

use std::path::Path;
use std::process::Command;

/// Escape `value` for use inside a TOML basic string's quotes. Paths are
/// the only input here, and POSIX allows `"` and `\` in a filename, so a
/// path can otherwise break the TOML-like map syntax Codex's `-c` parser
/// expects.
fn toml_basic_string_body(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn git_rev_parse(worktree: &Path, arg: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["rev-parse", arg])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// The fleet filesystem override for a Codex worktree, if it is real.
///
/// Returns `None` for a reader dispatch (`worktree` and `project_root`
/// resolve to the same place) or when `worktree` does not exist yet —
/// which it does not during planning/dry-run, since the runner has not
/// created the lane at that point.
pub fn codex_worktree_permissions(worktree: &Path, project_root: &Path) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    let resolved_worktree = worktree.canonicalize().ok()?;
    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());
    if resolved_worktree == resolved_root {
        return None;
    }

    let lane_gitdir = git_rev_parse(worktree, "--absolute-git-dir")?;
    let lane_gitdir = Path::new(&lane_gitdir).canonicalize().ok()?;
    let common_dir_raw = git_rev_parse(worktree, "--git-common-dir")?;
    let common_dir = Path::new(&common_dir_raw);
    let common_dir = if common_dir.is_absolute() {
        common_dir.canonicalize().ok()?
    } else {
        worktree.join(common_dir).canonicalize().ok()?
    };

    let home = mv_config::load::home_dir_from_env();
    let common_str = toml_basic_string_body(&common_dir.to_string_lossy());
    let lane_str = toml_basic_string_body(&lane_gitdir.to_string_lossy());
    let buildx_str = toml_basic_string_body(&home.join(".docker").join("buildx").to_string_lossy());
    let go_build_str = toml_basic_string_body(
        &home
            .join("Library")
            .join("Caches")
            .join("go-build")
            .to_string_lossy(),
    );
    let go_mod_str =
        toml_basic_string_body(&home.join("go").join("pkg").join("mod").to_string_lossy());

    Some(format!(
        "permissions.fleet.filesystem={{\"{common_str}\"=\"write\", \
         \"{lane_str}\"=\"write\", \"{buildx_str}\"=\"write\", \
         \"{go_build_str}\"=\"write\", \"{go_mod_str}\"=\"write\"}}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q"]);
        git(
            dir,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
    }

    #[test]
    fn returns_none_for_a_reader_dispatch_where_worktree_is_the_project_root() {
        let project = tempfile::tempdir().unwrap();
        init_repo(project.path());
        assert_eq!(
            codex_worktree_permissions(project.path(), project.path()),
            None
        );
    }

    #[test]
    fn returns_none_when_the_worktree_does_not_exist_yet() {
        let project = tempfile::tempdir().unwrap();
        init_repo(project.path());
        let not_yet = project.path().join("not-created-yet");
        assert_eq!(codex_worktree_permissions(&not_yet, project.path()), None);
    }

    #[test]
    fn builds_the_full_override_for_a_real_writer_worktree() {
        let project = tempfile::tempdir().unwrap();
        init_repo(project.path());
        let lane = project.path().join("lane");
        git(
            project.path(),
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "multiverse/test-lane",
                lane.to_str().unwrap(),
            ],
        );

        let permissions = codex_worktree_permissions(&lane, project.path()).unwrap();

        let common_gitdir = project.path().canonicalize().unwrap().join(".git");
        let lane_gitdir = common_gitdir
            .join("worktrees")
            .join("lane")
            .canonicalize()
            .unwrap();
        assert!(permissions.starts_with("permissions.fleet.filesystem={"));
        assert!(
            permissions.contains(&format!("\"{}\"=\"write\"", common_gitdir.display())),
            "{permissions}"
        );
        assert!(
            permissions.contains(&format!("\"{}\"=\"write\"", lane_gitdir.display())),
            "{permissions}"
        );
        assert!(
            permissions.contains(".docker/buildx\"=\"write\""),
            "{permissions}"
        );
        assert!(
            permissions.contains("Library/Caches/go-build\"=\"write\""),
            "{permissions}"
        );
        assert!(
            permissions.contains("go/pkg/mod\"=\"write\""),
            "{permissions}"
        );
    }

    #[test]
    fn a_non_worktree_project_root_still_resolves_the_common_dir() {
        // No `git worktree add` here: the worktree *is* a plain repo, so
        // `--git-common-dir` is relative (`.git`) rather than absolute —
        // exercising the relative-resolution branch `codex_worktree_
        // permissions` falls back to.
        let project = tempfile::tempdir().unwrap();
        init_repo(project.path());
        let other = tempfile::tempdir().unwrap();
        init_repo(other.path());

        let permissions = codex_worktree_permissions(project.path(), other.path()).unwrap();
        let gitdir = project.path().canonicalize().unwrap().join(".git");
        assert!(
            permissions.contains(&format!("\"{}\"=\"write\"", gitdir.display())),
            "{permissions}"
        );
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_a_path() {
        assert_eq!(toml_basic_string_body(r#"a"b\c"#), r#"a\"b\\c"#);
    }
}

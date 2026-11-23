//! Port of Python's `herdr.codex_worktree_permissions` (`herdr.py:141-189`),
//! called from [`crate::runner::dispatch`] alongside
//! [`crate::codex_trust::ensure_codex_trusted`] in the same Codex launch
//! path, once the worktree [`crate::runner::DispatchStore::prepare`]
//! creates actually exists.
//!
//! Every Codex seat launches with `-c default_permissions="fleet"`
//! (`mv_herdr::argv::native_args`), so this module is what defines the
//! `fleet` permission profile that selects: `permissions.fleet.extends =
//! ":workspace"` ([`CODEX_FLEET_PROFILE_EXTENDS`]) plus the
//! `permissions.fleet.filesystem` table [`codex_worktree_permissions`]
//! builds. Codex rejects a `default_permissions` that names an undefined
//! profile. The table is returned for every worktree that exists —
//! writer lane and reader dispatch alike. When the worktree is missing
//! or cannot be canonicalized the table is `None`, and
//! `runner::apply_codex_worktree_permissions` still emits the extends
//! override so the select is never an undefined profile (card
//! `20261122-b-a8b112`).
//!
//! Card `20261122-b-4971ad`, the read boundary: a profile with any `deny`
//! entry makes Codex's Seatbelt policy drop its blanket `file-read*`
//! allow, so a denied path is unreadable to every command Codex runs.
//! Denying the whole code root (the project's parent directory) and
//! re-granting the worktree was tried first and breaks git — its
//! realpath walk needs metadata reads on every ancestor of the worktree
//! (`fatal: Invalid path '<code root>': Operation not permitted`). So the
//! ancestor chain from the project's parent down to the worktree stays
//! readable, and every *other* entry at each level of that chain is
//! denied: sibling projects, the main checkout's own files and
//! `.multiverse` state, and every other lane's worktree. Entries on the
//! way to a granted path (the common `.git`, say) are never denied.
//! Ancestors of `$HOME` are skipped so a walk that starts above `$HOME`
//! never denies other users' homes. `$HOME` itself is listed when the
//! project sits directly in it, so sibling projects and `~/.ssh` are
//! denied; the toolchain directories this module already names
//! (`~/.cargo`, `~/.rustup`, `~/.codex`, plus ancestors of the
//! HOME-relative write grants) stay unlisted. `mv_config::load::resolve_home_dir`
//! is the home primitive: when it returns `None`, the HOME-relative write
//! grants are omitted rather than falling back to `/`. The denies are
//! listed when the lane launches: an entry created afterwards — a later
//! lane's worktree — stays readable. Glob denies cannot close that gap:
//! an explicit glob deny beats the worktree's own `write` re-grant.
//!
//! Finding `20261122-b-9b2612`: a writer lane still runs the lane-side
//! `multiverse` commands its brief names — `activity update` and
//! `activity inbox --peek` ([`mv_state::activity::ProjectLog`]) and
//! `scratch open|read|close` (the project lock in
//! `mv_state::log::ProjectLog`) — and those touch the project's own
//! `.multiverse` directory, not the lane's. They append to `routing.log`
//! under `routing.log.lock`, keep its `routing.log.index`/`.index.fp`
//! sidecars, rewrite `activity.lastseen`, and read `activity.cursor`. The
//! lock path `chmod`s `.multiverse` itself to `0700` and the sidecars are
//! replaced through a same-directory temp file, so a `write` on each file
//! is not enough (Codex sandbox: `file-write-mode` on `.multiverse`).
//! The state directory is therefore granted `write` itself
//! (`LANE_STATE_ENTRIES` stay undenied beneath it), the entry on the
//! way to the worktree (`worktrees`) and `activity.cursor` are narrowed
//! back to `read`, and every other entry there — other tasks, briefs,
//! inputs, evidence, the Nexus seed, PASSDOWN — stays `deny`, which also
//! refuses renaming or unlinking it. The lane's own task directory is
//! `<worktree>/.multiverse/tasks/<task>` (`runner::write_brief` writes it
//! under the launch cwd), so the worktree grant already covers its
//! `result.md`/`status`. The cost: a new top-level file under the
//! project's `.multiverse` is creatable, and an entry created there after
//! launch is readable.
//!
//! A reader dispatch (`worktree` and `project_root` resolve to the same
//! place) has to write its own `result.md`/`status` under the project's
//! `.multiverse/tasks/<task-id>` and still append to `routing.log`. It
//! gets `write` on `.multiverse` (lock `chmod` and sidecar temp files),
//! that task directory, and the lane-state files; `read` on every other
//! existing child of `.multiverse` and of the project (so
//! `:workspace`'s write on the cwd cannot rewrite PASSDOWN, other
//! tasks, briefs, or the checkout); and the same sibling denies as a
//! writer. A `read` on the project root itself loses to `:workspace`
//! write on that same path (`deny` > `write` > `read`); more-specific
//! child `read` is the supported carve-out, and child `write` under
//! `tasks` `read` is what lets the task directory stay writable.
//!
//! Codex's automatic workspace deny includes the worktree's own private
//! gitdir, so the common `.git` entry a main-checkout launch relies on
//! cannot by itself permit a commit from a writer lane. The common dir
//! stays `write`, then `<common>/.git/worktrees/*` is enumerated the
//! same way `worktrees/` is: other lanes' gitdirs are `deny`, the
//! lane's own gitdir stays `write`. `.git/hooks` is always `deny`, even
//! when the directory does not exist yet, so a planted hook cannot run
//! later unsandboxed. Codex's buildx integration also writes activity
//! beneath the user's Docker config, and Go's build/module caches are
//! shared in the user's home, so the lane gets those exact directories
//! in the same narrow override when home resolves. Plan time (before
//! the worktree exists) gets `None`. Never combine this profile with
//! `--sandbox`/`sandbox_mode`: Codex refuses both as overrides, and
//! the legacy sandbox cannot deny a read.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The `-c` override that makes the `fleet` profile inherit Codex's
/// `:workspace` built-in (`:root` readable; the workspace roots and temp
/// directories writable), which the filesystem table then narrows.
pub const CODEX_FLEET_PROFILE_EXTENDS: &str = "permissions.fleet.extends=\":workspace\"";

/// Entries directly under the project's `.multiverse` that the lane-side
/// `multiverse` commands write, left undenied beneath that directory's
/// `write` grant (see the module docs).
const LANE_STATE_ENTRIES: &[&str] = &[
    "routing.log",
    "routing.log.lock",
    "routing.log.index",
    "routing.log.index.fp",
    "activity.lastseen",
];

/// Entries directly under the project's `.multiverse` that the lane-side
/// commands only read: `activity inbox --peek` reads the Nexus's cursor
/// but must never advance it.
const LANE_STATE_READ_ENTRIES: &[&str] = &["activity.cursor"];

/// HOME-level directories a HOME-direct project must not deny. Write
/// grants under `$HOME` (buildx, Go caches) keep their ancestor
/// directories through the same `kept` filter.
const HOME_TOOLCHAIN_ENTRIES: &[&str] = &[".cargo", ".rustup", ".codex"];

/// Escape `value` for use inside a TOML basic string's quotes. Paths are
/// the only input here, and POSIX allows `"`, `\`, and C0 controls in a
/// filename. Unescaped, a newline or quote would break the inline table
/// and Codex would drop it (fail open). C0/DEL bytes are escaped so a
/// planted sibling still becomes a `deny` entry.
fn toml_basic_string_body(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{0008}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{000C}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if c.is_ascii_control() => {
                out.push_str(&format!("\\u{:04x}", u32::from(c)));
            }
            c => out.push(c),
        }
    }
    out
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

/// The worktree's common and private gitdirs, or `None` outside a repo.
fn git_dirs(worktree: &Path) -> Option<(PathBuf, PathBuf)> {
    let lane_gitdir = git_rev_parse(worktree, "--absolute-git-dir")?;
    let lane_gitdir = Path::new(&lane_gitdir).canonicalize().ok()?;
    let common_dir_raw = git_rev_parse(worktree, "--git-common-dir")?;
    let common_dir = Path::new(&common_dir_raw);
    let common_dir = if common_dir.is_absolute() {
        common_dir.canonicalize().ok()?
    } else {
        worktree.join(common_dir).canonicalize().ok()?
    };
    Some((common_dir, lane_gitdir))
}

fn skip_home_ancestor(home: Option<&Path>, level: &Path) -> bool {
    home.is_some_and(|home| home.starts_with(level) && home != level)
}

/// Every entry beside the chain from `project_parent` down to
/// `worktree`, at each level of it, except entries on the way to a
/// `kept` path. Ancestors of `home` are not listed (see the module
/// docs); `$HOME` itself is, so a HOME-direct project denies its
/// siblings. Nothing is denied when `worktree` is not under
/// `project_parent`.
fn sibling_denies(
    project_parent: &Path,
    worktree: &Path,
    kept: &[PathBuf],
    home: Option<&Path>,
) -> Vec<PathBuf> {
    let Ok(below) = worktree.strip_prefix(project_parent) else {
        return Vec::new();
    };
    let mut denies = Vec::new();
    let mut level = project_parent.to_path_buf();
    for component in below.components() {
        if !skip_home_ancestor(home, &level) {
            if let Ok(entries) = std::fs::read_dir(&level) {
                let mut children: Vec<PathBuf> =
                    entries.filter_map(Result::ok).map(|e| e.path()).collect();
                children.sort();
                denies.extend(
                    children
                        .into_iter()
                        .filter(|child| !kept.iter().any(|path| path.starts_with(child))),
                );
            }
        }
        level.push(component);
    }
    denies
}

/// Other lanes' gitdirs under `<common>/.git/worktrees/*`, plus
/// `.git/hooks`. The common dir stays `write`; these more-specific
/// `deny` entries carve it.
fn gitdir_denies(common_dir: &Path, kept: &[PathBuf]) -> Vec<PathBuf> {
    let mut denies = Vec::new();
    let worktrees = common_dir.join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&worktrees) {
        let mut children: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        children.sort();
        denies.extend(
            children
                .into_iter()
                .filter(|child| !kept.iter().any(|path| path.starts_with(child))),
        );
    }
    denies.push(common_dir.join("hooks"));
    denies
}

fn unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut unique = Vec::new();
    for path in paths {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    unique
}

/// The fleet profile's filesystem override for a Codex worktree, if it
/// is real.
///
/// Returns `None` when `worktree` does not exist yet — which it does not
/// during planning/dry-run, since the runner has not created the lane at
/// that point — or when canonicalize fails. That `None` is the filesystem
/// table only: `apply_codex_worktree_permissions` still emits
/// [`CODEX_FLEET_PROFILE_EXTENDS`] so `default_permissions="fleet"` stays
/// a defined profile (card `20261122-b-a8b112`). A reader dispatch
/// (`worktree` and `project_root` resolve to the same place) gets `write`
/// on `.multiverse`, `.multiverse/tasks/<task_id>`, and the lane-state
/// files; `read` on the other existing project and `.multiverse`
/// children; and the same sibling denies as a writer.
pub fn codex_worktree_permissions(
    worktree: &Path,
    project_root: &Path,
    task_id: &str,
) -> Option<String> {
    permissions_table(
        worktree,
        project_root,
        task_id,
        mv_config::load::resolve_home_dir(),
    )
}

fn permissions_table(
    worktree: &Path,
    project_root: &Path,
    task_id: &str,
    home: Option<PathBuf>,
) -> Option<String> {
    if !worktree.exists() {
        return None;
    }
    let resolved_worktree = worktree.canonicalize().ok()?;
    let resolved_root = project_root
        .canonicalize()
        .unwrap_or_else(|_| project_root.to_path_buf());

    let home = home.map(|h| h.canonicalize().unwrap_or(h));

    let mut granted: Vec<PathBuf> = Vec::new();
    let mut read_only: Vec<PathBuf> = Vec::new();
    let mut kept: Vec<PathBuf> = Vec::new();
    let is_reader = resolved_worktree == resolved_root;
    let state_dir = resolved_root.join(".multiverse");

    if is_reader {
        // `:workspace` already writes the cwd (the project). A `read` on
        // that same path loses (`deny` > `write` > `read`), so the
        // project root is not listed. `.multiverse` stays `write` so
        // routing-log sidecars (`NamedTempFile` in that directory) and
        // the lock `chmod` keep working; other existing children of
        // `.multiverse` and of the project are `read` so PASSDOWN,
        // other tasks, briefs, and the checkout cannot be rewritten.
        // Child `write` under that `read` is what keeps the task
        // directory writable.
        granted.push(state_dir.clone());
        if !task_id.is_empty() {
            granted.push(state_dir.join("tasks").join(task_id));
        }
        granted.extend(LANE_STATE_ENTRIES.iter().map(|name| state_dir.join(name)));
        read_only.extend(
            LANE_STATE_READ_ENTRIES
                .iter()
                .map(|name| state_dir.join(name)),
        );
    } else {
        granted.push(resolved_worktree.clone());
        if let Some(on_chain) = resolved_worktree
            .strip_prefix(&state_dir)
            .ok()
            .and_then(|below| below.components().next())
        {
            granted.push(state_dir.clone());
            read_only.push(state_dir.join(on_chain));
            read_only.extend(
                LANE_STATE_READ_ENTRIES
                    .iter()
                    .map(|name| state_dir.join(name)),
            );
            kept.extend(LANE_STATE_ENTRIES.iter().map(|name| state_dir.join(name)));
        }
    }

    let mut common_gitdir = None;
    if let Some((common_dir, lane_gitdir)) = git_dirs(worktree) {
        common_gitdir = Some(common_dir.clone());
        granted.push(common_dir);
        granted.push(lane_gitdir);
    }

    if let Some(home) = &home {
        granted.push(home.join(".docker").join("buildx"));
        granted.push(home.join("Library").join("Caches").join("go-build"));
        granted.push(home.join("go").join("pkg").join("mod"));
        kept.extend(HOME_TOOLCHAIN_ENTRIES.iter().map(|name| home.join(name)));
        if let Some(codex_home) = std::env::var_os("CODEX_HOME") {
            if !codex_home.is_empty() {
                kept.push(PathBuf::from(codex_home));
            }
        }
    }

    let granted = unique_paths(granted);
    let mut read_only = unique_paths(
        read_only
            .into_iter()
            .filter(|path| !granted.contains(path))
            .collect(),
    );
    if is_reader {
        for dir in [&resolved_worktree, &state_dir] {
            if let Ok(entries) = std::fs::read_dir(dir) {
                let mut children: Vec<PathBuf> =
                    entries.filter_map(Result::ok).map(|e| e.path()).collect();
                children.sort();
                for child in children {
                    if granted.contains(&child) || read_only.contains(&child) {
                        continue;
                    }
                    read_only.push(child);
                }
            }
        }
    }

    kept.extend(granted.iter().chain(&read_only).cloned());
    let mut denies = resolved_root
        .parent()
        .map(|parent| sibling_denies(parent, &resolved_worktree, &kept, home.as_deref()))
        .unwrap_or_default();
    if let Some(common_dir) = &common_gitdir {
        denies.extend(gitdir_denies(common_dir, &kept));
    }

    let entries = granted
        .iter()
        .map(|path| (path, "write"))
        .chain(read_only.iter().map(|path| (path, "read")))
        .chain(denies.iter().map(|path| (path, "deny")))
        .map(|(path, access)| {
            format!(
                "\"{}\"=\"{access}\"",
                toml_basic_string_body(&path.to_string_lossy())
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    Some(format!("permissions.fleet.filesystem={{{entries}}}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    const TASK: &str = "20261122-critic-abcdef";

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

    /// Parse the override's value the way Codex's `-c` parser does
    /// (TOML), returning each path's access. Fails on a duplicate key or
    /// a broken escape, either of which Codex would silently turn into a
    /// string and then reject.
    fn parse(permissions: &str) -> Vec<(String, String)> {
        let value = permissions
            .strip_prefix("permissions.fleet.filesystem=")
            .unwrap();
        let document: toml_edit::DocumentMut = format!("v = {value}").parse().unwrap();
        document["v"]
            .as_inline_table()
            .unwrap()
            .iter()
            .map(|(key, value)| (key.to_string(), value.as_str().unwrap().to_string()))
            .collect()
    }

    fn access(entries: &[(String, String)], path: &Path) -> Option<String> {
        let path = path.to_string_lossy();
        entries
            .iter()
            .find(|(key, _)| *key == path)
            .map(|(_, access)| access.clone())
    }

    fn add_worktrees(project: &Path, worktrees: &Path, names: &[&str]) {
        for name in names {
            git(
                project,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("multiverse/{name}"),
                    worktrees.join(name).to_str().unwrap(),
                ],
            );
        }
    }

    #[test]
    fn a_reader_dispatch_writes_only_its_task_dir_and_lane_state() {
        let code = tempfile::tempdir().unwrap();
        let code_root = code.path().canonicalize().unwrap();
        let project = code_root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(code_root.join("sibling")).unwrap();
        init_repo(&project);
        std::fs::write(project.join("README"), "main").unwrap();
        let state = project.join(".multiverse");
        let task_dir = state.join("tasks").join(TASK);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::create_dir_all(state.join("tasks").join("other")).unwrap();
        std::fs::write(state.join("PASSDOWN.md"), "").unwrap();
        std::fs::write(state.join("routing.log"), "").unwrap();

        let permissions = codex_worktree_permissions(&project, &project, TASK).unwrap();
        let entries = parse(&permissions);

        // `:workspace` already writes the project cwd; listing it as
        // `read` would lose on the same path. `.multiverse` stays
        // `write` for sidecar temps; PASSDOWN is a more-specific `read`.
        assert_eq!(access(&entries, &project), None, "{permissions}");
        assert_eq!(access(&entries, &state).as_deref(), Some("write"));
        assert_eq!(
            access(&entries, &project.join("README")).as_deref(),
            Some("read")
        );
        assert_eq!(access(&entries, &task_dir).as_deref(), Some("write"));
        assert_eq!(
            access(&entries, &state.join("routing.log")).as_deref(),
            Some("write")
        );
        assert_eq!(
            access(&entries, &state.join("PASSDOWN.md")).as_deref(),
            Some("read"),
            "{permissions}"
        );
        assert_eq!(
            access(&entries, &code_root.join("sibling")).as_deref(),
            Some("deny"),
            "{permissions}"
        );
        // The reader's gitdir is both its common and private gitdir: one
        // key, not two (which `parse` would refuse as a duplicate).
        assert_eq!(
            access(&entries, &project.join(".git")).as_deref(),
            Some("write")
        );
        assert!(
            entries
                .iter()
                .all(|(key, access)| access != "deny" || !project.starts_with(key)),
            "{permissions}"
        );
        assert_eq!(access(&entries, &code_root), None, "{permissions}");
    }

    #[test]
    fn returns_none_when_the_worktree_does_not_exist_yet() {
        // The filesystem table is absent; `apply_codex_worktree_permissions`
        // still defines `fleet` via `CODEX_FLEET_PROFILE_EXTENDS` so the
        // `default_permissions="fleet"` select cannot name an undefined
        // profile (card `20261122-b-a8b112`).
        let project = tempfile::tempdir().unwrap();
        init_repo(project.path());
        let not_yet = project.path().join("not-created-yet");
        assert_eq!(
            codex_worktree_permissions(&not_yet, project.path(), TASK),
            None
        );
    }

    #[test]
    fn builds_the_full_override_for_a_real_writer_worktree() {
        let code = tempfile::tempdir().unwrap();
        let code_root = code.path().canonicalize().unwrap();
        let project = code_root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(code_root.join("sibling")).unwrap();
        init_repo(&project);
        std::fs::write(project.join("README"), "main").unwrap();
        let state = project.join(".multiverse");
        for dir in ["tasks/other", "briefs", "inputs", "evidence"] {
            std::fs::create_dir_all(state.join(dir)).unwrap();
        }
        for file in [
            "routing.log",
            "routing.log.lock",
            "activity.cursor",
            "PASSDOWN.md",
        ] {
            std::fs::write(state.join(file), "").unwrap();
        }
        let worktrees = state.join("worktrees");
        add_worktrees(&project, &worktrees, &["lane", "other"]);
        let lane = worktrees.join("lane");

        let permissions = codex_worktree_permissions(&lane, &project, TASK).unwrap();
        let entries = parse(&permissions);

        let common_gitdir = project.join(".git");
        let lane_gitdir = common_gitdir
            .join("worktrees")
            .join("lane")
            .canonicalize()
            .unwrap();
        // The project's `.multiverse` itself is writable, so the lane-side
        // `multiverse` commands can lock and append to `routing.log`.
        for written in [&lane, &common_gitdir, &lane_gitdir, &state] {
            assert_eq!(
                access(&entries, written).as_deref(),
                Some("write"),
                "{written:?} in {permissions}"
            );
        }
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
        // Everything beside the chain is denied, at every level of it:
        // a sibling project, the main checkout's own files and state
        // (other tasks, briefs, inputs, evidence, PASSDOWN), and another
        // lane.
        for denied in [
            code_root.join("sibling"),
            project.join("README"),
            state.join("tasks"),
            state.join("briefs"),
            state.join("inputs"),
            state.join("evidence"),
            state.join("PASSDOWN.md"),
            worktrees.join("other"),
        ] {
            assert_eq!(
                access(&entries, &denied).as_deref(),
                Some("deny"),
                "{denied:?} in {permissions}"
            );
        }
        // The chain itself stays readable — git's realpath walk needs it.
        for chain in [&code_root, &project] {
            assert_eq!(access(&entries, chain), None, "{chain:?} in {permissions}");
        }
        // The routing log and its lock inherit the state directory's
        // `write` rather than being denied beside the chain.
        for routing in ["routing.log", "routing.log.lock"] {
            assert_eq!(
                access(&entries, &state.join(routing)),
                None,
                "{routing} in {permissions}"
            );
        }
        // Under that `write`, the way to the worktree and the Nexus's
        // inbox cursor are narrowed back to read-only.
        for read in [&worktrees, &state.join("activity.cursor")] {
            assert_eq!(
                access(&entries, read).as_deref(),
                Some("read"),
                "{read:?} in {permissions}"
            );
        }
    }

    #[test]
    fn other_lane_gitdirs_and_hooks_are_denied() {
        let code = tempfile::tempdir().unwrap();
        let code_root = code.path().canonicalize().unwrap();
        let project = code_root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        init_repo(&project);
        let worktrees = project.join(".multiverse").join("worktrees");
        add_worktrees(&project, &worktrees, &["lane", "other"]);
        let lane = worktrees.join("lane");

        let permissions = codex_worktree_permissions(&lane, &project, TASK).unwrap();
        let entries = parse(&permissions);

        let common_gitdir = project.join(".git");
        let own = common_gitdir.join("worktrees").join("lane");
        let other = common_gitdir.join("worktrees").join("other");
        assert_eq!(
            access(&entries, &own).as_deref(),
            Some("write"),
            "{permissions}"
        );
        assert_eq!(
            access(&entries, &other).as_deref(),
            Some("deny"),
            "{permissions}"
        );
        assert_eq!(
            access(&entries, &common_gitdir.join("hooks")).as_deref(),
            Some("deny"),
            "{permissions}"
        );
        assert_eq!(
            access(&entries, &common_gitdir).as_deref(),
            Some("write"),
            "{permissions}"
        );
    }

    #[test]
    fn home_relative_grants_are_omitted_when_home_does_not_resolve() {
        let code = tempfile::tempdir().unwrap();
        let project = code.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        init_repo(&project);

        let permissions = permissions_table(&project, &project, TASK, None).unwrap();
        assert!(!permissions.contains(".docker/buildx"), "{permissions}");
        assert!(
            !permissions.contains("Library/Caches/go-build"),
            "{permissions}"
        );
        assert!(!permissions.contains("go/pkg/mod"), "{permissions}");
        assert!(!permissions.contains("/.docker/"), "{permissions}");
    }

    #[test]
    fn a_home_direct_project_denies_home_siblings_except_toolchain() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let project = home.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        init_repo(&project);
        for name in [
            "other-project",
            ".ssh",
            ".cargo",
            ".rustup",
            ".codex",
            ".docker/buildx",
        ] {
            std::fs::create_dir_all(home.join(name)).unwrap();
        }

        let permissions = permissions_table(&project, &project, TASK, Some(home.clone())).unwrap();
        let entries = parse(&permissions);

        assert_eq!(
            access(&entries, &home.join("other-project")).as_deref(),
            Some("deny"),
            "{permissions}"
        );
        assert_eq!(
            access(&entries, &home.join(".ssh")).as_deref(),
            Some("deny"),
            "{permissions}"
        );
        for kept in [".cargo", ".rustup", ".codex", ".docker"] {
            assert_eq!(
                access(&entries, &home.join(kept)),
                None,
                "{kept} in {permissions}"
            );
        }
        assert!(
            permissions.contains(".docker/buildx\"=\"write\""),
            "{permissions}"
        );
    }

    #[test]
    fn home_ancestors_are_not_listed() {
        let users = tempfile::tempdir().unwrap();
        let users = users.path().canonicalize().unwrap();
        let me = users.join("me");
        let otheruser = users.join("otheruser");
        let project = me.join("proj");
        let lane = project.join("lane");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::create_dir_all(&otheruser).unwrap();
        std::fs::create_dir_all(me.join(".ssh")).unwrap();
        std::fs::create_dir_all(project.join("src")).unwrap();

        let denies = sibling_denies(&users, &lane, std::slice::from_ref(&lane), Some(&me));

        assert!(
            !denies.contains(&otheruser),
            "an ancestor of $HOME must not be listed: {denies:?}"
        );
        assert!(denies.contains(&me.join(".ssh")), "{denies:?}");
        assert!(denies.contains(&project.join("src")), "{denies:?}");
    }

    #[test]
    fn sibling_names_with_c0_controls_are_escaped_and_denied() {
        let code = tempfile::tempdir().unwrap();
        let code_root = code.path().canonicalize().unwrap();
        let project = code_root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        init_repo(&project);
        let planted = code_root.join("sib\nling");
        std::fs::create_dir(&planted).unwrap();

        let permissions = codex_worktree_permissions(&project, &project, TASK).unwrap();
        assert!(permissions.contains("sib\\nling"), "{permissions}");
        let entries = parse(&permissions);
        assert_eq!(
            access(&entries, &planted).as_deref(),
            Some("deny"),
            "{permissions}"
        );
    }

    #[test]
    fn a_worktree_outside_any_repo_still_defines_the_profile() {
        // `default_permissions="fleet"` rides on every Codex launch, and
        // Codex refuses an undefined profile — so no gitdir is no reason
        // to drop the table.
        let code = tempfile::tempdir().unwrap();
        let project = code.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let permissions = codex_worktree_permissions(&project, &project, TASK).unwrap();
        let entries = parse(&permissions);
        let project = project.canonicalize().unwrap();
        assert_eq!(access(&entries, &project), None, "{permissions}");
        assert_eq!(
            access(&entries, &project.join(".multiverse")).as_deref(),
            Some("write")
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

        let permissions = codex_worktree_permissions(project.path(), other.path(), TASK).unwrap();
        let gitdir = project.path().canonicalize().unwrap().join(".git");
        assert!(
            permissions.contains(&format!("\"{}\"=\"write\"", gitdir.display())),
            "{permissions}"
        );
        assert_eq!(
            parse(&permissions)
                .iter()
                .find(|(key, _)| *key == gitdir.join("hooks").to_string_lossy())
                .map(|(_, access)| access.as_str()),
            Some("deny"),
            "{permissions}"
        );
    }

    #[test]
    fn escapes_quotes_backslashes_and_c0_controls_in_a_path() {
        assert_eq!(toml_basic_string_body(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(toml_basic_string_body("a\nb\t"), r#"a\nb\t"#);
        assert_eq!(toml_basic_string_body("\u{0001}"), r#"\u0001"#);
    }
}

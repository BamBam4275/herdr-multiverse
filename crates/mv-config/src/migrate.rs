//! One-shot migrator: rewrites retired role keys — and retired role
//! *names* that appear as values rather than keys, e.g. an account's
//! `roles_allowed` entry — to the current spelling in place via
//! `toml_edit`, keeping comments, and writes a `config.toml.pre-rename`
//! sibling before it does.
//!
//! Nothing invokes this automatically: `load::load_config` refuses an
//! old key outright (exit code 3), pointing the operator at
//! `multiverse config migrate --dry-run` and `--backup`
//! (`crates/mv-cli/src/commands/migrate.rs`) rather than migrating in
//! place on the caller's behalf. Run manually, `migrate_file` rewrites
//! an old key (`[roles.fixer]`, a chain's `fixer = […]`) to the
//! current spelling, keeps every comment, and leaves a `.pre-rename`
//! backup next to the original. After migration, [`crate::load::load_config`] enforces
//! "new input accepts only new keys" on its own — see
//! [`crate::roles`]'s `from_config_key` — so there is nothing left for
//! a loader to special-case once this has run once. That includes
//! `load::parse_account`'s own refusal of a retired name inside
//! `roles_allowed`, which is why this module also rewrites that array
//! rather than only table keys and chain entries.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Key, Table, Value};

use crate::error::ConfigError;
use crate::roles::Role;

/// One renamed key: where it was, and what it became.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleKeyRename {
    /// The table that held the key, e.g. `"roles"` or
    /// `"projects.example.chains"`.
    pub location: String,
    pub old_key: String,
    pub new_key: String,
}

/// What [`migrate_role_keys`] did. Empty means the document already
/// used only current role keys — a no-op migration is not an error,
/// since re-running the migrator against an already-migrated file (or
/// one that never had the old keys) must be safe (`architecture.md`
/// calls this "one-shot", not "run exactly once ever").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub renames: Vec<RoleKeyRename>,
}

impl MigrationReport {
    pub fn is_empty(&self) -> bool {
        self.renames.is_empty()
    }
}

/// Rewrites every retired role key this document contains — `[roles.
/// <old>]` table headers, and `<old> = […]` chain entries under either
/// a global `[projects.<name>.chains]` table or a project-local
/// `roster.toml`'s `[chains]` table — to the current key, in place.
/// Also rewrites every retired role *name* that appears as a plain
/// string value rather than a key: today that is only
/// `[accounts.<name>]`'s `roles_allowed` array (`load::ACCOUNT_KEYS`;
/// `review_policy` and `[projects.<name>]`'s other keys never take a
/// role name as a value, and a project's `roster.toml` only ever
/// carries `[chains]`, already covered above).
///
/// Preserves the key's own leading comment/whitespace (its
/// [`toml_edit::Key`] decor), its own position among its siblings, and
/// the table's own header-adjacent comment block — none of them move
/// just because one key in the table got renamed (finding 5: the
/// previous version appended every rename to the end of its table,
/// which silently reordered every sibling declared after it on every
/// run). A key already spelled the current way is left alone, so
/// running this twice is a no-op the second time.
pub fn migrate_role_keys(doc: &mut DocumentMut) -> MigrationReport {
    let mut report = MigrationReport::default();

    if let Some(roles) = doc.get_mut("roles").and_then(Item::as_table_mut) {
        rename_role_keys_in(roles, "roles", &mut report);
    }

    // `roster.toml`'s shape: `[chains]` at the document root.
    if let Some(chains) = doc.get_mut("chains").and_then(Item::as_table_mut) {
        rename_role_keys_in(chains, "chains", &mut report);
    }

    // The global config's shape: `[projects.<name>.chains]`.
    if let Some(projects) = doc.get_mut("projects").and_then(Item::as_table_mut) {
        let project_names: Vec<String> = projects.iter().map(|(k, _)| k.to_string()).collect();
        for name in project_names {
            let chains = projects
                .get_mut(&name)
                .and_then(Item::as_table_mut)
                .and_then(|project| project.get_mut("chains"))
                .and_then(Item::as_table_mut);
            if let Some(chains) = chains {
                rename_role_keys_in(chains, &format!("projects.{name}.chains"), &mut report);
            }
        }
    }

    // `[accounts.<name>].roles_allowed` names roles as array VALUES,
    // not keys — the one place in the schema that does.
    if let Some(accounts) = doc.get_mut("accounts").and_then(Item::as_table_mut) {
        let account_names: Vec<String> = accounts.iter().map(|(k, _)| k.to_string()).collect();
        for name in account_names {
            if let Some(account) = accounts.get_mut(&name).and_then(Item::as_table_mut) {
                rename_roles_allowed_values_in(account, &name, &mut report);
            }
        }
    }

    report
}

fn rename_role_keys_in(table: &mut Table, location: &str, report: &mut MigrationReport) {
    // Decide the renames first, exactly as before: a retired key with
    // no current-spelling sibling already present gets renamed; one
    // that does (a half-done hand edit) is left alone for a human to
    // resolve.
    let mut rename_for: BTreeMap<String, &'static str> = BTreeMap::new();
    for role in Role::ALL {
        let old = role.legacy_config_key();
        let new = role.config_key();
        if old == new || !table.contains_key(old) || table.contains_key(new) {
            continue;
        }
        rename_for.insert(old.to_string(), new);
    }
    if rename_for.is_empty() {
        return;
    }

    // Snapshot every entry in its current order, then rebuild the table
    // from scratch in that same order, substituting the renamed key
    // (with the old key's own decor) where one applies. `Table::clear`
    // only empties the key-value map — the table's own header decor,
    // position, and implicit/dotted flags are separate fields and
    // survive untouched.
    let order: Vec<String> = table.iter().map(|(k, _)| k.to_string()).collect();
    let entries: Vec<(Key, Item)> = order
        .iter()
        .map(|k| {
            let (key, item) = table
                .get_key_value(k)
                .expect("key was just listed by this table's own iter()");
            (key.clone(), item.clone())
        })
        .collect();

    table.clear();
    for (old_key, item) in entries {
        match rename_for.get(old_key.get()) {
            Some(&new) => {
                let new_key = Key::new(new).with_leaf_decor(old_key.leaf_decor().clone());
                table.insert_formatted(&new_key, item);
                report.renames.push(RoleKeyRename {
                    location: location.to_string(),
                    old_key: old_key.get().to_string(),
                    new_key: new.to_string(),
                });
            }
            None => {
                table.insert_formatted(&old_key, item);
            }
        }
    }
}

/// Rewrites every retired role name inside one account's
/// `roles_allowed` array (if it has one) to the current spelling,
/// entry by entry, preserving that entry's own decor (its surrounding
/// whitespace/comment) via [`toml_edit::Array::replace`].
///
/// Mirrors [`rename_role_keys_in`]'s two guards: a role whose name did
/// not change (`council`) is skipped, and an array already carrying
/// both spellings is left alone rather than guessed at — a human
/// needs to resolve that by hand, same as a half-renamed table would.
fn rename_roles_allowed_values_in(
    account: &mut Table,
    account_name: &str,
    report: &mut MigrationReport,
) {
    let Some(array) = account
        .get_mut("roles_allowed")
        .and_then(Item::as_array_mut)
    else {
        return;
    };

    let location = format!("accounts.{account_name}.roles_allowed");
    for role in Role::ALL {
        let old = role.legacy_config_key();
        let new = role.config_key();
        if old == new {
            continue;
        }
        let has_new_already =
            (0..array.len()).any(|i| array.get(i).and_then(Value::as_str) == Some(new));
        if has_new_already {
            continue;
        }
        for i in 0..array.len() {
            if array.get(i).and_then(Value::as_str) == Some(old) {
                array.replace(i, new);
                report.renames.push(RoleKeyRename {
                    location: location.clone(),
                    old_key: old.to_string(),
                    new_key: new.to_string(),
                });
            }
        }
    }
}

/// Migrates the config file at `path` in place: reads it, runs
/// [`migrate_role_keys`], and — only if it actually changed anything —
/// writes a `<path>.pre-rename` sibling holding the original bytes
/// before replacing `path` with the migrated text.
///
/// The live-file replacement goes through [`mv_core::atomic::write_atomic`]
/// (same-directory temp file, fsync, rename): a kill or ENOSPC mid-write
/// leaves `path` exactly as it was, never truncated (finding 5). The
/// `.pre-rename` backup write goes through
/// [`mv_core::atomic::create_owner_read_write_only`] — mode `0600` set at
/// creation rather than chmod'd after, matching the live file's own
/// permissions regardless of umask — not
/// [`mv_core::atomic::write_atomic`]'s crash-atomic rename dance: it
/// happens first, before the live file is touched at
/// all, and `original` is already held in memory, so a partial *backup*
/// write is recoverable (re-run the migrator) rather than load-bearing
/// the way the live write is.
///
/// If anything already exists at the `.pre-rename` path — file, symlink,
/// or broken symlink, checked with [`Path::symlink_metadata`] so a
/// dangling link still counts — this refuses rather than overwriting it
/// (finding 8): a second `migrate` run after a legacy key crept back in
/// would otherwise silently clobber the one copy of the true pre-cutover
/// original with a since-remigrated one, and writing through a symlink
/// at that path would land wherever the link pointed instead.
pub fn migrate_file(path: &Path) -> Result<MigrationReport, ConfigError> {
    let original = fs::read_to_string(path)
        .map_err(|e| ConfigError::at(path, format!("could not read: {e}")))?;
    let mut doc: DocumentMut = original
        .parse()
        .map_err(|e| ConfigError::at(path, format!("malformed TOML: {e}")))?;

    let report = migrate_role_keys(&mut doc);
    if report.is_empty() {
        return Ok(report);
    }

    let backup_path = backup_path_for(path);
    if backup_path.symlink_metadata().is_ok() {
        return Err(ConfigError::at(
            &backup_path,
            "already exists — migrate refuses to overwrite an existing backup; move it aside and re-run",
        ));
    }
    mv_core::atomic::create_owner_read_write_only(&backup_path, original.as_bytes())
        .map_err(|e| ConfigError::at(&backup_path, format!("could not write backup: {e}")))?;
    mv_core::atomic::write_atomic(path, doc.to_string().as_bytes())
        .map_err(|e| ConfigError::at(path, format!("could not write migrated config: {e}")))?;

    Ok(report)
}

fn backup_path_for(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".pre-rename");
    path.with_file_name(name)
}

/// Every `<project>/.multiverse/roster.toml` that `config_content`
/// (a `config.toml`'s raw bytes, migrated or not) names and that
/// exists on disk — finding 7: a global `migrate_file` run never
/// touched these, so a project stayed unloadable after cutover even
/// though the fleet config itself was fixed.
///
/// Parsed loosely, independent of [`crate::load::load_config`]'s full
/// validation, on purpose: a `[projects.<name>]` table's `path` key
/// does not change shape across the role-key rename, so this can run
/// against the original, not-yet-migrated bytes, and a config with an
/// unrelated validation problem in some *other* project should not
/// block migrating every project whose own `path` is fine. A
/// placeholder or non-absolute `path` is skipped, the same way
/// `load_config` would never resolve one into a real directory.
pub fn discover_project_roster_paths(config_content: &str) -> Vec<PathBuf> {
    let Ok(root) = config_content.parse::<toml::value::Table>() else {
        return Vec::new();
    };
    let Some(projects) = root.get("projects").and_then(toml::Value::as_table) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for project in projects.values() {
        let Some(table) = project.as_table() else {
            continue;
        };
        let Some(path) = table.get("path").and_then(toml::Value::as_str) else {
            continue;
        };
        if crate::placeholder::is_placeholder(path) || !path.starts_with('/') {
            continue;
        }
        let roster_path = Path::new(path).join(".multiverse").join("roster.toml");
        if roster_path.is_file() {
            out.push(roster_path);
        }
    }
    out
}

/// [`migrate_file`] on `path` itself, then on every project roster
/// [`discover_project_roster_paths`] finds for it — the one call an
/// operator-facing `config migrate` command needs to cover finding 7
/// in full, rather than only the global file.
pub fn migrate_file_and_project_rosters(
    path: &Path,
) -> Result<(MigrationReport, Vec<(PathBuf, MigrationReport)>), ConfigError> {
    let content = fs::read_to_string(path)
        .map_err(|e| ConfigError::at(path, format!("could not read: {e}")))?;
    let roster_paths = discover_project_roster_paths(&content);

    let config_report = migrate_file(path)?;

    let mut roster_reports = Vec::with_capacity(roster_paths.len());
    for roster_path in roster_paths {
        let report = migrate_file(&roster_path)?;
        roster_reports.push((roster_path, report));
    }

    Ok((config_report, roster_reports))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn renames_a_retired_role_table_preserving_its_comment() {
        let input = r#"# fixer tuning
[roles.fixer]
write_scope = "declared"
# a nested comment
effort = "medium"
"#;
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert_eq!(report.renames.len(), 1);
        assert_eq!(report.renames[0].old_key, "fixer");
        assert_eq!(report.renames[0].new_key, "builder");

        let rendered = doc.to_string();
        assert!(rendered.contains("# fixer tuning"));
        assert!(rendered.contains("[roles.builder]"));
        assert!(!rendered.contains("[roles.fixer]"));
        assert!(rendered.contains("# a nested comment"));
        assert!(rendered.contains(r#"write_scope = "declared""#));
    }

    #[test]
    fn renames_chain_entries_under_a_project() {
        let input = "[projects.example.chains]\n# only claude for now\nfixer = [\"claude-work\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert_eq!(report.renames.len(), 1);
        assert_eq!(report.renames[0].location, "projects.example.chains");
        let rendered = doc.to_string();
        assert!(rendered.contains("builder = [\"claude-work\"]"));
        assert!(rendered.contains("# only claude for now"));
    }

    #[test]
    fn renames_chains_in_a_roster_toml_shaped_document() {
        let input = "[chains]\nreviewer = [\"codex-work\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert_eq!(report.renames.len(), 1);
        assert_eq!(report.renames[0].location, "chains");
        assert!(doc.to_string().contains("critic = [\"codex-work\"]"));
    }

    #[test]
    fn renames_a_retired_role_name_inside_roles_allowed_preserving_comment_and_siblings() {
        let input = "[accounts.local]\n# floor seat: quick, bounded roles only\nroles_allowed = [\"explorer\", \"fixer\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert_eq!(report.renames.len(), 2);
        assert_eq!(report.renames[0].location, "accounts.local.roles_allowed");
        assert_eq!(report.renames[0].old_key, "explorer");
        assert_eq!(report.renames[0].new_key, "scout");
        assert_eq!(report.renames[1].old_key, "fixer");
        assert_eq!(report.renames[1].new_key, "builder");

        let rendered = doc.to_string();
        assert!(rendered.contains("roles_allowed = [\"scout\", \"builder\"]"));
        assert!(rendered.contains("# floor seat: quick, bounded roles only"));
    }

    #[test]
    fn roles_allowed_with_both_spellings_present_is_left_alone() {
        let input = "[accounts.local]\nroles_allowed = [\"fixer\", \"builder\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert!(report.is_empty());
        assert!(doc
            .to_string()
            .contains("roles_allowed = [\"fixer\", \"builder\"]"));
    }

    #[test]
    fn roles_allowed_already_on_current_names_is_a_no_op() {
        let input = "[accounts.local]\nroles_allowed = [\"scout\", \"archivist\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);
        assert!(report.is_empty());
    }

    #[test]
    fn running_the_roles_allowed_migration_twice_is_a_no_op_the_second_time() {
        let input = "[accounts.local]\nroles_allowed = [\"fixer\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let first = migrate_role_keys(&mut doc);
        assert_eq!(first.renames.len(), 1);
        let second = migrate_role_keys(&mut doc);
        assert!(second.is_empty());
    }

    #[test]
    fn council_s_key_is_left_untouched() {
        let input = "[roles.council]\nwrite_scope = \"none\"\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);
        assert!(report.is_empty());
        assert!(doc.to_string().contains("[roles.council]"));
    }

    #[test]
    fn running_the_migrator_twice_is_a_no_op_the_second_time() {
        let input = "[roles.fixer]\nwrite_scope = \"declared\"\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let first = migrate_role_keys(&mut doc);
        assert_eq!(first.renames.len(), 1);
        let second = migrate_role_keys(&mut doc);
        assert!(second.is_empty());
    }

    #[test]
    fn a_config_already_on_new_keys_is_left_alone() {
        let input = "[roles.builder]\nwrite_scope = \"declared\"\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);
        assert!(report.is_empty());
    }

    #[test]
    fn migrate_file_writes_a_pre_rename_backup_and_rewrites_in_place() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "# header comment\n[roles.fixer]\nwrite_scope = \"declared\"\n";
        fs::write(&path, original).unwrap();

        let report = migrate_file(&path).unwrap();
        assert_eq!(report.renames.len(), 1);

        let backup = dir.path().join("config.toml.pre-rename");
        assert_eq!(fs::read_to_string(&backup).unwrap(), original);

        let migrated = fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[roles.builder]"));
        assert!(migrated.contains("# header comment"));
    }

    #[test]
    fn migrate_file_refuses_to_overwrite_an_existing_backup() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[roles.fixer]\nwrite_scope = \"declared\"\n").unwrap();

        let backup = dir.path().join("config.toml.pre-rename");
        fs::write(&backup, "the pre-cutover original, from an earlier run").unwrap();

        let err = migrate_file(&path).unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "expected a refusal mentioning the existing backup, got: {err}"
        );

        // Neither the stale backup nor the live config may be touched.
        assert_eq!(
            fs::read_to_string(&backup).unwrap(),
            "the pre-cutover original, from an earlier run"
        );
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[roles.fixer]\nwrite_scope = \"declared\"\n"
        );
    }

    #[test]
    fn migrate_file_does_not_write_a_backup_when_nothing_changed() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[roles.builder]\nwrite_scope = \"declared\"\n").unwrap();

        let report = migrate_file(&path).unwrap();
        assert!(report.is_empty());
        assert!(!dir.path().join("config.toml.pre-rename").exists());
    }

    /// A fixture shaped like a real fleet config: several accounts (one
    /// with a `roles_allowed` floor-seat eligibility list, spelled the
    /// retired way), every `[roles.*]` table, and both a global
    /// `[projects.<name>.chains]` table and a `roster.toml`-shaped
    /// `[chains]` table — every surface this module rewrites, all in
    /// one document, each with its own comment to prove none are lost.
    #[test]
    fn migrates_a_fixture_shaped_like_a_real_fleet_config() {
        let fixture = r#"# fleet-wide tuning
max_concurrent = 3

# --- accounts ----------------------------------------------------------
[accounts.claude-work]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/home/ops/.config/claude-work"
pool = "claude:work"

# floor seat: quick, bounded roles only — see config.example.toml
[accounts.local]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/home/ops/.config/codex-local"
pool = "local"
roles_allowed = ["explorer", "librarian"]

# --- roles ---------------------------------------------------------------
# fixer is the first writer
[roles.fixer]
write_scope = "declared"

# oracle takes the hardest questions
[roles.oracle]
on_exhausted = "queue"

# --- chains ----------------------------------------------------------------
[projects.example.chains]
# scout's only seat for now
fixer = ["claude-work"]

[projects.example2.chains]
reviewer = ["claude-work"]
"#;
        let mut doc: DocumentMut = fixture.parse().unwrap();
        let report = migrate_role_keys(&mut doc);

        assert_eq!(report.renames.len(), 6, "{report:?}");

        let rendered = doc.to_string();
        for comment in [
            "# fleet-wide tuning",
            "# floor seat: quick, bounded roles only — see config.example.toml",
            "# fixer is the first writer",
            "# oracle takes the hardest questions",
            "# scout's only seat for now",
        ] {
            assert!(rendered.contains(comment), "lost comment: {comment:?}");
        }
        assert!(rendered.contains("roles_allowed = [\"scout\", \"archivist\"]"));
        assert!(rendered.contains("[roles.builder]"));
        assert!(rendered.contains("[roles.seer]"));
        assert!(rendered.contains("builder = [\"claude-work\"]"));
        assert!(rendered.contains("critic = [\"claude-work\"]"));
        assert!(!rendered.contains("\"explorer\""));
        assert!(!rendered.contains("\"librarian\""));
        assert!(!rendered.contains("[roles.fixer]"));
        assert!(!rendered.contains("[roles.oracle]"));

        // Re-running against the migrated document is a true no-op.
        let second = migrate_role_keys(&mut doc);
        assert!(second.is_empty());
    }

    // -- finding 5: ordering ------------------------------------------

    #[test]
    fn a_renamed_role_table_stays_in_its_original_position_among_siblings() {
        let input = "[roles.archivist]\nwrite_scope = \"none\"\n\n\
                      [roles.fixer]\nwrite_scope = \"declared\"\n\n\
                      [roles.seer]\non_exhausted = \"queue\"\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);
        assert_eq!(report.renames.len(), 1);

        let rendered = doc.to_string();
        let archivist_pos = rendered.find("[roles.archivist]").unwrap();
        let builder_pos = rendered.find("[roles.builder]").unwrap();
        let seer_pos = rendered.find("[roles.seer]").unwrap();
        assert!(
            archivist_pos < builder_pos,
            "builder should stay after archivist: {rendered}"
        );
        assert!(
            builder_pos < seer_pos,
            "builder should stay before seer, not move to the end: {rendered}"
        );
    }

    #[test]
    fn a_renamed_chain_entry_stays_in_its_original_position_among_siblings() {
        let input =
            "[projects.example.chains]\narchivist = [\"a\"]\nfixer = [\"b\"]\nseer = [\"c\"]\n";
        let mut doc: DocumentMut = input.parse().unwrap();
        let report = migrate_role_keys(&mut doc);
        assert_eq!(report.renames.len(), 1);

        let rendered = doc.to_string();
        let archivist_pos = rendered.find("archivist = ").unwrap();
        let builder_pos = rendered.find("builder = ").unwrap();
        let seer_pos = rendered.find("seer = ").unwrap();
        assert!(archivist_pos < builder_pos);
        assert!(
            builder_pos < seer_pos,
            "builder should stay before seer, not move to the end: {rendered}"
        );
    }

    // -- finding 5: atomic write ---------------------------------------

    #[test]
    fn migrate_file_never_leaves_the_live_path_truncated_on_a_simulated_crash() {
        // `write_atomic` writes to a same-directory temp file and only
        // then renames it over `path` — so even if this test can't
        // simulate a real kill -9, the invariant it buys is directly
        // checkable: after `migrate_file`, there is no leftover temp
        // file next to `config.toml`, and the live file is never the
        // partially-written intermediate state (it is either the
        // original or the fully migrated text, nothing in between).
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = "[roles.fixer]\nwrite_scope = \"declared\"\n";
        fs::write(&path, original).unwrap();

        migrate_file(&path).unwrap();

        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .filter(|name| name != "config.toml" && name != "config.toml.pre-rename")
            .collect();
        assert!(leftovers.is_empty(), "leftover temp file(s): {leftovers:?}");

        let migrated = fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[roles.builder]"));
    }

    // -- finding 7: project roster.toml discovery/migration -----------

    #[test]
    fn discover_project_roster_paths_finds_an_existing_roster_and_skips_a_missing_one() {
        let dir = tempdir().unwrap();
        let with_roster = dir.path().join("with-roster");
        let without_roster = dir.path().join("without-roster");
        fs::create_dir_all(with_roster.join(".multiverse")).unwrap();
        fs::create_dir_all(&without_roster).unwrap();
        fs::write(
            with_roster.join(".multiverse").join("roster.toml"),
            "[chains]\n",
        )
        .unwrap();

        let config = format!(
            r#"
[projects.has-one]
path = "{with_roster}"

[projects.has-none]
path = "{without_roster}"

[projects.placeholder]
path = "<<PLACEHOLDER: absolute path to the project root>>"
"#,
            with_roster = with_roster.display(),
            without_roster = without_roster.display(),
        );

        let found = discover_project_roster_paths(&config);
        assert_eq!(found, vec![with_roster.join(".multiverse/roster.toml")]);
    }

    #[test]
    fn migrate_file_and_project_rosters_migrates_the_config_and_every_roster_it_names() {
        let dir = tempdir().unwrap();
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        fs::write(
            project_root.join(".multiverse").join("roster.toml"),
            "[chains]\nfixer = [\"claude-work\"]\n",
        )
        .unwrap();

        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            format!(
                "[roles.fixer]\nwrite_scope = \"declared\"\n\n[projects.example]\npath = \"{path}\"\n",
                path = project_root.display()
            ),
        )
        .unwrap();

        let (config_report, roster_reports) =
            migrate_file_and_project_rosters(&config_path).unwrap();
        assert_eq!(config_report.renames.len(), 1);
        assert_eq!(roster_reports.len(), 1);
        assert_eq!(roster_reports[0].1.renames.len(), 1);

        let migrated_config = fs::read_to_string(&config_path).unwrap();
        assert!(migrated_config.contains("[roles.builder]"));
        let migrated_roster =
            fs::read_to_string(project_root.join(".multiverse").join("roster.toml")).unwrap();
        assert!(migrated_roster.contains("builder = [\"claude-work\"]"));
    }
}

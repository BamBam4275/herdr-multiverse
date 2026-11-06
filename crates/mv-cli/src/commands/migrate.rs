//! `multiverse config migrate`: the operator-facing surface for
//! `mv-config::migrate`'s one-shot role-key rewrite.
//!
//! Unlike `dispatch`/`council`, this command is not waiting on an engine
//! that does not exist yet — `mv_config::migrate::migrate_role_keys`
//! already does the real rewrite, with comments intact. This module only
//! adds the CLI-level safety shape: a `--dry-run` preview, and a
//! `--backup` flag required to actually write (the operator's
//! confirmation that this is the live config, mirroring `dispatch`'s and
//! `council`'s own "requires an explicit flag to execute" rule). When it
//! does write, it keeps a timestamped copy of the original next to it —
//! deliberately distinct from `mv_config::migrate::migrate_file`'s own
//! fixed `.pre-rename` sibling, since a human re-running this by hand at
//! cutover may want more than one generation of backup — then replaces
//! the file atomically via `mv_core::atomic::write_atomic`.
//!
//! Also migrates every project's `.multiverse/roster.toml`
//! (`mv_config::migrate::discover_project_roster_paths`, task
//! `20261102-fixer-2bc5ba` / critic-config.md finding 7): a fleet config
//! migrated on its own but with an untouched roster left a project
//! unloadable after cutover, since `roster.rs` refuses a retired role
//! key the same way the global config does. Each roster gets its own
//! timestamped backup and its own atomic write, independent of the
//! global config's.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use toml_edit::DocumentMut;

use mv_config::error::ConfigError;
use mv_config::load::resolve_config_path_from_env;
use mv_config::migrate::{discover_project_roster_paths, migrate_role_keys, MigrationReport};
use mv_core::atomic::{create_owner_read_write_only, write_atomic};
use mv_core::time::now_stamp;

use crate::exit::Failure;

fn to_failure(error: ConfigError) -> Failure {
    Failure::Config(error.to_string())
}

fn resolve_path(args: &ArgMatches) -> Result<PathBuf, Failure> {
    let cli_override = args.get_one::<String>("config").map(PathBuf::from);
    resolve_config_path_from_env(cli_override.as_deref())
        .map_err(|e| Failure::Config(format!("could not resolve the config path: {e}")))
}

/// `now_stamp()`, with the punctuation a filename can't carry removed —
/// `2026-11-02T12:00:00+00:00` becomes `20261102T120000Z` (always UTC,
/// per `mv_core::time`'s own contract, so `Z` loses no information).
fn filename_stamp() -> String {
    now_stamp().replace(['-', ':'], "").replace("+0000", "Z")
}

fn timestamped_backup_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".bak-{}", filename_stamp()));
    path.with_file_name(name)
}

fn summarize(report: &MigrationReport) -> String {
    report
        .renames
        .iter()
        .map(|r| {
            format!(
                "{}.{} -> {}.{}",
                r.location, r.old_key, r.location, r.new_key
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------
// A small, dependency-free unified-style diff for `--dry-run`.
// ---------------------------------------------------------------------

enum Line<'a> {
    Context(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Longest-common-subsequence edit script between two line slices.
/// Workspace-size inputs only (an operator's config file, not a corpus) —
/// the backing table is `O(len(a) * len(b))`.
fn diff_lines<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Line<'a>> {
    let (n, m) = (a.len(), b.len());
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if a[i] == b[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let (mut i, mut j) = (0usize, 0usize);
    let mut out = Vec::new();
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Line::Context(a[i]));
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            out.push(Line::Removed(a[i]));
            i += 1;
        } else {
            out.push(Line::Added(b[j]));
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|l| Line::Removed(l)));
    out.extend(b[j..].iter().map(|l| Line::Added(l)));
    out
}

const DIFF_CONTEXT: usize = 3;

/// Render `ops` the way `diff -u` would, collapsing a long run of
/// unchanged lines around the parts that actually changed.
/// `mv_config::migrate` rewrites a renamed key in place — its table
/// position, decor, and every sibling's position are preserved — so an
/// ordinary rename's diff is just that one line, not a reshuffle of
/// everything declared after it.
fn render_diff(path: &Path, ops: &[Line<'_>]) -> String {
    let mut out = format!("--- {0}\n+++ {0} (migrated)\n", path.display());
    let mut i = 0;
    while i < ops.len() {
        if let Line::Context(_) = ops[i] {
            let start = i;
            while i < ops.len() && matches!(ops[i], Line::Context(_)) {
                i += 1;
            }
            let run = &ops[start..i];
            let at_start = start == 0;
            let at_end = i == ops.len();
            let visible = DIFF_CONTEXT * 2 + usize::from(!at_start && !at_end);
            if run.len() <= visible {
                for line in run {
                    push_context(&mut out, line);
                }
            } else {
                let lead = if at_start { 0 } else { DIFF_CONTEXT };
                for line in &run[..lead] {
                    push_context(&mut out, line);
                }
                let trail = if at_end { 0 } else { DIFF_CONTEXT };
                let hidden = run.len() - lead - trail;
                out.push_str(&format!("  ... {hidden} unchanged line(s) ...\n"));
                for line in &run[run.len() - trail..] {
                    push_context(&mut out, line);
                }
            }
        } else {
            match ops[i] {
                Line::Removed(l) => {
                    out.push_str("- ");
                    out.push_str(l);
                    out.push('\n');
                }
                Line::Added(l) => {
                    out.push_str("+ ");
                    out.push_str(l);
                    out.push('\n');
                }
                Line::Context(_) => unreachable!("handled above"),
            }
            i += 1;
        }
    }
    out
}

fn push_context(out: &mut String, line: &Line<'_>) {
    if let Line::Context(l) = line {
        out.push_str("  ");
        out.push_str(l);
        out.push('\n');
    }
}

// ---------------------------------------------------------------------

/// One discovered `.multiverse/roster.toml`, parsed and migrated the
/// same way the global config is, kept alongside its own original text
/// so a dry-run can diff it and a real run can back it up.
struct RosterMigration {
    path: PathBuf,
    original: String,
    doc: DocumentMut,
    report: MigrationReport,
}

fn load_roster_migrations(roster_paths: Vec<PathBuf>) -> Result<Vec<RosterMigration>, Failure> {
    let mut out = Vec::with_capacity(roster_paths.len());
    for path in roster_paths {
        let original = fs::read_to_string(&path)
            .map_err(|e| to_failure(ConfigError::at(&path, format!("could not read: {e}"))))?;
        let mut doc: DocumentMut = original
            .parse()
            .map_err(|e| to_failure(ConfigError::at(&path, format!("malformed TOML: {e}"))))?;
        let report = migrate_role_keys(&mut doc);
        out.push(RosterMigration {
            path,
            original,
            doc,
            report,
        });
    }
    Ok(out)
}

pub fn run(args: &ArgMatches, out: &mut dyn Write) -> Result<(), Failure> {
    let dry_run = args.get_flag("dry-run");
    let backup = args.get_flag("backup");
    let path = resolve_path(args)?;

    let original = fs::read_to_string(&path)
        .map_err(|e| to_failure(ConfigError::at(&path, format!("could not read: {e}"))))?;
    let mut doc = original
        .parse()
        .map_err(|e| to_failure(ConfigError::at(&path, format!("malformed TOML: {e}"))))?;
    let report = migrate_role_keys(&mut doc);

    // Every project roster.toml this config names gets migrated
    // alongside it (finding 7) — parsed from the original bytes, which
    // works whether or not the global config itself needed a rewrite,
    // since a `[projects.<name>].path` value never changes shape across
    // the role-key rename.
    let rosters = load_roster_migrations(discover_project_roster_paths(&original))?;

    if report.is_empty() && rosters.iter().all(|r| r.report.is_empty()) {
        let _ = writeln!(out, "{}: already on current role keys.", path.display());
        return Ok(());
    }

    let migrated = doc.to_string();

    if dry_run {
        if !report.is_empty() {
            let old: Vec<&str> = original.lines().collect();
            let new: Vec<&str> = migrated.lines().collect();
            let ops = diff_lines(&old, &new);
            let _ = write!(out, "{}", render_diff(&path, &ops));
        }
        for roster in &rosters {
            if roster.report.is_empty() {
                continue;
            }
            let roster_migrated = roster.doc.to_string();
            let old: Vec<&str> = roster.original.lines().collect();
            let new: Vec<&str> = roster_migrated.lines().collect();
            let ops = diff_lines(&old, &new);
            let _ = write!(out, "{}", render_diff(&roster.path, &ops));
        }
        return Ok(());
    }

    if !backup {
        return Err(Failure::Operation(
            "config migrate requires --backup to write changes; use --dry-run to preview".into(),
        ));
    }

    let mut summaries = Vec::new();

    if !report.is_empty() {
        let backup_path = timestamped_backup_path(&path);
        if backup_path.symlink_metadata().is_ok() {
            return Err(to_failure(ConfigError::at(
                &backup_path,
                "already exists — migrate refuses to overwrite an existing backup; move it aside and re-run",
            )));
        }
        create_owner_read_write_only(&backup_path, original.as_bytes()).map_err(|e| {
            to_failure(ConfigError::at(
                &backup_path,
                format!("could not write backup: {e}"),
            ))
        })?;
        write_atomic(&path, migrated.as_bytes()).map_err(|e| {
            to_failure(ConfigError::at(
                &path,
                format!("could not write migrated config: {e}"),
            ))
        })?;
        summaries.push(format!(
            "{}: migrated {} key(s) ({}); backup at {}.",
            path.display(),
            report.renames.len(),
            summarize(&report),
            backup_path.display()
        ));
    }

    for roster in &rosters {
        if roster.report.is_empty() {
            continue;
        }
        let roster_migrated = roster.doc.to_string();
        let backup_path = timestamped_backup_path(&roster.path);
        if backup_path.symlink_metadata().is_ok() {
            return Err(to_failure(ConfigError::at(
                &backup_path,
                "already exists — migrate refuses to overwrite an existing backup; move it aside and re-run",
            )));
        }
        create_owner_read_write_only(&backup_path, roster.original.as_bytes()).map_err(|e| {
            to_failure(ConfigError::at(
                &backup_path,
                format!("could not write backup: {e}"),
            ))
        })?;
        write_atomic(&roster.path, roster_migrated.as_bytes()).map_err(|e| {
            to_failure(ConfigError::at(
                &roster.path,
                format!("could not write migrated roster: {e}"),
            ))
        })?;
        summaries.push(format!(
            "{}: migrated {} key(s) ({}); backup at {}.",
            roster.path.display(),
            roster.report.renames.len(),
            summarize(&roster.report),
            backup_path.display()
        ));
    }

    for summary in summaries {
        let _ = writeln!(out, "{summary}");
    }
    Ok(())
}

pub fn run_default(args: &ArgMatches) -> Result<(), Failure> {
    match args.subcommand() {
        Some(("migrate", nested)) => run(nested, &mut std::io::stdout()),
        _ => unreachable!("clap requires a subcommand under `config`"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "mv-cli-migrate-test-{tag}-{}-{stamp}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("config")
            .unwrap()
            .subcommand_matches("migrate")
            .unwrap()
            .clone()
    }

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn an_already_current_config_is_a_no_op_under_every_flag_combination() {
        let dir = TempDir::new("noop");
        let path = write_config(dir.path(), "[roles.builder]\nwrite_scope = \"declared\"\n");
        for argv_flags in [vec![], vec!["--dry-run"], vec!["--backup"]] {
            let mut argv = vec!["config", "migrate", "--config", path.to_str().unwrap()];
            argv.extend(argv_flags);
            let args = matches(&argv);
            let mut out = Vec::new();
            run(&args, &mut out).unwrap();
            assert!(String::from_utf8(out)
                .unwrap()
                .contains("already on current role keys"));
        }
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "[roles.builder]\nwrite_scope = \"declared\"\n"
        );
    }

    #[test]
    fn dry_run_prints_a_diff_and_writes_nothing() {
        let dir = TempDir::new("dry-run");
        let original = "# fixer tuning\n[roles.fixer]\nwrite_scope = \"declared\"\n";
        let path = write_config(dir.path(), original);

        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--dry-run",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();

        assert!(printed.contains("- [roles.fixer]"));
        assert!(printed.contains("+ [roles.builder]"));
        assert!(printed.contains("# fixer tuning"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
        assert!(
            fs::read_dir(dir.path()).unwrap().count() == 1,
            "no backup should exist"
        );
    }

    #[test]
    fn writing_without_backup_refuses_and_names_both_flags() {
        let dir = TempDir::new("no-backup");
        let original = "[roles.fixer]\nwrite_scope = \"declared\"\n";
        let path = write_config(dir.path(), original);

        let args = matches(&["config", "migrate", "--config", path.to_str().unwrap()]);
        let mut out = Vec::new();
        let error = run(&args, &mut out).unwrap_err();
        assert_eq!(error.code(), 1);
        assert!(error.to_string().contains("--backup"));
        assert!(error.to_string().contains("--dry-run"));
        assert_eq!(fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn backup_migrates_atomically_and_keeps_a_timestamped_copy() {
        let dir = TempDir::new("backup");
        let original = "# header comment\n[roles.fixer]\nwrite_scope = \"declared\"\n";
        let path = write_config(dir.path(), original);

        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("migrated 1 key(s)"));
        assert!(printed.contains("roles.fixer -> roles.builder"));

        let migrated = fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[roles.builder]"));
        assert!(migrated.contains("# header comment"));

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "config.toml")
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup: {backups:?}");
        assert!(backups[0].starts_with("config.toml.bak-"));
        assert_eq!(
            fs::read_to_string(dir.path().join(&backups[0])).unwrap(),
            original
        );
    }

    #[test]
    #[cfg(unix)]
    fn backup_is_written_at_mode_0600_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("backup-mode");
        let original = "[roles.fixer]\nwrite_scope = \"declared\"\n";
        let path = write_config(dir.path(), original);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p != &path)
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup: {backups:?}");
        let mode = fs::metadata(&backups[0]).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "backup must not be more permissive than the live file it copies"
        );
    }

    #[test]
    fn running_twice_with_backup_is_idempotent_and_leaves_one_backup() {
        let dir = TempDir::new("idempotent");
        let path = write_config(dir.path(), "[roles.fixer]\nwrite_scope = \"declared\"\n");
        let argv = [
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ];

        let args = matches(&argv);
        run(&args, &mut Vec::new()).unwrap();
        let migrated_once = fs::read_to_string(&path).unwrap();

        let args = matches(&argv);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("already on current role keys"));
        assert_eq!(fs::read_to_string(&path).unwrap(), migrated_once);

        let backups = fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy() != "config.toml")
            .count();
        assert_eq!(backups, 1, "second run must not write another backup");
    }

    #[test]
    fn a_missing_config_file_is_a_config_error_not_a_panic() {
        let dir = TempDir::new("missing");
        let args = matches(&[
            "config",
            "migrate",
            "--config",
            dir.path().join("does-not-exist.toml").to_str().unwrap(),
            "--dry-run",
        ]);
        let error = run(&args, &mut Vec::new()).unwrap_err();
        assert_eq!(error.code(), 3);
    }

    /// `config.example.toml`, rewritten to the role keys it would have
    /// used before the rename — the realistic cutover scenario this
    /// command exists for.
    #[test]
    fn migrates_a_config_example_toml_shaped_file_with_old_keys() {
        let example = include_str!("../../../../config.example.toml")
            .replace("[roles.scout]", "[roles.explorer]")
            .replace("[roles.scout.models]", "[roles.explorer.models]")
            .replace(
                "scout = [\"claude-personal\"]",
                "explorer = [\"claude-personal\"]",
            );
        assert!(
            example.contains("[roles.explorer]"),
            "fixture setup sanity check"
        );

        let dir = TempDir::new("example");
        let path = write_config(dir.path(), &example);
        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();

        let migrated = fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[roles.scout]"));
        assert!(!migrated.contains("[roles.explorer]"));
        assert!(migrated.contains("scout = [\"claude-personal\"]"));
        assert!(migrated.contains("# Herdr Multiverse — example configuration"));
        assert!(migrated.contains("Never export ANTHROPIC_API_KEY"));
    }

    /// A synthetic, heavily commented config naming every retired role
    /// key at once, in both a `[roles.*]` table and a chains table —
    /// the "large commented config fixture" this task's brief calls for.
    #[test]
    fn preserves_every_comment_across_a_large_multi_role_fixture() {
        let fixture = r#"# fleet-wide tuning
max_concurrent = 3

# --- roles -----------------------------------------------------------
# orchestrator seeds every project; kept first on purpose
[roles.orchestrator]
write_scope = "none"

# explorer compresses context; cheapest tier
[roles.explorer]
write_scope = "none"
effort = "low"

# fixer is the first writer
[roles.fixer]
write_scope = "declared"
# nested: codex writers also get build-cache access
effort = "medium"

# reviewer excludes the author's vendor
[roles.reviewer]
write_scope = "none"

# oracle takes the hardest questions
[roles.oracle]
on_exhausted = "queue"

# council is a candidate pool, not a chain
[roles.council]
write_scope = "none"

# librarian archives history
[roles.librarian]
write_scope = "none"

# designer owns prompts/skills/templates
[roles.designer]
write_scope = "declared"

# observer watches screenshots; must be multimodal
[roles.observer]
write_scope = "none"

# --- chains ------------------------------------------------------------
[projects.example.chains]
# scout's only seat for now
fixer = ["claude-work"]
# critic's cross-vendor pair
reviewer = ["codex-work", "grok"]
"#;
        let dir = TempDir::new("large");
        let path = write_config(dir.path(), fixture);
        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        run(&args, &mut Vec::new()).unwrap();

        let migrated = fs::read_to_string(&path).unwrap();
        for comment in [
            "# fleet-wide tuning",
            "# orchestrator seeds every project; kept first on purpose",
            "# explorer compresses context; cheapest tier",
            "# fixer is the first writer",
            "# nested: codex writers also get build-cache access",
            "# reviewer excludes the author's vendor",
            "# oracle takes the hardest questions",
            "# council is a candidate pool, not a chain",
            "# librarian archives history",
            "# designer owns prompts/skills/templates",
            "# observer watches screenshots; must be multimodal",
            "# scout's only seat for now",
            "# critic's cross-vendor pair",
        ] {
            assert!(migrated.contains(comment), "lost comment: {comment:?}");
        }
        for new_key in [
            "[roles.nexus]",
            "[roles.scout]",
            "[roles.builder]",
            "[roles.critic]",
            "[roles.seer]",
            "[roles.council]",
            "[roles.archivist]",
            "[roles.artisan]",
            "[roles.watcher]",
        ] {
            assert!(
                migrated.contains(new_key),
                "missing renamed table: {new_key}"
            );
        }
        assert!(migrated.contains("builder = [\"claude-work\"]"));
        assert!(migrated.contains("critic = [\"codex-work\", \"grok\"]"));

        // A second, already-migrated run is a true no-op.
        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("already on current role keys"));
    }

    /// Finding 7: a project's `.multiverse/roster.toml` is migrated
    /// alongside the global config, with its own backup, not left
    /// behind on the retired role keys.
    #[test]
    fn backup_also_migrates_a_projects_roster_toml() {
        let dir = TempDir::new("roster");
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let roster_path = project_root.join(".multiverse").join("roster.toml");
        fs::write(
            &roster_path,
            "# roster\n[chains]\nfixer = [\"claude-work\"]\n",
        )
        .unwrap();

        let config = format!(
            "[roles.fixer]\nwrite_scope = \"declared\"\n\n[projects.example]\npath = \"{path}\"\n",
            path = project_root.display()
        );
        let path = write_config(dir.path(), &config);

        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("migrated 1 key(s)"));
        assert!(printed.contains(roster_path.to_str().unwrap()));

        let migrated_config = fs::read_to_string(&path).unwrap();
        assert!(migrated_config.contains("[roles.builder]"));

        let migrated_roster = fs::read_to_string(&roster_path).unwrap();
        assert!(migrated_roster.contains("builder = [\"claude-work\"]"));
        assert!(migrated_roster.contains("# roster"));

        let roster_backups: Vec<_> = fs::read_dir(roster_path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name != "roster.toml")
            .collect();
        assert_eq!(
            roster_backups.len(),
            1,
            "exactly one roster backup: {roster_backups:?}"
        );
        assert!(roster_backups[0].starts_with("roster.toml.bak-"));
    }

    /// A config whose own role keys are already current still migrates
    /// a roster that is not — the no-op check must not short-circuit
    /// before rosters are even looked at.
    #[test]
    fn a_current_config_with_a_stale_roster_still_migrates_the_roster() {
        let dir = TempDir::new("roster-only");
        let project_root = dir.path().join("example-project");
        fs::create_dir_all(project_root.join(".multiverse")).unwrap();
        let roster_path = project_root.join(".multiverse").join("roster.toml");
        fs::write(&roster_path, "[chains]\nfixer = [\"claude-work\"]\n").unwrap();

        let config = format!(
            "[roles.builder]\nwrite_scope = \"declared\"\n\n[projects.example]\npath = \"{path}\"\n",
            path = project_root.display()
        );
        let path = write_config(dir.path(), &config);

        let args = matches(&[
            "config",
            "migrate",
            "--config",
            path.to_str().unwrap(),
            "--backup",
        ]);
        let mut out = Vec::new();
        run(&args, &mut out).unwrap();
        assert!(!String::from_utf8(out)
            .unwrap()
            .contains("already on current role keys"));

        let migrated_roster = fs::read_to_string(&roster_path).unwrap();
        assert!(migrated_roster.contains("builder = [\"claude-work\"]"));
        // The global config had nothing to rename, so it keeps no backup.
        let config_backups = fs::read_dir(dir.path())
            .unwrap()
            .filter(|e| {
                let name = e.as_ref().unwrap().file_name();
                name != "config.toml" && name != "example-project"
            })
            .count();
        assert_eq!(config_backups, 0);
    }
}

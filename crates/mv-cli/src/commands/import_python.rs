//! `multiverse import-python`: copy a standalone Python install's config
//! and fleet state into this install's config and state directories —
//! never moving or deleting anything at the source. Exists for the one
//! case `config_state_dir` (`mv_config::load`) has no fallback for: a
//! Herdr-managed install, where `$HERDR_PLUGIN_CONFIG_DIR` and
//! `$HERDR_PLUGIN_STATE_DIR` are two directories Herdr created for this
//! plugin, neither of them the Python install's `$HOME/.config/multiverse`
//! (`docs/migrating-from-python.md`'s cutover runbook, Step 3).
//!
//! Destination split mirrors every reader in `mv-state`: `config.toml` and
//! `ntfy-topic` land next to each other in the config directory (Rust reads
//! `ntfy-topic` next to `config.toml`, not next to the other state files —
//! `commands::notify::resolve_topic`); `halt`, `directives/*.md`,
//! `breaker-state.json`, `gauge-state.json`, `seat-state.json`, and
//! `notify-state.json` land in the state directory
//! (`mv_state::halt`/`directives`/`breaker_store`/`gauge`/
//! `seatstate_store`/`notify_episodes`).
//!
//! `usage-cache.json`, `private-names.txt`, `shared/`, and any `*.lock`
//! sibling are left behind on purpose: nothing in `mv-state`/`mv-engine`
//! reads the first three today, and a lock file is advisory and
//! re-created on demand — the writing process's `flock` releases when it
//! exits, so a copied lock carries no stale lock state, only a stale,
//! meaningless file.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::ArgMatches;

use mv_core::atomic::{create_owner_read_write_only, write_atomic};

use super::doctor::{resolve_config_path, state_dir_from_config};
use crate::exit::Failure;

/// Copied verbatim into the config directory, alongside `config.toml`.
const CONFIG_DIR_FILES: &[&str] = &["config.toml", "ntfy-topic"];

/// Copied verbatim into the state directory.
const STATE_DIR_FILES: &[&str] = &[
    "halt",
    "breaker-state.json",
    "gauge-state.json",
    "seat-state.json",
    "notify-state.json",
];

/// Present in a Python install, read by nothing in this tree today —
/// left behind, not copied (this task's `result.md`).
const SKIPPED_NAMES: &[(&str, &str)] = &[
    ("usage-cache.json", "no Rust reader; left in place"),
    ("private-names.txt", "no Rust reader; left in place"),
    ("shared", "no Rust reader; left in place"),
];

enum PlanItem {
    Copy {
        source: PathBuf,
        dest: PathBuf,
    },
    Skip {
        source: PathBuf,
        reason: &'static str,
    },
}

/// Every candidate under `from`, in a fixed, deterministic order —
/// config-dir files, then state-dir files, then each `directives/*`
/// file sorted by name, then the names this command always leaves
/// behind, then any top-level `*.lock` sibling. Only entries that
/// actually exist at `from` appear at all: a Python install missing one
/// of these files (no `ntfy-topic` configured, say) gets no line for it,
/// the same way the explorer runbook's own `[ -f "$OLD/$f" ] && cp`
/// guard stays silent on a file that was never there.
fn build_plan(from: &Path, config_dir: &Path, state_dir: &Path) -> io::Result<Vec<PlanItem>> {
    let mut plan = Vec::new();

    for name in CONFIG_DIR_FILES {
        let source = from.join(name);
        if source.is_file() {
            plan.push(PlanItem::Copy {
                dest: config_dir.join(name),
                source,
            });
        }
    }

    for name in STATE_DIR_FILES {
        let source = from.join(name);
        if source.is_file() {
            plan.push(PlanItem::Copy {
                dest: state_dir.join(name),
                source,
            });
        }
    }

    let directives_source = from.join("directives");
    if directives_source.is_dir() {
        let dest_dir = state_dir.join("directives");
        let mut entries: Vec<PathBuf> = fs::read_dir(&directives_source)?
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| path.is_file())
            .collect();
        entries.sort();
        for source in entries {
            let Some(file_name) = source.file_name() else {
                continue;
            };
            plan.push(PlanItem::Copy {
                dest: dest_dir.join(file_name),
                source,
            });
        }
    }

    for (name, reason) in SKIPPED_NAMES {
        let source = from.join(name);
        if source.exists() {
            plan.push(PlanItem::Skip { source, reason });
        }
    }

    let mut locks: Vec<PathBuf> = fs::read_dir(from)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("lock"))
        .collect();
    locks.sort();
    for source in locks {
        plan.push(PlanItem::Skip {
            source,
            reason: "advisory lock; re-created on demand",
        });
    }

    Ok(plan)
}

/// `fs::create_dir_all`, then `chmod 0700` — idempotent, so calling this
/// on a directory a Herdr install already created just re-asserts the
/// mode the brief calls for rather than failing on `AlreadyExists`.
fn ensure_dir_0700(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Every `Copy` destination already present at `from`'s plan — the set
/// `--force` is required to cross. Checked once, up front, so a real
/// (non-`--dry-run`) run either copies everything or writes nothing: no
/// partial import left behind by the first conflict it happens to reach.
fn existing_destinations(plan: &[PlanItem]) -> Vec<&Path> {
    plan.iter()
        .filter_map(|item| match item {
            PlanItem::Copy { dest, .. } if dest.exists() => Some(dest.as_path()),
            _ => None,
        })
        .collect()
}

fn run_plan(
    plan: &[PlanItem],
    dry_run: bool,
    force: bool,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    if !dry_run && !force {
        let conflicts = existing_destinations(plan);
        if !conflicts.is_empty() {
            return Err(Failure::Operation(format!(
                "import-python refuses to overwrite existing file(s): {}; re-run with --force \
                 to overwrite, or move them aside first",
                conflicts
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }

    for item in plan {
        match item {
            PlanItem::Copy { source, dest } => {
                let exists = dest.exists();
                if dry_run {
                    let verb = match (exists, force) {
                        (false, _) => "would copy",
                        (true, true) => "would overwrite",
                        (true, false) => "would refuse (exists; needs --force)",
                    };
                    let _ = writeln!(out, "{verb}: {} -> {}", source.display(), dest.display());
                    continue;
                }
                if let Some(parent) = dest.parent() {
                    ensure_dir_0700(parent).map_err(|error| {
                        Failure::Operation(format!(
                            "could not create {}: {error}",
                            parent.display()
                        ))
                    })?;
                }
                let contents = fs::read(source).map_err(|error| {
                    Failure::Operation(format!("could not read {}: {error}", source.display()))
                })?;
                if exists {
                    write_atomic(dest, &contents).map_err(|error| {
                        Failure::Operation(format!("could not write {}: {error}", dest.display()))
                    })?;
                    let _ = writeln!(
                        out,
                        "copied (overwrote): {} -> {}",
                        source.display(),
                        dest.display()
                    );
                } else {
                    create_owner_read_write_only(dest, &contents).map_err(|error| {
                        Failure::Operation(format!("could not write {}: {error}", dest.display()))
                    })?;
                    let _ = writeln!(out, "copied: {} -> {}", source.display(), dest.display());
                }
            }
            PlanItem::Skip { source, reason } => {
                let _ = writeln!(out, "skipped: {} ({reason})", source.display());
            }
        }
    }
    Ok(())
}

pub(crate) fn run(args: &ArgMatches, out: &mut dyn Write) -> Result<(), Failure> {
    let dry_run = args.get_flag("dry-run");
    let force = args.get_flag("force");
    let from = args
        .get_one::<String>("from")
        .map(PathBuf::from)
        .unwrap_or_else(|| mv_config::load::home_dir_from_env().join(".config/multiverse"));

    if !from.is_dir() {
        return Err(Failure::Operation(format!(
            "import-python: source directory not found: {}",
            from.display()
        )));
    }

    let (config_path, _label) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let config_dir = config_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let state_dir = state_dir_from_config(&config_path);

    let plan = build_plan(&from, &config_dir, &state_dir).map_err(|error| {
        Failure::Operation(format!("could not read {}: {error}", from.display()))
    })?;

    run_plan(&plan, dry_run, force, out)?;

    let _ = writeln!(out, "config dir: {}", config_dir.display());
    let _ = writeln!(out, "state dir: {}", state_dir.display());
    let _ = writeln!(
        out,
        "Next: run `multiverse config migrate --dry-run`, then `multiverse config migrate --backup`."
    );
    Ok(())
}

pub(crate) fn run_default(args: &ArgMatches) -> Result<(), Failure> {
    run(args, &mut std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap()
            .subcommand_matches("import-python")
            .unwrap()
            .clone()
    }

    struct Layout {
        _dir: tempfile::TempDir,
        from: PathBuf,
        config_dir: PathBuf,
        state_dir: PathBuf,
    }

    fn layout() -> Layout {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        let config_dir = dir.path().join("config");
        let state_dir = dir.path().join("state");
        fs::create_dir_all(&from).unwrap();
        fs::create_dir_all(&config_dir).unwrap();
        Layout {
            _dir: dir,
            from,
            config_dir,
            state_dir,
        }
    }

    fn run_import(layout: &Layout, extra: &[&str]) -> Result<String, Failure> {
        let config_path = layout.config_dir.join("config.toml");
        let mut argv = vec![
            "import-python",
            "--from",
            layout.from.to_str().unwrap(),
            "--config",
            config_path.to_str().unwrap(),
        ];
        argv.extend_from_slice(extra);
        std::env::set_var("MULTIVERSE_STATE_DIR", &layout.state_dir);
        let args = matches(&argv);
        let mut out = Vec::new();
        let result = run(&args, &mut out);
        std::env::remove_var("MULTIVERSE_STATE_DIR");
        result.map(|()| String::from_utf8(out).unwrap())
    }

    fn write(path: &Path, contents: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    /// Serializes every test in this module against the process-wide
    /// `MULTIVERSE_STATE_DIR` env var — the one shared lock
    /// `commands::worktrees::herdr_env` already provides for exactly this
    /// (`commands::accounts`'s own `MULTIVERSE_READ_ONLY` tests reuse it
    /// the same way): `cargo test`'s default threaded runner shares one
    /// process, so an unguarded `set_var`/`remove_var` pair here would
    /// race any other test mutating process-wide env state concurrently.
    fn serialized() -> std::sync::MutexGuard<'static, ()> {
        crate::commands::worktrees::herdr_env::lock()
    }

    #[test]
    fn copies_config_and_state_files_to_their_own_directories() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");
        write(&fixture.from.join("ntfy-topic"), "super-secret-topic");
        write(&fixture.from.join("halt"), "{\"halted\": true}");
        write(&fixture.from.join("breaker-state.json"), "{}");
        write(&fixture.from.join("gauge-state.json"), "{}");
        write(&fixture.from.join("seat-state.json"), "{}");
        write(&fixture.from.join("notify-state.json"), "{}");
        write(&fixture.from.join("directives/one.md"), "note one");
        write(&fixture.from.join("directives/two.md"), "note two");

        let output = run_import(&fixture, &[]).unwrap();

        assert_eq!(
            fs::read_to_string(fixture.config_dir.join("config.toml")).unwrap(),
            "[projects.a]\n"
        );
        assert_eq!(
            fs::read_to_string(fixture.config_dir.join("ntfy-topic")).unwrap(),
            "super-secret-topic"
        );
        for name in [
            "halt",
            "breaker-state.json",
            "gauge-state.json",
            "seat-state.json",
            "notify-state.json",
        ] {
            assert!(
                fixture.state_dir.join(name).is_file(),
                "missing {name} in state dir"
            );
        }
        assert_eq!(
            fs::read_to_string(fixture.state_dir.join("directives/one.md")).unwrap(),
            "note one"
        );
        assert_eq!(
            fs::read_to_string(fixture.state_dir.join("directives/two.md")).unwrap(),
            "note two"
        );

        // Source is untouched — every write above is additive, never a move.
        assert!(fixture.from.join("config.toml").is_file());
        assert!(fixture.from.join("halt").is_file());

        assert!(output.contains(&format!("config dir: {}", fixture.config_dir.display())));
        assert!(output.contains(&format!("state dir: {}", fixture.state_dir.display())));
        assert!(output.contains("Next: run `multiverse config migrate"));
    }

    #[test]
    fn never_prints_the_ntfy_topic_secret_contents() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");
        write(&fixture.from.join("ntfy-topic"), "do-not-print-me-12345");

        let output = run_import(&fixture, &[]).unwrap();

        assert!(!output.contains("do-not-print-me-12345"), "{output}");
        let dry_run_output = {
            // A second pass, `--dry-run`, against a fresh destination —
            // must also never print the secret's contents.
            let fixture = layout();
            write(&fixture.from.join("ntfy-topic"), "do-not-print-me-12345");
            run_import(&fixture, &["--dry-run"]).unwrap()
        };
        assert!(
            !dry_run_output.contains("do-not-print-me-12345"),
            "{dry_run_output}"
        );
    }

    #[test]
    fn skips_files_with_no_rust_reader_and_lock_siblings() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");
        write(&fixture.from.join("usage-cache.json"), "{}");
        write(&fixture.from.join("private-names.txt"), "name\n");
        write(&fixture.from.join("shared/note.md"), "shared note");
        write(&fixture.from.join("breaker-state.json.lock"), "");
        write(&fixture.from.join("dispatch.lock"), "");

        let output = run_import(&fixture, &[]).unwrap();

        assert!(!fixture.config_dir.join("usage-cache.json").exists());
        assert!(!fixture.state_dir.join("usage-cache.json").exists());
        assert!(!fixture.state_dir.join("private-names.txt").exists());
        assert!(!fixture.state_dir.join("shared").exists());
        assert!(!fixture.state_dir.join("breaker-state.json.lock").exists());
        assert!(!fixture.state_dir.join("dispatch.lock").exists());
        assert!(output.contains("skipped: ") && output.contains("usage-cache.json"));
        assert!(output.contains("private-names.txt"));
        assert!(output.contains("dispatch.lock"));
    }

    #[test]
    fn dry_run_lists_the_plan_and_writes_nothing() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");
        write(&fixture.from.join("halt"), "{}");

        let output = run_import(&fixture, &["--dry-run"]).unwrap();

        assert!(output.contains("would copy"), "{output}");
        assert!(!fixture.config_dir.join("config.toml").exists());
        assert!(!fixture.state_dir.exists());
    }

    #[test]
    fn refuses_to_overwrite_an_existing_target_without_force() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "new contents\n");
        write(&fixture.config_dir.join("config.toml"), "old contents\n");

        let error = run_import(&fixture, &[]).unwrap_err();
        assert_eq!(error.code(), 1);
        assert!(error.to_string().contains("config.toml"));
        assert_eq!(
            fs::read_to_string(fixture.config_dir.join("config.toml")).unwrap(),
            "old contents\n",
            "refused import must not touch the existing file"
        );
    }

    #[test]
    fn a_conflict_refuses_the_whole_run_not_just_the_one_file() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "new config\n");
        write(&fixture.from.join("halt"), "new halt");
        write(&fixture.config_dir.join("config.toml"), "old config\n");

        run_import(&fixture, &[]).unwrap_err();

        assert!(
            !fixture.state_dir.join("halt").exists(),
            "an unrelated file must not be copied once the plan is refused"
        );
    }

    #[test]
    fn force_overwrites_an_existing_target() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "new contents\n");
        write(&fixture.config_dir.join("config.toml"), "old contents\n");

        run_import(&fixture, &["--force"]).unwrap();

        assert_eq!(
            fs::read_to_string(fixture.config_dir.join("config.toml")).unwrap(),
            "new contents\n"
        );
    }

    #[test]
    fn files_are_mode_0600_and_directories_are_mode_0700() {
        use std::os::unix::fs::PermissionsExt;

        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");
        write(&fixture.from.join("directives/one.md"), "note");

        run_import(&fixture, &[]).unwrap();

        let file_mode = fs::metadata(fixture.config_dir.join("config.toml"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(file_mode & 0o777, 0o600);
        let dir_mode = fs::metadata(fixture.state_dir.join("directives"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700);
    }

    #[test]
    fn a_missing_source_directory_is_a_reported_error_not_a_panic() {
        let _guard = serialized();
        let fixture = layout();
        fs::remove_dir_all(&fixture.from).unwrap();

        let error = run_import(&fixture, &[]).unwrap_err();
        assert_eq!(error.code(), 1);
        assert!(error.to_string().contains("source directory not found"));
    }

    #[test]
    fn missing_optional_source_files_are_silently_omitted() {
        let _guard = serialized();
        let fixture = layout();
        write(&fixture.from.join("config.toml"), "[projects.a]\n");

        let output = run_import(&fixture, &[]).unwrap();

        assert!(!output.contains("ntfy-topic"));
        assert!(!output.contains("halt"));
        assert!(!fixture.config_dir.join("ntfy-topic").exists());
    }
}

//! `multiverse new <name> [--path PATH] [--yes] [--dry-run]`: create,
//! register, and start a project — in-process `add` then `start`, matching
//! spec-cli.md §2's description of the Python original ("`new`... calls
//! `cmd_add`/`cmd_start` directly (in-process)").
//!
//! Git-init/commit steps shell directly to `git`, matching spec-cli.md §4
//! ("`git init -q -b main`, `git add .gitignore`, `git commit -q -m
//! \"Initial commit\"` ... `new` only") — this is `new`'s own documented
//! behavior in the Python original, not a layering violation the way a
//! direct Herdr call from `mv-cli` would be.
//!
//! `.gitignore`'s exact contents and the Nexus-start step's real seed/pane
//! behavior have no pinned fixture; see `result.md` for what is asserted
//! versus best-effort.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use super::add::{canonicalize_project_path, register};
use super::doctor::{project_path, resolve_config_path};
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::Failure;

/// Same fix as `start.rs`'s `print_config_load_error` (card
/// `20261102-b-5c41bb`): this file owns only this call site, so it keeps
/// its own copy rather than a cross-file shared helper.
fn print_config_load_error(message: &str) {
    println!("config error: {message}");
    if is_retired_role_key_error(message) {
        println!("{MIGRATE_FIX_LINE}");
    } else {
        println!("Run `multiverse doctor` for a full readiness check.");
    }
}

const COMMIT_IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "multiverse"),
    ("GIT_AUTHOR_EMAIL", "multiverse@localhost"),
    ("GIT_COMMITTER_NAME", "multiverse"),
    ("GIT_COMMITTER_EMAIL", "multiverse@localhost"),
];

fn run_git(path: &Path, args: &[&str]) -> Result<(), Failure> {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .envs(COMMIT_IDENTITY)
        .status()
        .map_err(|error| Failure::Operation(format!("could not run git: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(Failure::Operation(format!(
            "git {} failed in {}",
            args.join(" "),
            path.display()
        )))
    }
}

/// A leftover directory `new` may resume rather than refuse, per
/// spec-cli.md §7 (commit `9509779`): "`new` refused to resume a directory
/// holding only its own half-finished `git init`+`.gitignore`... as 'not
/// empty'." A resumable leftover has, at most, `.git` and `.gitignore`.
fn is_resumable_leftover(path: &Path) -> std::io::Result<bool> {
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        if name != ".git" && name != ".gitignore" {
            return Ok(false);
        }
    }
    Ok(true)
}

fn has_commits(path: &Path) -> bool {
    std::process::Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--verify",
            "-q",
            "HEAD",
        ])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let name = args.get_one::<String>("name").expect("name is required");
    let dry_run = args.get_flag("dry-run");
    let yes = args.get_flag("yes");

    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        println!(
            "config error: no config file found at {}\nRun `multiverse doctor` for a full readiness check.",
            config_path.display()
        );
        return Err(Failure::Reported(crate::exit::NEEDS_CONFIG));
    }
    let config = mv_config::load::load_config(&config_path).map_err(|error| {
        print_config_load_error(&error.to_string());
        Failure::Reported(crate::exit::NEEDS_CONFIG)
    })?;

    let target: PathBuf = match args.get_one::<String>("path") {
        Some(path) => PathBuf::from(path),
        None => mv_config::load::home_dir_from_env().join("Code").join(name),
    };

    let already_registered = config
        .projects
        .values()
        .any(|project| project_path(project).map(|p| p == target).unwrap_or(false));

    if target.exists() && !already_registered {
        let resumable = is_resumable_leftover(&target).map_err(|error| {
            Failure::Operation(format!("could not inspect {}: {error}", target.display()))
        })?;
        if !resumable {
            return Err(Failure::Operation(format!(
                "{} is not empty and is not registered as a project",
                target.display()
            )));
        }
    }

    if dry_run {
        println!("Plan for new project '{name}' at {}:", target.display());
        println!(
            "1. mkdir -p {} (if it does not already exist)",
            target.display()
        );
        println!("2. git init -q -b main (if not already a repository)");
        println!("3. write .gitignore and commit it (if there are no commits yet)");
        if already_registered {
            println!("4. start '{name}' (already registered)");
        } else {
            println!(
                "4. register project '{name}' at {} in config.toml",
                target.display()
            );
            println!("5. start '{name}'");
        }
        return Ok(());
    }
    if !yes {
        return Err(Failure::Operation(
            "new requires --yes to execute; use --dry-run to preview".to_string(),
        ));
    }

    std::fs::create_dir_all(&target).map_err(|error| {
        Failure::Operation(format!("could not create {}: {error}", target.display()))
    })?;

    if !target.join(".git").is_dir() {
        run_git(&target, &["init", "-q", "-b", "main"])?;
    }
    let gitignore = target.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "/target/\n")
            .map_err(|error| Failure::Operation(format!("could not write .gitignore: {error}")))?;
    }
    if !has_commits(&target) {
        run_git(&target, &["add", ".gitignore"])?;
        run_git(&target, &["commit", "-q", "-m", "Initial commit"])?;
    }

    let canonical = canonicalize_project_path(&target.to_string_lossy())?;
    if !already_registered {
        register(&config_path, &config, &canonical, name, false)?;
    }

    match super::start::start_one_by_path(name, &canonical, false) {
        Ok(message) => println!("{message}"),
        Err(message) => {
            println!("note: {name} was created and registered, but did not start: {message}")
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resumable_leftover_allows_git_and_gitignore_only() {
        let dir =
            std::env::temp_dir().join(format!("mv-new-test-resumable-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".gitignore"), "x\n").unwrap();
        assert!(is_resumable_leftover(&dir).unwrap());
        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        assert!(!is_resumable_leftover(&dir).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }
}

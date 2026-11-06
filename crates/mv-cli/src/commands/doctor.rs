//! `multiverse doctor`: installation-prerequisite checklist (exit 0 pass,
//! 1 fail). Shells directly to `git`, `$SHELL`, and the `herdr` binary
//! itself — per architecture.md's crate table, doctor "call\[s\] raw
//! `git`/`$SHELL`/`herdr --version` directly" and "should stay a thin
//! process-probing module, not folded into `herdr_client`" — so this file
//! never reaches for the `mv-herdr` crate.
//!
//! Config loading goes through the real `mv_config::load` (merged in from
//! `main` partway through this task, after an earlier version of this file
//! had to hand-roll a line-oriented stand-in because that module was still
//! an empty Wave-0 stub — see `result.md` for this task for that history).
//! `resolve_config_path`/`state_dir_from_config` below are thin
//! wrappers kept in this crate only to add the `(resolved via ...)` source
//! label doctor's own checklist line wants, which `mv_config::load` itself
//! does not track.

use std::path::{Path, PathBuf};

use clap::ArgMatches;

use mv_config::load::Project;

use crate::exit::{Failure, ERROR};

/// The floor `architecture.md` names for the Herdr plugin contract
/// (`min_herdr_version = "0.9.0"`).
const MIN_HERDR_VERSION: (u32, u32, u32) = (0, 9, 0);

/// Where `config.toml` lives, and the label doctor/check print next to it.
///
/// `explicit` is the global `--config PATH` flag (`args.rs`'s
/// `.arg(value("config", ...).global(true))`), when the caller passed one.
/// Delegates the actual precedence/normalization to
/// `mv_config::load::resolve_config_path_from_env` — "the one entry point
/// in this module mv-cli should call for an ordinary run", per that
/// function's own doc comment — and only adds the source label doctor's
/// checklist prints next to the path.
///
/// Honoring `--config` is not just fidelity to the Python original: a
/// handler that fell back to the real `~/.config/multiverse/config.toml`
/// whenever a test omitted it would read and write this machine's live
/// fleet state during `cargo test` — see `result.md` for this task, which
/// hit exactly that before this function honored `--config`.
pub(crate) fn resolve_config_path(
    explicit: Option<&str>,
) -> Result<(PathBuf, &'static str), Failure> {
    let cli_override = explicit.filter(|value| !value.trim().is_empty());
    let label = if cli_override.is_some() {
        "--config"
    } else if std::env::var("MULTIVERSE_CONFIG").is_ok_and(|value| !value.trim().is_empty()) {
        "$MULTIVERSE_CONFIG"
    } else {
        "default location"
    };
    let path = mv_config::load::resolve_config_path_from_env(cli_override.map(Path::new)).map_err(
        |error| Failure::Operation(format!("could not resolve the config path: {error}")),
    )?;
    Ok((path, label))
}

/// The fleet state directory for an already-resolved `config_path`
/// (`mv_config::load::config_state_dir_from_env`) — also where this task's
/// own [`super::halt`] keeps its halt marker, pending `mv-state` landing a
/// real fleet-state-dir consumer for it.
pub(crate) fn state_dir_from_config(config_path: &Path) -> PathBuf {
    mv_config::load::config_state_dir_from_env(config_path)
}

/// Resolve the token or path used to invoke Herdr: `$HERDR_BIN_PATH` when
/// set and non-blank, else the bare `herdr` token. Duplicates
/// `mv_herdr::argv::herdr_binary`'s exact semantics — `mv-cli` does not
/// depend on `mv-herdr` (see the crate DAG in architecture.md §1: Herdr
/// access is routed through `mv-engine`, never straight from `mv-cli`),
/// and doctor's own probes are the one documented exception that talks to
/// the `herdr` binary directly rather than through that layer.
///
/// Test builds never resolve to a real binary, full stop — a `cargo test`
/// run must never shell out to this machine's actual `herdr` install (see
/// `result.md` for this task: `args.rs`'s blanket invocation test calls
/// `doctor::run` end to end, and a dev machine running the live fleet this
/// task was dispatched from has a real `herdr` on `PATH`; an earlier
/// version of this code reached it, read-only but for real, before this
/// guard existed). A future test that wants a scripted fake `herdr` needs
/// its own seam, not `$HERDR_BIN_PATH`, while this guard stands.
#[cfg(not(test))]
fn herdr_binary() -> String {
    match std::env::var("HERDR_BIN_PATH") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "herdr".to_string(),
    }
}

#[cfg(test)]
fn herdr_binary() -> String {
    "mv-cli-test-herdr-stub-should-never-exist".to_string()
}

fn run_herdr(binary: &str, args: &[&str]) -> Option<(i32, String)> {
    let output = std::process::Command::new(binary)
        .args(args)
        .output()
        .ok()?;
    Some((
        output.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

fn parse_herdr_version(stdout: &str) -> Option<(u32, u32, u32)> {
    let token = stdout
        .split_whitespace()
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    // Semver core is MAJOR.MINOR.PATCH; strip +build then -prerelease so a
    // token like `0.9.5-beta` or `0.9.5+exp.sha` still yields (0, 9, 5).
    let numeric = token.split_once('+').map_or(token, |(core, _)| core);
    let numeric = numeric.split_once('-').map_or(numeric, |(core, _)| core);
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor, patch))
}

/// Whether `path` is inside a git work tree, via `git -C <path> rev-parse
/// --is-inside-work-tree` (spec-cli.md §4: the same probe doctor's Python
/// original runs).
pub(crate) fn git_is_repo(path: &Path) -> bool {
    std::process::Command::new("git")
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--is-inside-work-tree",
        ])
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
        })
        .unwrap_or(false)
}

fn git_config_get(key: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["config", "--get", key])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|value| !value.is_empty())
}

/// `git config user.name`/`user.email`, read from the process's current
/// directory (not a configured project) — the same way the conformance
/// fixture's single "git user configured" line is satisfied by the repo
/// `multiverse doctor` happens to be invoked from.
fn git_user_configured() -> bool {
    git_config_get("user.name").is_some() && git_config_get("user.email").is_some()
}

/// Days-since-epoch to a proleptic-Gregorian (year, month, day), Howard
/// Hinnant's `civil_from_days`. No external time crate is a declared
/// `mv-cli` dependency (`mv-core::time` exists on `main` now, per
/// `mv-core-partial.patch`, but `mv-cli` does not depend on `mv-core`'s
/// `time` module directly — only `mv-config`/`mv-engine`/`mv-board` are in
/// its `Cargo.toml` — so [`utc_timestamp`] below keeps its own small
/// calendar math rather than adding that dependency for one function).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// `now`, formatted the way every writer in this fleet stamps a record:
/// UTC, second precision, a numeric `+00:00` offset, never `Z`
/// (`mv-core-partial.patch`'s `time::format_seconds_utc`, mirroring
/// `datetime.now(timezone.utc).isoformat(timespec="seconds")`).
pub(crate) fn utc_timestamp() -> String {
    let total_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    )
}

/// A project's configured path, or `None` while it is still a
/// `<<PLACEHOLDER: ...>>`.
pub(crate) fn project_path(project: &Project) -> Option<&Path> {
    project.path.value().map(Path::new)
}

/// `multiverse doctor`: no arguments, exit 0 pass / 1 fail.
///
/// Matches `conformance/scenarios/doctor-missing-accounts.json` after
/// normalization. The harness drops Python's successful interpreter check:
/// it describes the oracle's host runtime, which this compiled CLI does
/// not require.
pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, source_label) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let state_dir = state_dir_from_config(&config_path);
    let mut lines: Vec<String> = Vec::new();
    let mut issues = 0usize;

    lines.push(mv_state::halt::format_halt_line(
        &mv_state::halt::read_halt_state(&state_dir),
    ));

    let config_exists = config_path.is_file();
    if config_exists {
        lines.push(format!(
            "\u{2713} config file found: {} (resolved via {source_label})",
            config_path.display()
        ));
    } else {
        lines.push(format!(
            "\u{2717} config file found: {} (resolved via {source_label})",
            config_path.display()
        ));
        lines.push(
            "  Fix: Create a config file (see config.example.toml) or point $MULTIVERSE_CONFIG at one."
                .to_string(),
        );
        issues += 1;
    }
    // Not a pass/fail check — just makes the fleet state directory
    // discoverable, which otherwise has no `herdr plugin config-dir`-style
    // lookup of its own under a Herdr-managed install (this task's
    // `result.md`: the cutover runbook's gap item 3).
    lines.push(format!("state directory: {}", state_dir.display()));
    let config = if config_exists {
        match mv_config::load::load_config(&config_path) {
            Ok(config) => Some(config),
            Err(error) => {
                lines.push(format!("\u{2717} config parses: {error}"));
                lines.push("  Fix: Fix the error above and re-run doctor.".to_string());
                issues += 1;
                None
            }
        }
    } else {
        None
    };

    let binary = herdr_binary();
    let config_dir_ok = run_herdr(&binary, &["plugin", "config-dir", "multiverse"])
        .is_some_and(|(code, _)| code == 0);
    let version_probe = run_herdr(&binary, &["--version"]);
    match version_probe {
        Some((0, stdout)) if config_dir_ok => match parse_herdr_version(&stdout) {
            Some(version) if version >= MIN_HERDR_VERSION => {
                lines.push(format!(
                    "\u{2713} herdr {}.{}.{}, minimum {}.{}.{}",
                    version.0,
                    version.1,
                    version.2,
                    MIN_HERDR_VERSION.0,
                    MIN_HERDR_VERSION.1,
                    MIN_HERDR_VERSION.2
                ));
            }
            Some(version) => {
                lines.push(format!(
                    "\u{2717} herdr {}.{}.{}, minimum {}.{}.{}",
                    version.0,
                    version.1,
                    version.2,
                    MIN_HERDR_VERSION.0,
                    MIN_HERDR_VERSION.1,
                    MIN_HERDR_VERSION.2
                ));
                lines.push(
                    "  Fix: Upgrade herdr to at least the minimum version above.".to_string(),
                );
                issues += 1;
            }
            None => {
                lines.push("\u{2717} herdr reachable".to_string());
                lines.push("  Fix: Could not parse `herdr --version` output.".to_string());
                issues += 1;
            }
        },
        _ => {
            lines.push("\u{2717} herdr reachable".to_string());
            lines.push(
                "  Fix: Install herdr and ensure it is on PATH, or set $HERDR_BIN_PATH."
                    .to_string(),
            );
            issues += 1;
        }
    }

    let accounts = config.as_ref().map_or(0, |config| config.accounts.len());
    if accounts == 0 {
        lines.push("\u{2717} accounts configured".to_string());
        lines.push("  Fix: Add at least one [accounts.<name>] table to the config.".to_string());
        issues += 1;
    } else {
        lines.push("\u{2713} accounts configured".to_string());
    }

    if let Some(config) = &config {
        let mut projects: Vec<&Project> = config.projects.values().collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        for project in projects {
            match project_path(project) {
                Some(path) if path.exists() => {
                    lines.push(format!(
                        "\u{2713} project {} path exists: {}",
                        project.name,
                        path.display()
                    ));
                    if git_is_repo(path) {
                        lines.push(format!("\u{2713} project {} git repo", project.name));
                    } else {
                        lines.push(format!("\u{2717} project {} git repo", project.name));
                        lines.push(format!("  Fix: Run `git init` in {}.", path.display()));
                        issues += 1;
                    }
                }
                Some(path) => {
                    lines.push(format!(
                        "\u{2717} project {} path exists: {}",
                        project.name,
                        path.display()
                    ));
                    lines.push(
                        "  Fix: Create the project directory, or correct its path in the config."
                            .to_string(),
                    );
                    issues += 1;
                }
                None => {
                    lines.push(format!(
                        "\u{2717} project {} path exists: {}",
                        project.name,
                        project.path.as_raw()
                    ));
                    lines.push(
                        "  Fix: Replace the placeholder path with the project's real location."
                            .to_string(),
                    );
                    issues += 1;
                }
            }
        }
    }

    if git_user_configured() {
        lines.push("\u{2713} git user configured".to_string());
    } else {
        lines.push("\u{2717} git user configured".to_string());
        lines.push("  Fix: Set `git config user.name` and `git config user.email`.".to_string());
        issues += 1;
    }

    let footer = if issues == 0 {
        "All checks passed.".to_string()
    } else {
        format!("Doctor found {issues} issue(s).")
    };
    println!("{}\n\n{footer}", lines.join("\n"));

    if issues == 0 {
        Ok(())
    } else {
        Err(Failure::Reported(ERROR))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(1), (1970, 1, 2));
        // 2026-11-02 is 20,759 days after the epoch.
        assert_eq!(civil_from_days(20_759), (2026, 11, 2));
    }

    #[test]
    fn utc_timestamp_has_the_fleet_shape() {
        let stamp = utc_timestamp();
        assert!(
            regex_shape(&stamp),
            "{stamp:?} does not look like YYYY-MM-DDTHH:MM:SS+00:00"
        );
    }

    fn regex_shape(value: &str) -> bool {
        let bytes = value.as_bytes();
        bytes.len() == 25
            && value.ends_with("+00:00")
            && value.as_bytes()[4] == b'-'
            && value.as_bytes()[7] == b'-'
            && value.as_bytes()[10] == b'T'
    }

    #[test]
    fn parse_herdr_version_reads_the_dotted_token() {
        assert_eq!(parse_herdr_version("herdr 0.9.3\n"), Some((0, 9, 3)));
        assert_eq!(parse_herdr_version("not a version"), None);
    }

    #[test]
    fn parse_herdr_version_strips_prerelease_and_build_suffixes() {
        assert_eq!(parse_herdr_version("herdr 0.9.5-beta\n"), Some((0, 9, 5)));
        assert_eq!(parse_herdr_version("herdr 0.9.0-rc1\n"), Some((0, 9, 0)));
        assert_eq!(parse_herdr_version("herdr 0.8.9-rc1\n"), Some((0, 8, 9)));
        assert_eq!(
            parse_herdr_version("herdr 0.9.5+build.1\n"),
            Some((0, 9, 5))
        );
        assert_eq!(
            parse_herdr_version("herdr 0.9.5-beta.1+exp.sha.5114f85\n"),
            Some((0, 9, 5))
        );
    }

    #[test]
    fn project_path_is_none_for_a_placeholder() {
        let content = "[projects.example]\npath = \"<<PLACEHOLDER: path>>\"\n";
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        let project = &config.projects["example"];
        assert_eq!(project_path(project), None);
    }

    #[test]
    fn project_path_is_some_for_a_real_value() {
        // `load_config` validates that a non-placeholder project path
        // exists on disk (`validate_project_paths_and_shorts`), so this
        // needs a real directory, not an arbitrary string.
        let content = "[projects.example]\npath = \"/tmp\"\n";
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        let project = &config.projects["example"];
        assert_eq!(project_path(project), Some(Path::new("/tmp")));
    }
}

//! `multiverse directives set|show`: standing project directives layered
//! onto the Nexus seed.
//!
//! The actual read/write/parse logic already lives in
//! `mv_engine::directives` (another lane's module, fully self-contained —
//! no `mv-herdr`/`mv-state` types in its public API); this file is only
//! the clap-to-library adapter plus project resolution, matching the
//! `--config`/project-selection conventions `doctor.rs`/`stop.rs` already
//! established.

use std::io::IsTerminal;
use std::path::Path;
use std::time::SystemTime;

use clap::ArgMatches;
use mv_config::load::Project;
use mv_engine::directives::{self, DirectivesError};

use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use crate::exit::Failure;

fn directives_error_message(project: &str, error: &DirectivesError) -> String {
    format!("{project}: {error}")
}

/// Projects named by positional args (by configured name or by path),
/// defaulting to the current project, or all projects outside a configured root.
fn select_projects<'a>(projects: &'a [&'a Project], requested: &[String]) -> Vec<&'a Project> {
    if !requested.is_empty() {
        return projects
            .iter()
            .copied()
            .filter(|project| {
                requested.iter().any(|token| {
                    token == &project.name
                        || project_path(project).is_some_and(|path| Path::new(token) == path)
                })
            })
            .collect();
    }
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let selected: Vec<_> = projects
        .iter()
        .copied()
        .filter(|project| {
            project_path(project).is_some_and(|path| cwd == path || cwd.starts_with(path))
        })
        .collect();
    if selected.is_empty() {
        projects.to_vec()
    } else {
        selected
    }
}

fn run_set(state_dir: &Path, selected: &[&Project], sub: &ArgMatches) -> Result<(), Failure> {
    let body = sub.get_one::<String>("body").expect("clap requires --body");
    let until = sub.get_one::<String>("until").map(String::as_str);
    let root = directives::directives_root(state_dir);

    let mut succeeded = 0usize;
    let mut failed = 0usize;
    for project in selected {
        match directives::write(&root, &project.name, body, until) {
            Ok(_) => {
                println!("{}: directive set", project.name);
                succeeded += 1;
            }
            Err(error) => {
                eprintln!("{}", directives_error_message(&project.name, &error));
                failed += 1;
            }
        }
    }
    if succeeded == 0 && failed > 0 {
        Err(Failure::Operation(
            "directives set: no directive was written".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn run_show(state_dir: &Path, selected: &[&Project]) -> Result<(), Failure> {
    let root = directives::directives_root(state_dir);
    let now = SystemTime::now();
    let mut failed = 0usize;
    for project in selected {
        match directives::read(&root, &project.name) {
            Ok(Some(directive)) => print!("{}", show_directive(&directive, now)),
            Ok(None) => println!("{}: no standing directives.", project.name),
            Err(error) => {
                eprintln!("{}", directives_error_message(&project.name, &error));
                failed += 1;
            }
        }
    }
    if failed > 0 && failed == selected.len() {
        Err(Failure::Operation(
            "directives show: could not read any directive".to_string(),
        ))
    } else {
        Ok(())
    }
}

/// Day-granularity age, matching Python's `(now - written_at).days` used
/// for the directive's "(Nd ago)" text (card `20261102-b-5e76db`): whole
/// days elapsed, floored, never negative (a clock skew that puts
/// `written_at` after `now` reads as "just now" rather than underflowing).
fn days_ago(now: SystemTime, written_at: SystemTime) -> u64 {
    now.duration_since(written_at).unwrap_or_default().as_secs() / 86_400
}

fn show_directive(directive: &directives::Directive, now: SystemTime) -> String {
    let status = if directive.is_expired(now) {
        "expired"
    } else {
        "active"
    };
    let until = directive.until.map_or_else(
        || "no expiry".to_string(),
        |until| format!("until {}", mv_core::time::format_seconds_utc(until.into())),
    );
    let written = mv_core::time::format_seconds_utc(directive.written_at.into());
    let age = days_ago(now, directive.written_at);
    let mut text = format!(
        "{}: {status}, {until}\n  written {written} ({age}d ago)\n  {}\n",
        directive.project,
        directive.path.display()
    );
    for line in directive.body.lines() {
        text.push_str(&format!("  {line}\n"));
    }
    text
}

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    // An operator's own terminal is required before loading config or writing.
    if args.subcommand_name() == Some("set")
        && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        return Err(Failure::Operation(
            "directives set requires an interactive terminal; run it from your own terminal".into(),
        ));
    }
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        return Err(Failure::Config(format!(
            "no config file found at {}; run `multiverse doctor`",
            config_path.display()
        )));
    }
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;
    let state_dir = state_dir_from_config(&config_path);

    let (sub_name, sub) = args
        .subcommand()
        .expect("clap requires a directives subcommand");
    let requested: Vec<String> = sub
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let all_projects: Vec<&Project> = config.projects.values().collect();
    let selected = select_projects(&all_projects, &requested);
    if selected.is_empty() {
        return Err(Failure::Operation(
            "directives: no matching project".to_string(),
        ));
    }

    match sub_name {
        "set" => run_set(&state_dir, &selected, sub),
        "show" => run_show(&state_dir, &selected),
        other => Err(Failure::Operation(format!(
            "directives: unknown subcommand {other}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::placeholder::MaybePlaceholder;
    use std::collections::BTreeMap;

    fn project(name: &str, path: &str) -> Project {
        Project {
            name: name.to_string(),
            path: MaybePlaceholder::parse(path),
            chains: BTreeMap::new(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        }
    }

    #[test]
    fn explicit_name_or_path_selects_a_project() {
        let a = project("a", "/tmp/a");
        let b = project("b", "/tmp/b");
        let projects = vec![&a, &b];
        assert_eq!(select_projects(&projects, &["b".to_string()])[0].name, "b");
        assert_eq!(
            select_projects(&projects, &["/tmp/a".to_string()])[0].name,
            "a"
        );
    }

    #[test]
    fn empty_request_outside_any_project_selects_all() {
        let a = project("a", "/does/not/exist/anywhere");
        let projects = vec![&a];
        assert_eq!(select_projects(&projects, &[])[0].name, "a");
    }

    /// Card 20261102-b-5e76db: `show_directive` never printed when the
    /// directive was written, even though `Directive::written_at` was
    /// already read off disk — Python's `cmd_directives` always prints a
    /// "written <timestamp> (Nd ago)" line as the second line, before the
    /// path.
    #[test]
    fn show_directive_prints_a_written_line_with_day_granularity_age() {
        let written_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let now = written_at + std::time::Duration::from_secs(4 * 86_400 + 3_600);
        let directive = directives::Directive {
            project: "demo".to_string(),
            body: "Continue history.".to_string(),
            until: None,
            path: std::path::PathBuf::from("/tmp/demo.md"),
            written_at,
        };
        let text = show_directive(&directive, now);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "demo: active, no expiry");
        assert_eq!(
            lines.next().unwrap(),
            format!(
                "  written {} (4d ago)",
                mv_core::time::format_seconds_utc(written_at.into())
            )
        );
        assert_eq!(lines.next().unwrap(), "  /tmp/demo.md");
        assert_eq!(lines.next().unwrap(), "  Continue history.");
    }

    #[test]
    fn days_ago_floors_and_never_underflows_on_clock_skew() {
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000);
        let written_at =
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000 + 10 * 86_400);
        assert_eq!(days_ago(now, now), 0);
        assert_eq!(
            days_ago(now + std::time::Duration::from_secs(2 * 86_400 + 1), now),
            2
        );
        assert_eq!(
            days_ago(now, written_at),
            0,
            "written_at after now must not underflow"
        );
    }
}

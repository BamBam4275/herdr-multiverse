//! `multiverse projects [--names]`: list configured projects. Spec-cli.md
//! §5: exit codes "OK only" for the happy path; a genuine config-load
//! failure still needs to surface, so this maps that one case to
//! `EXIT_NEEDS_CONFIG`, consistent with every other config-boundary error
//! in this port (architecture.md §5).

use clap::ArgMatches;
use mv_config::load::Project;

use super::doctor::{project_path, resolve_config_path};
use crate::exit::Failure;

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
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

    let mut projects: Vec<&Project> = config.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));

    if args.get_flag("names") {
        for project in projects {
            println!("{}", project.name);
        }
        return Ok(());
    }

    // Aligned by char count, not true terminal display width (`unicode-width`
    // is not a declared `mv-cli` dependency) — see result.md for this gap;
    // spec-cli.md §7 (commit 4dc61bb) requires display-width alignment for
    // full parity, not byte/char-count alignment.
    let width = projects
        .iter()
        .map(|project| project.name.chars().count())
        .max()
        .unwrap_or(0);
    for project in &projects {
        let location = match project_path(project) {
            Some(path) => path.display().to_string(),
            None => project.path.as_raw().to_string(),
        };
        println!("{:width$}  {location}", project.name);
    }
    Ok(())
}

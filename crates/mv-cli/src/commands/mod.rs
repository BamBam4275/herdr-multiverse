//! Flat command dispatch. Every registered command below is wired to a
//! real handler backed by `mv-state`/`mv-engine`; none remain stubs.

use std::path::{Path, PathBuf};

mod council;
mod dispatch;
mod migrate;

use clap::ArgMatches;

use crate::{args::WORKER_ROLES, exit::Failure};

mod accounts;
mod activity;
mod add;
mod backlog;
mod board;
mod board_data;
mod check;
mod directives;
mod doctor;
mod halt;
mod health;
mod import_python;
mod inspect;
mod lifecycle;
mod new;
mod notify;
mod presence;
mod projects;
mod result;
mod review_policy;
mod scratch;
mod seat;
mod start;
mod stop;
mod sweep;
mod tasks;
/// `pub(crate)`, not private: `args.rs`'s own blanket-invocation test reaches
/// this module's `herdr_env` lock across the `commands`/`args` module
/// boundary (task `20261105-fixer-e1b6cc`).
pub(crate) mod worktrees;

fn config_override(args: &ArgMatches) -> Option<PathBuf> {
    args.get_one::<String>("config").map(PathBuf::from)
}

/// Resolve and load `config.toml` the way `mv-config`'s own doc comment
/// says `mv-cli` should (`resolve_config_path_from_env` is "the one entry
/// point in this module mv-cli should call for an ordinary run").
pub(crate) fn load_config(args: &ArgMatches) -> Result<mv_config::load::Config, Failure> {
    let override_path = config_override(args);
    let path = mv_config::load::resolve_config_path_from_env(override_path.as_deref())
        .map_err(|e| Failure::Config(format!("cannot resolve config path: {e}")))?;
    mv_config::load::load_config(&path).map_err(|e| Failure::Config(e.to_string()))
}

/// Whether a `mv-config` refusal is the one `load_config` raises for a
/// Python-era role key (`[roles.fixer]`, `fixer = [...]`) that predates
/// the role rename and has never been migrated (card `20261102-b-4a5e3c`).
/// Matched by message content, not a structured kind: `ConfigError` (see
/// its own module docs) carries one formatted string on purpose, and
/// every one of `mv-config`'s retired-key refusals already names "retired
/// role key" verbatim (`crates/mv-config/src/load.rs`'s `roles_allowed`,
/// `chains`, and `[roles.<key>]` refusals).
pub(crate) fn is_retired_role_key_error(message: &str) -> bool {
    message.contains("retired role key")
}

/// The one line every retired-role-key refusal should end with, so an
/// operator on an unmigrated Python-era config sees the fix instead of
/// having to parse it out of the raw `ConfigError` text themselves.
pub(crate) const MIGRATE_FIX_LINE: &str =
    "run: multiverse config migrate --dry-run, then multiverse config migrate";

#[cfg(test)]
mod config_error_mapping_tests {
    use super::*;

    #[test]
    fn retired_role_key_messages_are_detected() {
        assert!(is_retired_role_key_error(
            "roles.fixer: \"fixer\" is a retired role key; run the config migrator \
             (`multiverse config migrate`) to rewrite it before loading this file"
        ));
        assert!(is_retired_role_key_error(
            "project.chains: \"explorer\" is a retired role key; run the config migrator \
             (`multiverse config migrate`) before adding new chains"
        ));
    }

    #[test]
    fn an_unrelated_config_error_is_not_mistaken_for_one() {
        assert!(!is_retired_role_key_error("malformed TOML: oops"));
        assert!(!is_retired_role_key_error("no such project: demo"));
    }

    /// Scout finding `20261117-scout-9077fc`#1: `load_config` used to map
    /// an unresolvable config *path* to `Failure::Operation` (exit 1),
    /// unlike `migrate.rs`/`review_policy.rs`'s identical failure, which
    /// already used `Failure::Config` (exit 3) — `exit.rs`'s own
    /// `NEEDS_CONFIG` doc places "bad config" here. Reached by the
    /// `.cargo/config.toml`-forced `MULTIVERSE_CONFIG` sentinel with no
    /// `--config` override, the same mechanism `mv-config`'s own
    /// `resolve_config_path_from_env_refuses_the_forced_sentinel_when_unguarded`
    /// test uses, so no cwd manipulation is needed. Revert-checked: this
    /// failed (`code 1`) while the mapping above was still `Operation`.
    #[test]
    fn an_unresolvable_config_path_is_needs_config_not_operation() {
        use crate::commands::worktrees::herdr_env::HerdrEnvGuard;

        struct ForbiddenConfigEnvGuard {
            _herdr: HerdrEnvGuard,
            previous: Option<std::ffi::OsString>,
        }
        impl ForbiddenConfigEnvGuard {
            fn set() -> Self {
                let herdr = HerdrEnvGuard::unreachable();
                let previous = std::env::var_os("MULTIVERSE_CONFIG");
                std::env::set_var(
                    "MULTIVERSE_CONFIG",
                    mv_config::load::FORBIDDEN_LIVE_CONFIG_SENTINEL,
                );
                Self {
                    _herdr: herdr,
                    previous,
                }
            }
        }
        impl Drop for ForbiddenConfigEnvGuard {
            fn drop(&mut self) {
                match self.previous.take() {
                    Some(value) => std::env::set_var("MULTIVERSE_CONFIG", value),
                    None => std::env::remove_var("MULTIVERSE_CONFIG"),
                }
            }
        }

        let _guard = ForbiddenConfigEnvGuard::set();
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "projects"])
            .unwrap();
        let err = load_config(matches.subcommand_matches("projects").unwrap()).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err.to_string().contains("cannot resolve config path"));
    }
}

#[cfg(test)]
mod project_for_path_tests {
    use super::*;
    use std::fs;

    fn config_with_nested_projects(root: &Path) -> mv_config::load::Config {
        let parent = root.join("parent");
        let child = parent.join("child");
        fs::create_dir_all(child.join(".multiverse").join("worktrees").join("lane")).unwrap();
        let toml = format!(
            "[projects.parent]\npath = {:?}\n\n[projects.child]\npath = {:?}\n",
            parent.to_str().unwrap(),
            child.to_str().unwrap(),
        );
        mv_config::load::load_config_str(&toml, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn a_lane_worktree_resolves_to_the_containing_project() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_nested_projects(tmp.path());
        let lane = tmp
            .path()
            .join("parent")
            .join("child")
            .join(".multiverse")
            .join("worktrees")
            .join("lane");
        let canonical = fs::canonicalize(&lane).unwrap();
        let (name, root) = project_for_path(&config, &canonical).expect("worktree is inside child");
        assert_eq!(name, "child");
        assert_eq!(
            root,
            fs::canonicalize(tmp.path().join("parent").join("child")).unwrap()
        );
    }

    #[test]
    fn a_nested_project_beats_its_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_nested_projects(tmp.path());
        let child = fs::canonicalize(tmp.path().join("parent").join("child")).unwrap();
        let (name, root) = project_for_path(&config, &child).expect("child root matches");
        assert_eq!(name, "child");
        assert_eq!(root, child);
    }

    #[test]
    fn a_path_outside_every_project_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_with_nested_projects(tmp.path());
        let outside = tmp.path().join("elsewhere");
        fs::create_dir_all(&outside).unwrap();
        let canonical = fs::canonicalize(&outside).unwrap();
        assert!(project_for_path(&config, &canonical).is_none());
    }
}

/// Walk up from the current directory looking for `.multiverse/`, the same
/// way `git` finds a repository root.
///
/// No Rust crate yet resolves "the current project" from the configured
/// project list and cwd — `spec-tasks-worktrees.md` §7 says "project scope
/// is inferred from cwd" but the resolver that would do that has not
/// landed in `mv-config` or `mv-engine`. This is the bounded, read-only,
/// dependency-free stand-in commands in this lane use instead; see this
/// task's `result.md` for the gap.
///
/// Refuses outright when [`config_available`] is true for `args`: a loaded
/// config is authoritative for "which project is this" (via
/// [`current_project`]), and a `.multiverse/` this walk finds instead is
/// never one that config vouched for — most often a dev lane's sandbox
/// config nested inside a real project's checkout, where the walk would
/// otherwise find the real project's own `.multiverse/` one level up and
/// hand it to a caller's write path (the dev/use hazard task
/// `20261106-fixer-dabb0d` fixed at its one call site; folded in here so no
/// future caller can reintroduce it by forgetting the same guard). Only
/// when no config loads at all does the walk run, same as before.
pub(crate) fn current_project_root(args: &ArgMatches) -> Result<PathBuf, Failure> {
    if config_available(args) {
        return Err(Failure::Config(
            "a project config loaded; resolve the current project through it instead of \
             walking cwd for an unvouched .multiverse/"
                .into(),
        ));
    }
    let cwd = std::env::current_dir()
        .map_err(|e| Failure::Operation(format!("cannot read current directory: {e}")))?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".multiverse").is_dir() {
            return Ok(dir.to_path_buf());
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => {
                return Err(Failure::Config(
                    "no project found above the current directory (no .multiverse/ found); \
                     run this from inside a registered project"
                        .into(),
                ))
            }
        }
    }
}

/// A configured project's root, by name, from an already-loaded config.
pub(crate) fn project_root_by_name(
    config: &mv_config::load::Config,
    name: &str,
) -> Result<PathBuf, Failure> {
    let project = config
        .projects
        .get(name)
        .ok_or_else(|| Failure::Config(format!("unknown project {name:?}")))?;
    let value = project.path.value().ok_or_else(|| {
        Failure::Config(format!("project {name:?} has an unfilled path placeholder"))
    })?;
    Ok(PathBuf::from(value))
}

/// Python's `Config.project_for_path`: longest-prefix containment.
/// Every `[projects.<name>]` whose canonical `path` is `target` or an
/// ancestor of it is a candidate; the deepest one wins, so a nested
/// project beats its parent and a lane worktree resolves to the project
/// that contains it.
pub(crate) fn project_for_path<'c>(
    config: &'c mv_config::load::Config,
    target: &Path,
) -> Option<(&'c str, PathBuf)> {
    let mut best: Option<(&str, PathBuf)> = None;
    for (name, project) in &config.projects {
        let Some(raw) = project.path.value() else {
            continue;
        };
        let Ok(root) = std::fs::canonicalize(raw) else {
            continue;
        };
        if target == root || target.starts_with(&root) {
            let is_deeper = best
                .as_ref()
                .is_none_or(|(_, prior)| root.components().count() > prior.components().count());
            if is_deeper {
                best = Some((name.as_str(), root));
            }
        }
    }
    best
}

/// The configured project that contains the current directory, after
/// canonicalization — the current project's registered *name* (for
/// routing-log `from`/`to` fields) alongside its configured root.
///
/// Deliberately matches against cwd directly rather than going through
/// [`current_project_root`]: that helper requires an existing
/// `.multiverse/` directory, which is exactly what a project's *first*
/// write (its first `activity send`, its first `backlog add`) has not
/// created yet. The config registry is authoritative for "which project
/// is this" regardless of whether anything has been written there.
pub(crate) fn current_project(args: &ArgMatches) -> Result<(String, PathBuf), Failure> {
    let cwd = std::env::current_dir()
        .map_err(|e| Failure::Operation(format!("cannot read current directory: {e}")))?;
    let canonical_cwd = std::fs::canonicalize(&cwd)
        .map_err(|e| Failure::Operation(format!("cannot resolve current directory: {e}")))?;
    let config = load_config(args)?;
    match project_for_path(&config, &canonical_cwd) {
        Some((name, root)) => Ok((name.to_owned(), root)),
        None => Err(Failure::Config(format!(
            "{} is not inside any configured project; configured: {}",
            canonical_cwd.display(),
            config
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Whether `load_config` resolves to a real, loadable config for `args` —
/// independent of whether cwd lands inside any project that config
/// registers. [`current_project_root`] refuses outright when this is true,
/// so a caller falling back to it after `current_project` fails no longer
/// needs to check this itself first (task `20261106-fixer-dabb0d` guarded
/// its one call site by hand; folded into `current_project_root` itself by
/// card `20261106-b-fd2d3e` so a future caller cannot forget it). Kept
/// `pub(crate)` for callers that need the distinction on its own terms.
pub(crate) fn config_available(args: &ArgMatches) -> bool {
    load_config(args).is_ok()
}

/// Run one Herdr command against `binary` (`argv[0]` overridden, the same
/// way `mv_state::worktrees::agent_list_snapshot` pins its own binary)
/// through `mv_herdr::run::run`. Cards `20261119-b-c1bb46` (board) and
/// `20261119-b-5f7cf6` (workers, doctor): a raw `std::process::Command`
/// spawn skipped that runner's bounded retry on `ExecutableFileBusy`, so
/// a Linux `execve` that raced another process's write fd on the binary —
/// a concurrent test thread's fake Herdr, or a real install replaced
/// mid-upgrade — read as an unreachable Herdr on the first attempt.
pub(crate) fn run_herdr(
    mut command: mv_engine::mv_herdr::argv::HerdrCommand,
    binary: &str,
) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::mv_herdr::error::HerdrError> {
    binary.clone_into(&mut command.argv[0]);
    mv_engine::mv_herdr::run::run(&command, None)
}

/// A Herdr command with no dedicated `mv_herdr::argv` builder: `args`
/// after a placeholder `argv[0]` that [`run_herdr`] replaces. `is_herdr`
/// is set so the runner still applies its bounded default timeout, as it
/// does for every builder-made command.
pub(crate) fn herdr_command<S: AsRef<str>>(args: &[S]) -> mv_engine::mv_herdr::argv::HerdrCommand {
    let mut argv = vec![String::new()];
    argv.extend(args.iter().map(|arg| arg.as_ref().to_string()));
    mv_engine::mv_herdr::argv::HerdrCommand {
        argv,
        is_herdr: true,
        ..Default::default()
    }
}
mod workers;

/// Checks the `--yes`/`--dry-run` gate `dispatch` and `council` share
/// before `run` reaches their handlers; every other command is a no-op
/// here. Called separately from `run` itself (see `main.rs`'s
/// `MissingRequiredArgument` handling) so the authorization refusal still
/// surfaces even when a required positional is missing.
pub fn authorization(command: &str, args: &ArgMatches) -> Result<(), Failure> {
    let needs_yes = matches!(command, "dispatch" | "council")
        && !args.get_flag("yes")
        && !args.get_flag("dry-run");
    if !needs_yes {
        return Ok(());
    }
    // `council` seats the real council and prints its roster before
    // refusing (see `council::refuse_without_yes`'s own doc comment);
    // `dispatch` keeps the plain generic refusal below.
    if command == "council" {
        return council::refuse_without_yes(args);
    }
    Err(Failure::Operation(format!(
        "{command} requires --yes to execute; use --dry-run to preview"
    )))
}

fn validate(args: &ArgMatches) -> Result<(), Failure> {
    if let Ok(Some(role)) = args.try_get_one::<String>("role") {
        if !WORKER_ROLES.contains(&role.as_str()) {
            return Err(Failure::UnknownRole(format!(
                "unknown role {role:?}; expected one of: {}",
                WORKER_ROLES.join(", ")
            )));
        }
    }
    for (name, allow_zero) in [("timeout", false), ("wait-for-seat-seconds", true)] {
        if let Ok(Some(raw)) = args.try_get_one::<String>(name) {
            let valid = raw
                .parse::<f64>()
                .is_ok_and(|n| n.is_finite() && if allow_zero { n >= 0.0 } else { n > 0.0 });
            if !valid {
                let constraint = if allow_zero {
                    "non-negative"
                } else {
                    "positive"
                };
                return Err(Failure::Operation(format!(
                    "--{name} must be a finite, {constraint} number of seconds"
                )));
            }
        }
    }
    if let Some((_, nested)) = args.subcommand() {
        validate(nested)?;
    }
    Ok(())
}

/// The one routing table from a clap subcommand name to its handler.
/// Validates shared argument constraints and the `--yes`/`--dry-run` gate
/// first, then dispatches — every registered command is wired to a real
/// handler, so reaching the final `unreachable!()` means clap accepted a
/// name this table does not know, which is a bug in the two staying in
/// sync, not a runtime condition.
pub fn run(matches: &ArgMatches) -> Result<(), Failure> {
    let (name, args) = matches.subcommand().expect("clap requires a subcommand");
    validate(args)?;
    authorization(name, args)?;
    // Keep the complete routing table visible here, like Python's handlers dict.
    match name {
        "dispatch" => dispatch::run_default(args),
        "halt" => halt::run(args),
        "scratch" => scratch::run(args),
        "start" => start::run(args),
        "down" => lifecycle::run_down(args),
        "up" => lifecycle::run_up(args),
        "projects" => projects::run(args),
        "stop" => stop::run(args),
        "add" => add::run(args),
        "new" => new::run(args),
        "tasks" => tasks::run(args),
        "board" => board::run(args),
        "council" => council::run_default(args),
        "accounts" => accounts::run(args),
        "seat" => seat::run(args),
        "worktrees" => worktrees::run(args),
        "check" => check::run(args),
        "doctor" => doctor::run(args),
        "import-python" => import_python::run_default(args),
        "presence" => presence::run(args),
        "health" => health::run(args),
        "review-policy" => review_policy::run(args),
        "sweep" => sweep::run(args),
        "result" => result::run(args),
        "activity" => activity::run(args),
        "backlog" => backlog::run(args),
        "notify" => notify::run(args),
        "inspect" => inspect::run(args),
        "directives" => directives::run(args),
        "workers" => workers::run(args),
        "config" => migrate::run_default(args),
        _ => unreachable!("clap only accepts commands in the routing table"),
    }
}

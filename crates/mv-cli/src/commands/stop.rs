//! `multiverse stop [projects...] [--force]`: stop project sessions,
//! refusing while an agent works unless `--force` overrides it. Omitted
//! project names select every configured project, regardless of cwd.
//!
//! The busy check keeps its own hand-rolled JSON reader, predating this
//! task, that shells straight to the `herdr` binary for `workspace list`,
//! `pane list`, and `agent list` rather than going through `mv-herdr` — a
//! workspace is this project's when its `label` equals the project's
//! configured name; a pane is this project's when its `foreground_cwd` is
//! the project's path or a descendant of it (compared as plain strings);
//! an agent counts against this project when its
//! `workspace_id` matches and, whenever a relevant pane was found at all,
//! its `pane_id` is one of them. Left as-is rather than rewritten against
//! `mv_engine::fleet_start::FleetState` (which `commands::start` now
//! uses): it is already tested, and this task's scope is the real `stop`,
//! not a refactor of a working busy check.
//!
//! Closing a workspace for real (`mv_engine::fleet_start::close_workspace`,
//! the same Herdr-facing module `commands::start` uses) is new in this
//! task. Ported from Python's `cli.cmd_stop`:
//!
//! - §5 / commit `08687f7`: refusing a busy workspace without `--force`
//!   must return `EXIT_ERROR`, never `EXIT_OK`.
//! - §7 / commit `4dc61bb`: an agent in an *unknown* status must be
//!   refused too, not only a known-busy one.
//!
//! Lanes with unrescued task output are always kept, including with
//! `--force`: closing their panes would destroy the account of work that
//! has not reached project state. The read-only artifact check lives in
//! `mv_state::tasks::lanes_holding_unrescued_output` and fails closed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use clap::ArgMatches;
use mv_config::load::Project;

use super::doctor::{project_path, resolve_config_path};
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::{Failure, ERROR, NEEDS_CONFIG};

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

/// An agent's liveness, as `stop` needs to reason about it: idle/done are
/// safe to stop; anything else — working, blocked, or genuinely unknown —
/// is busy and refused without `--force`.
pub(crate) struct LiveAgent {
    pub status: String,
    pub pane_id: String,
    /// `None` for an agent Herdr detected but never named — Python's
    /// `agent.get('name') or '(unnamed)'` in the `--force` announcement.
    pub name: Option<String>,
}

impl LiveAgent {
    fn is_safe_to_stop(&self) -> bool {
        matches!(self.status.as_str(), "idle" | "done")
    }
}

trait Fleet {
    /// `name`'s open workspace id, when one matches, and every agent
    /// Herdr reports as live inside it. `None` for the workspace id means
    /// no open workspace matched this project at all — distinct from an
    /// open, agent-less workspace (`Some(id)` with an empty `Vec`).
    fn agents_for(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<(Option<String>, Vec<LiveAgent>), String>;
    /// Close a workspace for real — the destructive half of `stop`.
    fn stop_project(&self, workspace_id: &str) -> Result<(), String>;
}

// --- a minimal, read-only JSON reader ----------------------------------
// Just enough to walk `{"result": {"<plural>": [{"key": "value", ...}, ...]}}`
// and pull out each record's string fields — not a general-purpose parser
// (no numbers/bools/nesting), since nothing here needs one.

/// The `[...]` body (brackets excluded) of the array named `"<plural>"`,
/// tracking bracket depth and quoted strings so a value that happens to
/// contain `[`/`]` cannot miscount.
fn find_array<'a>(stdout: &'a str, plural: &str) -> Option<&'a str> {
    let needle = format!("\"{plural}\"");
    let after_key = stdout.find(&needle)? + needle.len();
    let colon = after_key + stdout[after_key..].find(':')?;
    let open = colon + stdout[colon..].find('[')?;
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in stdout[open..].bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&stdout[open + 1..open + offset]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Every top-level `{...}` record inside an array body, the same
/// depth/quote tracking as [`find_array`].
fn split_records(array_body: &str) -> Vec<&str> {
    let mut records = Vec::new();
    let mut depth = 0i32;
    let mut start = None;
    let mut in_string = false;
    let mut escaped = false;
    for (offset, byte) in array_body.bytes().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = Some(offset);
                }
                depth += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(s) = start {
                        records.push(&array_body[s..=offset]);
                    }
                }
            }
            _ => {}
        }
    }
    records
}

/// One record's string fields, ignoring any non-string value — every
/// field this file reads (`label`, `workspace_id`, `pane_id`,
/// `foreground_cwd`, `agent_status`) is one.
fn record_fields(record: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let mut rest = record;
    while let Some(key_start) = rest.find('"') {
        rest = &rest[key_start + 1..];
        let Some(key_end) = rest.find('"') else {
            break;
        };
        let key = rest[..key_end].to_string();
        rest = &rest[key_end + 1..];
        let Some(colon) = rest.find(':') else {
            break;
        };
        let after_colon = rest[colon + 1..].trim_start();
        if let Some(value_rest) = after_colon.strip_prefix('"') {
            if let Some(value_end) = value_rest.find('"') {
                fields.insert(key, value_rest[..value_end].to_string());
                rest = &value_rest[value_end + 1..];
                continue;
            }
        }
        let Some(next) = after_colon.find([',', '}']) else {
            break;
        };
        rest = &after_colon[next..];
    }
    fields
}

/// `$HERDR_BIN_PATH` when set and non-blank, else the bare `herdr` token —
/// identical semantics to `doctor::herdr_binary`/`workers::herdr_binary`,
/// duplicated locally since those are private to their own files and this
/// task's write scope does not include editing them to share it.
#[cfg(not(test))]
fn herdr_binary() -> String {
    match std::env::var("HERDR_BIN_PATH") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "herdr".to_string(),
    }
}

/// Never resolves to a real binary under `cargo test`, for the same reason
/// `doctor.rs`/`workers.rs` isolate their own test builds this way.
#[cfg(test)]
fn herdr_binary() -> String {
    "mv-cli-test-herdr-stub-should-never-exist".to_string()
}

/// One `herdr <noun> list` call, parsed down to each record's string
/// fields. `None` means "could not get a trustworthy reply" (spawn
/// failure, nonzero exit, or the expected `"result"."<plural>"` array
/// missing) — the caller surfaces that as its own refusal rather than
/// guessing at liveness.
fn fetch_records(binary: &str, noun: &str, plural: &str) -> Option<Vec<BTreeMap<String, String>>> {
    let output = std::process::Command::new(binary)
        .args([noun, "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let body = find_array(&stdout, plural)?;
    Some(split_records(body).into_iter().map(record_fields).collect())
}

/// Queries real Herdr for `name`'s workspace, filters its panes down to
/// the ones under `path`, then reports every live agent in that
/// workspace (narrowed to those panes when any were found). Closing a
/// workspace for real goes through `mv_engine::fleet_start`, which is
/// also how `commands::start` reaches Herdr — this file keeps its own
/// hand-rolled JSON reader for the busy check (see the module doc for
/// why), but the one call that actually destroys something is not worth
/// a second, divergent implementation.
struct HerdrFleet {
    binary: String,
}

impl Fleet for HerdrFleet {
    fn agents_for(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<(Option<String>, Vec<LiveAgent>), String> {
        let workspaces = fetch_records(&self.binary, "workspace", "workspaces")
            .ok_or_else(|| format!("{name}: could not reach Herdr (workspace list)"))?;
        // Fetched unconditionally, even when no workspace below ends up
        // matching: `cli.cmd_stop` reads workspaces and panes as one batch
        // before it ever asks whether either project matched, so a stop on
        // an already-closed project still makes the same two calls Python
        // does.
        let panes = fetch_records(&self.binary, "pane", "panes")
            .ok_or_else(|| format!("{name}: could not reach Herdr (pane list)"))?;
        let Some(workspace_id) = workspaces
            .iter()
            .find(|workspace| workspace.get("label").map(String::as_str) == Some(name))
            .and_then(|workspace| workspace.get("workspace_id"))
            .cloned()
        else {
            return Ok((None, Vec::new()));
        };

        let relevant_panes: BTreeSet<String> = panes
            .iter()
            .filter(|pane| {
                pane.get("workspace_id").map(String::as_str) == Some(workspace_id.as_str())
            })
            .filter(|pane| {
                pane.get("foreground_cwd")
                    .map(|cwd| {
                        let cwd = Path::new(cwd);
                        cwd == path || cwd.starts_with(path)
                    })
                    .unwrap_or(false)
            })
            .filter_map(|pane| pane.get("pane_id").cloned())
            .collect();

        let agents = fetch_records(&self.binary, "agent", "agents")
            .ok_or_else(|| format!("{name}: could not reach Herdr (agent list)"))?;
        let agents = agents
            .into_iter()
            .filter(|agent| {
                agent.get("workspace_id").map(String::as_str) == Some(workspace_id.as_str())
            })
            .filter(|agent| {
                relevant_panes.is_empty()
                    || agent
                        .get("pane_id")
                        .is_some_and(|pane_id| relevant_panes.contains(pane_id))
            })
            .filter_map(|agent| {
                Some(LiveAgent {
                    status: agent.get("agent_status")?.clone(),
                    pane_id: agent.get("pane_id")?.clone(),
                    name: agent.get("name").cloned(),
                })
            })
            .collect();
        Ok((Some(workspace_id), agents))
    }

    fn stop_project(&self, workspace_id: &str) -> Result<(), String> {
        let mut herdr = mv_engine::runner::ProcessHerdr;
        mv_engine::fleet_start::close_workspace(&mut herdr, workspace_id)
    }
}

fn select_projects<'a>(projects: &'a [&'a Project], requested: &[String]) -> Vec<&'a Project> {
    projects
        .iter()
        .copied()
        .filter(|project| {
            requested.is_empty() || requested.iter().any(|name| name == &project.name)
        })
        .collect()
}

/// `  <name>` left-padded to this width before the KEPT/CLOSED verdict —
/// matches the column `stop-refusal-agent-not-settled.json` pins.
const LABEL_WIDTH: usize = 21;

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    if !config_path.is_file() {
        println!(
            "config error: no config file found at {}\nRun `multiverse doctor` for a full readiness check.",
            config_path.display()
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }
    let config = mv_config::load::load_config(&config_path).map_err(|error| {
        print_config_load_error(&error.to_string());
        Failure::Reported(NEEDS_CONFIG)
    })?;

    let force = args.get_flag("force");
    let requested: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();

    let all_projects: Vec<&Project> = config.projects.values().collect();
    let selected = select_projects(&all_projects, &requested);
    if selected.is_empty() {
        return Err(Failure::Operation("stop: no matching project".to_string()));
    }

    let fleet = HerdrFleet {
        binary: herdr_binary(),
    };
    // Only a project with an actually-open workspace counts toward
    // `total`/the loop below — Python's `open_spaces` filter runs before
    // `cmd_stop`'s own loop, so a selected-but-never-opened project is
    // silently absent from both, not reported as kept or failed.
    let mut open: Vec<(&Project, String, Vec<LiveAgent>)> = Vec::new();
    let mut path_failures = 0usize;
    for project in &selected {
        let Some(path) = project_path(project) else {
            println!(
                "  {:<width$}KEPT \u{2014} path is not configured",
                project.name,
                width = LABEL_WIDTH
            );
            path_failures += 1;
            continue;
        };
        match fleet.agents_for(&project.name, path) {
            Ok((Some(workspace_id), agents)) => open.push((project, workspace_id, agents)),
            Ok((None, _)) => {}
            Err(message) => {
                println!(
                    "  {:<width$}KEPT \u{2014} {message}",
                    project.name,
                    width = LABEL_WIDTH
                );
                path_failures += 1;
            }
        }
    }
    if open.is_empty() {
        if path_failures > 0 {
            return Err(Failure::Reported(ERROR));
        }
        println!("no project workspaces are open");
        return Ok(());
    }

    let total = open.len();
    let mut closed = 0usize;
    let mut refused = false;

    for (project, workspace_id, agents) in &open {
        let busy: Vec<&LiveAgent> = agents
            .iter()
            .filter(|agent| !agent.is_safe_to_stop())
            .collect();
        let held = match mv_state::tasks::lanes_holding_unrescued_output(
            project_path(project).expect("open project has a configured path"),
        ) {
            Ok(held) => held,
            Err(error) => {
                println!(
                    "  {:<width$}KEPT — could not check lane output: {error}",
                    project.name,
                    width = LABEL_WIDTH
                );
                refused = true;
                continue;
            }
        };
        let mut reasons: Vec<String> = if force {
            Vec::new()
        } else {
            busy.iter()
                .map(|agent| format!("{} agent in {}", agent.status, agent.pane_id))
                .collect()
        };
        reasons.extend(
            held.iter()
                .map(|lane| format!("lane {lane} holds unrescued output")),
        );
        if !reasons.is_empty() {
            println!(
                "  {:<width$}KEPT \u{2014} {}",
                project.name,
                reasons.join("; "),
                width = LABEL_WIDTH
            );
            refused = true;
            continue;
        }
        if !busy.is_empty() {
            // Named before it is destroyed — a `--force` that just does
            // it is a footgun with a flag on it.
            for agent in &busy {
                println!(
                    "  {:<width$}forcing: ending {} agent {}",
                    project.name,
                    agent.status,
                    agent.name.as_deref().unwrap_or("(unnamed)"),
                    width = LABEL_WIDTH
                );
            }
        }

        match fleet.stop_project(workspace_id) {
            Ok(()) => {
                println!(
                    "  {:<width$}closed ({workspace_id})",
                    project.name,
                    width = LABEL_WIDTH
                );
                closed += 1;
            }
            Err(message) => {
                println!(
                    "  {:<width$}FAILED \u{2014} {message}",
                    project.name,
                    width = LABEL_WIDTH
                );
            }
        }
    }

    println!();
    println!("closed {closed} of {total}");
    if closed == total && !refused && path_failures == 0 {
        Ok(())
    } else {
        Err(Failure::Reported(ERROR))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::placeholder::MaybePlaceholder;
    use std::collections::BTreeMap as Map;

    fn project(name: &str, path: &str) -> Project {
        Project {
            name: name.to_string(),
            path: MaybePlaceholder::parse(path),
            chains: Map::new(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        }
    }

    fn agent(status: &str) -> LiveAgent {
        LiveAgent {
            status: status.to_string(),
            pane_id: "w1:p1".to_string(),
            name: Some("test-agent".to_string()),
        }
    }

    #[test]
    fn idle_and_done_are_safe_to_stop() {
        for status in ["idle", "done"] {
            assert!(agent(status).is_safe_to_stop());
        }
    }

    #[test]
    fn working_blocked_and_unknown_are_not_safe_to_stop() {
        for status in ["working", "blocked", "unknown", ""] {
            assert!(!agent(status).is_safe_to_stop());
        }
    }

    #[test]
    fn select_projects_defaults_to_the_fleet_independent_of_cwd() {
        let a = project("a", "/does/not/exist/anywhere");
        let b = project("b", "/another/missing/project");
        let projects = vec![&a, &b];
        let selected = select_projects(&projects, &[]);
        assert_eq!(
            selected.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn explicit_project_names_limit_stop_to_the_requested_projects() {
        let a = project("a", "/project/a");
        let b = project("b", "/project/b");
        let projects = vec![&a, &b];
        let selected = select_projects(&projects, &["b".into()]);
        assert_eq!(
            selected.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["b"]
        );
        assert!(select_projects(&projects, &["missing".into()]).is_empty());
    }

    #[test]
    fn find_array_respects_bracket_depth_and_quoted_values() {
        let stdout =
            r#"{"result": {"panes": [{"pane_id": "w1:p1", "foreground_cwd": "/tmp/[weird]"}]}}"#;
        let body = find_array(stdout, "panes").unwrap();
        let records = split_records(body);
        assert_eq!(records.len(), 1);
        let fields = record_fields(records[0]);
        assert_eq!(fields.get("pane_id").map(String::as_str), Some("w1:p1"));
        assert_eq!(
            fields.get("foreground_cwd").map(String::as_str),
            Some("/tmp/[weird]")
        );
    }

    #[test]
    fn record_fields_skips_non_string_values() {
        let fields = record_fields(r#"{"pane_id": "w1:p1", "exit_code": 0, "ok": true}"#);
        assert_eq!(fields.get("pane_id").map(String::as_str), Some("w1:p1"));
        assert_eq!(fields.len(), 1);
    }
}

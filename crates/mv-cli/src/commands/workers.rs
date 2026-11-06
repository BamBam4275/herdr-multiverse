//! `multiverse workers`: one live listing joined to project routing history.
//! Task metadata supplies project, task, seat, role, and age; occupancy follows
//! the live sessions, including idle/done sessions. Unreachable Herdr renders
//! unknown counts and still exits successfully. Historical role names remain
//! readable while current live role labels use the current vocabulary.

use std::path::Path;

use clap::ArgMatches;
use mv_config::load::Config;
use mv_config::roles::Role;

use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use crate::exit::Failure;

// --- a minimal, read-only JSON reader (object/array/string/number/bool/null) ---
// Just enough to walk `{"result": {"agents": [...]}}`; not a general-purpose
// parser (no streaming, no error recovery beyond "this wasn't JSON").

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(entries) => entries.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(s) => Some(s),
            _ => None,
        }
    }

    fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(entries) => Some(entries),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            bytes: text.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.bytes.get(self.pos), Some(b) if b.is_ascii_whitespace()) {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, b: u8) -> Option<()> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Some(())
        } else {
            None
        }
    }

    fn parse(text: &'a str) -> Option<Json> {
        let mut parser = Self::new(text);
        let value = parser.parse_value()?;
        parser.skip_ws();
        if parser.pos == parser.bytes.len() {
            Some(value)
        } else {
            None
        }
    }

    fn parse_value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.peek()? {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Json::String),
            b't' => self.parse_literal("true", Json::Bool(true)),
            b'f' => self.parse_literal("false", Json::Bool(false)),
            b'n' => self.parse_literal("null", Json::Null),
            b'-' | b'0'..=b'9' => self.parse_number(),
            _ => None,
        }
    }

    fn parse_literal(&mut self, literal: &str, value: Json) -> Option<Json> {
        let end = self.pos + literal.len();
        if self.bytes.get(self.pos..end) == Some(literal.as_bytes()) {
            self.pos = end;
            Some(value)
        } else {
            None
        }
    }

    fn parse_number(&mut self) -> Option<Json> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while matches!(
            self.peek(),
            Some(b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
        ) {
            self.pos += 1;
        }
        std::str::from_utf8(&self.bytes[start..self.pos])
            .ok()?
            .parse::<f64>()
            .ok()
            .map(Json::Number)
    }

    fn parse_string(&mut self) -> Option<String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let b = *self.bytes.get(self.pos)?;
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let escape = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?)
                                .ok()?;
                            let code = u32::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    // Re-decode this byte (and any continuation bytes) as UTF-8
                    // rather than assuming ASCII.
                    let rest = &self.bytes[self.pos - 1..];
                    let ch_len = utf8_len(b);
                    let slice = rest.get(..ch_len)?;
                    out.push_str(std::str::from_utf8(slice).ok()?);
                    self.pos += ch_len - 1;
                }
            }
        }
    }

    fn parse_array(&mut self) -> Option<Json> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Some(Json::Array(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b']' => {
                    self.pos += 1;
                    return Some(Json::Array(items));
                }
                _ => return None,
            }
        }
    }

    fn parse_object(&mut self) -> Option<Json> {
        self.expect(b'{')?;
        let mut entries = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Some(Json::Object(entries));
        }
        loop {
            self.skip_ws();
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect(b':')?;
            let value = self.parse_value()?;
            entries.push((key, value));
            self.skip_ws();
            match self.peek()? {
                b',' => {
                    self.pos += 1;
                }
                b'}' => {
                    self.pos += 1;
                    return Some(Json::Object(entries));
                }
                _ => return None,
            }
        }
    }
}

fn utf8_len(first_byte: u8) -> usize {
    if first_byte & 0x80 == 0 {
        1
    } else if first_byte & 0xE0 == 0xC0 {
        2
    } else if first_byte & 0xF0 == 0xE0 {
        3
    } else {
        4
    }
}

// --- Herdr access -----------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LiveAgent {
    name: String,
    status: String,
    pane: String,
    cwd: String,
}

/// `$HERDR_BIN_PATH` when set and non-blank, else the bare `herdr` token —
/// identical semantics to `doctor::herdr_binary` (duplicated locally since
/// that one is private to `doctor.rs` and this task's write scope does not
/// include editing `doctor.rs` to share it).
#[cfg(not(test))]
fn herdr_binary() -> String {
    match std::env::var("HERDR_BIN_PATH") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "herdr".to_string(),
    }
}

/// Never resolves to a real binary under `cargo test`, for the same reason
/// `doctor.rs` isolates its own test build this way.
#[cfg(test)]
fn herdr_binary() -> String {
    "mv-cli-test-herdr-stub-should-never-exist".to_string()
}

/// One `herdr agent list` call. `None` means "could not get a trustworthy
/// reply" (spawn failure, nonzero exit, non-JSON stdout, or JSON lacking a
/// `result` object) — every caller renders that as `?`, never as zero.
fn fetch_live_agents(binary: &str) -> Option<Vec<LiveAgent>> {
    let output = std::process::Command::new(binary)
        .args(["agent", "list"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = JsonParser::parse(stdout.trim())?;
    let result = value.get("result")?.as_object()?;
    let agents = result
        .iter()
        .find(|(k, _)| k == "agents")
        .map(|(_, v)| v)
        .and_then(Json::as_array)
        .unwrap_or(&[]);
    Some(
        agents
            .iter()
            .map(|agent| LiveAgent {
                name: agent
                    .get("name")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned(),
                status: agent
                    .get("agent_status")
                    .and_then(Json::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                pane: agent
                    .get("pane_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned(),
                cwd: agent
                    .get("foreground_cwd")
                    .or_else(|| agent.get("cwd"))
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned(),
            })
            .collect(),
    )
}

// --- recognition and rendering -----------------------------------------

/// `<project>-<role_short>-<seat_short>`: split off the last two
/// dash-separated segments so a dashed project name still resolves.
fn split_dispatched_name(name: &str) -> Option<(&str, &str, &str)> {
    let seat_dash = name.rfind('-')?;
    let (head, seat_short) = (&name[..seat_dash], &name[seat_dash + 1..]);
    let role_dash = head.rfind('-')?;
    let (project, role_short) = (&head[..role_dash], &head[role_dash + 1..]);
    if project.is_empty() || role_short.is_empty() || seat_short.is_empty() {
        return None;
    }
    Some((project, role_short, seat_short))
}

struct Recognized<'c> {
    role: Role,
    seat: &'c str,
}

/// Resolve `name`'s role/seat short codes against `project`'s configured
/// chains — a role is recognized when its current *or* its retired
/// (pre-rename) config key starts with the name's role-short token
/// (case-insensitively), and a seat is recognized when some account
/// assigned to that chain entry has a matching `short`.
///
/// The legacy fallback matters for a live agent: Herdr may still be
/// running one named before the role rename landed (`sandbox-expl-sb`,
/// `expl` being Python's short code for what is now `scout`), and this
/// command must still show it rather than silently dropping it from the
/// table, the same way a history reader keeps accepting an old on-disk
/// role name.
fn recognize<'c>(config: &'c Config, name: &str) -> Option<(&'c str, Recognized<'c>)> {
    // Names may use a configured project short or a truncated/hashed slug,
    // and repeated sessions append a numeric instance. Reuse the writer's
    // naming algorithm before accepting the historical plain-name shape.
    for project in config.projects.values() {
        for (role, accounts) in &project.chains {
            for account_name in accounts {
                let Some(account) = config.accounts.get(account_name) else {
                    continue;
                };
                for key in [role.config_key(), role.legacy_config_key()] {
                    let code = &key[..key.len().min(4)];
                    let Ok(base) = mv_engine::runner::naming::agent_name(
                        &project.name,
                        code,
                        None,
                        &account.short,
                        project.short.as_deref().unwrap_or(""),
                    ) else {
                        continue;
                    };
                    if name == base
                        || name
                            .strip_prefix(&base)
                            .and_then(|suffix| suffix.strip_prefix('-'))
                            .is_some_and(|suffix| {
                                !suffix.is_empty()
                                    && suffix.bytes().all(|byte| byte.is_ascii_digit())
                            })
                    {
                        return Some((
                            &project.name,
                            Recognized {
                                role: *role,
                                seat: &account.name,
                            },
                        ));
                    }
                }
            }
        }
    }
    let (project_name, role_short, seat_short) = split_dispatched_name(name)?;
    let project = config.projects.get(project_name)?;
    let role_short = role_short.to_ascii_lowercase();
    for (role, accounts) in &project.chains {
        let recognized_role = role.config_key().starts_with(&role_short)
            || role.legacy_config_key().starts_with(&role_short);
        if !recognized_role {
            continue;
        }
        for account_name in accounts {
            if let Some(account) = config.accounts.get(account_name) {
                if account.short.eq_ignore_ascii_case(seat_short) {
                    return Some((
                        project.name.as_str(),
                        Recognized {
                            role: *role,
                            seat: account.name.as_str(),
                        },
                    ));
                }
            }
        }
    }
    None
}

/// The same worker recognition used by the table and backlog authority checks.
pub(super) fn is_dispatched_name(config: &Config, name: &str) -> bool {
    recognize(config, name).is_some_and(|(_, worker)| worker.role != Role::Nexus)
}

const PROJECT_WIDTH: usize = 17;
const TASK_WIDTH: usize = 27;
const ROLE_WIDTH: usize = 11;
const SEAT_WIDTH: usize = 17;
const STATUS_WIDTH: usize = 11;
const DASH: &str = "\u{2014}";

#[derive(Default)]
struct WorkerRow {
    project: String,
    task: String,
    age: String,
    role: String,
    seat: String,
    status: String,
}

fn render_table(rows: &[WorkerRow]) -> String {
    let mut out = format!(
        "{:PROJECT_WIDTH$}{:TASK_WIDTH$}{:ROLE_WIDTH$}{:SEAT_WIDTH$}{:STATUS_WIDTH$}AGE",
        "PROJECT", "TASK", "ROLE", "SEAT", "STATUS",
    );
    if rows.is_empty() {
        // Python `format_workers_table`: an ok empty `agent list` is a
        // live fleet of zero, so the header is followed by `(none)` rather
        // than a blank body (`tests/test_cli.py` test_workers_command_
        // prints_none_table_when_fleet_is_empty). Found via
        // `conformance/diff_fuzz.py` seed 9000.
        out.push('\n');
        out.push_str("(none)");
        return out;
    }
    for row in rows {
        out.push('\n');
        out.push_str(&format!(
            "{:PROJECT_WIDTH$}{:TASK_WIDTH$}{:ROLE_WIDTH$}{:SEAT_WIDTH$}{:STATUS_WIDTH$}{}",
            mv_board::layout::sanitize_title(if row.project.is_empty() {
                DASH
            } else {
                &row.project
            }),
            mv_board::layout::sanitize_title(if row.task.is_empty() { DASH } else { &row.task }),
            mv_board::layout::sanitize_title(&row.role),
            mv_board::layout::sanitize_title(&row.seat),
            mv_board::layout::sanitize_title(&row.status),
            if row.age.is_empty() { DASH } else { &row.age },
        ));
    }
    out
}

/// Builds the full `workers` report. Takes already-fetched live agents
/// (`None` means Herdr was unreachable or unparseable) so this stays unit
/// testable without a subprocess; see the module docs for why a live
/// fetch's output can only be compared to the recorded conformance
/// scenarios once the config-loading gap they both hit is fixed elsewhere.
fn render(config: &Config, agents: Option<&[LiveAgent]>) -> String {
    let mut projects: Vec<&String> = config.projects.keys().collect();
    projects.sort();
    let mut accounts: Vec<&String> = config.accounts.keys().collect();
    accounts.sort();

    let mut sections = Vec::new();
    let history: Vec<_> = config
        .projects
        .values()
        .flat_map(|project| {
            project_path(project)
                .map(mv_state::tasks::sweep_tasks)
                .unwrap_or_default()
                .into_iter()
                .map(move |task| (project, task))
        })
        .collect();
    let mut seat_counts = std::collections::BTreeMap::<&str, u32>::new();
    let mut project_counts = std::collections::BTreeMap::<&str, u32>::new();

    let (table, worker_count, orchestrator_count) = match agents {
        Some(agents) => {
            let mut rows = Vec::new();
            let mut orchestrators = 0u32;
            for agent in agents {
                let nexus = config.projects.values().any(|project| {
                    config
                        .accounts
                        .values()
                        .filter(|account| account.interactive)
                        .any(|account| {
                            [
                                mv_engine::fleet_start::nexus_agent_name(
                                    &project.name,
                                    &account.short,
                                    project.short.as_deref().unwrap_or(""),
                                ),
                                mv_engine::fleet_start::nexus_legacy_agent_name(
                                    &project.name,
                                    &account.short,
                                    project.short.as_deref().unwrap_or(""),
                                ),
                            ]
                            .into_iter()
                            .filter_map(Result::ok)
                            .any(|name| name == agent.name)
                        })
                });
                if nexus || config.projects.contains_key(&agent.name) {
                    orchestrators += 1;
                    continue;
                }
                if !is_dispatched_name(config, &agent.name) {
                    continue;
                }
                let joined = history
                    .iter()
                    .filter(|(_, task)| task.agent.as_deref() == Some(agent.name.as_str()))
                    .max_by_key(|(_, task)| task.dispatch_ordinal);
                if let Some((project, task)) = joined {
                    let seat = task.account.as_deref().unwrap_or(DASH);
                    *seat_counts.entry(seat).or_default() += 1;
                    *project_counts.entry(&project.name).or_default() += 1;
                    let age = task
                        .dispatched_at
                        .map(|stamp| {
                            mv_state::tasks::format_elapsed(mv_core::time::now_utc() - stamp)
                        })
                        .unwrap_or_default();
                    rows.push(WorkerRow {
                        project: project.name.clone(),
                        task: task.task_id.clone(),
                        age,
                        role: Role::from_any_config_key(&task.role)
                            .map_or_else(|_| task.role.clone(), |role| role.config_key().into()),
                        seat: seat.into(),
                        status: agent.status.clone(),
                    });
                } else if let Some((_project, recognized)) = recognize(config, &agent.name) {
                    rows.push(WorkerRow {
                        role: recognized.role.config_key().into(),
                        seat: recognized.seat.into(),
                        status: agent.status.clone(),
                        ..Default::default()
                    });
                } else if !agent.cwd.is_empty() {
                    // Lost leases retain project occupancy through cwd even when
                    // the agent no longer carries a dispatched name.
                    if let Some(project) = config
                        .projects
                        .values()
                        .filter(|project| {
                            project_path(project)
                                .is_some_and(|root| Path::new(&agent.cwd).starts_with(root))
                        })
                        .max_by_key(|project| {
                            project_path(project).map(|root| root.components().count())
                        })
                    {
                        if agent.name.is_empty() {
                            *project_counts.entry(&project.name).or_default() += 1;
                        }
                    }
                }
            }
            rows.sort_by(|a, b| {
                (&a.project, &a.task, &a.role, &a.seat)
                    .cmp(&(&b.project, &b.task, &b.role, &b.seat))
            });
            let count = rows.len();
            sections.push(render_table(&rows));
            (true, Some(count as u32), Some(orchestrators))
        }
        None => (false, None, None),
    };
    let _ = table;

    let worker_line = format!(
        "workers {}/{}",
        worker_count.map_or("?".to_string(), |n| n.to_string()),
        config.max_concurrent
    );

    let seat_line = format!(
        "seats: {}",
        accounts
            .iter()
            .map(|name| {
                let open = agents.map(|_| seat_counts.get(name.as_str()).copied().unwrap_or(0));
                format!("{name} {}", open.map_or("?".to_string(), |n| n.to_string()))
            })
            .collect::<Vec<_>>()
            .join("  ")
    );

    let project_line = format!(
        "projects: {}",
        projects
            .iter()
            .map(|name| {
                let project = &config.projects[*name];
                let ceiling = project.max_slots.unwrap_or(config.max_concurrent);
                let open_text = agents.map_or_else(
                    || "?".into(),
                    |_| {
                        project_counts
                            .get(name.as_str())
                            .copied()
                            .unwrap_or(0)
                            .to_string()
                    },
                );
                format!("{name} {open_text} open, ceiling {ceiling}")
            })
            .collect::<Vec<_>>()
            // Each project already contains a comma (`0 open, ceiling N`);
            // Python joins them with `"; "` (`board.format_workers_totals`).
            .join("; ")
    );

    let orchestrator_line = format!(
        "orchestrators {} (excluded from workers; they do not count against max_concurrent)",
        orchestrator_count.map_or("?".to_string(), |n| n.to_string())
    );

    sections.push(format!(
        "{worker_line}\n{seat_line}\n{project_line}\n{orchestrator_line}"
    ));
    sections.join("\n\n")
}

/// Python `cmd_workers` (`cli.py:8808-8810`) prints the fleet-halt line
/// before the table when a halt is on disk. Found via
/// `conformance/diff_fuzz.py` seed 9004.
fn halt_banner(state: &mv_state::halt::HaltState) -> Option<String> {
    if state.halted {
        Some(mv_state::halt::format_halt_line(state))
    } else {
        None
    }
}

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

    let state_dir = state_dir_from_config(&config_path);
    if let Some(line) = halt_banner(&mv_state::halt::read_halt_state(&state_dir)) {
        println!("{line}");
    }

    let binary = herdr_binary();
    let agents = fetch_live_agents(&binary);
    println!("{}", render(&config, agents.as_deref()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::load::load_config_str;
    use std::path::Path;

    fn config() -> Config {
        load_config_str(
            "[accounts.seat-b]\n\
             kind = \"codex\"\n\
             short = \"sb\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:b\"\n\n\
             [projects.sandbox]\n\
             path = \"/tmp\"\n\n\
             [projects.sandbox.chains]\n\
             scout = [\"seat-b\"]\n\n\
             [roles.scout]\n\
             write_scope = \"none\"\n\
             effort = \"low\"\n\
             on_exhausted = \"degrade\"\n\
             skills = []\n\n\
             [roles.scout.models]\n\
             codex = \"gpt-test\"\n",
            Path::new("config.toml"),
        )
        .unwrap()
    }

    #[test]
    fn json_parser_reads_the_fixture_shape() {
        let text = r#"{"result": {"agents": [{"agent": "codex", "agent_status": "working", "cwd": "/tmp/lane", "name": "sandbox-scou-sb", "pane_id": "w1:p1"}]}}"#;
        let value = JsonParser::parse(text).unwrap();
        let agents = value.get("result").unwrap().get("agents").unwrap();
        assert_eq!(agents.as_array().unwrap().len(), 1);
        assert_eq!(
            agents.as_array().unwrap()[0].get("name").unwrap().as_str(),
            Some("sandbox-scou-sb")
        );
    }

    #[test]
    fn json_parser_rejects_non_json() {
        assert!(JsonParser::parse("not json at all").is_none());
    }

    #[test]
    fn split_dispatched_name_takes_the_last_two_segments() {
        assert_eq!(
            split_dispatched_name("sandbox-scou-sb"),
            Some(("sandbox", "scou", "sb"))
        );
        assert_eq!(
            split_dispatched_name("multi-word-scou-sb"),
            Some(("multi-word", "scou", "sb"))
        );
        assert_eq!(split_dispatched_name("just-one"), None);
        assert_eq!(split_dispatched_name("none"), None);
    }

    #[test]
    fn recognize_matches_a_role_prefix_and_an_account_short() {
        let config = config();
        let (project, recognized) = recognize(&config, "sandbox-scou-sb").unwrap();
        assert_eq!(project, "sandbox");
        assert_eq!(recognized.role, Role::Scout);
        assert_eq!(recognized.seat, "seat-b");
        assert!(recognize(&config, "sandbox-scou-zz").is_none());
        assert!(recognize(&config, "unknown-scou-sb").is_none());
    }

    #[test]
    fn recognize_also_matches_a_live_agent_s_legacy_short_code() {
        // `expl` is Python's retired short code for the role now spelled
        // `scout` — a live Herdr agent dispatched before the rename landed
        // must still resolve, per `workers-herdr-shape-fidelity.json`.
        let config = config();
        let (project, recognized) = recognize(&config, "sandbox-expl-sb").unwrap();
        assert_eq!(project, "sandbox");
        assert_eq!(recognized.role, Role::Scout);
        assert_eq!(recognized.seat, "seat-b");
    }

    #[test]
    fn render_matches_the_conformance_shape_for_a_recognized_worker() {
        let config = config();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".to_string(),
            status: "working".to_string(),
            ..Default::default()
        }];
        let output = render(&config, Some(&agents));
        let expected = "\
PROJECT          TASK                       ROLE       SEAT             STATUS     AGE\n\
\u{2014}                \u{2014}                          scout      seat-b           working    \u{2014}\n\
\n\
workers 1/3\n\
seats: seat-b 0\n\
projects: sandbox 0 open, ceiling 3\n\
orchestrators 0 (excluded from workers; they do not count against max_concurrent)";
        assert_eq!(output, expected);
    }

    #[test]
    fn render_matches_the_conformance_shape_for_a_legacy_short_coded_worker() {
        let config = config();
        let agents = [LiveAgent {
            name: "sandbox-expl-sb".to_string(),
            status: "working".to_string(),
            ..Default::default()
        }];
        let output = render(&config, Some(&agents));
        let expected = "\
PROJECT          TASK                       ROLE       SEAT             STATUS     AGE\n\
\u{2014}                \u{2014}                          scout      seat-b           working    \u{2014}\n\
\n\
workers 1/3\n\
seats: seat-b 0\n\
projects: sandbox 0 open, ceiling 3\n\
orchestrators 0 (excluded from workers; they do not count against max_concurrent)";
        assert_eq!(output, expected);
    }

    #[test]
    fn recognizes_short_project_names_and_numbered_worker_instances() {
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().short = Some("sand".into());
        assert!(is_dispatched_name(&config, "sand-scou-sb-2"));
        assert!(is_dispatched_name(&config, "sand-expl-sb-2"));
        assert!(!is_dispatched_name(&config, "sand-nexu-sb"));
    }

    #[test]
    fn pane_history_does_not_turn_unrecognized_names_into_workers() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        std::fs::write(root.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-fixer-100001\trole=fixer\taccount=seat-b\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-fixer-100001\tpane=w1:p1\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=20250101-fixer-100001\tstatus=done\n"
        ).unwrap();
        let agents = [LiveAgent {
            name: "sandbox-worker-cx".into(),
            pane: "w1:p1".into(),
            cwd: root.path().display().to_string(),
            status: "done".into(),
        }];
        assert_eq!(render(&config, Some(&agents)), render(&config, Some(&[])));
    }

    #[test]
    fn recognized_name_joins_its_own_history_instead_of_another_tasks_pane() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        std::fs::write(root.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-fixer-100001\trole=fixer\taccount=seat-b\tagent=sandbox-other-cx\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-fixer-100001\tpane=w1:p1\n"
        ).unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            pane: "w1:p1".into(),
            status: "working".into(),
            ..Default::default()
        }];
        let output = render(&config, Some(&agents));
        assert!(!output.contains("20250101-fixer-100001"), "{output}");
        assert!(output.contains("scout"), "{output}");
    }

    #[test]
    fn workers_join_task_history_and_count_live_occupancy() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        let task = "20261105-builder-abcdef";
        let stamp = mv_core::time::format_seconds_utc(
            mv_core::time::now_utc() - time::Duration::minutes(2),
        );
        std::fs::write(root.path().join(".multiverse/routing.log"), format!(
            "{stamp}\tevent=dispatch\ttask={task}\tproject=sandbox\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n{stamp}\tevent=started\ttask={task}\tpane=w1:p1\n"
        )).unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "idle".into(),
            ..Default::default()
        }];
        let text = render(&config, Some(&agents));
        assert!(text.contains(task), "{text}");
        assert!(text.contains("seats: seat-b 1"), "{text}");
        assert!(text.contains("sandbox 1 open"), "{text}");
        // Python's `format_elapsed` (`tasks.py:335-349`, ported as
        // `mv_state::tasks::format_elapsed`) spells out the unit — "2 min",
        // never "2m". Found via `conformance/diff_fuzz.py` seed 50000.
        assert!(text.contains("2 min"), "{text}");
        // A recorded result does not mean the session is closed.
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join(".multiverse/routing.log"))
            .unwrap()
            .write_all(
                format!("{stamp}\tevent=result\ttask={task}\tstatus=done\tstate=done\n").as_bytes(),
            )
            .unwrap();
        assert!(render(&config, Some(&agents)).contains("sandbox 1 open"));
        assert!(render(&config, Some(&[])).contains("sandbox 0 open"));
    }

    #[test]
    fn halt_banner_matches_python_when_the_fleet_is_halted() {
        let halted = mv_state::halt::HaltState {
            halted: true,
            reason: Some("fuzz maintenance".into()),
            at: Some("2026-11-03T00:00:00+00:00".into()),
        };
        assert_eq!(
            halt_banner(&halted).as_deref(),
            Some("fleet halted since 2026-11-03T00:00:00+00:00: fuzz maintenance")
        );
        let clear = mv_state::halt::HaltState {
            halted: false,
            reason: None,
            at: None,
        };
        assert!(halt_banner(&clear).is_none());
    }

    #[test]
    fn render_empty_live_fleet_prints_none_row_and_joins_projects_with_semicolons() {
        // Failing observation from `conformance/diff_fuzz.py` seed 9000:
        // Python's empty table is a `(none)` body, and two projects are
        // joined with `"; "` because each entry already contains commas.
        let config = load_config_str(
            "[accounts.seat-b]\n\
             kind = \"codex\"\n\
             short = \"sb\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:b\"\n\n\
             [projects.sandbox]\n\
             path = \"/tmp/a\"\n\n\
             [projects.sandbox2]\n\
             path = \"/tmp/b\"\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let output = render(&config, Some(&[]));
        assert!(
            output.contains("(none)"),
            "empty live fleet must render a (none) table body: {output}"
        );
        assert!(
            output.contains("projects: sandbox 0 open, ceiling 3; sandbox2 0 open, ceiling 3"),
            "multi-project totals must use '; ' not ', ': {output}"
        );
    }

    #[test]
    fn render_fails_open_to_question_marks_with_no_table_when_herdr_is_unreachable() {
        let config = config();
        let output = render(&config, None);
        let expected = "\
workers ?/3\n\
seats: seat-b ?\n\
projects: sandbox ? open, ceiling 3\n\
orchestrators ? (excluded from workers; they do not count against max_concurrent)";
        assert_eq!(output, expected);
    }

    #[test]
    fn render_table_strips_osc_52_from_a_herdr_status_string() {
        let rows = [WorkerRow {
            role: "scout".into(),
            seat: "seat-b".into(),
            status: "work\u{1b}]52;c;EVIL\u{07}ing".into(),
            ..Default::default()
        }];
        let output = render_table(&rows);
        assert!(!output.contains('\u{1b}'));
        assert!(!output.contains('\u{07}'));
        assert!(!output.contains("]52;"));
        assert!(output.contains("working"));
    }
}

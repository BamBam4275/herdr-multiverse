//! `multiverse workers`: one live listing joined to project routing history.
//! Task metadata supplies project, task, seat, role, and age. The table follows
//! live sessions, including idle/done panes; the seats line follows Python's
//! `open_agent_counts` (working/blocked, plus unrecorded startup grace).
//! Unreachable Herdr renders unknown counts and still exits successfully.
//! Historical role names remain readable while current live role labels use
//! the current vocabulary.
//!
//! The STATUS column can also show `stalled`, overriding Herdr's own
//! `agent_status` display-only: Herdr cannot tell a lane quietly waiting on
//! its own background job from a dead one, since both can report the same
//! status indefinitely. Herdr's `revision`/`state_change_seq` cannot stand
//! in for that either — they sit still for the whole length of a long
//! build even though the agent is genuinely working, which would flag a
//! busy lane as stalled (card `20261115-b-59e017`). [`stalled_panes`]
//! instead watches a hash of each pane's own content (`herdr pane read
//! <pane> --lines` [`STALL_CONTENT_LINES`], via
//! [`mv_state::pane_watch_store`]) and flags one only once that hash has
//! gone unchanged for [`STALL_THRESHOLD`]. Seat and project occupancy
//! still read Herdr's raw status exactly as before — only the printed
//! STATUS cell changes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use clap::ArgMatches;
use mv_config::load::{Config, Project};
use mv_config::roles::Role;
use sha2::{Digest, Sha256};

use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use super::{herdr_command, run_herdr};
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

/// One `herdr agent list` call, through the shared runner
/// ([`super::run_herdr`]) so a transient spawn failure is retried rather
/// than read as an unreachable Herdr. `None` means "could not get a
/// trustworthy reply" (spawn failure, nonzero exit, non-JSON stdout, or
/// JSON lacking a `result` object) — every caller renders that as `?`,
/// never as zero.
fn fetch_live_agents(binary: &str) -> Option<Vec<LiveAgent>> {
    let output = run_herdr(mv_engine::mv_herdr::argv::agent_list(), binary).ok()?;
    if output.returncode != 0 {
        return None;
    }
    let value = JsonParser::parse(output.stdout.trim())?;
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

/// How many trailing lines of a pane's own content [`stalled_panes`] reads
/// to decide whether it has moved. A named const rather than a magic
/// number, since it is the one knob that trades a longer read for less
/// sensitivity to a pane that is merely scrolling the same handful of
/// lines (a spinner, a progress bar) without ever truly going quiet.
const STALL_CONTENT_LINES: u32 = 200;

/// Identical semantics to `mv_herdr::argv`'s own private `looks_like_flag`
/// (duplicated locally for the same reason `herdr_binary` above is: that
/// one is private to `argv.rs`, and `mv_herdr::argv::pane_read` itself
/// cannot be reused here without also taking on its `--source` flag,
/// which `fetch_pane_content`'s own conformance fixture pins the exact
/// absence of).
fn looks_like_flag(value: &str) -> bool {
    value.starts_with('-') && value != "-"
}

/// `pane read <pane_id> --lines` [`STALL_CONTENT_LINES`], with the
/// positional guard split out so it is testable without spawning a
/// process: flags before `--` so a dash-leading pane id cannot be parsed
/// as `--lines`/`--help` — the same guard `mv_herdr::argv::pane_read`
/// carries for its own callers, restated here (rather than reused)
/// because that function always appends `--source`, a call shape this
/// one's conformance fixture does not expect.
fn pane_read_args(pane_id: &str) -> Vec<String> {
    let mut args = vec!["pane".to_string(), "read".to_string()];
    if looks_like_flag(pane_id) {
        args.push("--lines".to_string());
        args.push(STALL_CONTENT_LINES.to_string());
        args.push("--".to_string());
        args.push(pane_id.to_string());
    } else {
        args.push(pane_id.to_string());
        args.push("--lines".to_string());
        args.push(STALL_CONTENT_LINES.to_string());
    }
    args
}

/// One [`pane_read_args`] call, through the shared runner
/// ([`super::run_herdr`]). `None` means Herdr could not be asked (spawn
/// failure or nonzero exit) — callers must treat that the same as "no
/// signal this cycle," never as "the pane is empty," so a fetch hiccup
/// never looks like content that vanished.
fn fetch_pane_content(binary: &str, pane_id: &str) -> Option<String> {
    let output = run_herdr(herdr_command(&pane_read_args(pane_id)), binary).ok()?;
    (output.returncode == 0).then_some(output.stdout)
}

/// A stable digest of one pane read, so the store never has to hold a
/// whole screen's worth of text just to notice it has not moved.
fn content_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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

const DASH: &str = "\u{2014}";

/// A pane whose own content has not moved in this long is shown `stalled`
/// regardless of what Herdr's own `agent_status` claims — Mike's rule.
/// Herdr alone cannot tell a lane quietly waiting on its own background
/// job from a dead one, since both can report the same status
/// indefinitely; a pane that has produced no new content for this long is
/// worth a human's look either way. Display only: this never closes or
/// signals the pane, it only changes what this table prints.
const STALL_THRESHOLD: time::Duration = time::Duration::minutes(30);

/// One `herdr pane read` per live agent with a pane id, hashed via
/// [`content_hash`] — the per-pane signal [`stalled_panes`] persists.
/// `None` (a failed read) is kept as `None` rather than dropped, so that
/// pane still fails open to "just changed" instead of silently vanishing
/// from this cycle's observation.
fn fetch_pane_hashes(binary: &str, agents: &[LiveAgent]) -> HashMap<String, Option<String>> {
    agents
        .iter()
        .filter(|agent| !agent.pane.is_empty())
        .map(|agent| {
            let hash = fetch_pane_content(binary, &agent.pane).map(|text| content_hash(&text));
            (agent.pane.clone(), hash)
        })
        .collect()
}

/// Persist this cycle's content hash for every pane in `pane_hashes` (via
/// [`mv_state::pane_watch_store`]), and return the `pane_id` of every one
/// that has gone unchanged for at least [`STALL_THRESHOLD`]. A lock
/// timeout or other store error fails open — an empty set, so a transient
/// store problem costs this cycle's staleness signal, never the table
/// itself.
fn stalled_panes(
    state_dir: &Path,
    pane_hashes: &HashMap<String, Option<String>>,
    now: time::OffsetDateTime,
) -> HashSet<String> {
    if pane_hashes.is_empty() {
        return HashSet::new();
    }
    let signals: Vec<_> = pane_hashes
        .iter()
        .map(|(pane_id, hash)| mv_state::pane_watch_store::PaneSignal {
            pane_id,
            content_hash: hash.clone(),
        })
        .collect();
    let Ok(last_changed) = mv_state::pane_watch_store::observe(state_dir, &signals, now) else {
        return HashSet::new();
    };
    last_changed
        .into_iter()
        .filter(|(_, changed_at)| now - *changed_at >= STALL_THRESHOLD)
        .map(|(pane_id, _)| pane_id)
        .collect()
}

#[derive(Default)]
struct WorkerRow {
    project: String,
    task: String,
    age: String,
    role: String,
    seat: String,
    status: String,
    outcome: String,
}

fn render_table(rows: &[WorkerRow]) -> String {
    let header = [
        "PROJECT", "TASK", "ROLE", "SEAT", "STATUS", "OUTCOME", "AGE",
    ];
    let cells: Vec<_> = rows
        .iter()
        .map(|row| {
            [
                &row.project,
                &row.task,
                &row.role,
                &row.seat,
                &row.status,
                &row.outcome,
                &row.age,
            ]
            .map(|cell| {
                // Keep each sanitized cell a single whitespace-delimited field,
                // including elapsed ages such as "2 min" and empty metadata.
                let cell = mv_board::layout::sanitize_title(cell)
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join("-");
                if cell.is_empty() {
                    DASH.into()
                } else {
                    cell
                }
            })
        })
        .collect();
    let mut widths = header.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let format_row = |row: [&str; 7]| {
        row.iter()
            .enumerate()
            .map(|(index, cell)| {
                if index + 1 == row.len() {
                    (*cell).to_owned()
                } else {
                    format!("{cell:width$}", width = widths[index])
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out = format_row(header);
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
    for row in &cells {
        out.push('\n');
        out.push_str(&format_row(row.each_ref().map(String::as_str)));
    }
    out
}

/// Whether a Herdr `agent_status` holds a seat — Python's
/// `_OPEN_AGENT_STATES` (`accounts.py:623-624`): only `working`/`blocked`.
fn is_open_agent_status(status: &str) -> bool {
    matches!(status, "working" | "blocked")
}

/// Python `accounts.open_agent_counts` (`accounts.py:686-861`), keyed by
/// today's config account name so the seats line can look up
/// `sorted(config.accounts)` the same way `board.workers_snapshot` does.
///
/// A recorded `event=result` frees the seat even if Herdr still lists the
/// pane. An unrecorded task occupies when Herdr reports working/blocked, or
/// for the account's startup grace after `started` (or `dispatch` when
/// `started` has not landed yet). Idle/done past that grace do not occupy.
fn open_seat_counts<'a>(
    config: &'a Config,
    history: &[(&'a Project, mv_state::tasks::SweepTask)],
    agents: &[LiveAgent],
) -> BTreeMap<&'a str, u32> {
    let status_by_name: HashMap<&str, &str> = agents
        .iter()
        .filter(|agent| !agent.name.is_empty())
        .map(|agent| (agent.name.as_str(), agent.status.as_str()))
        .collect();
    let mut project_root_status: HashMap<&str, &str> = HashMap::new();
    for agent in agents {
        if !agent.name.is_empty() || agent.cwd.is_empty() {
            continue;
        }
        project_root_status.insert(agent.cwd.as_str(), agent.status.as_str());
    }

    let mut latest: HashMap<&str, usize> = HashMap::new();
    for (index, (_, task)) in history.iter().enumerate() {
        let Some(agent) = task.agent.as_deref() else {
            continue;
        };
        let keep_existing = latest
            .get(agent)
            .copied()
            .is_some_and(|existing| history[existing].1.dispatch_ordinal >= task.dispatch_ordinal);
        if !keep_existing {
            latest.insert(agent, index);
        }
    }

    let now = mv_core::time::now_utc();
    let mut nameless_root_claimed: HashSet<&str> = HashSet::new();
    let mut counts = BTreeMap::<&str, u32>::new();
    for &index in latest.values() {
        let (project, task) = &history[index];
        if task.recorded {
            continue;
        }
        let Some(account) = super::dispatch::resolve_task_account(task, config) else {
            continue;
        };
        let mut status = task
            .agent
            .as_deref()
            .and_then(|name| status_by_name.get(name).copied())
            .unwrap_or("");
        let mut root_fallback = false;
        let write_scope_none = Role::from_any_config_key(&task.role)
            .ok()
            .and_then(|role| config.roles.get(&role))
            .is_some_and(|settings| settings.write_scope == "none");
        if status.is_empty() && task.branch.is_empty() && write_scope_none {
            if let Some(root) = project_path(project).and_then(Path::to_str) {
                if let Some(fallback) = project_root_status.get(root).copied() {
                    status = fallback;
                    root_fallback = !fallback.is_empty();
                }
            }
        }
        let grace_secs = task
            .account
            .as_deref()
            .and_then(|name| config.accounts.get(name))
            .and_then(|account| account.startup_grace_seconds)
            .unwrap_or(600);
        let grace = time::Duration::seconds(grace_secs.into());
        let started_open = match task.started_at {
            Some(started_at) => now - started_at <= grace,
            None => task
                .dispatched_at
                .is_some_and(|dispatched_at| now - dispatched_at <= grace),
        };
        if !is_open_agent_status(status) && !started_open {
            continue;
        }
        if root_fallback {
            let Some(root) = project_path(project).and_then(Path::to_str) else {
                continue;
            };
            if !nameless_root_claimed.insert(root) {
                continue;
            }
        }
        *counts.entry(account.name.as_str()).or_default() += 1;
    }
    counts
}

/// [`render_with_stalled`] with no pane known to be stalled — every
/// existing test that has no notion of pane staleness. `run` always goes
/// through `render_with_stalled` directly, so this wrapper is test-only.
#[cfg(test)]
fn render(config: &Config, agents: Option<&[LiveAgent]>) -> String {
    render_with_stalled(config, agents, &HashSet::new())
}

/// Structured form of `workers`' report: the live table rows (`None` only
/// when Herdr was unreachable — an empty, reachable fleet is `Some(vec![])`
/// and still renders a table) plus the four summary lines' own numbers.
/// Shared by the text renderer and `--json` so the seat/project occupancy
/// computation below (the heavily-tested part of this module) is derived
/// exactly once — the same split `mv_engine::health`'s `HealthReport` /
/// `problems_json` uses between computing and rendering.
struct WorkersReport {
    rows: Option<Vec<WorkerRow>>,
    worker_count: Option<u32>,
    max_concurrent: u32,
    /// `(seat name, open count)`; `open` is `None` only when Herdr was
    /// unreachable.
    seats: Vec<(String, Option<u32>)>,
    /// `(project name, open count, ceiling)`; `open` is `None` only when
    /// Herdr was unreachable.
    projects: Vec<(String, Option<u32>, u32)>,
    orchestrator_count: Option<u32>,
}

/// Builds the full `workers` report. Takes already-fetched live agents
/// (`None` means Herdr was unreachable or unparseable) so this stays unit
/// testable without a subprocess; see the module docs for why a live
/// fetch's output can only be compared to the recorded conformance
/// scenarios once the config-loading gap they both hit is fixed elsewhere.
///
/// `stalled_panes` (pane ids, from [`stalled_panes`]) overrides only the
/// STATUS column's displayed text — never seat/project occupancy, which
/// still reads `agent.status` exactly as Herdr reported it, so a pane
/// shown `stalled` here still occupies its seat precisely as before.
fn build_report(
    config: &Config,
    agents: Option<&[LiveAgent]>,
    stalled_panes: &HashSet<String>,
) -> WorkersReport {
    let display_status = |agent: &LiveAgent| {
        if !agent.pane.is_empty() && stalled_panes.contains(&agent.pane) {
            "stalled".to_string()
        } else {
            agent.status.clone()
        }
    };
    let mut projects: Vec<&String> = config.projects.keys().collect();
    projects.sort();
    let mut accounts: Vec<&String> = config.accounts.keys().collect();
    accounts.sort();

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
    let mut seat_counts = BTreeMap::<&str, u32>::new();
    let mut project_counts = BTreeMap::<&str, u32>::new();

    let (rows, worker_count, orchestrator_count) = match agents {
        Some(agents) => {
            seat_counts = open_seat_counts(config, &history, agents);
            let mut rows = Vec::new();
            let mut orchestrators = 0u32;
            // A recorded task's pane does not hold a fleet slot even while
            // Herdr has not reaped it yet — `open_seat_counts` above
            // already skips a `recorded` task for the same reason
            // (line ~710), and `commands::dispatch::fleet_open_counts`
            // skips it fleet-wide. The row still prints (still-open panes
            // worth a human's look stay visible), but it must not inflate
            // the `workers N/CAP` line past what dispatch actually
            // enforces (card `20261116-b-ed6d3c`).
            let mut cap_count = 0u32;
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
                    if !task.recorded {
                        cap_count += 1;
                    }
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
                        status: display_status(agent),
                        outcome: task.status.map_or_else(
                            || {
                                if task.file_status.is_some() {
                                    "done-unrecorded"
                                } else {
                                    "running"
                                }
                                .into()
                            },
                            |status| status.as_str().into(),
                        ),
                    });
                } else if let Some((_project, recognized)) = recognize(config, &agent.name) {
                    cap_count += 1;
                    rows.push(WorkerRow {
                        role: recognized.role.config_key().into(),
                        seat: recognized.seat.into(),
                        status: display_status(agent),
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
            (Some(rows), Some(cap_count), Some(orchestrators))
        }
        None => (None, None, None),
    };

    let seats = accounts
        .iter()
        .map(|name| {
            let open = agents.map(|_| seat_counts.get(name.as_str()).copied().unwrap_or(0));
            (name.to_string(), open)
        })
        .collect();

    let projects = projects
        .iter()
        .map(|name| {
            let project = &config.projects[*name];
            let ceiling = project.max_slots.unwrap_or(config.max_concurrent);
            let open = agents.map(|_| project_counts.get(name.as_str()).copied().unwrap_or(0));
            (name.to_string(), open, ceiling)
        })
        .collect();

    WorkersReport {
        rows,
        worker_count,
        max_concurrent: config.max_concurrent,
        seats,
        projects,
        orchestrator_count,
    }
}

/// Renders [`WorkersReport`] exactly as `workers`' plain-text output has
/// always read: `render_table`'s block (only when Herdr was reachable),
/// a blank line, then the four `workers`/`seats`/`projects`/`nexus
/// sessions` summary lines, each number a bare `?` for "unknown" (Herdr
/// unreachable) — never the `--json` renderer's `null`.
fn render_text(report: &WorkersReport) -> String {
    let mut sections = Vec::new();
    if let Some(rows) = &report.rows {
        sections.push(render_table(rows));
    }

    let worker_line = format!(
        "workers {}/{}",
        report
            .worker_count
            .map_or("?".to_string(), |n| n.to_string()),
        report.max_concurrent
    );

    let seat_line = format!(
        "seats: {}",
        report
            .seats
            .iter()
            .map(|(name, open)| format!(
                "{name} {}",
                open.map_or("?".to_string(), |n| n.to_string())
            ))
            .collect::<Vec<_>>()
            .join("  ")
    );

    let project_line = format!(
        "projects: {}",
        report
            .projects
            .iter()
            .map(|(name, open, ceiling)| {
                let open_text = open.map_or_else(|| "?".to_string(), |n| n.to_string());
                format!("{name} {open_text} open, ceiling {ceiling}")
            })
            .collect::<Vec<_>>()
            // Each project already contains a comma (`0 open, ceiling N`);
            // Python joins them with `"; "` (`board.format_workers_totals`).
            .join("; ")
    );

    let orchestrator_line = format!(
        "nexus sessions {} (excluded from workers; they do not count against max_concurrent)",
        report
            .orchestrator_count
            .map_or("?".to_string(), |n| n.to_string())
    );

    sections.push(format!(
        "{worker_line}\n{seat_line}\n{project_line}\n{orchestrator_line}"
    ));
    sections.join("\n\n")
}

/// [`build_report`] followed by [`render_text`] — test-only now that
/// `run` calls both directly so it can branch to [`render_json`] on
/// `--json` without re-running the whole computation.
#[cfg(test)]
fn render_with_stalled(
    config: &Config,
    agents: Option<&[LiveAgent]>,
    stalled_panes: &HashSet<String>,
) -> String {
    render_text(&build_report(config, agents, stalled_panes))
}

/// Minimal ASCII-only JSON string escaper (`ensure_ascii=True` style),
/// duplicated from `mv_state::halt`'s own private `json_escape` — that one
/// is out of this task's write scope to make `pub(crate)`, and `mv-cli`
/// carries no runtime `serde_json` dependency (`Cargo.toml`: dev-dependency
/// only), so every `--json` renderer in this crate hand-builds its output
/// (`commands::presence`, `mv_engine::seatstate::show_json`'s string
/// splice). `pub(super)` so `commands::accounts`'s own `--json` renderer
/// (same task, `20261117-builder-0cbb48`) reuses it instead of a second copy.
pub(super) fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        let code = ch as u32;
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            _ if (0x20..=0x7e).contains(&code) => out.push(ch),
            _ if code <= 0xffff => out.push_str(&format!("\\u{code:04x}")),
            _ => {
                let v = code - 0x10000;
                let high = 0xd800 + (v >> 10);
                let low = 0xdc00 + (v & 0x3ff);
                out.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    out.push('"');
    out
}

/// `WorkerRow`'s own empty-string convention (no data joined — rendered as
/// [`DASH`] in the table) becomes JSON `null`, not `""`.
fn json_string_or_null(value: &str) -> String {
    if value.is_empty() {
        "null".to_owned()
    } else {
        json_escape(value)
    }
}

fn json_opt_u32(value: Option<u32>) -> String {
    value.map_or_else(|| "null".to_owned(), |n| n.to_string())
}

/// Renders [`WorkersReport`] as one line of JSON, with `halt`'s state
/// folded into a top-level `halted` key (`null` when the fleet is not
/// halted) rather than `render_text`'s separate banner line — stdout must
/// stay one parseable JSON value, so no non-JSON line can precede it.
/// Object keys are alphabetical throughout, matching
/// `mv_engine::seatstate::show_json`'s own convention.
fn render_json(report: &WorkersReport, halt: &mv_state::halt::HaltState) -> String {
    let workers = match &report.rows {
        Some(rows) => {
            let items: Vec<String> = rows
                .iter()
                .map(|row| {
                    format!(
                        "{{\"age\": {}, \"outcome\": {}, \"project\": {}, \"role\": {}, \"seat\": {}, \"status\": {}, \"task\": {}}}",
                        json_string_or_null(&row.age),
                        json_string_or_null(&row.outcome),
                        json_string_or_null(&row.project),
                        json_string_or_null(&row.role),
                        json_string_or_null(&row.seat),
                        json_string_or_null(&row.status),
                        json_string_or_null(&row.task),
                    )
                })
                .collect();
            format!("[{}]", items.join(", "))
        }
        None => "null".to_owned(),
    };
    let seats: Vec<String> = report
        .seats
        .iter()
        .map(|(name, open)| {
            format!(
                "{{\"name\": {}, \"open\": {}}}",
                json_escape(name),
                json_opt_u32(*open)
            )
        })
        .collect();
    let projects: Vec<String> = report
        .projects
        .iter()
        .map(|(name, open, ceiling)| {
            format!(
                "{{\"ceiling\": {ceiling}, \"name\": {}, \"open\": {}}}",
                json_escape(name),
                json_opt_u32(*open)
            )
        })
        .collect();
    let halted = if halt.halted {
        format!(
            "{{\"at\": {}, \"reason\": {}}}",
            halt.at
                .as_deref()
                .map_or_else(|| "null".to_owned(), json_escape),
            halt.reason
                .as_deref()
                .map_or_else(|| "null".to_owned(), json_escape),
        )
    } else {
        "null".to_owned()
    };
    format!(
        "{{\"halted\": {halted}, \"max_concurrent\": {}, \"nexus_sessions\": {}, \"projects\": [{}], \"seats\": [{}], \"worker_count\": {}, \"workers\": {workers}}}",
        report.max_concurrent,
        json_opt_u32(report.orchestrator_count),
        projects.join(", "),
        seats.join(", "),
        json_opt_u32(report.worker_count),
    )
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

/// `multiverse workers`'s entry point: loads config, fetches live agents
/// from Herdr (best-effort — an unreachable fleet still renders, with `?`
/// where live counts would go), and prints the report. Plain text prints
/// the fleet-halt banner line first, then [`render_text`]'s report;
/// `--json` folds the halt state into the single JSON line's `halted` key
/// instead, so stdout stays one parseable value (card for this task,
/// `20261117-builder-0cbb48`).
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
    let halt_state = mv_state::halt::read_halt_state(&state_dir);

    let binary = herdr_binary();
    let agents = fetch_live_agents(&binary);
    let stalled = agents
        .as_deref()
        .map(|agents| {
            let pane_hashes = fetch_pane_hashes(&binary, agents);
            stalled_panes(&state_dir, &pane_hashes, mv_core::time::now_utc())
        })
        .unwrap_or_default();
    let report = build_report(&config, agents.as_deref(), &stalled);
    if args.get_flag("json") {
        println!("{}", render_json(&report, &halt_state));
    } else {
        if let Some(line) = halt_banner(&halt_state) {
            println!("{line}");
        }
        println!("{}", render_text(&report));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;
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
    fn regression_long_project_keeps_a_column_gap_and_the_header_field_count() {
        let rows = [WorkerRow {
            project: "LongExampleProjectNameXYZ".into(),
            task: "20261110-builder-f04153".into(),
            role: "builder".into(),
            seat: "seat-with-a-long-account-name".into(),
            status: "working".into(),
            age: "2 min".into(),
            ..Default::default()
        }];
        let output = render_table(&rows);
        let mut lines = output.lines();
        let header = lines.next().unwrap();
        let row = lines.next().unwrap();
        let gap = row
            .strip_prefix("LongExampleProjectNameXYZ")
            .unwrap()
            .split("20261110-builder-f04153")
            .next()
            .unwrap();
        assert!(
            !gap.is_empty() && gap.chars().all(|ch| ch == ' '),
            "{output}"
        );
        assert_eq!(
            row.split_whitespace().count(),
            header.split_whitespace().count(),
            "{output}"
        );
    }

    #[test]
    fn regression_herdr_done_without_a_task_status_file_still_has_a_running_outcome() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        std::fs::write(root.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20261110-builder-f04153\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n"
        ).unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "done".into(),
            ..Default::default()
        }];
        let output = render(&config, Some(&agents));
        let mut lines = output.lines();
        let header: Vec<_> = lines.next().unwrap().split_whitespace().collect();
        let row: Vec<_> = lines.next().unwrap().split_whitespace().collect();
        let status = header.iter().position(|cell| *cell == "STATUS").unwrap();
        assert_eq!(row[status], "done", "{output}");
        let outcome = header
            .iter()
            .position(|cell| *cell == "OUTCOME")
            .expect("OUTCOME column");
        assert_eq!(row[outcome], "running", "{output}");
        assert_eq!(row.len(), header.len(), "{output}");
    }

    #[test]
    fn task_file_completion_and_recorded_outcomes_are_distinct() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        let task = "20261110-builder-f04153";
        let task_dir = root.path().join(".multiverse/tasks").join(task);
        std::fs::create_dir_all(&task_dir).unwrap();
        let log = root.path().join(".multiverse/routing.log");
        let dispatch = format!(
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask={task}\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n"
        );
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "working".into(),
            ..Default::default()
        }];
        for status in ["done", "blocked", "failed"] {
            std::fs::write(&log, &dispatch).unwrap();
            std::fs::write(task_dir.join("status"), status).unwrap();
            let output = render(&config, Some(&agents));
            let row: Vec<_> = output.lines().nth(1).unwrap().split_whitespace().collect();
            assert_eq!(row[4], "working", "{output}");
            assert_eq!(row[5], "done-unrecorded", "{output}");

            // A recorded result remains authoritative after its lane is pruned.
            std::fs::remove_file(task_dir.join("status")).unwrap();
            std::fs::write(
                &log,
                format!(
                    "{dispatch}2020-01-01T00:00:01Z\tevent=result\ttask={task}\tstatus={status}\n"
                ),
            )
            .unwrap();
            let output = render(&config, Some(&agents));
            let row: Vec<_> = output.lines().nth(1).unwrap().split_whitespace().collect();
            assert_eq!(row[4], "working", "{output}");
            assert_eq!(row[5], status, "{output}");
        }
    }

    #[test]
    fn table_sizes_all_columns_and_sanitizes_every_cell_before_measuring() {
        let rows = [
            WorkerRow {
                project: "project with spaces".into(),
                task: "a-longer-task-than-the-header".into(),
                role: "a-long-role".into(),
                seat: "a-long-seat".into(),
                status: "a-long-status".into(),
                outcome: "done-unrecorded".into(),
                age: "100 hours".into(),
            },
            WorkerRow {
                status: "\u{1b}]52;c;EVIL\u{07}   ".into(),
                ..Default::default()
            },
        ];
        let output = render_table(&rows);
        let lines: Vec<_> = output.lines().collect();
        let column_starts = |line: &str| {
            let mut in_cell = false;
            line.chars()
                .enumerate()
                .filter_map(|(index, ch)| {
                    let start = !ch.is_whitespace() && !in_cell;
                    in_cell = !ch.is_whitespace();
                    start.then_some(index)
                })
                .collect::<Vec<_>>()
        };
        for line in &lines[1..] {
            assert_eq!(column_starts(line), column_starts(lines[0]), "{output}");
        }
        assert!(lines[1].contains("project-with-spaces"), "{output}");
        assert!(lines[1].ends_with("100-hours"), "{output}");
        assert!(!output.contains("EVIL"), "{output}");
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
PROJECT TASK ROLE  SEAT   STATUS  OUTCOME AGE\n\
\u{2014}       \u{2014}    scout seat-b working \u{2014}       \u{2014}\n\
\n\
workers 1/3\n\
seats: seat-b 0\n\
projects: sandbox 0 open, ceiling 3\n\
nexus sessions 0 (excluded from workers; they do not count against max_concurrent)";
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
PROJECT TASK ROLE  SEAT   STATUS  OUTCOME AGE\n\
\u{2014}       \u{2014}    scout seat-b working \u{2014}       \u{2014}\n\
\n\
workers 1/3\n\
seats: seat-b 0\n\
projects: sandbox 0 open, ceiling 3\n\
nexus sessions 0 (excluded from workers; they do not count against max_concurrent)";
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
        // Idle + unrecorded + started 2 min ago is still inside Python's
        // 600s startup grace (`accounts.open_agent_counts`), so the seat
        // stays occupied even though Herdr already reports idle.
        assert!(text.contains("seats: seat-b 1"), "{text}");
        assert!(text.contains("sandbox 1 open"), "{text}");
        // Python's `format_elapsed` (`tasks.py:335-349`, ported as
        // `mv_state::tasks::format_elapsed`) spells out the unit — "2 min",
        // rendered as one field, "2-min", in the workers table.
        assert!(text.contains("2-min"), "{text}");
        // A recorded result does not mean the session is closed: project
        // occupancy still follows the live pane. Seat occupancy does not —
        // `open_agent_counts` skips RECORDED (`accounts.py:794-795`).
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join(".multiverse/routing.log"))
            .unwrap()
            .write_all(
                format!("{stamp}\tevent=result\ttask={task}\tstatus=done\tstate=done\n").as_bytes(),
            )
            .unwrap();
        let after = render(&config, Some(&agents));
        assert!(after.contains("sandbox 1 open"), "{after}");
        assert!(after.contains("seats: seat-b 0"), "{after}");
        assert!(render(&config, Some(&[])).contains("sandbox 0 open"));
    }

    #[test]
    fn recorded_done_task_does_not_occupy_its_seat_while_the_pane_is_still_listed() {
        // Failing observation from `conformance/diff_fuzz.py` seed 60033:
        // after a dispatched task finishes (`event=result` status=done),
        // Python's seats line is 0 even when Herdr still lists the pane
        // working. Counting every joined live session overcounted.
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        let task = "20261105-builder-abcdef";
        let stamp = mv_core::time::format_seconds_utc(mv_core::time::now_utc());
        std::fs::write(
            root.path().join(".multiverse/routing.log"),
            format!(
                "{stamp}\tevent=dispatch\ttask={task}\tproject=sandbox\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n\
                 {stamp}\tevent=started\ttask={task}\tpane=w1:p1\n\
                 {stamp}\tevent=result\ttask={task}\tstatus=done\tstate=done\n"
            ),
        )
        .unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "working".into(),
            ..Default::default()
        }];
        let text = render(&config, Some(&agents));
        assert!(text.contains(task), "{text}");
        assert!(text.contains("seats: seat-b 0"), "{text}");
        assert!(text.contains("sandbox 1 open"), "{text}");
    }

    #[test]
    fn working_unrecorded_task_occupies_its_seat() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        let task = "20261105-builder-abcdef";
        let stamp = "2020-01-01T00:00:00Z";
        std::fs::write(
            root.path().join(".multiverse/routing.log"),
            format!(
                "{stamp}\tevent=dispatch\ttask={task}\tproject=sandbox\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n\
                 {stamp}\tevent=started\ttask={task}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "working".into(),
            ..Default::default()
        }];
        let text = render(&config, Some(&agents));
        assert!(text.contains("seats: seat-b 1"), "{text}");
        assert!(text.contains("sandbox 1 open"), "{text}");
    }

    #[test]
    fn idle_unrecorded_task_past_grace_does_not_occupy_its_seat() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        let task = "20261105-builder-abcdef";
        let stamp = "2020-01-01T00:00:00Z";
        std::fs::write(
            root.path().join(".multiverse/routing.log"),
            format!(
                "{stamp}\tevent=dispatch\ttask={task}\tproject=sandbox\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n\
                 {stamp}\tevent=started\ttask={task}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "idle".into(),
            ..Default::default()
        }];
        let text = render(&config, Some(&agents));
        assert!(text.contains("seats: seat-b 0"), "{text}");
        assert!(text.contains("sandbox 1 open"), "{text}");
    }

    /// `workers`'s own printed `workers N/CAP` line must stay equal to the
    /// number [`super::dispatch::fleet_open_counts`] actually feeds
    /// `check_fleet_capacity` — otherwise an operator can read `workers`
    /// as "there's room" the instant before dispatch refuses with "N
    /// agent(s) already running" against a different N (field report on
    /// 0.1.6: dispatch refused at 17 while `workers` showed 15/17). Card
    /// `20261116-b-ed6d3c`.
    ///
    /// One mixed fleet, across two projects, exercises every shape that
    /// could make the two drift apart: a busy and an idle Nexus pane (must
    /// cost neither count a slot — card `20261115-b-65fe80`), two running
    /// workers, a finished worker whose `event=result` is already recorded
    /// but whose pane Herdr has not reaped yet (must cost neither count a
    /// slot — card `20261110-b-fa4365`'s same reasoning applied to the
    /// live side, not only the status-file side it already covers), a
    /// task already closed out as stranded (recorded, pane long gone), and
    /// a live pane `workers` never dispatched at all — the last two must
    /// cost neither side anything.
    #[test]
    fn fleet_cap_count_agrees_with_the_workers_table_count_across_a_mixed_fleet() {
        let alpha = tempfile::tempdir().unwrap();
        let bravo = tempfile::tempdir().unwrap();
        std::fs::create_dir(alpha.path().join(".multiverse")).unwrap();
        std::fs::create_dir(bravo.path().join(".multiverse")).unwrap();

        // alpha: one running worker, one busy Nexus, one task already
        // closed out as stranded (`close_stranded`-shaped: recorded, pane
        // long gone — must cost nothing on either side).
        std::fs::write(
            alpha.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-builder-a00001\tproject=alpha\trole=builder\taccount=seat-b\tagent=alpha-buil-sb\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-builder-a00001\tpane=w1:p1\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-nexus-a00002\tproject=alpha\trole=nexus\taccount=nex\tagent=alpha-nexu-nx\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-nexus-a00002\tpane=w1:p2\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-builder-a00003\tproject=alpha\trole=builder\taccount=seat-b\tagent=alpha-buil-sb-2\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-builder-a00003\tpane=w1:p3\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=20250101-builder-a00003\tstatus=failed\tstate=stranded-uncommitted-work\n",
        )
        .unwrap();

        // bravo: one idle Nexus, one worker already recorded done whose
        // pane Herdr still lists, one running worker.
        std::fs::write(
            bravo.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-nexus-b00001\tproject=bravo\trole=nexus\taccount=nex\tagent=bravo-nexu-nx\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-nexus-b00001\tpane=w2:p1\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-builder-b00002\tproject=bravo\trole=builder\taccount=seat-b\tagent=bravo-buil-sb\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-builder-b00002\tpane=w2:p2\n\
             2020-01-01T00:00:00Z\tevent=result\ttask=20250101-builder-b00002\tstatus=done\tstate=done\n\
             2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-builder-b00003\tproject=bravo\trole=builder\taccount=seat-b\tagent=bravo-buil-sb-2\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-builder-b00003\tpane=w2:p3\n",
        )
        .unwrap();

        let toml = format!(
            "[accounts.seat-b]\nkind = \"codex\"\nshort = \"sb\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"/tmp\"\npool = \"codex:b\"\n\n\
             [accounts.nex]\nkind = \"codex\"\nshort = \"nx\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"/tmp\"\npool = \"codex:nex\"\ninteractive = true\n\n\
             [projects.alpha]\npath = \"{alpha}\"\n\n\
             [projects.alpha.chains]\nbuilder = [\"seat-b\"]\n\n\
             [projects.bravo]\npath = \"{bravo}\"\n\n\
             [projects.bravo.chains]\nbuilder = [\"seat-b\"]\n\n\
             [roles.builder]\nwrite_scope = \"declared\"\neffort = \"low\"\non_exhausted = \"degrade\"\nskills = []\n\n\
             [roles.builder.models]\ncodex = \"gpt-test\"\n",
            alpha = alpha.path().display(),
            bravo = bravo.path().display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();

        // Herdr's own listing: alpha's stranded task's pane (`w1:p3`) is
        // gone, matching its own already-recorded closure, and
        // `random-human-pane` was never dispatched by multiverse at all.
        let agents = [
            LiveAgent {
                name: "alpha-buil-sb".into(),
                status: "working".into(),
                pane: "w1:p1".into(),
                ..Default::default()
            },
            LiveAgent {
                name: "alpha-nexu-nx".into(),
                status: "working".into(),
                pane: "w1:p2".into(),
                ..Default::default()
            },
            LiveAgent {
                name: "bravo-nexu-nx".into(),
                status: "idle".into(),
                pane: "w2:p1".into(),
                ..Default::default()
            },
            LiveAgent {
                name: "bravo-buil-sb".into(),
                status: "idle".into(),
                pane: "w2:p2".into(),
                ..Default::default()
            },
            LiveAgent {
                name: "bravo-buil-sb-2".into(),
                status: "working".into(),
                pane: "w2:p3".into(),
                ..Default::default()
            },
            LiveAgent {
                name: "random-human-pane".into(),
                status: "working".into(),
                pane: "w3:p1".into(),
                cwd: "/home/human/scratch".into(),
            },
        ];

        let live_statuses = live_statuses_from(&agents);
        let (dispatch_open, _) =
            crate::commands::dispatch::fleet_open_counts(&config, "", Some(&live_statuses));

        let text = render(&config, Some(&agents));
        let workers_line = text
            .lines()
            .find(|line| line.starts_with("workers "))
            .unwrap_or_else(|| panic!("no 'workers N/CAP' line in: {text}"));
        let workers_open: u32 = workers_line
            .strip_prefix("workers ")
            .and_then(|rest| rest.split('/').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("could not parse {workers_line:?}"));

        assert_eq!(
            dispatch_open, workers_open,
            "dispatch's fleet cap ({dispatch_open}) must equal the workers table's own \
             count ({workers_open}); mismatched text:\n{text}"
        );
    }

    /// `live_statuses_from`'s own helper: [`crate::commands::dispatch::
    /// fleet_open_counts`] joins a task to Herdr's listing by agent name
    /// to raw status, the same map [`status_by_name`] builds here — so
    /// every `fleet_cap_count_*` test feeds it the identical listing
    /// `render`/[`open_seat_counts`] already see, instead of re-deriving a
    /// second shape of the same fixture.
    fn live_statuses_from(agents: &[LiveAgent]) -> HashMap<String, String> {
        agents
            .iter()
            .filter(|agent| !agent.name.is_empty())
            .map(|agent| (agent.name.clone(), agent.status.clone()))
            .collect()
    }

    /// Card `20261116-b-8b5b22`: a non-recorded, still-status-file-less
    /// task whose pane Herdr's own `agent list` no longer carries at all
    /// (not merely idle — genuinely absent, long past its account's
    /// startup grace) must not go on costing
    /// [`crate::commands::dispatch::fleet_open_counts`] a fleet slot,
    /// exactly as `workers`'s own `workers N/CAP` line already excludes
    /// it. Before this card, the two disagreed: `fleet_open_counts` kept
    /// counting the dead task until a Nexus's reconcile finally closed it
    /// out, blocking dispatch in the meantime even though `workers` showed
    /// room.
    #[test]
    fn fleet_cap_count_excludes_an_unrecorded_task_whose_pane_is_gone() {
        let charlie = tempfile::tempdir().unwrap();
        std::fs::create_dir(charlie.path().join(".multiverse")).unwrap();
        std::fs::write(
            charlie.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20250101-builder-c00001\tproject=charlie\trole=builder\taccount=seat-b\tagent=charlie-buil-sb\n\
             2020-01-01T00:00:00Z\tevent=started\ttask=20250101-builder-c00001\tpane=w4:p1\n",
        )
        .unwrap();

        let toml = format!(
            "[accounts.seat-b]\nkind = \"codex\"\nshort = \"sb\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"/tmp\"\npool = \"codex:b\"\n\n\
             [projects.charlie]\npath = \"{charlie}\"\n\n\
             [projects.charlie.chains]\nbuilder = [\"seat-b\"]\n\n\
             [roles.builder]\nwrite_scope = \"declared\"\neffort = \"low\"\non_exhausted = \"degrade\"\nskills = []\n\n\
             [roles.builder.models]\ncodex = \"gpt-test\"\n",
            charlie = charlie.path().display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();

        // Herdr's live listing is reachable and confirmed empty — the
        // stranded task's agent (`charlie-buil-sb`) is simply gone, not
        // merely idle.
        let agents: [LiveAgent; 0] = [];

        let live_statuses = live_statuses_from(&agents);
        let (dispatch_open, _) =
            crate::commands::dispatch::fleet_open_counts(&config, "", Some(&live_statuses));
        assert_eq!(
            dispatch_open, 0,
            "a stranded task's pane is gone from Herdr's own listing; it must not hold a fleet slot"
        );

        let text = render(&config, Some(&agents));
        let workers_line = text
            .lines()
            .find(|line| line.starts_with("workers "))
            .unwrap_or_else(|| panic!("no 'workers N/CAP' line in: {text}"));
        let workers_open: u32 = workers_line
            .strip_prefix("workers ")
            .and_then(|rest| rest.split('/').next())
            .and_then(|n| n.parse().ok())
            .unwrap_or_else(|| panic!("could not parse {workers_line:?}"));

        assert_eq!(
            dispatch_open, workers_open,
            "dispatch's fleet cap ({dispatch_open}) must equal the workers table's own \
             count ({workers_open}); mismatched text:\n{text}"
        );
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
nexus sessions ? (excluded from workers; they do not count against max_concurrent)";
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

    /// Card `20261115-b-59e017`'s three-state proof: a pane's content hash
    /// unchanged past [`STALL_THRESHOLD`] is `stalled`; a pane whose
    /// content changed is not; a pane seen for the first time is never
    /// `stalled`, regardless of how far in the past `now` is handed in
    /// (there is no prior observation to compare against yet).
    ///
    /// The second case is also this card's revert check: `4e8d178`'s
    /// detector keyed solely on Herdr's `revision`/`state_change_seq`,
    /// which sit still for the whole length of a long-running command even
    /// though the agent is genuinely busy — "status working, revision
    /// unchanged, content changed" would still have read as stalled to
    /// that detector once those two counters stopped moving, which is
    /// exactly the false positive the card reports. This call site cannot
    /// reproduce that failure even by construction: `pane_hashes` is built
    /// from pane content alone ([`fetch_pane_hashes`]), and [`LiveAgent`]
    /// no longer carries a `revision` or `state_change_seq` field for
    /// anything to key on — a changed hash is the only way the age here
    /// ever resets. The store-level half of the same proof is
    /// `pane_watch_store::a_changed_hash_resets_the_last_changed_instant_even_with_other_fields_held_constant`.
    #[test]
    fn stalled_panes_matches_the_cards_three_state_proof() {
        let dir = tempfile::tempdir().unwrap();
        let pane_hash = |hash: &str| HashMap::from([("w1:p1".to_string(), Some(hash.to_string()))]);

        let first_seen = mv_core::time::now_utc();
        assert!(
            stalled_panes(dir.path(), &pane_hash("hash-a"), first_seen).is_empty(),
            "first observation must never be stalled"
        );

        let past_threshold = first_seen + STALL_THRESHOLD + time::Duration::seconds(1);
        assert_eq!(
            stalled_panes(dir.path(), &pane_hash("hash-a"), past_threshold),
            HashSet::from(["w1:p1".to_string()]),
            "unchanged content past the threshold must be stalled"
        );

        assert!(
            stalled_panes(dir.path(), &pane_hash("hash-b"), past_threshold).is_empty(),
            "a pane whose content changed since its last observation must not be stalled"
        );
    }

    #[test]
    fn stalled_panes_fails_open_to_empty_when_no_pane_carries_a_change_signal() {
        let dir = tempfile::tempdir().unwrap();
        let pane_hashes = HashMap::from([("w1:p1".to_string(), None)]);
        assert!(stalled_panes(dir.path(), &pane_hashes, mv_core::time::now_utc()).is_empty());
    }

    #[test]
    fn fetch_pane_hashes_keeps_a_failed_read_as_a_missing_signal_not_a_dropped_pane() {
        // `herdr_binary()` never resolves to a real executable under
        // `cargo test`, so this exercises the same fail-open path a real
        // spawn failure would: the pane is still a key in the map, just
        // with no hash to compare — never silently absent from the cycle.
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            pane: "w1:p1".into(),
            ..Default::default()
        }];
        let pane_hashes = fetch_pane_hashes(&herdr_binary(), &agents);
        assert_eq!(pane_hashes, HashMap::from([("w1:p1".to_string(), None)]));
    }

    #[test]
    fn pane_read_args_matches_the_conformance_fixture_for_an_ordinary_pane_id() {
        assert_eq!(
            pane_read_args("w1:p1"),
            ["pane", "read", "w1:p1", "--lines", "200"]
        );
    }

    #[test]
    fn pane_read_args_guards_a_flag_looking_pane_id_with_a_double_dash() {
        assert_eq!(
            pane_read_args("--help"),
            ["pane", "read", "--lines", "200", "--", "--help"]
        );
    }

    #[test]
    fn render_with_stalled_overrides_display_only_not_seat_occupancy() {
        // A stalled pane must never read as freed: occupancy still comes
        // from Herdr's own `agent_status` ("working" here), never from the
        // display override — the card's "display only, never signal
        // anything" constraint.
        let root = tempfile::tempdir().unwrap();
        let mut config = config();
        config.projects.get_mut("sandbox").unwrap().path =
            mv_config::placeholder::MaybePlaceholder::parse(root.path().to_str().unwrap());
        std::fs::create_dir(root.path().join(".multiverse")).unwrap();
        let task = "20261105-builder-abcdef";
        let stamp = "2020-01-01T00:00:00Z";
        std::fs::write(
            root.path().join(".multiverse/routing.log"),
            format!(
                "{stamp}\tevent=dispatch\ttask={task}\tproject=sandbox\trole=builder\taccount=seat-b\tagent=sandbox-scou-sb\n\
                 {stamp}\tevent=started\ttask={task}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".into(),
            status: "working".into(),
            pane: "w1:p1".into(),
            ..Default::default()
        }];
        let stalled: HashSet<String> = ["w1:p1".to_string()].into_iter().collect();
        let text = render_with_stalled(&config, Some(&agents), &stalled);
        assert!(text.contains("stalled"), "{text}");
        assert!(text.contains("seats: seat-b 1"), "{text}");
    }

    #[test]
    fn json_escape_round_trips_quotes_backslashes_and_control_characters() {
        let escaped = json_escape("a\"b\\c\nd\u{7}e");
        let parsed: serde_json::Value = serde_json::from_str(&escaped).unwrap();
        assert_eq!(parsed, "a\"b\\c\nd\u{7}e");
    }

    /// Proof for Scout finding `20261117-scout-c950ad`#4: `--json` carries
    /// the same table shape `render_matches_the_conformance_shape_for_a_
    /// recognized_worker` pins for text, every documented key present, and
    /// the "no data joined" fields (`project`/`task`/`outcome`/`age`,
    /// `\u{2014}` in text) read as JSON `null`.
    #[test]
    fn render_json_includes_every_documented_key_for_a_recognized_worker() {
        let config = config();
        let agents = [LiveAgent {
            name: "sandbox-scou-sb".to_string(),
            status: "working".to_string(),
            ..Default::default()
        }];
        let report = build_report(&config, Some(&agents), &HashSet::new());
        let halt = mv_state::halt::HaltState {
            halted: false,
            reason: None,
            at: None,
        };
        let json = render_json(&report, &halt);
        assert!(
            !json.contains('\n'),
            "json output must be a single line: {json}"
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        for key in [
            "halted",
            "max_concurrent",
            "nexus_sessions",
            "projects",
            "seats",
            "worker_count",
            "workers",
        ] {
            assert!(
                value.get(key).is_some(),
                "missing top-level key {key:?}: {json}"
            );
        }
        assert_eq!(value["halted"], serde_json::Value::Null);
        assert_eq!(value["max_concurrent"], 3);
        assert_eq!(value["nexus_sessions"], 0);
        assert_eq!(value["worker_count"], 1);

        let workers = value["workers"].as_array().expect("workers array");
        assert_eq!(workers.len(), 1);
        let row = &workers[0];
        for key in [
            "age", "outcome", "project", "role", "seat", "status", "task",
        ] {
            assert!(
                row.get(key).is_some(),
                "missing worker row key {key:?}: {json}"
            );
        }
        assert_eq!(row["role"], "scout");
        assert_eq!(row["seat"], "seat-b");
        assert_eq!(row["status"], "working");
        assert_eq!(row["project"], serde_json::Value::Null);
        assert_eq!(row["task"], serde_json::Value::Null);
        assert_eq!(row["outcome"], serde_json::Value::Null);
        assert_eq!(row["age"], serde_json::Value::Null);

        let seats = value["seats"].as_array().expect("seats array");
        assert_eq!(seats.len(), 1);
        assert_eq!(seats[0]["name"], "seat-b");
        assert_eq!(seats[0]["open"], 0);

        let projects = value["projects"].as_array().expect("projects array");
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0]["name"], "sandbox");
        assert_eq!(projects[0]["open"], 0);
        assert_eq!(projects[0]["ceiling"], 3);
    }

    #[test]
    fn render_json_fails_open_to_null_when_herdr_is_unreachable() {
        let config = config();
        let report = build_report(&config, None, &HashSet::new());
        let halt = mv_state::halt::HaltState {
            halted: false,
            reason: None,
            at: None,
        };
        let json = render_json(&report, &halt);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["workers"], serde_json::Value::Null);
        assert_eq!(value["worker_count"], serde_json::Value::Null);
        assert_eq!(value["nexus_sessions"], serde_json::Value::Null);
        assert_eq!(value["seats"][0]["open"], serde_json::Value::Null);
        assert_eq!(value["projects"][0]["open"], serde_json::Value::Null);
    }

    /// `run`'s own reason for folding halt state into the JSON object
    /// rather than printing `halt_banner`'s line first: stdout must stay
    /// one parseable value.
    #[test]
    fn render_json_embeds_the_halt_state_instead_of_a_banner_line() {
        let config = config();
        let report = build_report(&config, Some(&[]), &HashSet::new());
        let halt = mv_state::halt::HaltState {
            halted: true,
            reason: Some("fuzz maintenance".into()),
            at: Some("2026-11-03T00:00:00+00:00".into()),
        };
        let json = render_json(&report, &halt);
        assert!(
            !json.contains('\n'),
            "json output must be a single line: {json}"
        );
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["halted"]["reason"], "fuzz maintenance");
        assert_eq!(value["halted"]["at"], "2026-11-03T00:00:00+00:00");
        assert_eq!(value["workers"], serde_json::json!([]));
    }

    /// Card for this task (`20261117-builder-0cbb48`): `--json` must not
    /// change `workers`' plain-text output at all. `json_flag_is_accepted`
    /// below proves the flag parses; every other `render`/`render_with_
    /// stalled` test in this module still asserts the identical literal
    /// text it always has, with no `--json`-aware branch in that path.
    #[test]
    fn json_flag_is_accepted_by_the_parser() {
        let m = args::command()
            .try_get_matches_from(["multiverse", "workers", "--json"])
            .unwrap();
        let workers = m.subcommand_matches("workers").unwrap();
        assert!(workers.get_flag("json"));
    }

    /// Writes `body` as a fake Herdr in `dir`, then holds it open for
    /// writing for 30ms on a background thread — exactly what makes
    /// Linux's `execve` return `ExecutableFileBusy`, the condition another
    /// test thread's `fork()` leaves behind by inheriting its write fd (see
    /// `mv_herdr::run`'s own
    /// `run_retries_past_a_transient_executable_file_busy_and_still_succeeds`).
    #[cfg(target_os = "linux")]
    fn busy_fake_herdr(
        dir: &Path,
        body: &str,
    ) -> (std::path::PathBuf, std::thread::JoinHandle<()>) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("herdr-fake");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let held_open = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        let closer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            drop(held_open);
        });
        (path, closer)
    }

    /// Card `20261119-b-5f7cf6`: `workers`' `agent list` must ride out a
    /// transient `ExecutableFileBusy` through the shared runner's retry
    /// rather than render every count as `?`. Linux-only: macOS `exec`
    /// ignores an open write fd, so this would pass vacuously there.
    #[test]
    #[cfg(target_os = "linux")]
    fn agent_list_survives_a_transient_executable_file_busy_on_the_herdr_binary() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, closer) = busy_fake_herdr(
            dir.path(),
            r#"printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","name":"sandbox-scou-sb","agent_status":"working"}]}}'"#,
        );

        let agents = fetch_live_agents(fake.to_str().unwrap());
        closer.join().unwrap();

        let agents = agents.expect("agent list must survive ExecutableFileBusy");
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].name, "sandbox-scou-sb");
        assert_eq!(agents[0].status, "working");
        assert_eq!(agents[0].pane, "w1:p1");
    }

    /// Same card, for the stall watch's `pane read`: a busy binary must
    /// not read as "no signal this cycle", and the argv reaching Herdr is
    /// unchanged by routing it through the runner.
    #[test]
    #[cfg(target_os = "linux")]
    fn pane_read_survives_a_transient_executable_file_busy_on_the_herdr_binary() {
        let dir = tempfile::tempdir().unwrap();
        let (fake, closer) = busy_fake_herdr(dir.path(), r#"printf 'argv:%s\n' "$*""#);

        let content = fetch_pane_content(fake.to_str().unwrap(), "w1:p1");
        closer.join().unwrap();

        assert_eq!(
            content.as_deref(),
            Some("argv:pane read w1:p1 --lines 200\n")
        );
    }
}

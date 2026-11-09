//! [`LiveSource`]: the real [`super::board::BoardDataSource`], wiring
//! `multiverse board` to configured projects (`mv-config`), their tasks
//! and backlog (`mv-state::{tasks,backlog,index}`), fleet account/seat
//! health (`mv-engine::{breaker,seatstate,limits}`), and a live Herdr
//! `agent list`/`workspace list` (shelled out directly, the same
//! `std::process::Command` + hand-rolled JSON reader pattern
//! `commands::workers` already uses — `mv-cli` does not depend on
//! `mv-herdr` or `serde_json`, and widening its `Cargo.toml` is outside
//! this task's declared write scope).
//!
//! A one-shot board reconciles settled tasks, closes confirmed idle panes,
//! and expires stranded AWAITING tasks through `mv_state::tasks::reconcile_project`.
//! `--no-reconcile`, `--read-only`, and watch skip these lifecycle writes.
//! A completed pass stamps the project `.multiverse/orchestrator-pass` marker.
//! A failed Herdr listing also skips reconciliation. Prompt reads never trip
//! breakers or append limit events. Rendering remains in the pure `mv-board` crate.
//!
//! Remaining resolution details:
//!
//! - An UNRECORDED task whose recorded pane is confirmed gone, or whose
//!   status file says `done` while its own writing-role lane still holds
//!   uncommitted work (`dirty_lane_detail`), is now forced into BLOCKED by
//!   `mv_board::card::split_dispatched`, mirroring `board.py`'s
//!   `_split_dispatched`'s own `if t.dirty_done or t.reason.startswith
//!   ("pane gone, status "):` bypass (critic-sweep-board.md finding 6).
//!   The dirty-lane check itself (`dirty_lane_detail`,
//!   `lane_porcelain`, `describe_dirty` below) is narrowed relative to
//!   `worktrees.DirtyDetail.describe()`: it reports modified/untracked
//!   counts and up to three example paths, but not the trailing "N
//!   commits ahead", which needs a merge-base lookup this file does not
//!   make.
//! - `limited_count`/`acknowledged` come from a raw count of
//!   `event=limit`/`event=acknowledged` records naming the task — real,
//!   but not `supersede`-aware the way `board.py`'s fuller resolve is.
//! - OWED REPLIES, MAIL, and LAST CYCLE are filled from each project's
//!   routing log / `activity.cursor` / `activity.lastseen` /
//!   `orchestrator-pass`. MAIL is one line per project **in the board's
//!   view** even at unread 0 (`board.py`'s `_mail_card_lines`); a project
//!   with no live orchestrator pane is marked `offline (no live
//!   orchestrator)` rather than stale. LAST CYCLE is scoped to cycling
//!   projects only (live orchestrator pane + interactive seat), so an
//!   empty Herdr listing renders `(none)`. `read_inbox` is never called:
//!   it writes `activity.lastseen`, which `--read-only` must not do.
//! - Every account now gets a quota window line (`LiveSource::read_quota`,
//!   card `20261102-b-c1a2e5`): Codex keeps its live session-log reader
//!   (`limits::read_and_record_codex_usage`); every other kind reads back
//!   whatever the shared `mv_state::gauge` store already holds for that
//!   pool (plain structs, not `serde_json::Value` — no new dependency
//!   needed), falling back to the same `"window: n/a (not reported)"` line
//!   `board.py`'s `_window_lines` does when nothing has been recorded yet.
//! - Claude accounts now get a real 24h token total (`claude_tokens_24h`,
//!   card `20261102-b-2c4922`'s token-usage half): every `*.jsonl` under
//!   `<config_dir>/projects/`, summed with `mv_engine::usage::
//!   sum_assistant_tokens_since` — the same per-message, not per-file,
//!   24h cutoff that function was already built for. Codex accounts now
//!   get the same treatment (`codex_tokens_24h`, card `20261102-b-06b7a0`):
//!   every `*.jsonl` under `<config_dir>/sessions/` modified in the last
//!   24h, read with the new `mv_engine::usage::codex_session_tokens`
//!   primitive — each file's own newest `total_token_usage` reading is
//!   already cumulative for that whole session, so this sums *across*
//!   files rather than across lines within one, mirroring Python's
//!   `limits.codex_tokens_last_24h`/`_codex_session_tokens`. Grok still
//!   leaves `tokens_24h: None` (renders as the existing no-data case):
//!   Python's own `usage.py` never computes a real number for it either
//!   (no evidenced log source).

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use time::OffsetDateTime;

use mv_board::accounts::{AccountSnapshot, AccountState};
use mv_board::card::{
    BacklogItemSnapshot, BacklogStatus, DispatchedTaskSnapshot, LiveAgentStatus, TaskOutcome,
};
use mv_board::render::{BoardSnapshot, FleetHealth, FleetSnapshot, ProjectSnapshot};
use mv_board::status_cards::{LastCycleEntry, MailEntry, OwedReply, SeatsSnapshot};
use mv_config::load::{Account, Config, Project};
use mv_config::roles::Role;
use mv_core::time::now_utc;
use mv_engine::{breaker, limits, usage};
use mv_state::activity::{self, ActivityError, ProjectLog};
use mv_state::backlog;
use mv_state::index::{records as index_records, Selector};
use mv_state::log::LogRecord;
use mv_state::paths::is_valid_task_id;
use mv_state::tasks::{AgentPanes, ReconcileRequest, TaskStatus};

use super::board::{BoardDataSource, BoardScope, BoardView};
use crate::exit::Failure;

// --- a minimal, read-only JSON reader, duplicated from
// `commands::workers` rather than shared: that module's copy is private
// to it, and neither `mv-herdr` nor `serde_json` is a dependency of this
// crate (see the module doc above). ---

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
                            let mut code = self.parse_hex_quad()?;
                            if (0xD800..=0xDBFF).contains(&code) {
                                self.expect(b'\\')?;
                                self.expect(b'u')?;
                                let low = self.parse_hex_quad()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return None;
                                }
                                code = 0x10000 + ((code - 0xD800) << 10) + low - 0xDC00;
                            }
                            out.push(char::from_u32(code)?);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    let rest = &self.bytes[self.pos - 1..];
                    let ch_len = utf8_len(b);
                    let slice = rest.get(..ch_len)?;
                    out.push_str(std::str::from_utf8(slice).ok()?);
                    self.pos += ch_len - 1;
                }
            }
        }
    }

    fn parse_hex_quad(&mut self) -> Option<u32> {
        let hex = std::str::from_utf8(self.bytes.get(self.pos..self.pos + 4)?).ok()?;
        let code = u32::from_str_radix(hex, 16).ok()?;
        self.pos += 4;
        Some(code)
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

// --- Herdr access -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct LiveAgent {
    pane_id: String,
    name: String,
    status: String,
}

/// One account's most recent quota reading — percent left on its primary
/// window, and secondary (e.g. weekly) window if it has one, plus the
/// per-window clocks `at_or_below` needs to reuse
/// [`super::accounts::gauge_is_stale`]. See `LiveSource::read_quota`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct QuotaReading {
    primary: Option<f64>,
    secondary: Option<f64>,
    observed_at: Option<OffsetDateTime>,
    resets_at: Option<OffsetDateTime>,
    resets_at_secondary: Option<OffsetDateTime>,
    window_minutes: Option<u32>,
    window_minutes_secondary: Option<u32>,
}

impl QuotaReading {
    /// Copy this gauge entry's observation/reset/window clocks onto a
    /// reading whose percents are already filled — the same fields
    /// [`super::accounts::gauge_is_stale`] consults.
    fn with_clocks(mut self, entry: &mv_state::gauge::GaugeEntry) -> Self {
        self.observed_at = entry
            .observed_at
            .as_deref()
            .and_then(breaker::parse_stored_timestamp);
        self.resets_at = entry
            .resets_at
            .as_deref()
            .and_then(breaker::parse_stored_timestamp);
        self.resets_at_secondary = entry
            .resets_at_secondary
            .as_deref()
            .and_then(breaker::parse_stored_timestamp);
        self.window_minutes = entry.window_minutes;
        self.window_minutes_secondary = entry.window_minutes_secondary;
        self
    }

    /// Whether either *fresh* window is at or below `floor` — `board.py`'s
    /// `is_draining` check, which treats either window crossing the fleet
    /// quota floor as draining, not only the primary one. A window
    /// [`super::accounts::gauge_is_stale`] would discard is ignored, the
    /// same per-window gate `accounts::draining_detail` already applies: a
    /// stale primary must not hide a still-live, still-critical secondary,
    /// and vice versa.
    fn at_or_below(&self, floor: f64, now: OffsetDateTime) -> bool {
        let primary = (!super::accounts::gauge_is_stale(
            self.resets_at,
            self.observed_at,
            self.window_minutes,
            now,
        ))
        .then_some(self.primary)
        .flatten()
        .is_some_and(|p| p <= floor);
        let secondary = (!super::accounts::gauge_is_stale(
            self.resets_at_secondary,
            self.observed_at,
            self.window_minutes_secondary,
            now,
        ))
        .then_some(self.secondary)
        .flatten()
        .is_some_and(|p| p <= floor);
        primary || secondary
    }
}

/// `$HERDR_BIN_PATH` when set and non-blank, else the bare `herdr` token —
/// the same semantics every other direct-subprocess command in this crate
/// (`doctor`, `workers`) already duplicates locally.
#[cfg(not(test))]
fn herdr_binary() -> String {
    match std::env::var("HERDR_BIN_PATH") {
        Ok(value) if !value.trim().is_empty() => value,
        _ => "herdr".to_string(),
    }
}

/// Never resolves to a real binary under `cargo test`.
#[cfg(test)]
fn herdr_binary() -> String {
    "mv-cli-test-herdr-stub-should-never-exist".to_string()
}

/// One `herdr agent list` call. `Err` means "could not get a trustworthy
/// reply" (spawn failure, nonzero exit, non-JSON stdout, or JSON lacking a
/// `result` object) — every caller renders that as "unreachable", never as
/// an empty fleet. Carries why, not just that: `cmd_board`'s own "herdr
/// unavailable — showing recorded state unchanged (...)" banner
/// (`cli.py:8375`) names the actual `DispatchError`, and a caller here
/// needs that same text to print the matching banner rather than silently
/// rendering a fleet with every agent missing.
fn fetch_live_agents(binary: &str) -> Result<Vec<LiveAgent>, String> {
    let output = std::process::Command::new(binary)
        .args(["agent", "list"])
        .output()
        .map_err(|error| {
            // Same wording `mv_herdr::error::HerdrError::MissingBinary`
            // uses for the identical failure — this crate has no
            // dependency on `mv-herdr` to import that type from (see this
            // module's doc), so the format string is duplicated rather
            // than the dependency widened.
            format!(
                "herdr binary {binary:?} could not be run: {error}.\n\
                 Herdr must be installed and on PATH, or HERDR_BIN_PATH set to its path."
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "herdr agent list exited {}",
            output
                .status
                .code()
                .map_or_else(|| "?".to_string(), |code| code.to_string())
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_live_agents(stdout.trim())
}

fn parse_live_agents(stdout: &str) -> Result<Vec<LiveAgent>, String> {
    let value = JsonParser::parse(stdout.trim())
        .ok_or_else(|| "herdr agent list did not print valid JSON".to_string())?;
    let result = value
        .get("result")
        .and_then(Json::as_object)
        .ok_or_else(|| "herdr agent list's reply had no \"result\" object".to_string())?;
    let agents = result
        .iter()
        .find(|(k, _)| k == "agents")
        .map(|(_, v)| v)
        .and_then(Json::as_array)
        .unwrap_or(&[]);
    Ok(agents
        .iter()
        .map(|agent| LiveAgent {
            pane_id: agent
                .get("pane_id")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
            name: agent
                .get("name")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
            status: agent
                .get("agent_status")
                .and_then(Json::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

fn agent_statuses(agents: &[LiveAgent]) -> Vec<mv_state::tasks::LiveAgentStatus> {
    agents
        .iter()
        .filter(|agent| !agent.pane_id.is_empty())
        .map(|agent| mv_state::tasks::LiveAgentStatus {
            pane_id: agent.pane_id.clone(),
            name: (!agent.name.is_empty()).then(|| agent.name.clone()),
            status: agent.status.parse().ok(),
        })
        .collect()
}

/// Reuse the board's first fleet probe; pane teardown still requires a fresh one.
struct BoardAgents<'a> {
    binary: &'a str,
    first: Option<Vec<mv_state::tasks::LiveAgentStatus>>,
}

impl AgentPanes for BoardAgents<'_> {
    fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
        self.list_checked()
            .unwrap_or_default()
            .into_iter()
            .map(|agent| mv_state::tasks::LiveAgent {
                pane_id: agent.pane_id,
            })
            .collect()
    }

    fn list_checked(&mut self) -> Option<Vec<mv_state::tasks::LiveAgentStatus>> {
        if let Some(first) = self.first.take() {
            return Some(first);
        }
        fetch_live_agents(self.binary)
            .ok()
            .map(|agents| agent_statuses(&agents))
    }

    fn close(&mut self, pane: &str) -> bool {
        std::process::Command::new(self.binary)
            .args(["pane", "close", pane])
            .output()
            .is_ok_and(|output| output.status.success())
    }
}

/// Adapter for the engine's report-only stuck probe. Only pane reads are allowed.
struct BoardReads<'a>(&'a str);

impl mv_engine::runner::Herdr for BoardReads<'_> {
    fn call(
        &mut self,
        command: &mv_engine::mv_herdr::argv::HerdrCommand,
        timeout: std::time::Duration,
    ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError> {
        if command.argv.get(1).map(String::as_str) != Some("pane")
            || command.argv.get(2).map(String::as_str) != Some("read")
        {
            return Err(mv_engine::runner::RunnerError::Refused(
                "board probe only permits pane reads".into(),
            ));
        }
        let mut command = command.clone();
        command.argv[0] = self.0.to_owned();
        Ok(mv_engine::mv_herdr::run::run(&command, Some(timeout))?)
    }
}

fn workspace_labels(value: &Json) -> Option<Vec<String>> {
    let result = value.get("result")?.as_object()?;
    let workspaces = result
        .iter()
        .find(|(k, _)| k == "workspaces")
        .map(|(_, v)| v)
        .and_then(Json::as_array)
        .unwrap_or(&[]);
    Some(
        workspaces
            .iter()
            .filter_map(|workspace| workspace.get("label")?.as_str().map(str::to_string))
            .collect(),
    )
}

// --- scope resolution ----------------------------------------------------

/// What [`LiveSource::resolve_scope_with`] handed back: a scoped project list
/// with its scope-header line, or a fully-formed message (the degraded
/// `--active` outcomes `board.py` can reach without an error) in place of
/// a rendered board.
enum Resolved {
    Projects(Vec<Project>, Option<String>),
    Message(Vec<String>),
}

/// `project: {name} only` for one project, `projects: a, b, c` for more —
/// `cmd_board`'s own scope-header fallback once neither the `fleet` nor
/// `active` scope kind applies.
fn scope_header(projects: &[Project]) -> String {
    if projects.len() == 1 {
        format!("project: {} only", projects[0].name)
    } else {
        format!(
            "projects: {}",
            projects
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Select the closest configured ancestor of cwd, including nested projects.
fn cwd_project(config: &Config) -> Option<Project> {
    cwd_project_at(config, &std::env::current_dir().ok()?)
}

fn cwd_project_at(config: &Config, cwd: &Path) -> Option<Project> {
    let canonical_cwd = std::fs::canonicalize(cwd).ok()?;
    projects_in_declaration_order(config)
        .into_iter()
        .filter_map(|project| {
            let path = std::fs::canonicalize(project.path.value()?).ok()?;
            canonical_cwd.starts_with(&path).then_some((path, project))
        })
        .max_by_key(|(path, _)| path.components().count())
        .map(|(_, project)| project)
}

/// A project's abbreviation for a merged board, derived the same way a
/// missing `short` reads as "nothing configured yet" everywhere else in
/// `board.py` / `cli.cmd_board`: `project.short or project.name`. An
/// unconfigured short must not invent a four-character slug — MAIL,
/// LAST CYCLE, and merged-view footers all print this label, and the
/// fuzz config has no `short` so Python shows the full project name.
fn project_short(project: &Project) -> String {
    match &project.short {
        Some(short) if !short.is_empty() => short.clone(),
        _ => project.name.clone(),
    }
}

/// Every configured project, in `config.toml`'s own declaration order
/// rather than `config.projects`' alphabetical `BTreeMap` order — mirrors
/// `commands::backlog`'s own `projects_in_declaration_order` (card
/// `20261102-b-624f17`) and `commands::tasks`'s copy of the same fix: the
/// `--all` board used to list every project alphabetically instead of
/// matching Python's board, which iterates `config.projects` in file
/// order. `project_order` already guarantees every key in `projects`
/// appears exactly once (`mv_config::load::Config::project_order`'s own
/// doc comment), so the lookup below cannot miss.
fn projects_in_declaration_order(config: &Config) -> Vec<Project> {
    config
        .project_order
        .iter()
        .filter_map(|name| config.projects.get(name).cloned())
        .collect()
}

// --- the real data source -------------------------------------------------

pub(crate) struct LiveSource {
    config: Config,
    state_dir: PathBuf,
    active: bool,
    reconcile: bool,
    agent_fingerprint: std::cell::Cell<u64>,
}

impl LiveSource {
    pub(crate) fn load(args: &ArgMatches) -> Result<Self, Failure> {
        let path = mv_config::load::resolve_config_path_from_env(
            args.get_one::<String>("config").map(Path::new),
        )
        .map_err(|error| {
            Failure::Operation(format!("could not resolve the config path: {error}"))
        })?;
        let config = mv_config::load::load_config(&path)
            .map_err(|error| Failure::Config(error.to_string()))?;
        let state_dir = mv_config::load::config_state_dir_from_env(&path);
        Ok(Self {
            config,
            state_dir,
            active: args.get_flag("active"),
            reconcile: !args.get_flag("no-reconcile")
                && !args.get_flag("read-only")
                && args.get_one::<crate::args::WatchArg>("watch").is_none(),
            agent_fingerprint: std::cell::Cell::new(0),
        })
    }

    /// `[board].refresh_seconds`, the interval a bare `board --watch` uses.
    pub(crate) fn refresh_seconds(&self) -> u32 {
        self.config.board.refresh_seconds.max(1)
    }

    #[cfg(test)]
    fn resolve_scope(&self, scope: &BoardScope) -> Result<Resolved, Failure> {
        self.resolve_scope_with(scope, &herdr_binary())
    }

    fn resolve_scope_with(&self, scope: &BoardScope, binary: &str) -> Result<Resolved, Failure> {
        let resolved = self.resolve_base_scope(scope)?;
        if !self.active && *scope != BoardScope::Active {
            return Ok(resolved);
        }
        let Resolved::Projects(mut projects, header) = resolved else {
            return Ok(resolved);
        };
        let output = match std::process::Command::new(binary)
            .args(["workspace", "list"])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                return Ok(Resolved::Message(vec![format!(
                    "herdr unavailable — showing recorded state unchanged ({error})"
                )]))
            }
        };
        if !output.status.success() {
            return Err(Failure::Operation(format!(
                "could not reach Herdr to check active workspaces: exit {}",
                output
                    .status
                    .code()
                    .map_or_else(|| "?".into(), |code| code.to_string())
            )));
        }
        let labels = JsonParser::parse(String::from_utf8_lossy(&output.stdout).trim())
            .as_ref()
            .and_then(workspace_labels)
            .unwrap_or_default();
        projects.retain(|project| labels.contains(&project.name));
        if projects.is_empty() {
            return Ok(Resolved::Message(vec!["no active projects".into()]));
        }
        let word = if projects.len() == 1 {
            "project"
        } else {
            "projects"
        };
        let header = if *scope == BoardScope::Active {
            let total = self.config.projects.len();
            let total_word = if total == 1 { "project" } else { "projects" };
            format!("fleet: active {} of {total} {total_word}", projects.len())
        } else if header
            .as_deref()
            .is_some_and(|h| h.starts_with("fleet: all "))
        {
            format!("fleet: all {} {word}", projects.len())
        } else {
            scope_header(&projects)
        };
        Ok(Resolved::Projects(projects, Some(header)))
    }

    fn resolve_base_scope(&self, scope: &BoardScope) -> Result<Resolved, Failure> {
        match scope {
            BoardScope::Named(names) => {
                let mut seen = HashSet::new();
                let mut unknown = Vec::new();
                let mut projects = Vec::new();
                for name in names {
                    if !seen.insert(name.clone()) {
                        continue;
                    }
                    match self.config.projects.get(name) {
                        Some(project) => projects.push(project.clone()),
                        None => unknown.push(name.clone()),
                    }
                }
                if !unknown.is_empty() {
                    let mut known: Vec<&str> =
                        self.config.projects.keys().map(String::as_str).collect();
                    known.sort_unstable();
                    let known = if known.is_empty() {
                        "(none)".to_string()
                    } else {
                        known.join(", ")
                    };
                    return Err(Failure::Config(format!(
                        "no such project: {}; configured: {known} (see `multiverse projects`)",
                        unknown.join(", ")
                    )));
                }
                let header = scope_header(&projects);
                Ok(Resolved::Projects(projects, Some(header)))
            }
            BoardScope::All => {
                let projects = projects_in_declaration_order(&self.config);
                let word = if projects.len() == 1 {
                    "project"
                } else {
                    "projects"
                };
                let header = format!("fleet: all {} {word}", projects.len());
                Ok(Resolved::Projects(projects, Some(header)))
            }
            BoardScope::Here => match cwd_project(&self.config) {
                Some(project) => {
                    let header = scope_header(std::slice::from_ref(&project));
                    Ok(Resolved::Projects(vec![project], Some(header)))
                }
                None => Err(Failure::Config(
                    "--here requires the current directory to be inside a configured project"
                        .to_owned(),
                )),
            },
            BoardScope::Active => Ok(Resolved::Projects(
                projects_in_declaration_order(&self.config),
                None,
            )),
            BoardScope::Default => match cwd_project(&self.config) {
                Some(project) if project.name != mv_state::activity::HUB_PROJECT_NAME => {
                    let header = scope_header(std::slice::from_ref(&project));
                    Ok(Resolved::Projects(vec![project], Some(header)))
                }
                _ => {
                    let projects = projects_in_declaration_order(&self.config);
                    let word = if projects.len() == 1 {
                        "project"
                    } else {
                        "projects"
                    };
                    let header = format!("fleet: all {} {word}", projects.len());
                    Ok(Resolved::Projects(projects, Some(header)))
                }
            },
        }
    }

    fn account_snapshot(
        &self,
        account: &Account,
        agents: Option<&[LiveAgent]>,
        _as_of: &str,
        now: OffsetDateTime,
    ) -> AccountSnapshot {
        let status = breaker::pool_status(&self.state_dir, &account.pool);
        // Both cap and lend are seat-state, written by `accounts limit`/
        // `accounts lend` keyed by the account's own config name
        // (`commands::accounts::LiveConfig::seat_name` resolves a short or
        // name argument to `account.name` before calling either
        // `seatstate::set_cap`/`set_lend`) — not its `short` abbreviation,
        // which is reserved for Herdr pane-name matching (`seat_open_count`
        // below). Reading either one back by `short` silently missed every
        // cap/lend a human actually set (critic finding 6).
        let cap = mv_engine::seatstate::get_cap(&self.state_dir, &account.name);
        let open = agents.map(|list| seat_open_count(list, &account.short));
        let lent = mv_engine::seatstate::is_lent(&self.state_dir, &account.name);
        // `account.unavailable_until` is a scheduled return time, not a
        // standing flag — only still-future counts against the seat
        // (`selector::availability` check 2 / `accounts._availability`).
        let unavailable_until_future = account
            .unavailable_until
            .as_deref()
            .and_then(|value| mv_core::time::parse_flexible(value).ok())
            .is_some_and(|until| until > now);
        // A capped seat whose open-agent count is unreadable (Herdr
        // unreachable) reads AT CAP, not AVAILABLE — `selector::
        // availability` check 5's own fail-closed posture (`None` is
        // "treated as at-cap for a capped seat, not as zero open"); an
        // uncapped seat never reaches this check at all. Confirmed on the
        // real fleet via `scripts/rehearsal/read_parity.sh` (this task's
        // `result.md`): every capped seat read AVAILABLE here with a fake
        // `HERDR_BIN_PATH`, where Python's board reads it as full.
        let at_cap = cap.is_some_and(|cap| open.is_none_or(|open| open >= cap));

        let quota = self.read_quota(account);
        let floor = self.config.dispatch_min_quota_percent;
        let draining = floor > 0
            && quota
                .as_ref()
                .is_some_and(|q| q.at_or_below(f64::from(floor), now));

        // Mirrors `selector::availability`'s check order (1. available, 2.
        // unavailable_until, 3. lent, 4. breaker, 5. cap) with the same
        // HALF-OPEN carve-out `board.py`'s `account_states` gives it ahead
        // of that chain: a trial seat still reads HALF-OPEN, not FENCED,
        // as long as the three human-owned gates (available/until/lent)
        // and the cap are otherwise clear.
        let (state, detail) = if status.half_open
            && account.available
            && !unavailable_until_future
            && lent
            && !at_cap
        {
            (
                AccountState::HalfOpen,
                Some(format!(
                    "(trial: {})",
                    status.trial_task_id.as_deref().unwrap_or("unclaimed")
                )),
            )
        } else if !account.available {
            (
                AccountState::Fenced,
                Some(
                    account
                        .unavailable_reason
                        .clone()
                        .unwrap_or_else(|| "unavailable in config".to_string()),
                ),
            )
        } else if unavailable_until_future {
            (
                AccountState::Fenced,
                Some("unavailable until a future time".to_string()),
            )
        } else if !lent {
            (
                AccountState::Fenced,
                Some(
                    "lent off \u{2014} not in chains right now (`accounts lend` to change it)"
                        .to_string(),
                ),
            )
        } else if status.open {
            (
                AccountState::Fenced,
                Some(fenced_board_detail(&status, now)),
            )
        } else if at_cap {
            (AccountState::AtCap, None)
        } else if draining {
            (
                AccountState::Draining,
                Some(format!(
                    "draining: at or below the fleet quota floor of {floor}%"
                )),
            )
        } else {
            (AccountState::Available, None)
        };

        let mut quota_windows = Vec::new();
        match &quota {
            Some(reading) => {
                if let Some(primary) = reading.primary {
                    quota_windows.push(format!("primary: {primary:.0}% left"));
                }
                if let Some(secondary) = reading.secondary {
                    quota_windows.push(format!("secondary: {secondary:.0}% left"));
                }
            }
            None => quota_windows.push(if account.kind == "claude" {
                "window: n/a (API billing)".to_string()
            } else {
                "window: n/a (not reported)".to_string()
            }),
        }

        let model = account
            .model
            .clone()
            .or_else(|| {
                self.config
                    .roles
                    .get(&Role::Builder)
                    .and_then(|settings| settings.models.get(&account.kind).cloned())
            })
            .unwrap_or_else(|| account.vendor.clone());
        let over_cap = matches!((open, cap), (Some(o), Some(c)) if o > c);
        // Python's `_account_card_corner` only prints `as of HH:MM` when
        // `account_usage.cached_at` is set. A seat with no usage snapshot
        // (the fuzz CONFIG's empty Codex home) draws a bare title.

        AccountSnapshot {
            name: account.name.clone(),
            model,
            vendor: account.vendor.clone(),
            kind: account.kind.clone(),
            state,
            seats_open: open,
            seats_free: cap.zip(open).map(|(cap, open)| cap.saturating_sub(open)),
            seats_total: cap,
            over_cap,
            detail,
            quota_windows,
            tokens_24h: self.tokens_24h(account, now),
            as_of: None,
        }
    }

    /// `account`'s most recent quota reading, for the ACCOUNTS card's
    /// window line(s) and the DRAINING check above. Codex refreshes the
    /// gauge live from its own session logs
    /// (`limits::read_and_record_codex_usage`, keeping its existing
    /// behavior); every other kind — claude, grok, claude-work — has no
    /// equivalent live reader here, so it reads back whatever the shared
    /// `mv_state::gauge` store already holds for this pool (written by
    /// whichever process last recorded a reading for it). That store is a
    /// plain struct, not `serde_json::Value`, so reading it needs no new
    /// dependency on this crate (card `20261102-b-c1a2e5`'s own worry) —
    /// but it does mean a claude/grok seat with nothing yet recorded
    /// renders the same `"window: n/a (not reported)"` line `board.py`'s
    /// `_window_lines` falls back to, rather than the API-billing-specific
    /// wording that needs an `account.kind`-aware flag this struct does
    /// not carry.
    fn read_quota(&self, account: &Account) -> Option<QuotaReading> {
        if account.kind == "codex" {
            if let Some(dir) = account.config_dir.value() {
                if let Some(reading) = limits::read_and_record_codex_usage(
                    Path::new(dir),
                    &self.state_dir,
                    &account.pool,
                ) {
                    // Live percents keep this reading's own reset clocks —
                    // overlaying the persisted gauge would mix a newer
                    // store's clocks onto an older transcript when
                    // `read_and_record_codex_usage` skips the write.
                    return Some(QuotaReading {
                        primary: Some(reading.primary_percent_left),
                        secondary: reading.secondary_percent_left,
                        resets_at: reading
                            .resets_at
                            .as_deref()
                            .and_then(breaker::parse_stored_timestamp),
                        resets_at_secondary: reading
                            .resets_at_secondary
                            .as_deref()
                            .and_then(breaker::parse_stored_timestamp),
                        ..Default::default()
                    });
                }
            }
        }
        let gauges = mv_state::gauge::load(&self.state_dir);
        let entry = gauges.get(&account.pool)?;
        entry.percent_left.map(|primary| {
            QuotaReading {
                primary: Some(primary),
                secondary: entry.percent_left_secondary,
                ..Default::default()
            }
            .with_clocks(entry)
        })
    }

    /// `account`'s 24h token total for the ACCOUNTS card, or `None` for a
    /// kind this crate has no reader for (see the module doc's "Claude
    /// accounts now get a real 24h token total" note). `claude` and
    /// `codex` are wired today, matching Python's own `kind`-exact dispatch
    /// (`usage._compute_account_usage`: only those two kinds have an
    /// evidenced log source) — `claude-work`/`codex-work` sibling pools are not.
    fn tokens_24h(&self, account: &Account, now: OffsetDateTime) -> Option<String> {
        let dir = account.config_dir.value()?;
        match account.kind.as_str() {
            "claude" => claude_tokens_24h(dir, now, &self.state_dir).map(format_tokens_24h),
            "codex" => codex_tokens_24h(dir, now).map(format_tokens_24h),
            _ => None,
        }
    }

    /// `agents` is `None` exactly when the live `herdr agent list` call
    /// failed — the SEATS card still renders then (card
    /// `20261102-b-48f352`), with `live_agents`/`open_seats` left `None`
    /// rather than guessed at zero; `seat_cap` is config and stays known
    /// either way, matching `board.py`'s `WorkersSnapshot`.
    fn seats_snapshot(&self, agents: Option<&[LiveAgent]>) -> SeatsSnapshot {
        let seat_cap = self.config.max_concurrent;
        let Some(agents) = agents else {
            return SeatsSnapshot {
                working: None,
                live_agents: None,
                open_seats: None,
                seat_cap,
                unrecognized: Vec::new(),
            };
        };
        let workers: Vec<_> = agents
            .iter()
            .filter(|agent| super::workers::is_dispatched_name(&self.config, &agent.name))
            .collect();
        let live = workers.len() as u32;
        let working = workers
            .iter()
            .filter(|agent| agent.status == "working")
            .count() as u32;
        SeatsSnapshot {
            working: Some(working),
            live_agents: Some(live),
            open_seats: Some(seat_cap.saturating_sub(live)),
            seat_cap,
            unrecognized: Vec::new(),
        }
    }

    fn fleet_health(
        &self,
        view: &[Project],
        agents: Option<&[LiveAgent]>,
        now: OffsetDateTime,
    ) -> FleetHealth {
        let as_of = clock_hhmm(now);
        let mut accounts: Vec<&Account> = self.config.accounts.values().collect();
        accounts.sort_by(|a, b| a.name.cmp(&b.name));
        let accounts = accounts
            .iter()
            .map(|account| self.account_snapshot(account, agents, &as_of, now))
            .collect();
        // Built unconditionally — `seat_cap` is config, known regardless of
        // whether Herdr answered, and `board.py` keeps rendering the SEATS
        // card with `?` placeholders rather than dropping the whole row
        // when the live listing failed (card `20261102-b-48f352`).
        let seats = Some(self.seats_snapshot(agents));
        let (cycling, parked) = cycling_and_parked(view, agents, interactive_account(&self.config));
        let cycling_names: HashSet<String> = cycling.iter().map(|p| p.name.clone()).collect();
        FleetHealth {
            accounts,
            // `running` is only knowable once Herdr answered; `capacity` is
            // config and stays known even when it did not (matches
            // `board.py`'s `WorkersSnapshot.max_concurrent`, never gated on
            // `herdr_ok`).
            accounts_running: seats.as_ref().and_then(|s| s.live_agents),
            accounts_capacity: Some(self.config.max_concurrent),
            seats,
            owed_replies: self.owed_replies(view, now),
            mail: self.mail_entries(view, &cycling_names, now),
            last_cycle: self.last_cycle_entries(&cycling, &parked, now),
        }
    }

    fn configured_projects(&self) -> Vec<(&Project, PathBuf)> {
        let mut projects: Vec<&Project> = self.config.projects.values().collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        projects
            .into_iter()
            .filter_map(|project| {
                let path = project.path.value()?.to_owned();
                Some((project, PathBuf::from(path)))
            })
            .collect()
    }

    fn mail_entries(
        &self,
        view: &[Project],
        cycling: &HashSet<String>,
        now: OffsetDateTime,
    ) -> Vec<MailEntry> {
        // `board.py`'s `_mail_card_lines` sorts the board's own view by
        // name and emits one line per project, even at unread 0. A
        // project with no live orchestrator pane is offline: its unread
        // count still prints, but the age is replaced so old mail cannot
        // read as a stale-cycle warning.
        let mut projects: Vec<&Project> = view.iter().collect();
        projects.sort_by(|a, b| a.name.cmp(&b.name));
        projects
            .into_iter()
            .filter_map(|project| {
                let path = project.path.value()?.to_owned();
                Some((project, PathBuf::from(path)))
            })
            .map(|(project, root)| {
                let label = project_short(project);
                let state_dir = root.join(".multiverse");
                let log = ProjectLog::open(project.name.clone(), &root);
                let offline = !cycling.contains(&project.name);
                match activity::read_cursor(&state_dir, &log) {
                    Err(ActivityError::CursorAheadOfLog { cursor, high_water }) => MailEntry {
                        project: format!("⚠ {label}"),
                        unread: 0,
                        last_look_age: Some(format!("cursor corrupt ({cursor}>{high_water})")),
                    },
                    Err(_) => MailEntry {
                        project: label,
                        unread: 0,
                        last_look_age: Some("mail unreadable".into()),
                    },
                    Ok(cursor) => {
                        let unread = log
                            .inbox_messages()
                            .unwrap_or_default()
                            .iter()
                            .filter(|m| m.seq > cursor)
                            .count() as u32;
                        let last_look_age = if offline {
                            Some("offline (no live orchestrator)".into())
                        } else {
                            match activity::read_marker(&state_dir) {
                                None => None,
                                Some((_, when)) => {
                                    elapsed_compact(&when, now).or(Some("unknown".into()))
                                }
                            }
                        };
                        MailEntry {
                            project: label,
                            unread,
                            last_look_age,
                        }
                    }
                }
            })
            .collect()
    }

    fn last_cycle_entries(
        &self,
        cycling: &[&Project],
        parked: &HashSet<String>,
        now: OffsetDateTime,
    ) -> Vec<LastCycleEntry> {
        // LAST CYCLE is scoped to cycling projects only — a project with
        // no live orchestrator pane is dormant, not stale
        // (`board.py` `render_status_cards`'s `cycling` argument). An
        // empty cycling set renders the card's own `(none)` line.
        const STALE_SECS: u64 = (mv_board::layout::STALE_AFTER_MINUTES * 60) as u64;
        cycling
            .iter()
            .filter_map(|project| {
                let path = project.path.value()?.to_owned();
                Some((*project, PathBuf::from(path)))
            })
            .map(|(project, root)| {
                let label = project_short(project);
                let state_dir = root.join(".multiverse");
                let mail_secs = activity::read_marker(&state_dir)
                    .and_then(|(_, when)| elapsed_secs(&when, now));
                let (reconcile_stamp, backlog_stamp) = read_orchestrator_pass(&state_dir);
                let reconcile_secs = reconcile_stamp
                    .as_deref()
                    .and_then(|stamp| elapsed_secs(stamp, now));
                let backlog_secs = backlog_stamp
                    .as_deref()
                    .and_then(|stamp| elapsed_secs(stamp, now));
                let overdue = [mail_secs, reconcile_secs, backlog_secs]
                    .into_iter()
                    .any(|secs| secs.is_none_or(|s| s >= STALE_SECS));
                LastCycleEntry {
                    project: label,
                    mail_at: mail_secs.map(compact_elapsed_u64),
                    reconcile_at: reconcile_secs.map(compact_elapsed_u64),
                    backlog_at: backlog_secs.map(compact_elapsed_u64),
                    overdue,
                    parked: parked.contains(&project.name),
                }
            })
            .collect()
    }

    fn owed_replies(&self, view: &[Project], now: OffsetDateTime) -> Vec<OwedReply> {
        let projects = self.configured_projects();
        let shorts: HashMap<String, String> = projects
            .iter()
            .map(|(project, _)| (project.name.clone(), project_short(project)))
            .collect();
        let all_projects: Vec<(String, PathBuf)> = projects
            .iter()
            .map(|(project, root)| (project.name.clone(), root.clone()))
            .collect();
        let view_names: HashSet<String> = view.iter().map(|p| p.name.clone()).collect();
        let mut replied: HashSet<(String, String, u64)> = HashSet::new();
        for (name, root) in &all_projects {
            let log = ProjectLog::open(name.clone(), root);
            if let Ok(messages) = log.inbox_messages() {
                for message in messages {
                    if let Some(re_seq) = message.reply_to {
                        replied.insert((message.from_project.clone(), name.clone(), re_seq));
                    }
                }
            }
        }
        let mut owed = Vec::new();
        for (name, root) in &all_projects {
            if !view_names.contains(name) {
                continue;
            }
            let log = ProjectLog::open(name.clone(), root);
            let Ok(messages) = log.inbox_messages() else {
                continue;
            };
            for message in messages {
                if !message.ask {
                    continue;
                }
                if activity::is_closed(&log, message.seq).unwrap_or(false) {
                    continue;
                }
                if replied.contains(&(name.clone(), message.from_project.clone(), message.seq)) {
                    continue;
                }
                let wait_secs = elapsed_secs(&message.when, now);
                owed.push((
                    message.when.clone(),
                    OwedReply {
                        project: shorts.get(name).cloned().unwrap_or_else(|| name.clone()),
                        sender: shorts
                            .get(&message.from_project)
                            .cloned()
                            .unwrap_or_else(|| message.from_project.clone()),
                        seq: message.seq,
                        wait_text: wait_secs.map_or_else(|| "?".to_string(), compact_elapsed_u64),
                        warn: wait_secs.is_some_and(|s| s >= 24 * 3600),
                    },
                ));
            }
        }
        owed.sort_by(|a, b| a.0.cmp(&b.0));
        owed.into_iter().map(|(_, reply)| reply).collect()
    }

    fn project_snapshot(
        &self,
        project: &Project,
        agents: Option<&[LiveAgent]>,
        now: OffsetDateTime,
        binary: &str,
    ) -> ProjectSnapshot {
        let short = project_short(project);
        let Some(raw_path) = project.path.value() else {
            return ProjectSnapshot {
                name: project.name.clone(),
                short,
                tasks: Vec::new(),
                backlog: Vec::new(),
            };
        };
        let root = PathBuf::from(raw_path);
        let mut tasks = resolve_tasks_with(&root, agents, now, Some((&self.config, binary)));
        let stuck = if let Some(live) = agents {
            mv_engine::sweep_nudges::stuck_notes(
                &root,
                &mv_state::tasks::sweep_tasks(&root),
                &agent_statuses(live),
                &self.config,
                now,
                &mut BoardReads(binary),
            )
        } else {
            Vec::new()
        };
        for task in &mut tasks {
            let mut notes = Vec::new();
            if let Some(note) = task.state_note.take() {
                notes.push(note);
            }
            if let Some(note) = stuck.iter().find(|note| {
                note.strip_prefix("stuck: ")
                    .is_some_and(|rest| rest.starts_with(&format!("{} (", task.task_id)))
            }) {
                notes.push(note.clone());
            }
            if mv_state::nudge_memory::finish_nudge_already_sent(&root, &task.task_id) {
                notes.push(format!("nudged: {}", task.task_id));
            }
            task.state_note = (!notes.is_empty()).then(|| notes.join(", "));
        }
        ProjectSnapshot {
            name: project.name.clone(),
            short,
            tasks,
            backlog: resolve_backlog(&root, now),
        }
    }
}

impl BoardDataSource for LiveSource {
    fn fingerprint(&self, view: &BoardView) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (
            super::board::fingerprint_view(view),
            self.agent_fingerprint.get(),
        )
            .hash(&mut hasher);
        hasher.finish()
    }

    fn snapshot(&self, scope: &BoardScope) -> Result<(Option<String>, BoardView), Failure> {
        self.snapshot_with(scope, &herdr_binary())
    }
}

impl LiveSource {
    fn snapshot_with(
        &self,
        scope: &BoardScope,
        binary: &str,
    ) -> Result<(Option<String>, BoardView), Failure> {
        if self.config.projects.is_empty() {
            return Err(Failure::Config(
                "no projects configured; run `multiverse add` or `multiverse new`".to_owned(),
            ));
        }
        match self.resolve_scope_with(scope, binary)? {
            Resolved::Message(lines) => Ok((None, BoardView::Message(lines))),
            Resolved::Projects(projects, header) => {
                // `cmd_board` prints its own "herdr unavailable — showing
                // recorded state unchanged (...)" banner the instant this
                // call fails (`cli.py:8375`) and then renders the rest of
                // the board anyway, with every live-agent fact reading
                // "unknown" rather than "none" — never silently rendering
                // as if the fleet were simply idle.
                let fetched = fetch_live_agents(binary);
                let banner = fetched.as_ref().err().map(|reason| {
                    format!(
                        "herdr unavailable \u{2014} showing recorded state unchanged ({reason})"
                    )
                });
                let agents = fetched.ok();
                {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    let mut signature: Vec<_> = agents
                        .as_deref()
                        .unwrap_or_default()
                        .iter()
                        .map(|agent| (&agent.pane_id, &agent.name, &agent.status))
                        .collect();
                    signature.sort_unstable();
                    (agents.is_some(), signature).hash(&mut hasher);
                    self.agent_fingerprint.set(hasher.finish());
                }
                let now = now_utc();
                if self.reconcile {
                    if let Some(live) = agents.as_deref() {
                        let overrides = self
                            .config
                            .accounts
                            .values()
                            .filter_map(|account| {
                                account
                                    .awaiting_lease_minutes_override
                                    .map(|minutes| (account.name.clone(), minutes))
                            })
                            .collect();
                        for project in &projects {
                            let Some(root) = project.path.value().map(Path::new) else {
                                continue;
                            };
                            let mut panes = BoardAgents {
                                binary,
                                first: Some(agent_statuses(live)),
                            };
                            mv_state::tasks::reconcile_project(
                                root,
                                ReconcileRequest {
                                    only_task: None,
                                    close_panes: true,
                                    expire_stale: true,
                                    lease_minutes: self.config.awaiting_lease_minutes,
                                    lease_overrides: &overrides,
                                    now,
                                    require_live: true,
                                },
                                &mut panes,
                            );
                            let marker = root.join(".multiverse/orchestrator-pass");
                            let stamp = mv_core::time::format_seconds_utc(now);
                            if let Some(parent) = marker.parent() {
                                if let Err(error) = std::fs::create_dir_all(parent).and_then(|()| {
                                    std::fs::write(
                                        &marker,
                                        format!("reconcile={stamp}\tbacklog={stamp}\n"),
                                    )
                                }) {
                                    return Err(Failure::Operation(format!(
                                        "could not write orchestrator-pass: {error}"
                                    )));
                                }
                            }
                        }
                    }
                }
                let fleet_health = self.fleet_health(&projects, agents.as_deref(), now);
                let view = if projects.len() == 1 {
                    BoardView::Project(BoardSnapshot {
                        project: self.project_snapshot(
                            &projects[0],
                            agents.as_deref(),
                            now,
                            binary,
                        ),
                        fleet_health,
                    })
                } else {
                    BoardView::Fleet(FleetSnapshot {
                        projects: projects
                            .iter()
                            .map(|project| {
                                self.project_snapshot(project, agents.as_deref(), now, binary)
                            })
                            .collect(),
                        fleet_health,
                    })
                };
                let header = match (banner, header) {
                    (Some(banner), Some(header)) => Some(format!("{banner}\n{header}")),
                    (Some(banner), None) => Some(banner),
                    (None, header) => header,
                };
                Ok((header, view))
            }
        }
    }
}

fn clock_hhmm(now: OffsetDateTime) -> String {
    format!("{:02}:{:02}", now.hour(), now.minute())
}

fn today_str(now: OffsetDateTime) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    )
}

/// Counts all open sessions attributed to `short` by name, including
/// idle/done ones — the same "ends with `-{short}`, before a numeric
/// dedup suffix" rule `mv_engine::seatstate::open_count_from_payload`
/// applies to a `serde_json::Value` payload, re-expressed here over this
/// file's own [`Json`]/[`LiveAgent`] so no `serde_json` dependency is
/// needed to reuse it.
fn seat_open_count(agents: &[LiveAgent], short: &str) -> u32 {
    let suffix = format!("-{short}");
    agents
        .iter()
        .filter(|agent| {
            let base = agent
                .name
                .rsplit_once('-')
                .filter(|(_, tail)| !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()))
                .map_or(agent.name.as_str(), |(base, _)| base);
            agent.name.ends_with(&suffix) || base.ends_with(&suffix)
        })
        .count() as u32
}

/// Every `*.jsonl` under `dir`, recursively — the same depth-bounded
/// walk `mv_engine::limits::read_and_record_codex_usage` uses for
/// Codex's own sessions tree, duplicated here rather than shared: that
/// walk is private to `mv-engine`, and nothing in this crate's reach
/// exposes an equivalent for Claude's `projects/` tree.
fn collect_jsonl(dir: &Path, depth: usize, paths: &mut Vec<PathBuf>) {
    if depth == 0 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        if kind.is_dir() {
            collect_jsonl(&path, depth - 1, paths);
        } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
            paths.push(path);
        }
    }
}

/// `path`'s own `(size, mtime)` identity — the cache key
/// [`claude_tokens_24h`]'s on-disk per-file cache and [`load_token_cache`]/
/// [`save_token_cache`] key on. `mtime_nsec` (not just whole seconds)
/// because a fast-moving transcript can be rewritten more than once a
/// second; keying on seconds alone would let a same-second edit look
/// unchanged and read a stale cached sum.
fn file_identity(metadata: &std::fs::Metadata) -> (u64, i64, i64) {
    (metadata.len(), metadata.mtime(), metadata.mtime_nsec())
}

/// One transcript's extracted `(timestamp_ms, tokens)` pairs, kept
/// per-message rather than folded into a single sum — a cache hit can then
/// reapply *this* call's own 24h cutoff without re-reading or re-parsing
/// the file, which a cached final sum could not do once the sliding
/// window moved past the instant it was computed. Mirrors
/// `mv_engine::usage::sum_assistant_tokens_since`'s own filter/parse
/// (`type == "assistant"`, `message.usage`'s numeric fields summed,
/// missing `timestamp_ms` kept rather than excluded) using this file's own
/// hand-rolled `Json` reader in place of `serde_json::Value` — `mv-cli`
/// does not depend on `serde_json` (module doc above), so this duplicates
/// that parse rather than widening `Cargo.toml`.
fn extract_assistant_token_messages(transcript_jsonl: &str) -> Vec<(Option<i64>, u64)> {
    transcript_jsonl
        .lines()
        .filter_map(JsonParser::parse)
        .filter(|record| record.get("type").and_then(Json::as_str) == Some("assistant"))
        .filter_map(|record| {
            let usage = record.get("message")?.get("usage")?;
            let tokens = usage
                .as_object()?
                .iter()
                .filter_map(|(_, value)| match value {
                    Json::Number(n) if *n >= 0.0 && n.fract() == 0.0 => Some(*n as u64),
                    _ => None,
                })
                .sum();
            let timestamp_ms = record.get("timestamp_ms").and_then(|value| match value {
                Json::Number(n) => Some(*n as i64),
                _ => None,
            });
            Some((timestamp_ms, tokens))
        })
        .collect()
}

type TokenCacheEntry = (u64, i64, i64, Vec<(Option<i64>, u64)>);

/// Where [`claude_tokens_24h`] keeps its per-file message cache — inside
/// the plugin state dir (`HERDR_PLUGIN_STATE_DIR`), alongside
/// `breaker-state.json`/`seat-state.json`/`gauge-state.json`, rather than
/// a new `mv-state` store: no file in this crate's assigned list owns a
/// usage cache yet (`mv_engine::usage`'s own module doc), and this cache
/// is read and written by `mv-cli` alone, so it does not need one.
fn claude_token_cache_path(state_dir: &Path) -> PathBuf {
    state_dir.join("claude-token-scan-cache.tsv")
}

/// `size\tmtime_sec\tmtime_nsec\tmessages\tpath` per line — `messages` is
/// `;`-joined `timestamp_or_empty:tokens` pairs. A line that fails to
/// parse (truncated write, foreign format) is dropped rather than
/// aborting the whole cache load: a cold cache is only a missed
/// optimization, never a wrong answer, since every entry is re-verified
/// against the file's current `(size, mtime)` before use.
fn load_token_cache(cache_path: &Path) -> HashMap<PathBuf, TokenCacheEntry> {
    let Ok(contents) = std::fs::read_to_string(cache_path) else {
        return HashMap::new();
    };
    contents
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(5, '\t');
            let size: u64 = fields.next()?.parse().ok()?;
            let mtime_sec: i64 = fields.next()?.parse().ok()?;
            let mtime_nsec: i64 = fields.next()?.parse().ok()?;
            let messages_field = fields.next()?;
            let path = PathBuf::from(fields.next()?);
            let messages = if messages_field.is_empty() {
                Vec::new()
            } else {
                messages_field
                    .split(';')
                    .filter_map(|entry| {
                        let (ts, tokens) = entry.split_once(':')?;
                        let tokens: u64 = tokens.parse().ok()?;
                        let ts = if ts.is_empty() {
                            None
                        } else {
                            Some(ts.parse::<i64>().ok()?)
                        };
                        Some((ts, tokens))
                    })
                    .collect()
            };
            Some((path, (size, mtime_sec, mtime_nsec, messages)))
        })
        .collect()
}

/// Writes exactly the entries [`claude_tokens_24h`] actually consulted
/// this run — every file older than the 24h mtime prefilter is already
/// dropped before it ever reaches the cache, so the file a save writes is
/// always the current candidate set, never an ever-growing history of
/// files that aged out, let alone ones since deleted. Best-effort: a
/// failed write (missing state dir, no space) only costs the next call a
/// re-read, never a wrong total, so this ignores the result rather than
/// surfacing an error board rendering has no use for.
fn save_token_cache(cache_path: &Path, entries: &HashMap<PathBuf, TokenCacheEntry>) {
    let Some(parent) = cache_path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let mut paths: Vec<&PathBuf> = entries.keys().collect();
    paths.sort();
    let mut out = String::new();
    for path in paths {
        let (size, mtime_sec, mtime_nsec, messages) = &entries[path];
        let joined = messages
            .iter()
            .map(|(ts, tokens)| match ts {
                Some(ts) => format!("{ts}:{tokens}"),
                None => format!(":{tokens}"),
            })
            .collect::<Vec<_>>()
            .join(";");
        out.push_str(&format!(
            "{size}\t{mtime_sec}\t{mtime_nsec}\t{joined}\t{}\n",
            path.display()
        ));
    }
    let _ = mv_core::atomic::write_atomic(cache_path, out.as_bytes());
}

/// Claude's 24h token total: every `*.jsonl` under `<config_dir>/projects/`
/// modified in the last 24h (Python's `usage._claude_tokens_last_24h`'s own
/// tree and file-mtime prefilter — the same recently-modified-only bound
/// [`codex_tokens_24h`] uses, for the same reason). Surviving files are
/// then summed with each message's own usage block, filtered to the same
/// per-message 24h cutoff `mv_engine::usage::sum_assistant_tokens_since`
/// applies — except a file whose `(size, mtime)` matches
/// [`load_token_cache`]'s on-disk entry skips the read and re-parse
/// entirely, reusing its cached `(timestamp_ms, tokens)` pairs instead.
/// Tens of thousands of these files are typically untouched between two
/// `board` calls — only a handful of actively-written sessions change —
/// so a warm cache turns most of this scan into a cache-file read instead
/// of walking every transcript on disk. `None` only when `projects/`
/// itself cannot be listed (missing, unreadable, or not a directory) — no
/// evidenced log at all, same as Python's "nothing to show" case; a
/// directory that exists but has had no recent activity still returns
/// `Some(0)`, a real answer rather than "n/a".
fn claude_tokens_24h(config_dir: &str, now: OffsetDateTime, state_dir: &Path) -> Option<u64> {
    let root = Path::new(config_dir).join("projects");
    if !root.is_dir() {
        return None;
    }
    let mut paths = Vec::new();
    collect_jsonl(&root, 6, &mut paths);
    let cutoff = now - time::Duration::hours(24);
    let cutoff_ms = cutoff.unix_timestamp() * 1000;

    let cache_path = claude_token_cache_path(state_dir);
    let previous = load_token_cache(&cache_path);
    let mut next = HashMap::with_capacity(paths.len());
    let mut total = 0u64;
    // Whether this pass found anything [`load_token_cache`] didn't already
    // have — a fully warm run (every candidate file already cached at its
    // current size/mtime) has nothing new to persist, so it skips the
    // fsync'd rewrite below entirely rather than paying that cost just to
    // write back the same bytes it read.
    let mut cache_changed = previous.len() != paths.len();

    for path in &paths {
        let Ok(metadata) = std::fs::metadata(path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if OffsetDateTime::from(modified) < cutoff {
            continue;
        }
        let (size, mtime_sec, mtime_nsec) = file_identity(&metadata);

        let cached = previous.get(path).and_then(|entry| {
            let &(cached_size, cached_sec, cached_nsec, ref messages) = entry;
            (cached_size == size && cached_sec == mtime_sec && cached_nsec == mtime_nsec)
                .then(|| messages.clone())
        });
        let messages = match cached {
            Some(messages) => messages,
            None => {
                cache_changed = true;
                let Some(contents) = std::fs::read_to_string(path).ok() else {
                    continue;
                };
                extract_assistant_token_messages(&contents)
            }
        };

        total += messages
            .iter()
            .filter(|(ts, _)| ts.is_none_or(|ts| ts >= cutoff_ms))
            .map(|(_, tokens)| tokens)
            .sum::<u64>();
        next.insert(path.clone(), (size, mtime_sec, mtime_nsec, messages));
    }

    if cache_changed {
        save_token_cache(&cache_path, &next);
    }
    Some(total)
}

/// Reads at most the final `max_bytes` of `path` — enough, almost always,
/// for [`codex_session_tokens`](usage::codex_session_tokens) to find the
/// newest `total_token_usage` reading it wants, without a full read of a
/// session log that can run to many megabytes once a long session has
/// piled up. The first (possibly truncated) line in a seeked-into read is
/// simply unparsable JSON and already skipped by that function's own
/// per-line tolerance, so no explicit trimming is needed here.
fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max_bytes);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
    }
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Bytes [`read_tail`] reads before trying [`usage::codex_session_tokens`]
/// on a Codex session log — generous enough that the newest
/// `total_token_usage` reading is virtually always inside it, since it is
/// written last.
const CODEX_TAIL_BYTES: u64 = 256 * 1024;

/// [`usage::codex_session_tokens`] only wants the single newest reading in
/// the file, so reading its tail is (almost always) exactly as correct as
/// reading the whole thing, for a fraction of the bytes. The rare miss — a
/// reading that sits further back than [`CODEX_TAIL_BYTES`] from EOF, past
/// a very large trailing write with no usage block of its own — falls
/// back to a full read, so the result this returns never actually depends
/// on the tail guess: it is either the same answer, faster, or the same
/// answer, at the old cost.
fn codex_session_tokens_from_tail(path: &Path) -> Option<(u64, u64)> {
    if let Some(tail) = read_tail(path, CODEX_TAIL_BYTES) {
        if let Some(tokens) = usage::codex_session_tokens(&tail) {
            return Some(tokens);
        }
    }
    let contents = std::fs::read_to_string(path).ok()?;
    usage::codex_session_tokens(&contents)
}

/// Codex's 24h token total (card `20261102-b-06b7a0`): every `*.jsonl`
/// under `<config_dir>/sessions/` with an mtime inside the last 24h —
/// Python's `limits.codex_tokens_last_24h`'s own file-level cutoff, used
/// here (unlike Claude's per-message cutoff above) because a Codex log's
/// `total_token_usage` reading is already that whole session's cumulative
/// total: filtering by file mtime is what "modified in the last 24h" means
/// for a file-level running total, where a per-message timestamp check
/// would not apply. [`codex_session_tokens_from_tail`] reads the newest
/// such reading out of one file without a full read; this sums both token
/// counts, across files, into the one combined number `format_tokens_24h`
/// prints (see its own doc comment). `None` only when `sessions/` itself
/// cannot be listed, same posture as [`claude_tokens_24h`].
fn codex_tokens_24h(config_dir: &str, now: OffsetDateTime) -> Option<u64> {
    let root = Path::new(config_dir).join("sessions");
    if !root.is_dir() {
        return None;
    }
    let mut paths = Vec::new();
    collect_jsonl(&root, 5, &mut paths);
    let cutoff = now - time::Duration::hours(24);
    let (total_in, total_out) = paths
        .iter()
        .filter(|path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .is_ok_and(|modified| OffsetDateTime::from(modified) >= cutoff)
        })
        .filter_map(|path| codex_session_tokens_from_tail(path))
        .fold((0u64, 0u64), |(total_in, total_out), (input, output)| {
            (total_in + input, total_out + output)
        });
    Some(total_in + total_out)
}

/// `n` tokens, compacted the same way Python's `_format_token_pair` scales
/// one side of its in/out pair: millions/thousands to one decimal place,
/// anything under 1,000 printed exactly. This crate only ever has the
/// combined total (`mv_engine::usage::sum_assistant_tokens_since` sums a
/// message's whole `usage` block, not `tokens_in`/`tokens_out`
/// separately), so the ACCOUNTS card shows one number, not Python's
/// `"41.2M in · 0.9M out"` pair.
fn format_tokens_24h(total: u64) -> String {
    if total >= 1_000_000 {
        format!("{:.1}M", total as f64 / 1_000_000.0)
    } else if total >= 1_000 {
        format!("{:.1}K", total as f64 / 1_000.0)
    } else {
        total.to_string()
    }
}

/// `board.py`'s `_format_reset_eta`: hours-and-minutes (`2h10m`), with a
/// `Nd` prefix past 24h that drops minutes. Used only on the board's
/// FENCED account/SEATS detail — `breaker::describe` is the listing skip
/// reason, a different string.
fn format_reset_eta(seconds: i64) -> String {
    let total = seconds.max(0);
    let days = total / 86_400;
    let hours = (total % 86_400) / 3_600;
    let minutes = (total % 3_600) / 60;
    if days > 0 {
        if hours > 0 {
            format!("{days}d{hours}h")
        } else {
            format!("{days}d")
        }
    } else if hours > 0 && minutes > 0 {
        format!("{hours}h{minutes}m")
    } else if hours > 0 {
        format!("{hours}h")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        "under 1m".to_string()
    }
}

/// `board.py` `account_states`' FENCED branch (not `breaker::describe`).
fn fenced_board_detail(status: &breaker::BreakerStatus, now: OffsetDateTime) -> String {
    let mut detail = if status
        .reset_at
        .is_some_and(|reset| reset.year() == breaker::NO_AUTOMATIC_RESET_YEAR)
    {
        "needs human (out of credits)".to_string()
    } else if status.open {
        match status.reset_at {
            Some(reset) if reset > now => {
                format!(
                    "resets in {}",
                    format_reset_eta((reset - now).whole_seconds())
                )
            }
            _ => "resets now".to_string(),
        }
    } else {
        "needs human".to_string()
    };
    if status.manual {
        detail = format!("(manual) {detail}");
    }
    if status.gauge_would_reopen {
        detail = format!("{detail} \u{2014} gauge shows open; manual fence held");
    }
    detail
}

fn interactive_account(config: &Config) -> Option<&Account> {
    config.accounts.values().find(|account| account.interactive)
}

/// `cli.cmd_board`'s cycling/parked sets: a project is cycling only when
/// Herdr answered, an interactive seat is configured, and exactly one
/// orchestrator pane (`-orch-` / `-nexu-`) for that project is live.
/// Parked when that pane's `agent_status` is `blocked`.
fn cycling_and_parked<'a>(
    view: &'a [Project],
    agents: Option<&[LiveAgent]>,
    seat: Option<&Account>,
) -> (Vec<&'a Project>, HashSet<String>) {
    let mut cycling = Vec::new();
    let mut parked = HashSet::new();
    let (Some(agents), Some(seat)) = (agents, seat) else {
        return (cycling, parked);
    };
    for project in view {
        let short = project_short(project);
        let found = agents.iter().find(|agent| {
            let name = agent.name.as_str();
            let is_orch = name.contains("-orch-") || name.contains("-nexu-");
            is_orch
                && (name.starts_with(&format!("{short}-"))
                    || name.starts_with(&format!("{}-", project.name)))
                && name.ends_with(&format!("-{}", seat.short))
        });
        if let Some(agent) = found {
            cycling.push(project);
            if agent.status == "blocked" {
                parked.insert(project.name.clone());
            }
        }
    }
    (cycling, parked)
}

/// `Ns`/`Nm`/`Nh` — `board.py`'s `_compact_elapsed`: a card's status line
/// has room for "13m", not "13 min".
fn compact_elapsed(seconds: i64) -> String {
    compact_elapsed_u64(seconds.max(0) as u64)
}

fn compact_elapsed_u64(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / 3_600)
    }
}

fn elapsed_secs(when: &str, now: OffsetDateTime) -> Option<u64> {
    let at = mv_core::time::parse_flexible(when).ok()?;
    if at > now {
        return None;
    }
    Some((now - at).whole_seconds().max(0) as u64)
}

fn elapsed_compact(when: &str, now: OffsetDateTime) -> Option<String> {
    elapsed_secs(when, now).map(compact_elapsed_u64)
}

fn display_role(raw: &str) -> String {
    // `board.py`'s `_task_card`/`_backlog_card` print `task.role`/
    // `item.role` verbatim — never title-cased — so a freshly dispatched
    // task whose log already carries the current, lowercase config key
    // (`scout`, not `Scout`) must render exactly that, not `Role::
    // display_name()`'s capitalized form (found via `conformance/
    // diff_fuzz.py` seed 50034: a dispatch's own board card showed
    // `Scout` where Python showed `scout`). A legacy on-disk key (e.g.
    // `fixer`) maps to its current lowercase config key (`builder`) so
    // the board never shows a retired name — history readers still
    // accept the old key on disk, this only changes what the card
    // prints. A string that is neither a current nor a legacy key
    // (`archaeologist`) renders verbatim.
    match Role::from_any_config_key(raw) {
        Ok(role) => role.config_key().to_string(),
        Err(_) => raw.to_string(),
    }
}

fn month_abbrev(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

fn card_corner_from_stamp(when: &str, now: OffsetDateTime) -> (Option<String>, Option<String>) {
    match mv_core::time::parse_flexible(when) {
        Ok(at) => card_corner_from_instant(at, now),
        Err(_) => (None, None),
    }
}

/// Resolve the local calendar fields through the system timezone rules,
/// including `TZ` and the offset at this instant, like `astimezone(None)`.
/// A render touching thousands of cards shares one parse of the IANA
/// tzdata (`mv_core::time::local_datetime` caches it per process); every
/// card after that is a pure offset lookup, not a spawned `date`.
fn card_corner_local_time(instant: OffsetDateTime) -> Option<time::PrimitiveDateTime> {
    mv_core::time::local_datetime(instant)
}

fn card_corner_from_instant(
    at: OffsetDateTime,
    now: OffsetDateTime,
) -> (Option<String>, Option<String>) {
    if at > now {
        return (None, None);
    }
    let (Some(local), Some(local_now)) = (card_corner_local_time(at), card_corner_local_time(now))
    else {
        return (None, None);
    };
    let clock = if local.date() == local_now.date() {
        format!("{:02}:{:02}", local.hour(), local.minute())
    } else {
        let yesterday = local_now - time::Duration::days(1);
        if local.date() == yesterday.date() {
            format!("yday {:02}:{:02}", local.hour(), local.minute())
        } else {
            let mut date_text = format!("{} {}", month_abbrev(local.month()), local.day());
            if local.year() != local_now.year() {
                date_text.push_str(&format!(" {}", local.year()));
            }
            date_text
        }
    };
    let minutes = ((now - at).whole_seconds().max(0) as u64) / 60;
    let age = if minutes < 60 {
        format!("{minutes}m")
    } else {
        let hours = minutes / 60;
        if hours < 24 {
            format!("{hours}h")
        } else {
            let days = hours / 24;
            if days < 30 {
                format!("{days}d")
            } else {
                let months = days / 30;
                if months < 12 {
                    format!("{months}mo")
                } else {
                    format!("{}y", months / 12)
                }
            }
        }
    };
    (Some(clock), Some(age))
}

fn read_orchestrator_pass(state_dir: &Path) -> (Option<String>, Option<String>) {
    let text = std::fs::read_to_string(state_dir.join("orchestrator-pass")).ok();
    let Some(text) = text else {
        return (None, None);
    };
    let mut reconcile = None;
    let mut backlog = None;
    for field in text.split_whitespace() {
        if let Some((key, value)) = field.split_once('=') {
            if value.is_empty() {
                continue;
            }
            match key {
                "reconcile" => reconcile = Some(value.to_string()),
                "backlog" => backlog = Some(value.to_string()),
                _ => {}
            }
        }
    }
    (reconcile, backlog)
}

#[cfg(test)]
fn latest_matching<'a>(
    records: &'a [LogRecord],
    event: &str,
    task_id: &str,
) -> Option<&'a LogRecord> {
    records.iter().rev().find(|record| {
        record.event() == event && record.fields.get("task").map(String::as_str) == Some(task_id)
    })
}

/// Last-write-wins fold of every task-scoped event in `records`. One
/// forward pass replaces the per-task reverse scans (last matching
/// dispatch/started/acknowledged, last result/supersede, and a full-log
/// `limit` count) that [`resolve_one_task`] used to do — at fleet scale
/// that was O(tasks × log lines) and dominated a warm `board --all`.
struct TaskEvents<'a> {
    dispatch: Option<&'a LogRecord>,
    started: Option<&'a LogRecord>,
    acknowledged: Option<&'a LogRecord>,
    outcome: Option<&'a LogRecord>,
    limited_count: u32,
}

fn index_task_events(records: &[LogRecord]) -> HashMap<&str, TaskEvents<'_>> {
    let mut map: HashMap<&str, TaskEvents<'_>> = HashMap::new();
    for record in records {
        let Some(task_id) = record.fields.get("task").map(String::as_str) else {
            continue;
        };
        if task_id.is_empty() {
            continue;
        }
        let entry = map.entry(task_id).or_insert(TaskEvents {
            dispatch: None,
            started: None,
            acknowledged: None,
            outcome: None,
            limited_count: 0,
        });
        match record.event() {
            "dispatch" => entry.dispatch = Some(record),
            "started" => entry.started = Some(record),
            "acknowledged" => entry.acknowledged = Some(record),
            "limit" => entry.limited_count += 1,
            "result" if record.fields.get("status").map(String::as_str) != Some("none") => {
                entry.outcome = Some(record);
            }
            "supersede" => entry.outcome = Some(record),
            _ => {}
        }
    }
    map
}

/// A task's current outcome record: the *last* `event=result`/`event=
/// supersede` line naming it, in file order — mirroring `tasks.py`'s own
/// fold (`resolve`'s log walk sets `task.status` from whichever of the two
/// event kinds it meets later in the file, a `supersede` simply overwriting
/// whatever an earlier `result` left there). Scanning for only `"result"`
/// here would leave a task's outcome stuck at a long-superseded `blocked`
/// verdict forever once a later `supersede` line corrected it to `done` —
/// confirmed against the real fleet, where every project has many more
/// `event=supersede` lines than raw `status=blocked` results (this task's
/// `result.md`). A `result` line carrying `status=none` is skipped, same as
/// `tasks.py`'s `resolve`: a past runner bug once wrote that for a turn that
/// only looked settled, and `TaskStatus` has no such value.
#[cfg(test)]
fn latest_outcome<'a>(records: &'a [LogRecord], task_id: &str) -> Option<&'a LogRecord> {
    records.iter().rev().find(|record| {
        record.fields.get("task").map(String::as_str) == Some(task_id)
            && match record.event() {
                "result" => record.fields.get("status").map(String::as_str) != Some("none"),
                "supersede" => true,
                _ => false,
            }
    })
}

/// Every dispatched task the project's own `routing.log` knows about, each
/// resolved further against its own status file under
/// `root/.multiverse/tasks`. See the module doc for what this narrows
/// relative to `tasks.resolve`.
///
/// The task-id universe comes from `event=dispatch` records alone, the
/// same way `tasks.py`'s own `resolve` builds it — never from listing
/// `.multiverse/tasks/` (card `20261102-b-8fad3e`). A directory listing
/// picks up a task whose own writeup (`<task-id>-result.md`) sits as a
/// loose *file* directly under `tasks/` (confirmed on the real fleet:
/// one project has 34 of them) and whose name still matches
/// `is_valid_task_id`'s permissive `[A-Za-z0-9._-]` pattern — rendering a
/// phantom IN PROGRESS card Python never shows, since it never learns a
/// dispatch-less task exists at all. A task that predates structured
/// dispatch logging (no `event=dispatch` line at all, only a leftover
/// directory) is the same case from the other side, and Python never
/// shows that one either.
#[cfg(test)]
fn resolve_tasks(
    root: &Path,
    agents: Option<&[LiveAgent]>,
    now: OffsetDateTime,
) -> Vec<DispatchedTaskSnapshot> {
    resolve_tasks_with(root, agents, now, None)
}

fn resolve_tasks_with(
    root: &Path,
    agents: Option<&[LiveAgent]>,
    now: OffsetDateTime,
    context: Option<(&Config, &str)>,
) -> Vec<DispatchedTaskSnapshot> {
    let log_path = root.join(".multiverse/routing.log");
    let records: Vec<LogRecord> = index_records(&log_path, Selector::Tasks)
        .map(|indexed| indexed.iter().filter_map(|r| r.parse().ok()).collect())
        .unwrap_or_default();

    let events = index_task_events(&records);
    let mut newest_name = HashMap::new();
    for record in &records {
        if record.event() == "dispatch" {
            if let (Some(name), Some(task)) =
                (record.fields.get("agent"), record.fields.get("task"))
            {
                newest_name.insert(name.as_str(), task.as_str());
            }
        }
    }
    let mut ids: Vec<&str> = events
        .iter()
        .filter(|(task_id, entry)| entry.dispatch.is_some() && is_valid_task_id(task_id))
        .map(|(task_id, _)| *task_id)
        .collect();
    ids.sort_unstable();

    // Per-task log scans used to dominate a warm board; the index above is
    // one pass. `dirty_lane_detail`'s `git status` is still the only
    // subprocess, and only for a rare unrecorded-done writing-role lane,
    // so a thread per historical dispatch is not worth the spawn cost.
    ids.into_iter()
        .filter_map(|task_id| {
            let event = events.get(task_id).expect("indexed from the same map");
            let legacy_owner = event
                .dispatch
                .and_then(|record| record.fields.get("agent"))
                .is_some_and(|name| newest_name.get(name.as_str()) == Some(&task_id));
            let mut task = resolve_one_task(task_id, root, event, agents, now, legacy_owner)?;
            if let Some((config, binary)) = context {
                probe_task_prompt(&mut task, event, agents, config, binary);
            }
            Some(task)
        })
        .collect()
}

/// The two roles Python's reference gives their own worktree and branch
/// (`roles.WRITING_ROLES`: Builder, Artisan — see
/// `AGENTS.md`'s new-name list). Mirrors `mv_state::tasks::is_writing_role`,
/// private to its own crate; duplicated here rather than exposed, the same
/// call this file already made for its own JSON reader (module doc) and
/// `herdr_binary`'s format string.
fn is_writing_role(role: &str) -> bool {
    matches!(role, "fixer" | "builder" | "designer" | "artisan")
}

/// `git status --porcelain` for a lane's own worktree directory, mirroring
/// `mv_state::tasks::lane_porcelain` (private to its crate, duplicated for
/// the same reason as `is_writing_role` above). `None` when the lane
/// directory is gone or git could not be read — never read as dirty by
/// omission.
fn lane_porcelain(root: &Path, lane_task_id: &str) -> Option<String> {
    let lane_root = root.join(".multiverse/worktrees").join(lane_task_id);
    if !lane_root.is_dir() {
        return None;
    }
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(&lane_root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What's dirty in a lane, for the "done but lane has uncommitted
/// changes(...)" note — a narrowed `worktrees.DirtyDetail.describe()`:
/// modified/untracked counts plus up to three example paths, in the same
/// wording. Narrowed because it drops the trailing "N commits ahead",
/// which needs a merge-base lookup this function does not make.
fn describe_dirty(porcelain: &str) -> String {
    let mut lines: Vec<String> = porcelain.lines().map(str::to_string).collect();
    if let Some(first) = lines.first_mut() {
        if first.len() > 2 && first.as_bytes().get(2) != Some(&b' ') {
            first.insert(0, ' ');
        }
    }
    let untracked = lines.iter().filter(|line| line.starts_with("??")).count();
    let modified = lines.len() - untracked;
    let shown: Vec<&str> = lines
        .iter()
        .take(3)
        .map(|line| line.get(3..).unwrap_or(""))
        .collect();
    let more = lines.len().saturating_sub(shown.len());
    let counts = format!("{modified} modified + {untracked} untracked");
    if shown.is_empty() {
        return counts;
    }
    let mut named = shown.join(", ");
    if more > 0 {
        named.push_str(&format!(", and {more} more"));
    }
    format!("{counts}: {named}")
}

/// Whether `task_id`'s own lane still holds uncommitted work, for a status
/// file that says `done` with no `event=result` confirming it yet — the
/// contradiction `board.py`'s `_dirty_done`/`_dirty_lane_detail` refuses to
/// paper over (critic finding 6: "dirty-done left IN PROGRESS"). `branch`
/// is the dispatch record's own `branch=` field — the lane a task
/// dispatched `--into` another one actually owns — falling back to
/// `task_id` itself when no dispatch record carried it.
fn dirty_lane_detail(
    root: &Path,
    role: &str,
    branch: Option<&str>,
    task_id: &str,
) -> Option<String> {
    if !is_writing_role(role) {
        return None;
    }
    let lane_task_id = branch
        .and_then(|b| b.strip_prefix("multiverse/"))
        .unwrap_or(task_id);
    let porcelain = lane_porcelain(root, lane_task_id)?;
    if porcelain.is_empty() {
        return None;
    }
    Some(describe_dirty(&porcelain))
}

fn read_pane_visible(binary: &str, pane: &str) -> Option<String> {
    let output = std::process::Command::new(binary)
        .args(["agent", "read", pane, "--source", "visible"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

fn probe_task_prompt(
    task: &mut DispatchedTaskSnapshot,
    events: &TaskEvents<'_>,
    agents: Option<&[LiveAgent]>,
    config: &Config,
    binary: &str,
) {
    // Python's `cmd_board` (`cli.py:8674-8680`) only calls
    // `find_blocking_prompt` for `task.state in (RUNNING, STALLED)` — a
    // task Herdr already reports `blocked` is routed straight to the
    // BLOCKED column by `herdr_blocked` instead (a separate, pre-computed
    // set), never through this probe. Found via `conformance/diff_fuzz.py`
    // seed 50007 (an extra `agent read` for a herdr-blocked pane).
    if task.outcome.is_some()
        || !matches!(
            task.live_status,
            Some(LiveAgentStatus::Running | LiveAgentStatus::Stalled)
        )
    {
        return;
    }
    if task.agent.as_deref().is_none_or(str::is_empty) {
        return;
    }
    let Some(pane) = events.started.and_then(|event| event.fields.get("pane")) else {
        return;
    };
    let Some(current) =
        agents.and_then(|agents| agents.iter().find(|agent| &agent.pane_id == pane))
    else {
        return;
    };
    if current.status == "working" {
        return;
    }
    let Some(account) = config.accounts.get(&task.account) else {
        return;
    };
    let Some(text) = read_pane_visible(binary, pane) else {
        return;
    };
    let rule = if account.kind == "codex" && text.to_lowercase().contains("trust and continue") {
        Some("codex_directory_trust_prompt".to_string())
    } else {
        mv_engine::blocking_prompt::classify_prompt(&text, &account.kind).map(|prompt| prompt.rule)
    };
    if let Some(rule) = rule {
        task.live_status = Some(LiveAgentStatus::PromptBlocked);
        task.state_note = Some(format!("blocked on prompt: {rule}"));
    }
}

fn resolve_title(root: &Path, task_id: &str, branch: Option<&str>) -> Option<String> {
    use std::io::Read;
    let relative = Path::new(".multiverse/tasks")
        .join(task_id)
        .join("brief.md");
    let mut reader = match mv_core::secure_open::open_regular(root, &relative, "brief") {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let lane_id = branch
                .and_then(|b| b.strip_prefix("multiverse/"))
                .unwrap_or(task_id);
            let lane = mv_state::tasks::safe_lane_root(root, lane_id)?;
            mv_core::secure_open::open_regular(&lane, &relative, "brief").ok()?
        }
        Err(_) => return None,
    };
    let mut text = String::new();
    reader.read_to_string(&mut text).ok()?;
    let mut lines = text.lines().skip_while(|line| *line != "## Goal");
    lines.next()?;
    let line = lines.map(str::trim).find(|line| !line.is_empty())?;
    if line.starts_with('#') || line.starts_with("```") {
        return None;
    }
    Some(mv_board::layout::sanitize_title(line))
}

fn resolve_one_task(
    task_id: &str,
    root: &Path,
    events: &TaskEvents<'_>,
    agents: Option<&[LiveAgent]>,
    now: OffsetDateTime,
    legacy_owner: bool,
) -> Option<DispatchedTaskSnapshot> {
    let dispatch = events.dispatch;
    let role = dispatch
        .and_then(|r| r.fields.get("role").cloned())
        .unwrap_or_default();
    let account = dispatch
        .and_then(|r| r.fields.get("account").cloned())
        .unwrap_or_else(|| "-".to_string());
    let recorded_agent = dispatch.and_then(|r| r.fields.get("agent").cloned());
    let pane = events
        .started
        .and_then(|r| r.fields.get("pane"))
        .filter(|p| !p.is_empty());
    let matched = agents
        .and_then(|list| pane.and_then(|pane| list.iter().find(|agent| &agent.pane_id == pane)));
    let branch = dispatch.and_then(|r| r.fields.get("branch").cloned());
    let dispatched_at = dispatch.and_then(|r| mv_core::time::parse_flexible(&r.when).ok());
    let started_at = events
        .started
        .and_then(|r| mv_core::time::parse_flexible(&r.when).ok());
    let finished_at = events
        .outcome
        .and_then(|record| mv_core::time::parse_flexible(&record.when).ok());
    // A writer runs in its own worktree, so its status file lands at
    // `.multiverse/worktrees/<lane_task_id>/` under the project root, not
    // in the main checkout — the log alone never says which, so both
    // locations are checked, lane first (`tasks.py`'s `resolve`). Reusing
    // `mv_state::tasks::read_writer_status` (already `sweep`'s own lane
    // lookup) rather than reading `tasks_dir` directly — found via
    // `conformance/diff_fuzz.py` seed 50010, which left a writing role's
    // freshly-finished task reading STALLED instead of UNRECORDED because
    // only the main checkout's status file was ever consulted.
    let lane_task_id = branch
        .as_deref()
        .and_then(|b| b.strip_prefix("multiverse/"))
        .unwrap_or(task_id);
    let status = mv_state::tasks::read_writer_status(
        root,
        task_id,
        lane_task_id,
        started_at.or(dispatched_at),
    );

    let recorded_status = events.outcome.and_then(|r| r.fields.get("status").cloned());

    let limited_count = events.limited_count;
    let acknowledged = events.acknowledged.is_some();

    // `board.py`'s `_state_elapsed`: "how long has this task held its
    // *current* state" — `finished_at` (when a terminal outcome exists)
    // or `dispatched_at` otherwise, never `started_at`. Using `started_at`
    // here (as this used to) keeps counting from the original dispatch's
    // own stamp even after a later `tasks --close-stranded`/`backlog done`
    // etc. records an outcome, so a task blocked moments ago read back with
    // years of elapsed time inherited from a stale fixture's dispatch —
    // found via `conformance/diff_fuzz.py` seed 50000. `_stale_pane_note`
    // is the one caller that still wants `started_at or dispatched_at`
    // (below); this is only the card's own displayed/sorted elapsed.
    let elapsed_at = finished_at.or(dispatched_at);
    let elapsed_seconds = elapsed_at.map(|at| (now - at).whole_seconds().max(0) as u64);
    let elapsed_text = elapsed_seconds
        .map(|seconds| compact_elapsed(seconds as i64))
        .unwrap_or_else(|| "?".to_string());
    let base_at = started_at.or(dispatched_at);

    let mut outcome = None;
    let mut state_note = None;
    let mut stale_pane_note = None;
    let mut live_status = None;
    let mut dirty_done_detail = None;

    if let Some(recorded) = recorded_status.as_deref() {
        match recorded {
            "blocked" => outcome = Some(TaskOutcome::Blocked),
            "done" | "failed" => return None,
            _ => {}
        }
    } else if let Some(status) = status {
        live_status = Some(LiveAgentStatus::Unrecorded);
        // UNRECORDED: a terminal status was written, but no `event=result`
        // confirms it yet.
        let still_working = matched.is_some_and(|agent| agent.status == "working");
        // `_dirty_done` only ever fires for a `done` status — Python's
        // `_dirty_lane_detail` gates on `recovered_status == DONE` first —
        // and never when the task's own pane is confirmed still running a
        // fix round (checked first, same order `tasks.py`'s own
        // `if ... elif (detail := _dirty_lane_detail(...)) ...` uses).
        if !still_working && status == TaskStatus::Done {
            dirty_done_detail = dirty_lane_detail(root, &role, branch.as_deref(), task_id);
        }
        // `tasks.py`'s `resolve`: `live_agents is not None and task.pane and
        // task.pane not in live_agents.panes` — a herdr listing alone is not
        // "pane gone"; the recorded pane must be missing *from* that listing.
        // A pane that is still present (just not `working`) is the ordinary
        // "needs --reconcile" case below, same as when no listing exists at
        // all. Checking `agents.is_some()` alone wrongly called every
        // UNRECORDED task "pane gone" whenever Herdr was merely reachable —
        // found via `conformance/diff_fuzz.py` seed 50034, which also forced
        // the card into BLOCKED (`card::split_dispatched`'s "pane gone,
        // status " bypass) where Python kept it IN PROGRESS.
        let pane_confirmed_gone = agents.is_some() && pane.is_some() && matched.is_none();
        state_note = match (
            still_working,
            dirty_done_detail.is_some(),
            pane_confirmed_gone,
        ) {
            (true, _, _) => Some(
                "unrecorded, but its pane is still working \u{2014} not --reconcile's".to_string(),
            ),
            // `dirty_done_detail` already carries the "lane has
            // uncommitted changes" note (`card::task_card`); a generic
            // state note here would just repeat it.
            (false, true, _) => None,
            (false, false, true) => Some(format!("pane gone, status {}", status.as_str())),
            (false, false, false) => Some("unrecorded, needs --reconcile".to_string()),
        };
    } else {
        // No status file: still actively dispatched (RUNNING/AWAITING/
        // STALLED).
        let legacy_live = pane.is_none()
            && legacy_owner
            && agents.is_some_and(|agents| {
                recorded_agent
                    .as_deref()
                    .is_some_and(|name| agents.iter().any(|agent| agent.name == name))
            });
        match (agents, matched) {
            (Some(_), _) if legacy_live => live_status = Some(LiveAgentStatus::Running),
            (None, _) => live_status = Some(LiveAgentStatus::Running),
            (Some(_), Some(agent))
                if !matches!(
                    agent.status.as_str(),
                    "idle" | "done" | "blocked" | "unknown"
                ) =>
            {
                live_status = Some(LiveAgentStatus::Running);
            }
            (Some(_), Some(_)) => {
                live_status = Some(LiveAgentStatus::Stalled);
                state_note = Some("stalled, agent is live but not working".to_string());
            }
            (Some(_), None) => {
                live_status = Some(LiveAgentStatus::Awaiting);
                state_note = Some("awaiting, no live agent".to_string());
                if pane.is_some() {
                    if let Some(at) = base_at {
                        let elapsed = now - at;
                        if elapsed > time::Duration::minutes(mv_board::layout::STALE_AFTER_MINUTES)
                        {
                            stale_pane_note = Some(format!(
                                "stale: pane gone {}",
                                compact_elapsed(elapsed.whole_seconds().max(0))
                            ));
                        }
                    }
                }
            }
        }
        // `board.py`'s `_split_dispatched`: a RUNNING/STALLED task whose own
        // *recorded* `agent=` name (not necessarily the pane this match just
        // looked at) is one Herdr currently reports `blocked` is routed to
        // BLOCKED outright, with no note (`_blocked_card` never calls
        // `_state_note`) — `t.agent and t.agent in herdr_blocked`. The
        // `and` matters: an old dispatch line with no `agent=` field (a
        // pre-`agent=` fixture, or any other reason the name was never
        // recorded) can never satisfy it, so that task stays STALLED/
        // IN PROGRESS even while its own live pane happens to read
        // `blocked` — found via `conformance/diff_fuzz.py` seed 50005,
        // which seeds exactly that shape of history.
        if matches!(
            live_status,
            Some(LiveAgentStatus::Running | LiveAgentStatus::Stalled)
        ) {
            let herdr_blocked = recorded_agent.as_deref().is_some_and(|name| {
                agents.is_some_and(|list| {
                    list.iter()
                        .any(|agent| agent.status == "blocked" && agent.name == name)
                })
            });
            if herdr_blocked {
                live_status = Some(LiveAgentStatus::HerdrBlocked);
                state_note = None;
            }
        }
    }

    let (corner_clock, corner_age) = match finished_at.or(dispatched_at) {
        Some(at) => card_corner_from_instant(at, now),
        None => (None, None),
    };
    let role = display_role(&role);

    Some(DispatchedTaskSnapshot {
        task_id: task_id.to_string(),
        title: dispatch
            .and_then(|r| r.fields.get("title"))
            .and_then(|title| JsonParser::parse(title))
            .and_then(|title| title.as_str().map(mv_board::layout::sanitize_title))
            .or_else(|| resolve_title(root, task_id, branch.as_deref())),
        role,
        account,
        elapsed_text,
        elapsed_seconds,
        limited_count,
        outcome,
        live_status,
        state_note,
        stale_pane_note,
        dirty_done_detail,
        acknowledged,
        corner_clock,
        corner_age,
        agent: recorded_agent,
    })
}

/// Every backlog item in `root`'s own log, mapped onto the board's
/// snapshot shape. `split_backlog` (already real in `mv-board::card`)
/// does the `open`/`asked`/`done` column routing from here.
fn resolve_backlog(root: &Path, now: OffsetDateTime) -> Vec<BacklogItemSnapshot> {
    let today = today_str(now);
    backlog::read_items(root, false)
        .into_iter()
        .enumerate()
        .map(|(order, item)| {
            let status = match item.state.as_str() {
                "asked" => BacklogStatus::Asked,
                "done" => BacklogStatus::Done,
                _ => BacklogStatus::Open,
            };
            let priority = match item.priority.as_str() {
                "P1" => 1,
                "P3" => 3,
                _ => 2,
            };
            let has_human_annotation = backlog::has_human_annotation(&item);
            let overdue = item.is_overdue(&today);
            let elapsed_seconds = mv_core::time::parse_flexible(&item.updated_at)
                .ok()
                .map(|at| (now - at).whole_seconds().max(0) as u64);
            let (corner_clock, corner_age) = card_corner_from_stamp(&item.updated_at, now);
            let role_hint = (!item.role.is_empty()).then(|| display_role(&item.role));
            BacklogItemSnapshot {
                id: item.item,
                title: item.title,
                priority,
                created_order: order as u64,
                due_text: (!item.due.is_empty()).then_some(item.due),
                overdue,
                role_hint,
                status,
                has_human_annotation,
                elapsed_seconds,
                corner_clock,
                corner_age,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_parser_reads_the_agent_list_fixture_shape() {
        let text = r#"{"result": {"agents": [{"agent": "codex", "agent_status": "working", "name": "sandbox-scou-sb"}]}}"#;
        let value = JsonParser::parse(text).unwrap();
        let agents = value.get("result").unwrap().get("agents").unwrap();
        assert_eq!(agents.as_array().unwrap().len(), 1);
    }

    #[test]
    fn json_parser_reads_workspace_labels() {
        let text = r#"{"result": {"workspaces": [{"label": "sandbox"}, {"label": "widgets"}]}}"#;
        let value = JsonParser::parse(text).unwrap();
        assert_eq!(
            workspace_labels(&value).unwrap(),
            vec!["sandbox".to_string(), "widgets".to_string()]
        );
    }

    #[test]
    fn compact_elapsed_picks_the_right_unit() {
        assert_eq!(compact_elapsed(5), "5s");
        assert_eq!(compact_elapsed(125), "2m");
        assert_eq!(compact_elapsed(7_200), "2h");
    }

    #[test]
    fn card_corner_uses_system_local_timezone() {
        const CHILD: &str = "MV_BOARD_CORNER_TEST_CHILD";
        let Ok(zone) = std::env::var(CHILD) else {
            // Re-exec rather than change the environment of parallel tests.
            for zone in ["America/Los_Angeles", "Asia/Kolkata", "UTC0", "EST5"] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "commands::board_data::tests::card_corner_uses_system_local_timezone",
                        "--test-threads",
                        "4",
                    ])
                    .env(CHILD, zone)
                    .env("TZ", zone)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "TZ={zone}:\n{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr),
                );
            }
            return;
        };
        assert_eq!(std::env::var("TZ").unwrap(), zone);
        let cases: &[(&str, &str, &str, &str)] = match zone.as_str() {
            "America/Los_Angeles" => &[
                (
                    "2025-01-15T20:04:00Z",
                    "2025-01-15T20:16:00Z",
                    "12:04",
                    "12m",
                ),
                // UTC midnight does not change the local calendar date.
                (
                    "2025-07-15T23:50:00Z",
                    "2025-07-16T00:10:00Z",
                    "16:50",
                    "20m",
                ),
                // Local midnight can fall within a single UTC date.
                (
                    "2025-07-16T06:50:00Z",
                    "2025-07-16T07:10:00Z",
                    "yday 23:50",
                    "20m",
                ),
                (
                    "2025-01-01T00:30:00Z",
                    "2025-01-03T10:00:00Z",
                    "Dec 31 2024",
                    "2d",
                ),
                (
                    "2024-12-29T10:00:00Z",
                    "2025-01-01T04:00:00Z",
                    "Dec 29",
                    "2d",
                ),
                // Each instant uses its own offset across DST transitions.
                (
                    "2025-03-09T09:30:00Z",
                    "2025-03-09T10:30:00Z",
                    "01:30",
                    "1h",
                ),
                (
                    "2025-11-02T07:30:00Z",
                    "2025-11-03T08:15:00Z",
                    "yday 00:30",
                    "1d",
                ),
            ],
            "Asia/Kolkata" => &[
                (
                    "2025-07-15T20:00:00Z",
                    "2025-07-16T00:00:00Z",
                    "01:30",
                    "4h",
                ),
                (
                    "2024-12-31T20:00:00Z",
                    "2025-01-03T00:00:00Z",
                    "Jan 1",
                    "2d",
                ),
                (
                    "2025-01-01T21:00:00+02:00",
                    "2025-01-01T20:00:00Z",
                    "00:30",
                    "1h",
                ),
                (
                    "2025-01-01T18:00:00Z",
                    "2025-01-01T21:00:00+02:00",
                    "yday 23:30",
                    "1h",
                ),
            ],
            "UTC0" => &[
                (
                    "2025-07-15T23:50:00Z",
                    "2025-07-16T00:10:00Z",
                    "yday 23:50",
                    "20m",
                ),
                (
                    "2025-07-15T20:00:00Z",
                    "2025-07-18T00:00:00Z",
                    "Jul 15",
                    "2d",
                ),
            ],
            "EST5" => &[(
                "2025-07-15T20:00:00Z",
                "2025-07-15T21:00:00Z",
                "15:00",
                "1h",
            )],
            other => panic!("unexpected timezone {other}"),
        };
        for &(when, now, clock, age) in cases {
            let now = mv_core::time::parse_flexible(now).unwrap();
            assert_eq!(
                card_corner_from_stamp(when, now),
                (Some(clock.into()), Some(age.into())),
                "TZ={zone}, when={when}, now={now}",
            );
        }
        let now = mv_core::time::parse_flexible("2025-07-16T00:10:00Z").unwrap();
        for unusable in ["", "invalid", "2025-07-16T00:10:01Z"] {
            assert_eq!(card_corner_from_stamp(unusable, now), (None, None));
        }
    }

    /// Reproduces the card's own repro: a board render used to spawn `date`
    /// up to twice per card (once for the card's instant, once for `now`),
    /// so rendering thousands of cards spawned thousands of processes. This
    /// puts a counting stub ahead of the real `date` on `PATH` and renders
    /// 500 cards spanning ~4 years of distinct hours; the stub must see at
    /// most a constant, small number of calls regardless of card count.
    /// Reverting `card_corner_local_time` to spawn `date` per call fails
    /// this (it would record ~1000 calls, one or two per card).
    #[test]
    fn card_corner_rendering_does_not_spawn_a_process_per_card() {
        const CHILD: &str = "MV_BOARD_CORNER_SPAWN_TEST_CHILD";
        let Ok(counter_path) = std::env::var(CHILD) else {
            use std::os::unix::fs::PermissionsExt;

            let dir = tempfile::tempdir().unwrap();
            let counter_path = dir.path().join("date-calls");
            let stub_path = dir.path().join("date");
            std::fs::write(
                &stub_path,
                format!(
                    "#!/bin/sh\necho called >> \"{}\"\nexec /bin/date \"$@\"\n",
                    counter_path.display()
                ),
            )
            .unwrap();
            let mut perms = std::fs::metadata(&stub_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&stub_path, perms).unwrap();

            let path = std::env::var("PATH").unwrap_or_default();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "commands::board_data::tests::card_corner_rendering_does_not_spawn_a_process_per_card",
                    "--test-threads",
                    "4",
                ])
                .env(CHILD, counter_path.to_str().unwrap())
                .env("PATH", format!("{}:{}", dir.path().display(), path))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            let calls = std::fs::read_to_string(&counter_path)
                .map(|text| text.lines().count())
                .unwrap_or(0);
            assert!(
                calls <= 2,
                "expected O(1) process spawns for 500 cards, got {calls}"
            );
            return;
        };
        let counter_path = PathBuf::from(counter_path);

        let start = mv_core::time::parse_flexible("2000-01-01T00:00:00+00:00").unwrap();
        let now = start + time::Duration::hours(500 * 73 + 24);
        for i in 0..500i64 {
            let when = mv_core::time::format_seconds_utc(start + time::Duration::hours(i * 73));
            card_corner_from_stamp(&when, now);
        }
        // Leave the counter file's absence/presence to the parent process
        // to assert on; this child only needs to exit 0 having rendered.
        let _ = counter_path;
    }

    #[test]
    fn format_reset_eta_keeps_hours_and_minutes() {
        assert_eq!(format_reset_eta(0), "under 1m");
        assert_eq!(format_reset_eta(59), "under 1m");
        assert_eq!(format_reset_eta(60), "1m");
        assert_eq!(format_reset_eta(2 * 3_600 + 10 * 60), "2h10m");
        assert_eq!(format_reset_eta(2 * 3_600), "2h");
        assert_eq!(format_reset_eta(5 * 86_400 + 8 * 3_600), "5d8h");
        assert_eq!(format_reset_eta(5 * 86_400), "5d");
    }

    #[test]
    fn fenced_board_detail_matches_python_manual_reset_eta() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let status = breaker::BreakerStatus {
            open: true,
            reset_at: Some(now + time::Duration::hours(2) + time::Duration::minutes(10)),
            rule: Some("weekly".into()),
            kind: Some("weekly".into()),
            vendor: Some("codex".into()),
            caller: None,
            evidence_path: None,
            manual: true,
            gauge_would_reopen: false,
            half_open: false,
            trial_task_id: None,
        };
        assert_eq!(
            fenced_board_detail(&status, now),
            "(manual) resets in 2h10m"
        );
    }

    #[test]
    fn fenced_board_detail_names_a_budget_trip_as_out_of_credits() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let reset = now.replace_year(breaker::NO_AUTOMATIC_RESET_YEAR).unwrap();
        let status = breaker::BreakerStatus {
            open: true,
            reset_at: Some(reset),
            rule: None,
            kind: Some("budget".into()),
            vendor: None,
            caller: None,
            evidence_path: None,
            manual: false,
            gauge_would_reopen: false,
            half_open: false,
            trial_task_id: None,
        };
        assert_eq!(
            fenced_board_detail(&status, now),
            "needs human (out of credits)"
        );
    }

    #[test]
    fn format_tokens_24h_picks_the_right_scale() {
        assert_eq!(format_tokens_24h(999), "999");
        assert_eq!(format_tokens_24h(1_500), "1.5K");
        assert_eq!(format_tokens_24h(2_500_000), "2.5M");
    }

    #[test]
    fn claude_tokens_24h_sums_every_jsonl_under_projects_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("projects").join("some-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("session.jsonl"),
            concat!(
                r#"{"type":"assistant","timestamp_ms":100,"message":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
                "\n",
                r#"{"type":"assistant","timestamp_ms":200,"message":{"usage":{"input_tokens":3}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            claude_tokens_24h(dir.path().to_str().unwrap(), now, state_dir.path()),
            Some(18)
        );
    }

    #[test]
    fn claude_tokens_24h_is_none_without_a_projects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let state_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            claude_tokens_24h(dir.path().to_str().unwrap(), now, state_dir.path()),
            None
        );
    }

    #[test]
    fn claude_tokens_24h_skips_jsonl_older_than_24h() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("projects").join("some-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("session.jsonl");
        let now = now_utc();
        let ts_ms = now.unix_timestamp() * 1000;
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"assistant\",\"timestamp_ms\":{ts_ms},\"message\":{{\"usage\":{{\"input_tokens\":10,\"output_tokens\":5}}}}}}\n"
            ),
        )
        .unwrap();
        let status = std::process::Command::new("touch")
            .args(["-t", "202001010000", path.to_str().unwrap()])
            .status()
            .unwrap();
        assert!(status.success(), "touch -t to age the jsonl mtime");
        let state_dir = tempfile::tempdir().unwrap();
        assert_eq!(
            claude_tokens_24h(dir.path().to_str().unwrap(), now, state_dir.path()),
            Some(0)
        );
    }

    #[test]
    fn claude_tokens_24h_reads_from_cache_rather_than_disk_on_an_unchanged_file() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = dir.path().join("projects").join("some-project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let path = project_dir.join("session.jsonl");
        let original = concat!(
            r#"{"type":"assistant","timestamp_ms":100,"message":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            "\n",
        );
        std::fs::write(&path, original).unwrap();
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let state_dir = tempfile::tempdir().unwrap();

        assert_eq!(
            claude_tokens_24h(dir.path().to_str().unwrap(), now, state_dir.path()),
            Some(15)
        );
        assert!(claude_token_cache_path(state_dir.path()).is_file());

        // Same byte length, same digit counts, different sum (108, not
        // 15) — then the mtime is forced back to its pre-edit value so the
        // file's `(size, mtime)` identity is unchanged. A correct cache
        // hit answers from the cached pairs and still reports 15; only a
        // cache miss (a full re-read) would see 108.
        let replaced = concat!(
            r#"{"type":"assistant","timestamp_ms":100,"message":{"usage":{"input_tokens":99,"output_tokens":9}}}"#,
            "\n",
        );
        assert_eq!(original.len(), replaced.len());
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, replaced).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );

        assert_eq!(
            claude_tokens_24h(dir.path().to_str().unwrap(), now, state_dir.path()),
            Some(15)
        );
    }

    #[test]
    fn codex_tokens_24h_sums_every_jsonl_under_sessions_recursively() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions").join("2026").join("11");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(
            session_dir.join("rollout.jsonl"),
            concat!(
                r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":10,"output_tokens":2}}}}"#,
                "\n",
                r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":40,"output_tokens":9}}}}"#,
                "\n",
            ),
        )
        .unwrap();
        // A freshly written file's mtime is "now," so this exercises the
        // real last-24h window rather than a fixed past instant.
        let now = now_utc();
        assert_eq!(
            codex_tokens_24h(dir.path().to_str().unwrap(), now),
            Some(49)
        );
    }

    #[test]
    fn codex_tokens_24h_is_none_without_a_sessions_directory() {
        let dir = tempfile::tempdir().unwrap();
        let now = OffsetDateTime::from_unix_timestamp(0).unwrap();
        assert_eq!(codex_tokens_24h(dir.path().to_str().unwrap(), now), None);
    }

    #[test]
    fn seats_snapshot_counts_only_recognized_workers() {
        let dir = tempfile::tempdir().unwrap();
        let config = mv_config::load::load_config_str(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"cx\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"/tmp\"\npool = \"codex:test\"\n[projects.sandbox]\npath = \"/tmp/sandbox\"\n[projects.sandbox.chains]\nscout = [\"seat\"]\n",
            Path::new("config.toml"),
        ).unwrap();
        let source = live_source(config, dir.path());
        let mut agents = vec![LiveAgent {
            pane_id: "w1:p1".into(),
            name: "sandbox-worker-cx".into(),
            status: "working".into(),
        }];
        let empty = source.seats_snapshot(Some(&agents));
        assert_eq!(empty.live_agents, Some(0));
        assert_eq!(empty.working, Some(0));
        assert!(empty.unrecognized.is_empty());
        agents.push(LiveAgent {
            pane_id: "w1:p2".into(),
            name: "sandbox-scou-cx".into(),
            status: "working".into(),
        });
        let snapshot = source.seats_snapshot(Some(&agents));
        assert_eq!(snapshot.live_agents, Some(1));
        assert_eq!(snapshot.working, Some(1));
    }

    #[test]
    fn seat_open_count_matches_a_dedup_suffixed_name() {
        let agents = [
            LiveAgent {
                pane_id: String::new(),
                name: "mv-builder-cx-2".into(),
                status: "working".into(),
            },
            LiveAgent {
                pane_id: String::new(),
                name: "mv-critic-cxl".into(),
                status: "idle".into(),
            },
        ];
        assert_eq!(seat_open_count(&agents, "cx"), 1);
        assert_eq!(seat_open_count(&agents, "cl"), 0);
    }

    #[test]
    fn scope_header_names_a_single_project_only() {
        let project = Project {
            name: "sandbox".into(),
            path: mv_config::placeholder::MaybePlaceholder::parse("/tmp/sandbox"),
            chains: Default::default(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        };
        assert_eq!(scope_header(&[project]), "project: sandbox only");
    }

    #[test]
    fn scope_header_lists_several_projects() {
        let project = |name: &str| Project {
            name: name.into(),
            path: mv_config::placeholder::MaybePlaceholder::parse("/tmp"),
            chains: Default::default(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        };
        assert_eq!(
            scope_header(&[project("alpha"), project("bravo")]),
            "projects: alpha, bravo"
        );
    }

    #[test]
    fn project_short_falls_back_to_the_project_name_when_unconfigured() {
        let project = Project {
            name: "Multi-Word".into(),
            path: mv_config::placeholder::MaybePlaceholder::parse("/tmp"),
            chains: Default::default(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        };
        assert_eq!(project_short(&project), "Multi-Word");
    }

    #[test]
    fn project_short_prefers_the_configured_value() {
        let mut project = Project {
            name: "sandbox".into(),
            path: mv_config::placeholder::MaybePlaceholder::parse("/tmp"),
            chains: Default::default(),
            short: None,
            lane_setup: None,
            min_slots: 0,
            max_slots: None,
        };
        project.short = Some("sbx".into());
        assert_eq!(project_short(&project), "sbx");
    }

    fn task_dir(root: &Path, task_id: &str) -> PathBuf {
        let dir = root.join(".multiverse/tasks").join(task_id);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn temp_project(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mv-cli-board-data-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn an_unrecorded_terminal_status_with_no_live_agents_gets_the_generic_note() {
        let root = temp_project("unrecorded-no-herdr");
        let task_id = "20250101-librarian-dddddd";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=librarian\taccount=old-seat\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        // The recorded `role=librarian` is a legacy on-disk key; the card
        // footer shows its current config key, `archivist` — see
        // `display_role_never_title_cases_a_current_config_key`.
        assert_eq!(task.role, "archivist");
        assert_eq!(task.account, "old-seat");
        assert_eq!(
            task.state_note.as_deref(),
            Some("unrecorded, needs --reconcile")
        );
        assert!(task.outcome.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `conformance/diff_fuzz.py` seed 50034: a herdr listing that is merely
    /// reachable is not "pane gone" — the recorded pane must be missing
    /// *from* that listing. A done-but-unrecorded task whose pane is still
    /// listed (just not `working`, e.g. `done`) is the ordinary "needs
    /// --reconcile" case, same as when Herdr cannot be reached at all; it
    /// must not read as "pane gone, status done" (which also forces the
    /// card into BLOCKED via `card::split_dispatched`) just because *some*
    /// agent list came back.
    #[test]
    fn an_unrecorded_done_task_whose_pane_is_still_listed_needs_reconcile_not_pane_gone() {
        let root = temp_project("unrecorded-pane-still-listed");
        let task_id = "20261105-scout-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-05T00:00:00Z\tevent=dispatch\ttask={task_id}\trole=scout\taccount=seat-a\n\
                 2026-11-05T00:00:01Z\tevent=started\ttask={task_id}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"sandbox-scou-ex","agent_status":"done"}]}}"#,
        )
        .unwrap();

        let tasks = resolve_tasks(&root, Some(&parsed), now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].state_note.as_deref(),
            Some("unrecorded, needs --reconcile")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The pane-confirmed-gone counterpart to the test above: the same
    /// done-but-unrecorded task, but its recorded pane is genuinely absent
    /// from a reachable listing. This is the one case `state_note` must
    /// still call "pane gone", so BLOCKED still gets it
    /// (`card::split_dispatched`'s bypass, critic finding 6).
    #[test]
    fn an_unrecorded_done_task_whose_pane_is_confirmed_gone_still_reports_pane_gone() {
        let root = temp_project("unrecorded-pane-confirmed-gone");
        let task_id = "20261105-scout-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-05T00:00:00Z\tevent=dispatch\ttask={task_id}\trole=scout\taccount=seat-a\n\
                 2026-11-05T00:00:01Z\tevent=started\ttask={task_id}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p2","name":"sandbox-other","agent_status":"idle"}]}}"#,
        )
        .unwrap();

        let tasks = resolve_tasks(&root, Some(&parsed), now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].state_note.as_deref(),
            Some("pane gone, status done")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `conformance/diff_fuzz.py` seed 50010: a writing role's status file
    /// lands in its own lane worktree (`.multiverse/worktrees/<task_id>/`),
    /// never the main checkout — the one place this file ever wrote before
    /// `read_writer_status`'s lane-first lookup landed, a task whose agent
    /// had already finished and written `done` to its lane still read
    /// STALLED instead of UNRECORDED, because the main checkout's
    /// `.multiverse/tasks/<task_id>/status` was never written at all.
    #[test]
    fn an_unrecorded_status_written_only_to_the_lane_worktree_is_found() {
        let root = temp_project("unrecorded-status-in-lane-worktree");
        let task_id = "20261105-builder-abcdef";
        let lane_status_dir = root
            .join(".multiverse/worktrees")
            .join(task_id)
            .join(".multiverse/tasks")
            .join(task_id);
        std::fs::create_dir_all(&lane_status_dir).unwrap();
        std::fs::write(lane_status_dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-05T00:00:00Z\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\n\
                 2026-11-05T00:00:01Z\tevent=started\ttask={task_id}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"sandbox-buil-ex","agent_status":"unknown"}]}}"#,
        )
        .unwrap();

        let tasks = resolve_tasks(&root, Some(&parsed), now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].state_note.as_deref(),
            Some("unrecorded, needs --reconcile")
        );
        assert!(tasks[0].outcome.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `conformance/diff_fuzz.py` seed 50034: Python's `_task_card`/
    /// `_backlog_card` print the stored role verbatim, never title-cased —
    /// so a freshly dispatched task whose log already carries the current,
    /// lowercase config key must render exactly that, not `Role::
    /// display_name()`'s capitalized form. A legacy on-disk key maps to
    /// its current config key instead of rendering verbatim, so the
    /// board never shows a retired role name.
    #[test]
    fn display_role_never_title_cases_a_current_config_key() {
        assert_eq!(display_role("scout"), "scout");
        assert_eq!(display_role("builder"), "builder");
        // Legacy on-disk keys render as their current config key — see
        // `an_unrecorded_terminal_status_with_no_live_agents_gets_the_
        // generic_note` for the end-to-end version of this.
        assert_eq!(display_role("librarian"), "archivist");
        assert_eq!(display_role("fixer"), "builder");
        assert_eq!(display_role("orchestrator"), "nexus");
        // A string that is neither a current nor a legacy key renders
        // verbatim.
        assert_eq!(display_role("archaeologist"), "archaeologist");
    }

    #[test]
    fn a_recorded_done_outcome_drops_the_task_entirely() {
        let root = temp_project("recorded-done");
        let task_id = "20261102-builder-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\n\
                 2026-11-02T00:05:00+00:00\tevent=result\ttask={task_id}\tstatus=done\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert!(tasks.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_recorded_blocked_outcome_renders_with_no_note() {
        let root = temp_project("recorded-blocked");
        let task_id = "20261102-builder-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "blocked").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\n\
                 2026-11-02T00:05:00+00:00\tevent=result\ttask={task_id}\tstatus=blocked\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].outcome, Some(TaskOutcome::Blocked));
        assert!(tasks[0].state_note.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_still_dispatched_task_with_unreachable_herdr_stays_running() {
        let root = temp_project("awaiting");
        let task_id = "20261102-builder-abcdef";
        task_dir(&root, task_id);
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].state_note, None);
        assert_eq!(tasks[0].live_status, Some(LiveAgentStatus::Running));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn backlog_snapshot_maps_priority_due_and_asked_state() {
        let root = temp_project("backlog");
        backlog::add(
            &root,
            "fix the thing",
            "claude",
            "it breaks",
            None,
            None,
            None,
            "P1",
            None,
        )
        .unwrap();
        let items = resolve_backlog(&root, now_utc());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "fix the thing");
        assert_eq!(items[0].priority, 1);
        assert_eq!(items[0].status, BacklogStatus::Open);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// An unanswered `asked` item (via `backlog::ask` directly — the CLI's
    /// own `backlog ask --recommend` currently refuses a numeric 1-based
    /// index; see this task's `result.md`) resolves to a snapshot that
    /// `mv_board::card::split_backlog` routes to BLOCKED with the "asked"
    /// footer, end to end through this file's own mapping.
    #[test]
    fn an_asked_backlog_item_renders_as_a_blocked_card_with_the_asked_footer() {
        let root = temp_project("backlog-asked");
        backlog::ask(
            &root,
            "which account should we use?",
            &["A".to_string(), "B".to_string()],
            1,
            "claude",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();

        let items = resolve_backlog(&root, now_utc());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].status, BacklogStatus::Asked);
        assert!(!items[0].has_human_annotation);

        let (blocked, backlog_cards) = mv_board::card::split_backlog(&items);
        assert_eq!(blocked.len(), 1);
        assert!(backlog_cards.is_empty());
        assert_eq!(blocked[0].role.as_deref(), Some("asked"));
        assert_eq!(blocked[0].title, "which account should we use?");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Card `20261102-b-8fad3e`: the task-id universe comes from
    /// `event=dispatch` records, never from listing `.multiverse/tasks/`.
    /// A task directory with no dispatch line in the log (a leftover from
    /// before structured dispatch logging, or a loose non-task file) must
    /// not become a phantom card — the directory here has a `status` file
    /// and would have passed `is_valid_task_id`, but nothing ever
    /// dispatched it.
    #[test]
    fn a_task_directory_with_no_dispatch_record_is_never_a_phantom_card() {
        let root = temp_project("no-dispatch-record");
        let task_id = "20260906-explorer-279f31";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        // No routing.log at all — the directory predates structured logging.

        let tasks = resolve_tasks(&root, None, now_utc());
        assert!(tasks.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The mirror image of the above: a dispatched task discovered purely
    /// from the log, with no `.multiverse/tasks/<id>` directory ever
    /// created on disk (e.g. a role dispatched straight to `main`, or a
    /// task directory a prior cleanup already removed). `tasks.py` never
    /// needed a directory to know a task exists; this must not either.
    #[test]
    fn a_dispatched_task_with_no_task_directory_still_resolves() {
        let root = temp_project("no-task-dir");
        let task_id = "20261102-builder-abcdef";
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task_id, task_id);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn row36_title_prefers_log_then_project_brief_then_owning_lane() {
        let root = temp_project("titles");
        let task = "20261105-scout-abcdef";
        let dir = task_dir(&root, task);
        let log = root.join(".multiverse/routing.log");
        let dispatch = format!("2026-11-05T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\tbranch=multiverse/owner\n");
        std::fs::write(&log, &dispatch).unwrap();
        std::fs::write(
            dir.join("brief.md"),
            "# Task\n## Goal\n\nRecovered goal\n## Constraints\n",
        )
        .unwrap();
        assert_eq!(
            resolve_tasks(&root, None, now_utc())[0].title.as_deref(),
            Some("Recovered goal")
        );
        std::fs::write(
            &log,
            dispatch.replace("\trole=", "\ttitle=\"Logged goal\"\trole="),
        )
        .unwrap();
        assert_eq!(
            resolve_tasks(&root, None, now_utc())[0].title.as_deref(),
            Some("Logged goal")
        );
        std::fs::write(&log, &dispatch).unwrap();
        std::fs::remove_file(dir.join("brief.md")).unwrap();
        let lane = root
            .join(".multiverse/worktrees/owner/.multiverse/tasks")
            .join(task);
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::write(lane.join("brief.md"), "## Goal\nLane goal\n").unwrap();
        assert_eq!(
            resolve_tasks(&root, None, now_utc())[0].title.as_deref(),
            Some("Lane goal")
        );
        std::fs::write(dir.join("brief.md"), "## Goal\n## Empty\n").unwrap();
        assert_eq!(resolve_tasks(&root, None, now_utc())[0].title, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row36_json_title_decodes_non_bmp_and_sanitizes_terminal_controls() {
        let root = temp_project("title-json");
        let task = "20000101-scout-abcdef";
        task_dir(&root, task);
        let title = r#""Goal \ud83d\ude00 \u001b]52;c;payload\u0007safe""#;
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2000-01-01T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\ttitle={title}\n"
            ),
        )
        .unwrap();
        let found = resolve_tasks(&root, None, now_utc());
        assert_eq!(found[0].title.as_deref(), Some("Goal 😀 safe"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row9_started_pane_controls_liveness_even_without_dispatch_agent() {
        let root = temp_project("pane-identity");
        let task = "20261105-scout-abcdef";
        task_dir(&root, task);
        let log = root.join(".multiverse/routing.log");
        let history = format!("2026-11-05T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\n2026-11-05T00:00:01Z\tevent=started\ttask={task}\tpane=w1:p1\n");
        std::fs::write(&log, &history).unwrap();
        let parsed = parse_live_agents(r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"reissued","agent_status":"working"}]}}"#).unwrap();
        assert_eq!(
            resolve_tasks(&root, Some(&parsed), now_utc())[0].state_note,
            None
        );
        let parsed = parse_live_agents(r#"{"result":{"agents":[{"pane_id":"w1:p2","name":"reissued","agent_status":"working"}]}}"#).unwrap();
        std::fs::write(&log, history.replace("\trole=", "\tagent=reissued\trole=")).unwrap();
        assert_eq!(
            resolve_tasks(&root, Some(&parsed), now_utc())[0]
                .state_note
                .as_deref(),
            Some("awaiting, no live agent")
        );
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","agent_status":"unknown"}]}}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_tasks(&root, Some(&parsed), now_utc())[0]
                .state_note
                .as_deref(),
            Some("stalled, agent is live but not working")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// `conformance/diff_fuzz.py` seed 50005: a dispatch line from before
    /// `agent=` was recorded on it (this project's own fixture history is
    /// one such line — no `agent=` field at all) must not be promoted to
    /// BLOCKED just because its live pane now reads `blocked` — Python's
    /// `t.agent and t.agent in herdr_blocked` can never pass with no
    /// recorded name, so the task stays STALLED/IN PROGRESS, same as it
    /// would for `idle`/`done`/`unknown`.
    #[test]
    fn a_herdr_blocked_pane_with_no_recorded_agent_name_stays_stalled() {
        let root = temp_project("blocked-no-agent-name");
        let task = "20261105-scout-abcdef";
        task_dir(&root, task);
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-05T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\n2026-11-05T00:00:01Z\tevent=started\ttask={task}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"sandbox-other","agent_status":"blocked"}]}}"#,
        )
        .unwrap();
        let found = resolve_tasks(&root, Some(&parsed), now_utc());
        assert_eq!(found[0].live_status, Some(LiveAgentStatus::Stalled));
        assert_eq!(
            found[0].state_note.as_deref(),
            Some("stalled, agent is live but not working")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The counterpart to the test above: once the dispatch line *does*
    /// carry the `agent=` name that now reads `blocked`, the task is routed
    /// to BLOCKED outright, with no state note (`board.py`'s `_blocked_card`
    /// never calls `_state_note`).
    #[test]
    fn a_herdr_blocked_pane_with_its_recorded_agent_name_lands_in_blocked() {
        let root = temp_project("blocked-with-agent-name");
        let task = "20261105-scout-abcdef";
        task_dir(&root, task);
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-05T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\tagent=sandbox-other\n2026-11-05T00:00:01Z\tevent=started\ttask={task}\tpane=w1:p1\n"
            ),
        )
        .unwrap();
        let parsed = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"sandbox-other","agent_status":"blocked"}]}}"#,
        )
        .unwrap();
        let found = resolve_tasks(&root, Some(&parsed), now_utc());
        assert_eq!(found[0].live_status, Some(LiveAgentStatus::HerdrBlocked));
        assert_eq!(found[0].state_note, None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row34_cwd_selects_longest_configured_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("nested");
        let child = nested.join("src");
        std::fs::create_dir_all(&child).unwrap();
        let text = format!(
            "[projects.outer]\npath = {:?}\n[projects.inner]\npath = {:?}\n",
            root.path().to_str().unwrap(),
            nested.to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&text, Path::new("config.toml")).unwrap();
        assert_eq!(cwd_project_at(&config, &child).unwrap().name, "inner");
        assert_eq!(cwd_project_at(&config, root.path()).unwrap().name, "outer");
        let neighbor = root.path().with_extension("neighbor");
        std::fs::create_dir_all(&neighbor).unwrap();
        assert!(cwd_project_at(&config, &neighbor).is_none());
        std::fs::remove_dir_all(neighbor).unwrap();
    }

    fn fake_board_herdr(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("herdr-fake");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn row33_active_filters_named_and_all_scopes_by_workspace_labels() {
        let dir = tempfile::tempdir().unwrap();
        let config = mv_config::load::load_config_str(
            "[projects.one]\npath = '/tmp/one'\n[projects.two]\npath = '/tmp/two'\n",
            Path::new("config.toml"),
        )
        .unwrap();
        let mut source = live_source(config, dir.path());
        source.active = true;
        let fake = fake_board_herdr(
            dir.path(),
            r#"printf '%s\n' '{"result":{"workspaces":[{"label":"two"}]}}'"#,
        );
        let resolved = source
            .resolve_scope_with(
                &BoardScope::Named(vec!["one".into()]),
                fake.to_str().unwrap(),
            )
            .unwrap();
        assert!(matches!(resolved, Resolved::Message(lines) if lines == ["no active projects"]));
        let Resolved::Projects(projects, header) = source
            .resolve_scope_with(&BoardScope::All, fake.to_str().unwrap())
            .unwrap()
        else {
            panic!("expected projects")
        };
        assert_eq!(
            projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
            ["two"]
        );
        assert_eq!(header.as_deref(), Some("fleet: all 1 project"));
    }

    #[test]
    fn row7_default_board_records_settled_tasks_closes_idle_and_expires_awaiting() {
        let root = temp_project("board-reconcile");
        let done = "20000101-scout-aaaaaa";
        let awaiting = "20000101-scout-bbbbbb";
        let dir = task_dir(&root, done);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(dir.join("result.md"), "Useful result\n").unwrap();
        std::fs::write(root.join(".multiverse/routing.log"), format!("2000-01-01T00:00:00Z\tevent=dispatch\ttask={done}\trole=scout\taccount=seat\tagent=scout\n2000-01-01T00:01:00Z\tevent=started\ttask={done}\tpane=w1:p1\n2000-01-01T00:00:00Z\tevent=dispatch\ttask={awaiting}\trole=scout\taccount=seat\n")).unwrap();
        let config = mv_config::load::load_config_str(
            &format!("[projects.demo]\npath = {:?}\n", root.to_str().unwrap()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut source = live_source(config, &root.join("state"));
        source.reconcile = true;
        let fake = fake_board_herdr(
            &root,
            r#"case "$1 $2" in
'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","name":"scout","agent_status":"idle"}]}}' ;;
'pane close') printf '%s\n' "$3" >> "$(dirname "$0")/closed" ;;
*) exit 1 ;;
esac"#,
        );
        let (_, view) = source
            .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
            .unwrap();
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains(&format!("event=result\ttask={done}\tstatus=done")),
            "{log}"
        );
        assert!(
            log.contains(&format!("event=result\ttask={awaiting}\tstatus=failed")),
            "{log}"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("closed")).unwrap().trim(),
            "w1:p1"
        );
        let BoardView::Project(snapshot) = view else {
            panic!("one project")
        };
        assert!(snapshot.project.tasks.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    /// Sibling to the test above, pinned by 47d6342: a legacy dispatch line
    /// with no recorded `agent=` field must leave the pane open rather than
    /// guessing it belongs to the live `scout` agent.
    /// `close_settled_panes`' identity check reads
    /// `current.name != recorded_agent.unwrap_or("")`, and an empty string
    /// never matches a real agent name, so the pane is never closed.
    #[test]
    fn row7_legacy_dispatch_without_agent_leaves_pane_open() {
        let root = temp_project("board-reconcile-legacy");
        let done = "20000101-scout-aaaaaa";
        let awaiting = "20000101-scout-bbbbbb";
        let dir = task_dir(&root, done);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(dir.join("result.md"), "Useful result\n").unwrap();
        std::fs::write(root.join(".multiverse/routing.log"), format!("2000-01-01T00:00:00Z\tevent=dispatch\ttask={done}\trole=scout\taccount=seat\n2000-01-01T00:01:00Z\tevent=started\ttask={done}\tpane=w1:p1\n2000-01-01T00:00:00Z\tevent=dispatch\ttask={awaiting}\trole=scout\taccount=seat\n")).unwrap();
        let config = mv_config::load::load_config_str(
            &format!("[projects.demo]\npath = {:?}\n", root.to_str().unwrap()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut source = live_source(config, &root.join("state"));
        source.reconcile = true;
        let fake = fake_board_herdr(
            &root,
            r#"case "$1 $2" in
'agent list') printf '%s\n' '{"result":{"agents":[{"pane_id":"w1:p1","name":"scout","agent_status":"idle"}]}}' ;;
'pane close') printf '%s\n' "$3" >> "$(dirname "$0")/closed" ;;
*) exit 1 ;;
esac"#,
        );
        source
            .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
            .unwrap();
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains(&format!("event=result\ttask={done}\tstatus=done")),
            "{log}"
        );
        assert!(
            log.contains(&format!("event=result\ttask={awaiting}\tstatus=failed")),
            "{log}"
        );
        assert!(!root.join("closed").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row7_read_modes_and_unreachable_herdr_skip_reconcile() {
        let root = temp_project("board-no-reconcile");
        let task = "20000101-scout-aaaaaa";
        let dir = task_dir(&root, task);
        std::fs::write(dir.join("status"), "failed").unwrap();
        let history = format!("2000-01-01T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\n");
        let log = root.join(".multiverse/routing.log");
        std::fs::write(&log, &history).unwrap();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[projects.demo]\npath = {:?}\n", root.to_str().unwrap()),
        )
        .unwrap();
        let fake = fake_board_herdr(&root, r#"printf '%s\n' '{"result":{"agents":[]}}'"#);
        for flag in ["--no-reconcile", "--read-only", "--watch"] {
            let args = crate::args::command()
                .try_get_matches_from([
                    "multiverse",
                    "--config",
                    config_path.to_str().unwrap(),
                    "board",
                    flag,
                ])
                .unwrap();
            let mut source = LiveSource::load(args.subcommand_matches("board").unwrap()).unwrap();
            source.state_dir = root.join("state");
            assert!(!source.reconcile, "{flag}");
            source
                .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
                .unwrap();
            assert_eq!(std::fs::read_to_string(&log).unwrap(), history, "{flag}");
            assert!(!root.join(".multiverse/orchestrator-pass").exists());
        }
        let args = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "--config",
                config_path.to_str().unwrap(),
                "board",
            ])
            .unwrap();
        let mut source = LiveSource::load(args.subcommand_matches("board").unwrap()).unwrap();
        source.state_dir = root.join("state");
        assert!(source.reconcile);
        let down = fake_board_herdr(&root, "exit 1");
        source
            .snapshot_with(&BoardScope::All, down.to_str().unwrap())
            .unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row37_one_shot_board_stamps_both_orchestrator_pass_legs() {
        let root = temp_project("board-pass");
        let config = mv_config::load::load_config_str(
            &format!("[projects.demo]\npath = {:?}\n", root.to_str().unwrap()),
            Path::new("config.toml"),
        )
        .unwrap();
        let mut source = live_source(config, &root.join("state"));
        source.reconcile = true;
        let fake = fake_board_herdr(&root, r#"printf '%s\n' '{"result":{"agents":[]}}'"#);
        let before = now_utc();
        source
            .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
            .unwrap();
        let (reconcile, backlog) = read_orchestrator_pass(&root.join(".multiverse"));
        assert!(reconcile.is_some());
        assert_eq!(reconcile, backlog);
        let stamp = mv_core::time::parse_flexible(reconcile.as_deref().unwrap()).unwrap();
        assert!(stamp >= before.replace_nanosecond(0).unwrap() && stamp <= now_utc());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row8_idle_done_unknown_dialogs_are_blocked_without_limit_writes() {
        let root = temp_project("board-dialogs");
        let task = "20000101-scout-aaaaaa";
        task_dir(&root, task);
        let stamp = mv_core::time::now_stamp();
        let history = format!("{stamp}\tevent=dispatch\ttask={task}\trole=scout\taccount=seat\tagent=mv-scout-cx\n{stamp}\tevent=started\ttask={task}\tpane=w1:p1\n");
        let log = root.join(".multiverse/routing.log");
        std::fs::write(&log, &history).unwrap();
        let config = mv_config::load::load_config_str(&format!("[accounts.seat]\nkind = 'codex'\nconfig_dir_var = 'CODEX_HOME'\nconfig_dir = {:?}\nshort = 'cx'\n[projects.demo]\npath = {:?}\n", root.join("vendor").to_str().unwrap(), root.to_str().unwrap()), Path::new("config.toml")).unwrap();
        let source = live_source(config, &root.join("state"));
        std::fs::write(
            root.join("pane-text"),
            include_str!("../../../../tests/fixtures/rate-limit/codex-out-of-credits.txt"),
        )
        .unwrap();
        let fake = fake_board_herdr(
            &root,
            r#"cd "$(dirname "$0")" || exit 1
case "$1 $2" in
'agent list') printf '{"result":{"agents":[{"pane_id":"w1:p1","name":"mv-scout-cx","agent_status":"%s"}]}}\n' "$(cat pane-status)" ;;
'agent read') printf '%s\n' "$3" >> reads; cat pane-text ;;
*) exit 1 ;;
esac"#,
        );
        for status in ["idle", "done", "unknown"] {
            std::fs::write(root.join("pane-status"), status).unwrap();
            let (_, view) = source
                .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
                .unwrap();
            let BoardView::Project(snapshot) = view else {
                panic!("one project")
            };
            let (blocked, progress) = mv_board::card::split_dispatched(&snapshot.project.tasks);
            assert_eq!(blocked.len(), 1, "{status}");
            assert!(progress.is_empty());
            assert!(blocked[0]
                .status_text
                .as_deref()
                .unwrap()
                .contains("blocked on prompt:"));
        }
        assert_eq!(
            std::fs::read_to_string(root.join("reads"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        std::fs::write(root.join("pane-status"), "working").unwrap();
        source
            .snapshot_with(&BoardScope::All, fake.to_str().unwrap())
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("reads"))
                .unwrap()
                .lines()
                .count(),
            3
        );
        assert_eq!(std::fs::read_to_string(log).unwrap(), history);
        assert!(
            !root.join("state").exists(),
            "prompt inspection must not trip a breaker or write evidence"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row38_stuck_requires_two_passes_and_nudged_reads_existing_memory() {
        let root = temp_project("board-stuck");
        let task = "20000101-scout-aaaaaa";
        task_dir(&root, task);
        let history = format!("2000-01-01T00:00:00Z\tevent=dispatch\ttask={task}\trole=scout\taccount=seat\tagent=mv-scout-cx\n2000-01-01T00:00:01Z\tevent=started\ttask={task}\tpane=w1:p1\n");
        let log = root.join(".multiverse/routing.log");
        std::fs::write(&log, &history).unwrap();
        let config = mv_config::load::load_config_str(&format!("[accounts.seat]\nkind = 'codex'\nconfig_dir_var = 'CODEX_HOME'\nconfig_dir = {:?}\nshort = 'cx'\n[projects.demo]\npath = {:?}\n", root.join("vendor").to_str().unwrap(), root.to_str().unwrap()), Path::new("config.toml")).unwrap();
        let source = live_source(config, &root.join("state"));
        let now = now_utc();
        assert!(mv_state::nudge_memory::claim_finish_nudge(&root, task, now));
        std::fs::write(root.join("pane-text"), "Reconnecting: connection lost").unwrap();
        let fake = fake_board_herdr(
            &root,
            r#"[ "$1 $2" = 'pane read' ] || exit 1
cat "$(dirname "$0")/pane-text""#,
        );
        let live = parse_live_agents(r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"mv-scout-cx","agent_status":"unknown"}]}}"#).unwrap();
        let project = source.config.projects.get("demo").unwrap();
        let first = source.project_snapshot(project, Some(&live), now, fake.to_str().unwrap());
        assert!(first.tasks[0]
            .state_note
            .as_deref()
            .unwrap_or_default()
            .contains(&format!("nudged: {task}")));
        assert!(!first.tasks[0]
            .state_note
            .as_deref()
            .unwrap_or_default()
            .contains("stuck:"));
        let second = source.project_snapshot(
            project,
            Some(&live),
            now + time::Duration::seconds(1),
            fake.to_str().unwrap(),
        );
        let note = second.tasks[0].state_note.as_deref().unwrap_or_default();
        assert!(note.contains(&format!("stuck: {task} (seat)")), "{note}");
        assert!(note.contains(&format!("nudged: {task}")));
        std::fs::write(root.join("pane-text"), "Normal work").unwrap();
        let third = source.project_snapshot(
            project,
            Some(&live),
            now + time::Duration::seconds(2),
            fake.to_str().unwrap(),
        );
        assert!(!third.tasks[0]
            .state_note
            .as_deref()
            .unwrap_or_default()
            .contains("stuck:"));
        assert_eq!(std::fs::read_to_string(log).unwrap(), history);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn row9_legacy_name_fallback_belongs_only_to_its_newest_dispatch() {
        let root = temp_project("legacy-name");
        let older = "20000101-scout-aaaaaa";
        let newer = "20000101-scout-bbbbbb";
        task_dir(&root, older);
        let log = root.join(".multiverse/routing.log");
        std::fs::write(&log, format!("2000-01-01T00:00:00Z\tevent=dispatch\ttask={older}\trole=scout\tagent=shared\n2000-01-01T00:00:01Z\tevent=dispatch\ttask={newer}\trole=scout\tagent=shared\n")).unwrap();
        let live = parse_live_agents(
            r#"{"result":{"agents":[{"pane_id":"w1:p1","name":"shared","agent_status":"idle"}]}}"#,
        )
        .unwrap();
        let found = resolve_tasks(&root, Some(&live), now_utc());
        assert_eq!(
            found
                .iter()
                .find(|task| task.task_id == older)
                .unwrap()
                .state_note
                .as_deref(),
            Some("awaiting, no live agent")
        );
        assert_eq!(
            found
                .iter()
                .find(|task| task.task_id == newer)
                .unwrap()
                .state_note,
            None
        );
        let _ = std::fs::remove_dir_all(root);
    }

    fn init_git_lane(lane_root: &Path) {
        std::fs::create_dir_all(lane_root).unwrap();
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(lane_root)
                .args(args)
                .status()
                .unwrap()
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "test"]);
        std::fs::write(lane_root.join("README.md"), "hello\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "init"]);
    }

    /// Card `20261102-b-...` / critic finding 6: a writing role's status
    /// file says `done` with no `event=result` confirming it, and its own
    /// lane still holds an uncommitted change — `dirty_done_detail` must
    /// be populated (and the task not dropped as finished), so
    /// `mv_board::card::split_dispatched` routes it to BLOCKED instead of
    /// silently finishing it.
    #[test]
    fn a_done_status_with_an_uncommitted_lane_is_dirty_done_not_finished() {
        let root = temp_project("dirty-done");
        let task_id = "20261102-builder-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\tbranch=multiverse/{task_id}\n"
            ),
        )
        .unwrap();
        let lane_root = root.join(".multiverse/worktrees").join(task_id);
        init_git_lane(&lane_root);
        std::fs::write(lane_root.join("uncommitted.txt"), "oops\n").unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        let task = &tasks[0];
        assert!(task.dirty_done_detail.is_some(), "{task:?}");
        assert!(task
            .dirty_done_detail
            .as_deref()
            .unwrap()
            .contains("untracked"));

        let (blocked, in_progress) = mv_board::card::split_dispatched(&tasks);
        assert_eq!(blocked.len(), 1);
        assert!(in_progress.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same status/role/branch shape, but a clean lane: `_dirty_done`
    /// never fires, and the task is dropped as genuinely finished — same
    /// as any other RECORDED-free `done` status with a clean tree.
    #[test]
    fn a_done_status_with_a_clean_lane_is_not_dirty_done() {
        let root = temp_project("clean-done-lane");
        let task_id = "20261102-builder-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=builder\taccount=seat-a\tbranch=multiverse/{task_id}\n"
            ),
        )
        .unwrap();
        let lane_root = root.join(".multiverse/worktrees").join(task_id);
        init_git_lane(&lane_root);

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].dirty_done_detail.is_none());
        assert_eq!(
            tasks[0].state_note.as_deref(),
            Some("unrecorded, needs --reconcile")
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A non-writing role (no lane of its own) never asks git anything,
    /// even with a `done` status and no result event — `is_writing_role`
    /// gates `dirty_lane_detail` before it ever touches disk.
    #[test]
    fn a_non_writing_role_is_never_checked_for_a_dirty_lane() {
        let root = temp_project("non-writing-done");
        let task_id = "20261102-scout-abcdef";
        let dir = task_dir(&root, task_id);
        std::fs::write(dir.join("status"), "done").unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={task_id}\trole=scout\taccount=seat-a\n"
            ),
        )
        .unwrap();

        let tasks = resolve_tasks(&root, None, now_utc());
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].dirty_done_detail.is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn one_account_config(toml_extra: &str) -> Config {
        let content = format!(
            r#"
[accounts.anthropic-main]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
short = "cl"
{toml_extra}
"#
        );
        mv_config::load::load_config_str(&content, Path::new("config.toml")).expect("valid fixture")
    }

    fn live_source(config: Config, state_dir: &Path) -> LiveSource {
        LiveSource {
            config,
            state_dir: state_dir.to_path_buf(),
            active: false,
            reconcile: false,
            agent_fingerprint: std::cell::Cell::new(0),
        }
    }

    /// Critic finding 6: a seat's cap/lend are written keyed by its
    /// *name* (`accounts limit`/`accounts lend`, via
    /// `commands::accounts::LiveConfig::seat_name`), not its `short`
    /// abbreviation. Reading either back by `short` silently missed every
    /// cap/lend a human actually set — this seat's `short` ("cl") is
    /// deliberately different from its `name` ("anthropic-main") so the
    /// old bug would show up as AVAILABLE/uncapped here instead of AT CAP.
    #[test]
    fn account_snapshot_reads_cap_by_the_accounts_own_name_not_its_short() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        mv_engine::seatstate::set_cap(dir.path(), "anthropic-main", Some(1)).unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let agents = [LiveAgent {
            pane_id: String::new(),
            name: "sandbox-buil-cl".into(),
            status: "working".into(),
        }];
        let snapshot = source.account_snapshot(&account, Some(&agents), "12:00", now_utc());
        assert_eq!(snapshot.seats_total, Some(1));
        assert_eq!(snapshot.state, AccountState::AtCap);
    }

    /// A capped seat whose open-agent count is unreadable (Herdr
    /// unreachable, `agents: None`) reads AT CAP, the same fail-closed
    /// posture `selector::availability`'s own cap check takes — not
    /// AVAILABLE, which would read as uncapped. An uncapped seat is
    /// unaffected (`account_snapshot_falls_back_to_a_window_placeholder_
    /// with_no_recorded_gauge` above covers that case, implicitly, by
    /// asserting `Available` with no cap configured at all).
    #[test]
    fn account_snapshot_reads_at_cap_when_the_open_count_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        mv_engine::seatstate::set_cap(dir.path(), "anthropic-main", Some(1)).unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.state, AccountState::AtCap);
        assert_eq!(snapshot.seats_total, Some(1));
        assert_eq!(snapshot.seats_free, None);
    }

    /// A seat still scheduled `unavailable_until` a future time reads
    /// FENCED, matching `selector::availability`'s check 2 — `board.py`'s
    /// own `account_states` applies the identical check.
    #[test]
    fn account_snapshot_fences_a_seat_still_unavailable_until_the_future() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("unavailable_until = \"2999-01-01T00:00:00+00:00\"");
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.state, AccountState::Fenced);
    }

    /// A seat lent off (`accounts lend <seat> off`) reads FENCED even
    /// though `available = true` and no breaker is tripped — check 3 of
    /// `selector::availability`, a human's own switch.
    #[test]
    fn account_snapshot_fences_a_seat_lent_off() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        mv_engine::seatstate::set_lend(dir.path(), "anthropic-main", false).unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.state, AccountState::Fenced);
        assert!(snapshot.detail.as_deref().unwrap().contains("lent off"));
    }

    /// A claude seat with nothing recorded in the shared gauge store still
    /// gets a window line — the literal `board.py`-style fallback, not a
    /// blank card (card `20261102-b-c1a2e5`).
    #[test]
    fn account_snapshot_falls_back_to_a_window_placeholder_with_no_recorded_gauge() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.quota_windows, vec!["window: n/a (API billing)"]);
    }

    /// A claude seat whose pool already has a recorded gauge (written by
    /// some other probe, e.g. a classified pane banner) shows that
    /// reading — the same `mv_state::gauge` store `breaker::pool_status`
    /// already reads, so no new dependency is needed for this (card
    /// `20261102-b-c1a2e5`'s own worry).
    #[test]
    fn account_snapshot_reads_a_recorded_gauge_for_a_non_codex_seat() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        let pool = source_pool(&config, "anthropic-main");
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.entry(pool.clone()).or_default().percent_left = Some(40.0);
        })
        .unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.quota_windows, vec!["primary: 40% left"]);
    }

    fn source_pool(config: &Config, name: &str) -> String {
        config.accounts.get(name).unwrap().pool.clone()
    }

    /// A seat otherwise usable but at or below the fleet-wide quota floor
    /// reads DRAINING, not AVAILABLE — `board.py`'s `is_draining` check,
    /// gated on `dispatch_min_quota_percent` (0 disables it, so this test
    /// sets one explicitly).
    #[test]
    fn account_snapshot_marks_a_usable_seat_draining_at_or_below_the_fleet_floor() {
        let dir = tempfile::tempdir().unwrap();
        let config = one_account_config("");
        let mut config = config;
        config.dispatch_min_quota_percent = 10;
        let pool = source_pool(&config, "anthropic-main");
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.entry(pool.clone()).or_default().percent_left = Some(5.0);
        })
        .unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.state, AccountState::Draining);
    }

    /// Card `20261105-b-901769`: `QuotaReading::at_or_below` used to
    /// compare raw percents, so a gauge whose own window had already
    /// reset still read DRAINING on the board. `board.py`'s `is_draining`
    /// ignores that window; this reuses `accounts::gauge_is_stale` so
    /// the two surfaces agree.
    #[test]
    fn account_snapshot_does_not_mark_draining_from_a_stale_gauge() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = one_account_config("");
        config.dispatch_min_quota_percent = 10;
        let pool = source_pool(&config, "anthropic-main");
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                pool.clone(),
                mv_state::gauge::GaugeEntry {
                    percent_left: Some(5.0),
                    observed_at: Some("2020-01-01T00:00:00Z".to_owned()),
                    resets_at: Some("2020-01-01T05:00:00Z".to_owned()),
                    window_minutes: Some(300),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let source = live_source(config, dir.path());
        let account = source
            .config
            .accounts
            .get("anthropic-main")
            .unwrap()
            .clone();
        let snapshot = source.account_snapshot(&account, None, "12:00", now_utc());
        assert_eq!(snapshot.state, AccountState::Available);
    }

    /// A stale primary must not hide a still-live, still-critical
    /// secondary — the same per-window split `accounts::draining_detail`
    /// already applies through `gauge_is_stale`.
    #[test]
    fn at_or_below_ignores_a_stale_primary_but_still_sees_a_live_secondary() {
        // One hour after observation, well inside GAUGE_MAX_AGE, so only
        // a window whose own reset clock is already past is stale.
        let now = time::macros::datetime!(2020-01-01 01:00:00 UTC);
        let reading = QuotaReading {
            primary: Some(1.0),
            secondary: Some(5.0),
            observed_at: breaker::parse_stored_timestamp("2020-01-01T00:00:00Z"),
            resets_at: breaker::parse_stored_timestamp("2020-01-01T00:30:00Z"),
            resets_at_secondary: breaker::parse_stored_timestamp("2020-01-08T00:00:00Z"),
            window_minutes: Some(300),
            window_minutes_secondary: Some(10080),
        };
        assert!(
            reading.at_or_below(10.0, now),
            "a live secondary at or below the floor is still draining"
        );
        let both_stale = QuotaReading {
            resets_at_secondary: breaker::parse_stored_timestamp("2020-01-01T00:30:00Z"),
            window_minutes_secondary: Some(300),
            ..reading
        };
        assert!(
            !both_stale.at_or_below(10.0, now),
            "once every window is past its own reset, the reading is not draining"
        );
    }

    /// Card `20261102-b-624f17`'s ordering gap, hit from the board's own
    /// `--all` scope this time: `config.projects` is a `BTreeMap`, so
    /// resolving `BoardScope::All` by calling `.values()` on it lists
    /// `alpha` before `zeta` regardless of which one `config.toml`
    /// declared first. Python's board walks `config.projects` in file
    /// order, so this must too.
    #[test]
    fn board_scope_all_lists_projects_in_declaration_order_not_alphabetical_order() {
        let content = r#"
[accounts.seat]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
short = "cl"

[projects.zeta]
path = "/tmp/zeta"

[projects.zeta.chains]
scout = ["seat"]

[projects.alpha]
path = "/tmp/alpha"

[projects.alpha.chains]
scout = ["seat"]
"#;
        let config = mv_config::load::load_config_str(content, Path::new("config.toml"))
            .expect("valid fixture");
        let dir = tempfile::tempdir().unwrap();
        let source = live_source(config, dir.path());
        let resolved = source.resolve_scope(&BoardScope::All).unwrap();
        let Resolved::Projects(projects, _) = resolved else {
            panic!("expected BoardScope::All to resolve to a project list");
        };
        let names: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["zeta", "alpha"]);
    }

    #[test]
    fn task_event_index_matches_per_task_last_write_scans() {
        let root = temp_project("task-event-index");
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        let a = "20261102-builder-aaaaaa";
        let b = "20261102-scout-bbbbbb";
        std::fs::write(
            root.join(".multiverse/routing.log"),
            format!(
                "2026-11-02T00:00:00+00:00\tevent=dispatch\ttask={a}\trole=builder\taccount=first\n\
                 2026-11-02T00:01:00+00:00\tevent=dispatch\ttask={a}\trole=builder\taccount=second\n\
                 2026-11-02T00:02:00+00:00\tevent=started\ttask={a}\n\
                 2026-11-02T00:03:00+00:00\tevent=limit\ttask={a}\n\
                 2026-11-02T00:04:00+00:00\tevent=limit\ttask={a}\n\
                 2026-11-02T00:05:00+00:00\tevent=result\ttask={a}\tstatus=none\n\
                 2026-11-02T00:06:00+00:00\tevent=result\ttask={a}\tstatus=blocked\n\
                 2026-11-02T00:07:00+00:00\tevent=acknowledged\ttask={a}\n\
                 2026-11-02T00:08:00+00:00\tevent=dispatch\ttask={b}\trole=scout\taccount=seat-b\n"
            ),
        )
        .unwrap();
        let records: Vec<LogRecord> =
            index_records(&root.join(".multiverse/routing.log"), Selector::Tasks)
                .unwrap()
                .iter()
                .filter_map(|r| r.parse().ok())
                .collect();
        let events = index_task_events(&records);
        for (task_id, entry) in &events {
            assert_eq!(
                entry.dispatch,
                latest_matching(&records, "dispatch", task_id)
            );
            assert_eq!(entry.started, latest_matching(&records, "started", task_id));
            assert_eq!(
                entry.acknowledged,
                latest_matching(&records, "acknowledged", task_id)
            );
            assert_eq!(entry.outcome, latest_outcome(&records, task_id));
            let limited = records
                .iter()
                .filter(|r| {
                    r.event() == "limit"
                        && r.fields.get("task").map(String::as_str) == Some(*task_id)
                })
                .count() as u32;
            assert_eq!(entry.limited_count, limited);
        }
        assert_eq!(
            events
                .get(a)
                .unwrap()
                .dispatch
                .unwrap()
                .fields
                .get("account"),
            Some(&"second".to_string())
        );
        assert_eq!(events.get(a).unwrap().limited_count, 2);
        assert_eq!(
            events.get(a).unwrap().outcome.unwrap().fields.get("status"),
            Some(&"blocked".to_string())
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}

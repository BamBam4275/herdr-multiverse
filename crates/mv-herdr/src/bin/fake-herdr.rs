//! A real `herdr`-shaped executable for the conformance harness: it
//! answers from the captures in `fixtures/herdr/` instead of a live
//! Herdr install.
//!
//! Set MV_FAKE_HERDR_STATE to a disposable JSON path to persist mutations
//! across subprocesses. Without it, only read-only capture queries are allowed.
//! Set MV_FAKE_HERDR_CALLS to a disposable JSONL path to log every argv,
//! including failed calls, and serialize logged transactions across processes.
//!
//! `script queue-text --file <path>` is a test-only hook: it queues pane
//! text that the next `agent start` / `agent rename` will stamp instead of
//! the default `Ready >` prompt, so a live dispatch can drive
//! `limits::classify` against captured `tests/fixtures/rate-limit/` banners
//! without wrapping this binary. The queue is consumed one start/rename at
//! a time; workspace create/close are owned by another lane.

use mv_herdr::shapes::{pre_rename_agent, Shape};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

type FakeResult<T> = Result<T, String>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct State {
    workspaces: Vec<Value>,
    panes: Vec<Value>,
    agents: Vec<Value>,
    tabs: Vec<Value>,
    next_pane: u64,
    next_workspace: u64,
    sequence: u64,
    text: BTreeMap<String, String>,
    /// One-shot pane banners for the next `agent start` / `agent rename`.
    /// Missing from older state files; empty means the default Ready prompt.
    #[serde(default)]
    pending_text: Vec<String>,
    /// Prompts pasted without their first Enter, keyed by pane id.
    #[serde(default)]
    pending_prompts: BTreeMap<String, String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            workspaces: Shape::Workspace.records(),
            panes: Shape::Pane.records(),
            agents: Shape::Agent.records(),
            tabs: Shape::Tab.records(),
            next_pane: 100,
            next_workspace: 100,
            sequence: 5000,
            text: BTreeMap::new(),
            pending_text: Vec::new(),
            pending_prompts: BTreeMap::new(),
        }
    }
}

impl State {
    fn records(&self, shape: Shape) -> &[Value] {
        match shape {
            Shape::Workspace => &self.workspaces,
            Shape::Pane => &self.panes,
            Shape::Agent => &self.agents,
            Shape::Tab => &self.tabs,
        }
    }

    fn validate(&self) -> FakeResult<()> {
        for shape in [Shape::Workspace, Shape::Pane, Shape::Agent, Shape::Tab] {
            for record in self.records(shape) {
                shape.validate(record).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn pane_index(&self, id: &str) -> FakeResult<usize> {
        self.panes
            .iter()
            .position(|p| p["pane_id"] == id)
            .ok_or_else(|| format!("pane not found: {id}"))
    }

    fn agent_index(&self, name: &str) -> FakeResult<usize> {
        self.agents
            .iter()
            .position(|a| a["name"] == name || a["pane_id"] == name)
            .ok_or_else(|| format!("agent not found: {name}"))
    }

    fn workspace_index(&self, id: &str) -> FakeResult<usize> {
        self.workspaces
            .iter()
            .position(|w| w["workspace_id"] == id)
            .ok_or_else(|| format!("workspace not found: {id}"))
    }

    fn recount(&mut self) {
        for tab in &mut self.tabs {
            tab["pane_count"] = json!(self
                .panes
                .iter()
                .filter(|p| p["tab_id"] == tab["tab_id"])
                .count());
        }
        for workspace in &mut self.workspaces {
            workspace["pane_count"] = json!(self
                .panes
                .iter()
                .filter(|p| p["workspace_id"] == workspace["workspace_id"])
                .count());
            workspace["tab_count"] = json!(self
                .tabs
                .iter()
                .filter(|t| t["workspace_id"] == workspace["workspace_id"])
                .count());
        }
    }

    /// Next `agent start`/`agent rename` ready-screen: a queued banner if
    /// one is waiting, otherwise the default prompt that `wait_ready` can
    /// treat as readable.
    fn take_ready_text(&mut self) -> String {
        if self.pending_text.is_empty() {
            "Ready >\n".into()
        } else {
            self.pending_text.remove(0)
        }
    }

    fn queue_text(&mut self, args: &[String]) -> FakeResult<Reply> {
        let opts = options(args, &["--file"], &[])?;
        let path = required(&opts, "--file")?;
        let text = fs::read_to_string(path).map_err(|e| format!("read --file {path}: {e}"))?;
        self.pending_text.push(text);
        Ok(Reply::Text(String::new()))
    }

    fn execute(&mut self, args: &[String]) -> FakeResult<Reply> {
        if args == ["--version"] {
            return Ok(Reply::Text("herdr 0.9.3\n".into()));
        }
        // `plugin` is not one of the four shapes below — it is state-free,
        // so it is handled here rather than threaded through `records`/
        // `validate`. Only `config-dir` is implemented: the one subcommand
        // `doctor::run`'s herdr-reachable check actually shells out to
        // (`herdr plugin config-dir <id>`); see fixtures/herdr/README.md.
        if let (Some("plugin"), Some("config-dir"), Some(id)) =
            (arg(args, 0).ok(), arg(args, 1).ok(), arg(args, 2).ok())
        {
            return Ok(Reply::Text(format!("/fake-herdr-plugin-config/{id}\n")));
        }
        let group = arg(args, 0)?;
        let action = arg(args, 1)?;
        let tail = &args[2..];
        if group == "script" {
            return match action {
                "queue-text" => self.queue_text(tail),
                _ => Err(format!("unsupported command: script {action}")),
            };
        }
        let shape = match group {
            "workspace" => Shape::Workspace,
            "pane" => Shape::Pane,
            "agent" => Shape::Agent,
            "tab" => Shape::Tab,
            _ => return Err(format!("unsupported group: {group}")),
        };
        if action == "list" {
            let options = options(tail, &["--workspace"], &[])?;
            if group != "pane" && group != "tab" && !options.is_empty() {
                return Err(format!("{group} list does not accept --workspace"));
            }
            let records = self
                .records(shape)
                .iter()
                .filter(|r| {
                    options
                        .get("--workspace")
                        .is_none_or(|id| r["workspace_id"] == *id)
                })
                .cloned()
                .collect();
            return Ok(Reply::Json(
                shape.envelope(records).map_err(|e| e.to_string())?,
            ));
        }
        match (group, action) {
            ("workspace", "create") => self.create_workspace(tail),
            ("workspace", "close") => {
                exact_len(tail, 1)?;
                let id = arg(tail, 0)?;
                let index = self.workspace_index(id)?;
                self.workspaces.remove(index);
                for pane in self.panes.iter().filter(|p| p["workspace_id"] == id) {
                    if let Some(pane_id) = pane["pane_id"].as_str() {
                        self.text.remove(pane_id);
                    }
                }
                self.agents.retain(|a| a["workspace_id"] != id);
                self.panes.retain(|p| p["workspace_id"] != id);
                self.tabs.retain(|t| t["workspace_id"] != id);
                Ok(Reply::Text(String::new()))
            }
            ("pane", "split") => self.split(tail),
            ("pane", "get") => {
                exact_len(tail, 1)?;
                let pane = &self.panes[self.pane_index(arg(tail, 0)?)?];
                Ok(Reply::Json(envelope("pane", "get", json!({"pane": pane}))))
            }
            ("pane", "rename") => {
                let positionals = positionals_skipping_end_of_options(tail, 2)?;
                let index = self.pane_index(positionals[0])?;
                self.panes[index]["label"] = json!(positionals[1]);
                Ok(Reply::Text(String::new()))
            }
            ("pane", "close") => {
                exact_len(tail, 1)?;
                let id = arg(tail, 0)?;
                let index = self.pane_index(id)?;
                self.panes.remove(index);
                self.agents.retain(|a| a["pane_id"] != id);
                self.text.remove(id);
                self.recount();
                Ok(Reply::Text(String::new()))
            }
            ("pane", "run") => {
                // Intentionally never execute the script or launch a vendor CLI.
                exact_len(tail, 2)?;
                let index = self.pane_index(arg(tail, 0)?)?;
                self.classify_command_route(index, arg(tail, 1)?)?;
                Ok(Reply::Text(String::new()))
            }
            ("agent", "start") => self.start(tail),
            ("agent", "rename") => self.rename_agent(tail),
            ("agent", "get") => {
                exact_len(tail, 1)?;
                let agent = &self.agents[self.agent_index(arg(tail, 0)?)?];
                Ok(Reply::Json(envelope(
                    "agent",
                    "get",
                    json!({"agent": agent}),
                )))
            }
            ("agent", "prompt") => self.prompt(tail),
            ("agent", "send-keys") => self.send_keys(tail),
            ("agent" | "pane", "read") => {
                let target = arg(tail, 0)?;
                let opts = options(&tail[1..], &["--source", "--lines"], &[])?;
                let pane = if group == "agent" {
                    self.agents[self.agent_index(target)?]["pane_id"]
                        .as_str()
                        .ok_or("agent has no pane_id")?
                } else {
                    self.pane_index(target)?;
                    target
                };
                let mut text = self.text.get(pane).cloned().unwrap_or_default();
                if let Some(lines) = opts.get("--lines") {
                    let count: usize = lines.parse().map_err(|_| "--lines must be an integer")?;
                    let all: Vec<_> = text.split_inclusive('\n').collect();
                    text = all[all.len().saturating_sub(count)..].concat();
                }
                Ok(Reply::Text(text))
            }
            _ => Err(format!("unsupported command: {group} {action}")),
        }
    }

    /// Create a workspace with one tab and one root pane, rooted at `--cwd`.
    /// The multi-project primitive `fleet_start::create_workspace` opens
    /// with no current pane required, unlike `pane split --current`.
    fn create_workspace(&mut self, args: &[String]) -> FakeResult<Reply> {
        let opts = options(
            args,
            &["--label", "--cwd", "--env"],
            &["--focus", "--no-focus"],
        )?;
        let label = required(&opts, "--label")?;
        let cwd = required(&opts, "--cwd")?;
        let focus = opts.contains_key("--focus");
        let mut workspace_id;
        loop {
            workspace_id = format!("w{}", self.next_workspace);
            self.next_workspace += 1;
            if !self
                .workspaces
                .iter()
                .any(|w| w["workspace_id"] == workspace_id)
            {
                break;
            }
        }
        let tab_id = format!("{workspace_id}:t1");
        let pane_id = format!("{workspace_id}:p1");
        if focus {
            for pane in &mut self.panes {
                pane["focused"] = json!(false);
            }
            for tab in &mut self.tabs {
                tab["focused"] = json!(false);
            }
            for workspace in &mut self.workspaces {
                workspace["focused"] = json!(false);
            }
        }
        let pane = Shape::Pane
            .record(object(json!({
                "pane_id": pane_id, "workspace_id": workspace_id, "tab_id": tab_id,
                "cwd": cwd, "focused": focus,
                "agent_status": "unknown", "revision": 0, "terminal_id": format!("fake-{pane_id}")
            })))
            .map_err(|e| e.to_string())?;
        let tab = Shape::Tab
            .record(object(json!({
                "tab_id": tab_id, "workspace_id": workspace_id, "label": "1",
                "number": 1, "agent_status": "unknown", "focused": focus
            })))
            .map_err(|e| e.to_string())?;
        let workspace = Shape::Workspace
            .record(object(json!({
                "workspace_id": workspace_id, "label": label, "active_tab_id": tab_id,
                "number": self.workspaces.len() as u64 + 1, "agent_status": "unknown",
                "focused": focus
            })))
            .map_err(|e| e.to_string())?;
        self.panes.push(pane.clone());
        self.tabs.push(tab);
        self.workspaces.push(workspace.clone());
        self.recount();
        Ok(Reply::Json(envelope(
            "workspace",
            "create",
            json!({"workspace": workspace, "root_pane": pane}),
        )))
    }

    fn split(&mut self, args: &[String]) -> FakeResult<Reply> {
        let opts = options(
            args,
            &["--pane", "--direction", "--cwd", "--env"],
            &["--current", "--focus", "--no-focus"],
        )?;
        if opts.contains_key("--pane") && opts.contains_key("--current") {
            return Err("--pane and --current are mutually exclusive".into());
        }
        let direction = required(&opts, "--direction")?;
        if direction != "right" && direction != "down" {
            return Err("--direction must be right or down".into());
        }
        let cwd = required(&opts, "--cwd")?;
        let parent = if let Some(id) = opts.get("--pane") {
            &self.panes[self.pane_index(id)?]
        } else {
            self.panes
                .iter()
                .find(|p| p["focused"] == true)
                .or(self.panes.first())
                .ok_or("no current pane")?
        };
        let workspace = parent["workspace_id"]
            .as_str()
            .ok_or("parent has no workspace_id")?;
        let mut id;
        loop {
            id = format!("{workspace}:p{}", self.next_pane);
            self.next_pane += 1;
            if !self.panes.iter().any(|p| p["pane_id"] == id) {
                break;
            }
        }
        let pane = Shape::Pane
            .record(object(json!({
                "pane_id": id, "workspace_id": workspace, "tab_id": parent["tab_id"],
                "cwd": cwd, "focused": opts.contains_key("--focus"),
                "agent_status": "unknown", "revision": 0, "terminal_id": format!("fake-{id}")
            })))
            .map_err(|e| e.to_string())?;
        if opts.contains_key("--focus") {
            for existing in &mut self.panes {
                existing["focused"] = json!(false);
            }
        }
        self.panes.push(pane.clone());
        self.recount();
        Ok(Reply::Json(envelope(
            "pane",
            "split",
            json!({"pane": pane}),
        )))
    }

    fn start(&mut self, args: &[String]) -> FakeResult<Reply> {
        let (name, flag_args) = split_start_args(args)?;
        let name = name.as_str();
        let opts = options(&flag_args, &["--kind", "--pane", "--timeout"], &[])?;
        let pane_id = required(&opts, "--pane")?;
        let kind = required(&opts, "--kind")?;
        let index = self.pane_index(pane_id)?;
        if self
            .agents
            .iter()
            .any(|a| a["name"] == name || a["pane_id"] == pane_id)
        {
            return Err(format!("agent name or pane already occupied: {name}"));
        }
        self.sequence += 1;
        let ready = self.take_ready_text();
        let pane = &mut self.panes[index];
        pane["agent"] = json!(kind);
        pane["agent_status"] = json!("idle");
        // A real agent's pane always has *something* on screen once it is
        // up; without this, `pane read`/`agent read` of "visible" stays
        // the empty string `start` never otherwise sets, which a real
        // caller's readiness wait (`mv_engine::runner::wait_ready`'s
        // `visible` check) reads as "pane is unreadable" forever — a real
        // dispatch against this fake could never leave `agent start` no
        // matter how long it polled. `prompt` already overwrites this
        // once a turn actually lands. A queued `script queue-text` banner
        // replaces the default Ready prompt for exactly one start/rename.
        self.text.insert(pane_id.to_string(), ready);
        let agent = Shape::Agent
            .record(object(json!({
                "name": name, "agent": kind, "agent_status": "idle", "pane_id": pane_id,
                "workspace_id": pane["workspace_id"], "tab_id": pane["tab_id"],
                "cwd": pane["cwd"], "foreground_cwd": pane["foreground_cwd"],
                "focused": pane["focused"], "terminal_id": pane["terminal_id"],
                "state_change_seq": self.sequence
            })))
            .map_err(|e| e.to_string())?;
        self.agents.push(agent.clone());
        Ok(Reply::Json(envelope(
            "agent",
            "start",
            json!({"agent": agent}),
        )))
    }

    /// Real Herdr auto-classifies a pane once a seat's own command has run
    /// in it — with a kind and status, but no name until `agent rename`
    /// supplies one (card `20261102-b-c72988`). `rename_agent`'s existing
    /// fallback already covers a rename against a pane this fake never
    /// classified; what was missing is the half before it, the `agent
    /// list` poll `mv_engine::runner::wait_detected` makes between `pane
    /// run` and that rename, which previously never saw this pane and
    /// timed out before the rename the fallback would have answered was
    /// ever attempted.
    ///
    /// The two scripts `pane run` is ever asked to execute without a seat
    /// command are a bare `cd <dir>` and `cd <dir> && unset ...` (clearing
    /// inherited auth before `agent start`, `mv_engine::runner::dispatch`'s
    /// non-command route); the command route splices a seat's own wrapper
    /// in after that same `&& ` instead (`quote_command_words` + native
    /// args), landing here with `agent_status: "unknown"` since
    /// `wait_detected` (the only gate this route waits on, card
    /// `20261102-b-c72988`) only needs the pane to appear at all.
    /// `mv_engine::fleet_start::launch_and_seed` (card `20261103-b-7febbe`)
    /// runs a third shape neither of those: a bare launch command with no
    /// `cd` prefix at all and no `&& `, because the Nexus/orchestrator
    /// seat's pane is already rooted in the project directory at split
    /// time. Every other `pane run` script in this workspace either chains
    /// onto a leading `cd <dir>` or *is* that bare `cd <dir>`, so "does not
    /// start with `cd `" distinguishes this third shape from the
    /// non-command route's bare cd without needing to know the wrapper's
    /// name. Unlike the command route, `launch_and_seed` waits on
    /// `wait_pane_ready`'s `is_ready_for_input` before it will rename —
    /// there is no later tick that would ever move an `"unknown"` status
    /// forward in this fake, so this third shape has to land already
    /// ready, the same immediate-ready contract `agent_start` gives a
    /// freshly launched agent.
    fn classify_command_route(&mut self, pane_index: usize, script: &str) -> FakeResult<()> {
        let agent_status = match script.split_once("&& ") {
            Some((_, rest)) if !rest.starts_with("unset ") => "unknown",
            Some(_) => return Ok(()), // `cd <dir> && unset ...`: not a wrapper
            None if !script.starts_with("cd ") => "idle",
            None => return Ok(()), // bare `cd <dir>`: not a wrapper
        };
        let pane = self.panes[pane_index].clone();
        let pane_id = pane["pane_id"]
            .as_str()
            .ok_or("pane has no pane_id")?
            .to_owned();
        if self.agents.iter().any(|a| a["pane_id"] == pane_id) {
            return Ok(());
        }
        self.sequence += 1;
        let agent = pre_rename_agent(object(json!({
            "pane_id": pane_id, "agent_status": agent_status,
            "workspace_id": pane["workspace_id"], "tab_id": pane["tab_id"],
            "cwd": pane["cwd"], "foreground_cwd": pane["foreground_cwd"],
            "focused": pane["focused"], "terminal_id": pane["terminal_id"],
            "state_change_seq": self.sequence
        })))
        .map_err(|e| e.to_string())?;
        self.agents.push(agent);
        Ok(())
    }

    fn rename_agent(&mut self, args: &[String]) -> FakeResult<Reply> {
        let positionals = positionals_skipping_end_of_options(args, 2)?;
        let target = positionals[0];
        let name = positionals[1];
        if self.agents.iter().any(|a| a["name"] == name) {
            return Err(format!("agent already exists: {name}"));
        }
        if let Ok(index) = self.agent_index(target) {
            // The only pre-existing, still-unnamed entry a rename target can
            // match is `classify_command_route`'s own pre-rename stub
            // (nothing else in this fake ever adds an agent without a
            // name); bring it up to the same ready baseline `start` gives a
            // freshly-launched agent, rather than leaving it parked at
            // `unknown` forever once it has a name.
            self.agents[index]["name"] = json!(name);
            self.agents[index]["agent_status"] = json!("idle");
            let pane_id = self.agents[index]["pane_id"]
                .as_str()
                .ok_or("agent has no pane_id")?
                .to_owned();
            if let Ok(pane_index) = self.pane_index(&pane_id) {
                self.panes[pane_index]["agent_status"] = json!("idle");
            }
            let ready = self.take_ready_text();
            self.text.insert(pane_id, ready);
            return Ok(Reply::Text(String::new()));
        }
        // Command-routed accounts run a wrapper in a pane, then rename it.
        // Flags before `--`, name after: safe regardless of whether `name`
        // itself looks like `--pane`/`--kind`, matching `split_start_args`.
        self.start(&[
            "--pane".into(),
            target.into(),
            "--kind".into(),
            "claude".into(),
            "--".into(),
            name.into(),
        ])?;
        Ok(Reply::Text(String::new()))
    }

    fn prompt(&mut self, args: &[String]) -> FakeResult<Reply> {
        let (positionals, flag_args) = split_prompt_args(args)?;
        let index = self.agent_index(&positionals[0])?;
        let prompt = positionals[1].as_str();
        options(&flag_args, &["--timeout"], &["--wait"])?;
        if prompt.is_empty() {
            return Ok(Reply::Json(json!({"error": {
                "code": "empty_agent_prompt", "message": "agent prompt must not be empty"
            }})));
        }
        self.complete_prompt(index, prompt, "prompt")
    }

    fn send_keys(&mut self, args: &[String]) -> FakeResult<Reply> {
        let args = args.strip_prefix(&["--".into()]).unwrap_or(args);
        let index = self.agent_index(arg(args, 0)?)?;
        if args.len() < 2 || args[1..].iter().any(|key| key != "Enter") {
            return Err("fake send-keys requires Enter keys".into());
        }
        let pane = self.agents[index]["pane_id"]
            .as_str()
            .ok_or("agent has no pane_id")?;
        if let Some(prompt) = self.pending_prompts.remove(pane) {
            self.complete_prompt(index, &prompt, "send-keys")
        } else {
            Ok(Reply::Text(String::new()))
        }
    }

    fn complete_prompt(&mut self, index: usize, prompt: &str, action: &str) -> FakeResult<Reply> {
        // Match FakeHerdr's completion shortcut: only an existing brief named
        // in the prompt authorizes creation of its sibling result and status.
        if let Some(brief) = brief_path(prompt) {
            let parent = brief.parent().ok_or("brief has no parent")?;
            fs::write(
                parent.join("result.md"),
                "# Fake Herdr result\n\nTask completed.\n",
            )
            .map_err(|e| e.to_string())?;
            fs::write(parent.join("status"), "done\n").map_err(|e| e.to_string())?;
        }
        self.sequence += 1;
        let agent = &mut self.agents[index];
        agent["agent_status"] = json!("done");
        agent["state_change_seq"] = json!(self.sequence);
        agent["completion_seq"] = json!(self.sequence);
        let pane_id = agent["pane_id"]
            .as_str()
            .ok_or("agent has no pane_id")?
            .to_owned();
        self.text
            .insert(pane_id.clone(), "Task completed.\n".into());
        let pane_index = self.pane_index(&pane_id)?;
        self.panes[pane_index]["agent_status"] = json!("done");
        Ok(Reply::Json(envelope(
            "agent",
            action,
            json!({"agent": self.agents[index]}),
        )))
    }
}

enum Reply {
    Json(Value),
    Text(String),
}

fn object(value: Value) -> Map<String, Value> {
    value.as_object().expect("literal object").clone()
}

fn envelope(group: &str, action: &str, fields: Value) -> Value {
    let mut result = object(fields);
    result.insert("type".into(), json!(format!("{group}_{action}")));
    json!({"id": format!("cli:{group}:{action}"), "result": result})
}

fn arg(args: &[String], index: usize) -> FakeResult<&str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| "missing argument".into())
}

fn exact_len(args: &[String], expected: usize) -> FakeResult<()> {
    if args.len() == expected {
        Ok(())
    } else {
        Err(format!("expected {expected} arguments"))
    }
}

fn required<'a>(opts: &'a BTreeMap<String, String>, key: &str) -> FakeResult<&'a str> {
    opts.get(key)
        .map(String::as_str)
        .ok_or_else(|| format!("missing {key}"))
}

fn positionals_skipping_end_of_options(args: &[String], expected: usize) -> FakeResult<Vec<&str>> {
    let out: Vec<&str> = args
        .iter()
        .filter(|part| part.as_str() != "--")
        .map(String::as_str)
        .collect();
    if out.len() == expected {
        Ok(out)
    } else {
        Err(format!("expected {expected} arguments"))
    }
}

/// Split `agent start` argv into the agent name and `--kind`/`--pane`/
/// `--timeout` flag tokens. The name may sit before the flags (the
/// ordinary shape) or, preceded by `--`, after them (card
/// `20261103-b-0511f0`'s protection for a dash-leading name, mirroring
/// `split_prompt_args`). Whatever follows the name once found is the
/// `-- NATIVE_ARGS` this fake never interprets.
fn split_start_args(args: &[String]) -> FakeResult<(String, Vec<String>)> {
    let mut name = None;
    let mut flag_args = Vec::new();
    let mut index = 0;
    let mut end_of_options = false;
    while index < args.len() {
        let key = args[index].as_str();
        if !end_of_options && key == "--" {
            end_of_options = true;
            index += 1;
            continue;
        }
        if !end_of_options && matches!(key, "--kind" | "--pane" | "--timeout") {
            flag_args.push(args[index].clone());
            index += 1;
            flag_args.push(arg(args, index)?.to_owned());
            index += 1;
            continue;
        }
        if name.is_none() {
            name = Some(args[index].clone());
            index += 1;
            continue;
        }
        break;
    }
    name.ok_or_else(|| "missing agent name".to_string())
        .map(|name| (name, flag_args))
}

/// Split `agent prompt` argv into `[name, text]` and flag tokens. Flags may
/// sit before or after the positionals; `--` ends option parsing so a
/// dash-leading brief is a positional rather than `--wait`/`--help`.
fn split_prompt_args(args: &[String]) -> FakeResult<([String; 2], Vec<String>)> {
    let mut positionals = Vec::new();
    let mut flag_args = Vec::new();
    let mut index = 0;
    let mut end_of_options = false;
    while index < args.len() {
        let key = args[index].as_str();
        if !end_of_options && key == "--" {
            end_of_options = true;
            index += 1;
            continue;
        }
        if !end_of_options
            && (key == "--wait" || key == "--timeout" || key.starts_with("--timeout="))
        {
            flag_args.push(args[index].clone());
            if key == "--timeout" {
                index += 1;
                flag_args.push(arg(args, index)?.to_owned());
            }
            index += 1;
            continue;
        }
        positionals.push(args[index].clone());
        index += 1;
    }
    if positionals.len() != 2 {
        return Err("expected 2 arguments".into());
    }
    Ok(([positionals[0].clone(), positionals[1].clone()], flag_args))
}

fn options(
    args: &[String],
    valued: &[&str],
    flags: &[&str],
) -> FakeResult<BTreeMap<String, String>> {
    let mut opts = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let key = args[index].as_str();
        if let Some((flag, inline)) = key.split_once('=') {
            if valued.contains(&flag) {
                if opts.insert(flag.into(), inline.to_owned()).is_some() && flag != "--env" {
                    return Err(format!("duplicate option: {flag}"));
                }
                index += 1;
                continue;
            }
            return Err(format!("unexpected argument: {key}"));
        }
        let value = if valued.contains(&key) {
            index += 1;
            arg(args, index)?.to_owned()
        } else if flags.contains(&key) {
            String::new()
        } else {
            return Err(format!("unexpected argument: {key}"));
        };
        if opts.insert(key.into(), value).is_some() && key != "--env" {
            return Err(format!("duplicate option: {key}"));
        }
        index += 1;
    }
    if opts.contains_key("--focus") && opts.contains_key("--no-focus") {
        return Err("--focus and --no-focus are mutually exclusive".into());
    }
    Ok(opts)
}

fn brief_path(prompt: &str) -> Option<PathBuf> {
    // Backticks are the dispatch prompt's normal delimiter, and permit spaces.
    prompt
        .split('`')
        .chain(prompt.split_whitespace())
        .find_map(|token| {
            let path = Path::new(token.trim_end_matches(['.', ',']).trim_matches(['"', '\'']));
            (path.file_name().is_some_and(|name| name == "brief.md") && path.is_file())
                .then(|| path.to_owned())
        })
}

fn run(args: &[String], state_path: Option<&Path>) -> FakeResult<Reply> {
    let readonly = args == ["--version"]
        || args
            .get(1)
            .is_some_and(|action| matches!(action.as_str(), "list" | "get" | "read"))
        || args.first().is_some_and(|group| group == "plugin");
    if !readonly && state_path.is_none() {
        return Err("set MV_FAKE_HERDR_STATE to a disposable file before mutating the fake".into());
    }
    let mut state = match state_path.map(fs::read).transpose() {
        Ok(Some(bytes)) => {
            serde_json::from_slice(&bytes).map_err(|e| format!("invalid fake state: {e}"))?
        }
        Ok(None) => State::default(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.to_string()),
    };
    state.validate()?;
    let reply = state.execute(args)?;
    if !readonly {
        let path = state_path.expect("mutations require state path");
        let bytes = serde_json::to_vec(&state).map_err(|e| e.to_string())?;
        mv_core::atomic::write_atomic(path, &bytes).map_err(|e| e.to_string())?;
    }
    Ok(reply)
}

/// Serialize a fake daemon transaction and its argv log together. Councils
/// issue concurrent subprocesses; none may overwrite a sibling's state.
fn run_logged(
    args: &[String],
    state_path: Option<&Path>,
    calls_path: Option<&Path>,
) -> FakeResult<Reply> {
    // Logging opts into serialized daemon transactions. Ordinary unlogged
    // reads retain the fake's existing no-filesystem-write contract.
    let _guard = state_path
        .filter(|_| calls_path.is_some())
        .map(|path| mv_core::lock::acquire_default(&path.with_extension("lock"), "fake-herdr"))
        .transpose()
        .map_err(|e| e.to_string())?;
    if let Some(path) = calls_path {
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| e.to_string())?;
        let line = serde_json::to_string(args).map_err(|e| e.to_string())?;
        writeln!(log, "{line}").map_err(|e| e.to_string())?;
    }
    run(args, state_path)
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    let path = env::var_os("MV_FAKE_HERDR_STATE").map(PathBuf::from);
    let calls_path = env::var_os("MV_FAKE_HERDR_CALLS").map(PathBuf::from);
    match run_logged(&args, path.as_deref(), calls_path.as_deref()) {
        Ok(Reply::Json(value)) if value.get("error").is_some() => {
            eprintln!("{value}");
            ExitCode::from(1)
        }
        Ok(Reply::Json(value)) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Ok(Reply::Text(text)) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!(
                "{}",
                json!({"id": "cli:fake-herdr", "error": {"code": "fake_herdr_error", "message": message}})
            );
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Sandbox(PathBuf);

    impl Sandbox {
        fn new() -> Self {
            let path = env::temp_dir().join(format!(
                "mv-fake-herdr-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn call(&self, args: &[&str]) -> FakeResult<Reply> {
            run(
                &args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>(),
                Some(&self.0.join("state.json")),
            )
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    fn payload(reply: Reply) -> Value {
        match reply {
            Reply::Json(value) => value,
            Reply::Text(text) => panic!("expected JSON, got {text:?}"),
        }
    }

    fn split(sandbox: &Sandbox) -> String {
        payload(
            sandbox
                .call(&[
                    "pane",
                    "split",
                    "--pane",
                    "w52:p1",
                    "--direction",
                    "right",
                    "--cwd",
                    "/project",
                    "--no-focus",
                ])
                .unwrap(),
        )["result"]["pane"]["pane_id"]
            .as_str()
            .unwrap()
            .into()
    }

    #[test]
    fn capture_queries_are_read_only_and_match_all_four_captures() {
        let sandbox = Sandbox::new();
        for shape in [Shape::Workspace, Shape::Pane, Shape::Agent, Shape::Tab] {
            assert_eq!(
                payload(sandbox.call(&[shape.singular(), "list"]).unwrap()),
                shape.capture()
            );
        }
        assert!(!sandbox.0.join("state.json").exists());
    }

    #[test]
    fn empty_prompt_is_refused_and_enter_submits_the_pending_composer() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        sandbox
            .call(&[
                "agent",
                "start",
                "--kind",
                "claude",
                "--pane",
                &pane,
                "--",
                "--builder",
            ])
            .unwrap();
        let task_dir = sandbox.0.join("pending-task");
        fs::create_dir(&task_dir).unwrap();
        let brief = task_dir.join("brief.md");
        fs::write(&brief, "Task").unwrap();
        let prompt = format!("Read `{}` and execute it.", brief.display());
        let state_path = sandbox.0.join("state.json");
        let mut state: State = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        state.pending_prompts.insert(pane.clone(), prompt.clone());
        state.text.insert(pane.clone(), format!("❯ {prompt}"));
        fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        let before = fs::read(&state_path).unwrap();
        let error = payload(
            sandbox
                .call(&["agent", "prompt", "--", "--builder", ""])
                .unwrap(),
        );
        assert_eq!(error["error"]["code"], "empty_agent_prompt");
        assert_eq!(error["error"]["message"], "agent prompt must not be empty");
        assert_eq!(fs::read(&state_path).unwrap(), before);
        assert!(!task_dir.join("status").exists());
        sandbox
            .call(&["agent", "send-keys", "--", "--builder", "Enter"])
            .unwrap();
        let state: State = serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
        assert!(!state.pending_prompts.contains_key(&pane));
        assert_eq!(state.text[&pane], "Task completed.\n");
        let agent = &state.agents[state.agent_index("--builder").unwrap()];
        assert_eq!(agent["agent_status"], "done");
        assert_eq!(agent["completion_seq"], agent["state_change_seq"]);
        assert_eq!(
            fs::read_to_string(task_dir.join("status")).unwrap(),
            "done\n"
        );
    }

    #[test]
    fn split_start_prompt_get_read_close_survive_reloading_state() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        let started = payload(
            sandbox
                .call(&[
                    "agent",
                    "start",
                    "test-agent",
                    "--kind",
                    "codex",
                    "--pane",
                    &pane,
                    "--",
                    "--model",
                    "test-model",
                ])
                .unwrap(),
        );
        assert_eq!(started["result"]["agent"]["agent_status"], "idle");
        assert_eq!(started["result"]["agent"]["foreground_cwd"], "/project");
        let baseline = started["result"]["agent"]["state_change_seq"]
            .as_u64()
            .unwrap();
        let task_dir = sandbox.0.join("task with spaces");
        fs::create_dir(&task_dir).unwrap();
        let brief = task_dir.join("brief.md");
        fs::write(&brief, "Task").unwrap();
        let prompt = format!("Read `{}` and execute it.", brief.display());
        sandbox
            .call(&["agent", "prompt", "test-agent", &prompt])
            .unwrap();
        let agent = payload(sandbox.call(&["agent", "get", "test-agent"]).unwrap())["result"]
            ["agent"]
            .clone();
        assert_eq!(agent["agent_status"], "done");
        assert!(agent["state_change_seq"].as_u64().unwrap() > baseline);
        assert_eq!(agent["completion_seq"], agent["state_change_seq"]);
        assert_eq!(
            fs::read_to_string(task_dir.join("status")).unwrap(),
            "done\n"
        );
        assert!(!fs::read_to_string(task_dir.join("result.md"))
            .unwrap()
            .trim()
            .is_empty());
        match sandbox
            .call(&[
                "agent",
                "read",
                "test-agent",
                "--source",
                "visible",
                "--lines",
                "1",
            ])
            .unwrap()
        {
            Reply::Text(text) => {
                assert_eq!(text, "Task completed.\n");
                assert!(serde_json::from_str::<Value>(&text).is_err());
            }
            Reply::Json(_) => panic!("agent read must be plain text"),
        }
        sandbox.call(&["pane", "close", &pane]).unwrap();
        assert!(sandbox.call(&["agent", "get", "test-agent"]).is_err());
        assert!(sandbox.call(&["pane", "get", &pane]).is_err());
        assert_eq!(
            payload(sandbox.call(&["pane", "list"]).unwrap()),
            Shape::Pane.capture()
        );
    }

    #[test]
    fn errors_do_not_mutate_persisted_state() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        let before = fs::read(sandbox.0.join("state.json")).unwrap();
        for args in [
            vec![
                "pane",
                "split",
                "--direction",
                "diagonal",
                "--cwd",
                "/project",
            ],
            vec![
                "agent", "start", "test", "--kind", "codex", "--pane", "missing",
            ],
            vec!["pane", "close", &pane, "extra"],
            vec!["pane", "list", "--invented"],
        ] {
            assert!(sandbox.call(&args).is_err());
            assert_eq!(fs::read(sandbox.0.join("state.json")).unwrap(), before);
        }
    }

    #[test]
    fn split_ids_are_unique_and_workspace_filters_are_applied() {
        let sandbox = Sandbox::new();
        let first = split(&sandbox);
        let second = split(&sandbox);
        assert_ne!(first, second);
        let list = payload(
            sandbox
                .call(&["pane", "list", "--workspace", "w52"])
                .unwrap(),
        );
        let panes = list["result"]["panes"].as_array().unwrap();
        assert_eq!(panes.len(), 4);
        assert!(panes.iter().all(|p| p["workspace_id"] == "w52"));
        let workspaces = payload(sandbox.call(&["workspace", "list"]).unwrap());
        assert_eq!(workspaces["result"]["workspaces"][1]["pane_count"], 4);
    }

    #[test]
    fn unlogged_read_does_not_create_state_or_lock_files() {
        let sandbox = Sandbox::new();
        let state = sandbox.0.join("state.json");
        run_logged(&["agent".into(), "list".into()], Some(&state), None).unwrap();
        assert_eq!(fs::read_dir(&sandbox.0).unwrap().count(), 0);
    }

    #[test]
    fn call_log_includes_successful_reads_and_failed_mutations_in_order() {
        let sandbox = Sandbox::new();
        let state = sandbox.0.join("state.json");
        let calls = sandbox.0.join("calls.jsonl");
        let read = vec!["agent".to_owned(), "list".to_owned()];
        let close = vec!["pane".to_owned(), "close".to_owned(), "missing".to_owned()];
        run_logged(&read, Some(&state), Some(&calls)).unwrap();
        assert!(run_logged(&close, Some(&state), Some(&calls)).is_err());
        let logged: Vec<Vec<String>> = fs::read_to_string(&calls)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(logged, vec![read, close]);
    }

    #[test]
    fn workspace_create_returns_a_sibling_root_pane_and_registers_a_tab() {
        let sandbox = Sandbox::new();
        let created = payload(
            sandbox
                .call(&[
                    "workspace",
                    "create",
                    "--label",
                    "demo",
                    "--cwd",
                    "/project",
                    "--no-focus",
                ])
                .unwrap(),
        );
        let workspace_id = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(created["result"]["workspace"]["label"], "demo");
        assert_eq!(created["result"]["root_pane"]["cwd"], "/project");
        assert_eq!(created["result"]["root_pane"]["workspace_id"], workspace_id);

        let workspaces = payload(sandbox.call(&["workspace", "list"]).unwrap());
        let listed = workspaces["result"]["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|w| w["workspace_id"] == workspace_id)
            .expect("new workspace must be listed");
        assert_eq!(listed["pane_count"], 1);
        assert_eq!(listed["tab_count"], 1);

        let tabs = payload(
            sandbox
                .call(&["tab", "list", "--workspace", &workspace_id])
                .unwrap(),
        );
        let tabs = tabs["result"]["tabs"].as_array().unwrap();
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0]["tab_id"], listed["active_tab_id"]);

        let panes = payload(
            sandbox
                .call(&["pane", "list", "--workspace", &workspace_id])
                .unwrap(),
        );
        let panes = panes["result"]["panes"].as_array().unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["pane_id"], pane_id);
    }

    #[test]
    fn workspace_create_without_label_or_cwd_is_an_error() {
        let sandbox = Sandbox::new();
        assert!(sandbox
            .call(&["workspace", "create", "--cwd", "/project", "--no-focus"])
            .is_err());
        assert!(sandbox
            .call(&["workspace", "create", "--label", "demo", "--no-focus"])
            .is_err());
    }

    #[test]
    fn workspace_close_removes_its_panes_tabs_and_agents() {
        let sandbox = Sandbox::new();
        let created = payload(
            sandbox
                .call(&[
                    "workspace",
                    "create",
                    "--label",
                    "demo",
                    "--cwd",
                    "/project",
                    "--no-focus",
                ])
                .unwrap(),
        );
        let workspace_id = created["result"]["workspace"]["workspace_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_owned();
        sandbox
            .call(&[
                "agent",
                "start",
                "demo-nexus",
                "--kind",
                "claude",
                "--pane",
                &pane_id,
            ])
            .unwrap();

        sandbox
            .call(&["workspace", "close", &workspace_id])
            .unwrap();

        let workspaces = payload(sandbox.call(&["workspace", "list"]).unwrap());
        assert!(workspaces["result"]["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .all(|w| w["workspace_id"] != workspace_id));
        assert!(sandbox.call(&["pane", "get", &pane_id]).is_err());
        assert!(sandbox.call(&["agent", "get", "demo-nexus"]).is_err());
        let tabs = payload(sandbox.call(&["tab", "list"]).unwrap());
        assert!(tabs["result"]["tabs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["workspace_id"] != workspace_id));
    }

    #[test]
    fn workspace_close_of_an_unknown_id_is_an_error() {
        let sandbox = Sandbox::new();
        assert!(sandbox.call(&["workspace", "close", "w999"]).is_err());
    }

    #[test]
    fn command_route_rename_creates_an_agent_without_running_script() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        sandbox.call(&["pane", "run", &pane, "exit 99"]).unwrap();
        sandbox
            .call(&["agent", "rename", &pane, "command-agent"])
            .unwrap();
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "command-agent"]).unwrap())["result"]["agent"]
                ["pane_id"],
            pane
        );
        assert!(sandbox
            .call(&[
                "agent",
                "start",
                "command-agent",
                "--kind",
                "codex",
                "--pane",
                &pane
            ])
            .is_err());
    }

    #[test]
    fn pane_run_classifies_a_seat_command_wrapper_but_not_a_bare_cd_or_auth_clear() {
        // Card 20261102-b-c72988: a bare `cd <dir>` (the non-command
        // route's own pane-run script when there is nothing to clear) and
        // `cd <dir> && unset ...` (the same route clearing inherited auth)
        // must not synthesize an agent -- only a seat's own wrapper,
        // spliced in after that same `&& `, does.
        let sandbox = Sandbox::new();
        let before = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .len();
        for script in ["cd '/project'", "cd '/project' && unset FOO BAR"] {
            let pane = split(&sandbox);
            sandbox.call(&["pane", "run", &pane, script]).unwrap();
            let agents = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
                .as_array()
                .unwrap()
                .len();
            assert_eq!(agents, before, "{script:?} must not classify a pane");
        }
    }

    #[test]
    fn pane_run_auto_classifies_a_command_route_wrapper_so_wait_detected_can_see_it() {
        let sandbox = Sandbox::new();
        let before = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .len();
        let pane = split(&sandbox);
        sandbox
            .call(&[
                "pane",
                "run",
                &pane,
                "cd '/project' && CACHE='1' 'claude-interactive' '--model' 'test-model'",
            ])
            .unwrap();
        let agents = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(agents.len(), before + 1);
        let classified = agents
            .iter()
            .find(|a| a["pane_id"] == pane)
            .expect("pane_run must classify the pane");
        assert_eq!(classified["agent_status"], "unknown");
        assert!(classified.get("name").is_none());

        // `mv_engine::runner::dispatch`'s command route renames this
        // already-classified entry; it must not hit the "agent already
        // occupied"/"already exists" refusal `agent start` or a second
        // classification would.
        sandbox
            .call(&["agent", "rename", &pane, "command-agent"])
            .unwrap();
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "command-agent"]).unwrap())["result"]["agent"]
                ["pane_id"],
            pane
        );
        let agents = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(agents, before + 1, "rename must not add a second agent");

        // Re-running `pane run` against the same already-classified pane
        // (a retried split-and-run, say) must stay idempotent.
        sandbox
            .call(&[
                "pane",
                "run",
                &pane,
                "cd '/project' && CACHE='1' 'claude-interactive' '--model' 'test-model'",
            ])
            .unwrap();
        let agents = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(agents, before + 1);
    }

    #[test]
    fn pane_run_classifies_a_bare_launch_command_with_no_cd_prefix() {
        // Card 20261103-b-7febbe: `mv_engine::fleet_start::launch_and_seed`
        // runs the seat's launch command directly, with no `cd` prefix and
        // no `&& ` at all (the pane is already rooted at the project
        // directory from `pane split`). This must classify just like the
        // command route's `cd <dir> && <wrapper>` shape does, or the
        // Nexus/orchestrator seat it seeds is reported unseeded. Unlike
        // that route, `launch_and_seed` waits for `is_ready_for_input`
        // before it renames (`wait_pane_ready`, not `wait_detected`), so
        // this shape must land ready rather than "unknown" -- there is no
        // later tick in this fake that would ever move "unknown" forward.
        let sandbox = Sandbox::new();
        let before = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .len();
        let pane = split(&sandbox);
        sandbox
            .call(&[
                "pane",
                "run",
                &pane,
                "claude --disallowedTools Edit(/etc/passwd)",
            ])
            .unwrap();
        let agents = payload(sandbox.call(&["agent", "list"]).unwrap())["result"]["agents"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(agents.len(), before + 1);
        let classified = agents
            .iter()
            .find(|a| a["pane_id"] == pane)
            .expect("pane_run must classify the pane");
        assert_eq!(classified["agent_status"], "idle");
        assert!(classified.get("name").is_none());
    }

    #[test]
    fn corrupt_or_invented_state_fails_closed_and_mutation_needs_a_path() {
        let sandbox = Sandbox::new();
        fs::write(sandbox.0.join("state.json"), "broken").unwrap();
        assert!(sandbox.call(&["agent", "list"]).is_err());
        let mut state = State::default();
        state.workspaces[0]["cwd"] = json!("/invented");
        fs::write(
            sandbox.0.join("state.json"),
            serde_json::to_vec(&state).unwrap(),
        )
        .unwrap();
        assert!(sandbox.call(&["workspace", "list"]).is_err());
        assert!(run(&["pane".into(), "close".into(), "w52:p1".into()], None).is_err());
    }

    #[test]
    fn plugin_config_dir_is_readonly_and_needs_no_state_path() {
        // `doctor::run`'s herdr-reachable check shells out to exactly this
        // (card 20261102-b-a61e83); it must succeed with no
        // `MV_FAKE_HERDR_STATE`, the same as `--version`.
        let reply = run(
            &["plugin".into(), "config-dir".into(), "multiverse".into()],
            None,
        )
        .unwrap();
        match reply {
            Reply::Text(text) => assert!(text.contains("multiverse")),
            Reply::Json(value) => panic!("expected text, got {value:?}"),
        }
    }

    #[test]
    fn plugin_without_config_dir_is_still_an_unsupported_group() {
        assert!(run(&["plugin".into(), "list".into()], None).is_err());
    }

    #[test]
    fn queued_pane_text_is_shown_on_the_next_start_and_consumed_once() {
        let sandbox = Sandbox::new();
        let banner = sandbox.0.join("banner.txt");
        fs::write(&banner, "Usage limit reached\n").unwrap();
        sandbox
            .call(&["script", "queue-text", "--file", banner.to_str().unwrap()])
            .unwrap();
        let pane = split(&sandbox);
        sandbox
            .call(&[
                "agent",
                "start",
                "test-agent",
                "--kind",
                "codex",
                "--pane",
                &pane,
            ])
            .unwrap();
        match sandbox.call(&["pane", "read", &pane]).unwrap() {
            Reply::Text(text) => assert_eq!(text, "Usage limit reached\n"),
            Reply::Json(value) => panic!("pane read must be plain text, got {value:?}"),
        }

        let pane2 = split(&sandbox);
        sandbox
            .call(&[
                "agent",
                "start",
                "second-agent",
                "--kind",
                "codex",
                "--pane",
                &pane2,
            ])
            .unwrap();
        match sandbox.call(&["pane", "read", &pane2]).unwrap() {
            Reply::Text(text) => assert_eq!(text, "Ready >\n"),
            Reply::Json(value) => panic!("pane read must be plain text, got {value:?}"),
        }
    }

    #[test]
    fn queued_pane_text_is_shown_on_command_route_rename() {
        let sandbox = Sandbox::new();
        let banner = sandbox.0.join("banner.txt");
        fs::write(&banner, "You hit your weekly limit.\n").unwrap();
        sandbox
            .call(&["script", "queue-text", "--file", banner.to_str().unwrap()])
            .unwrap();
        let pane = split(&sandbox);
        sandbox
            .call(&[
                "pane",
                "run",
                &pane,
                "cd '/project' && CACHE='1' 'claude-interactive' '--model' 'test-model'",
            ])
            .unwrap();
        sandbox
            .call(&["agent", "rename", &pane, "command-agent"])
            .unwrap();
        match sandbox.call(&["pane", "read", &pane]).unwrap() {
            Reply::Text(text) => assert_eq!(text, "You hit your weekly limit.\n"),
            Reply::Json(value) => panic!("pane read must be plain text, got {value:?}"),
        }
    }

    #[test]
    fn script_queue_text_is_a_mutation_and_unknown_script_actions_fail() {
        assert!(run(
            &[
                "script".into(),
                "queue-text".into(),
                "--file".into(),
                "/dev/null".into()
            ],
            None
        )
        .is_err());
        let sandbox = Sandbox::new();
        assert!(sandbox.call(&["script", "invented"]).is_err());
    }

    #[test]
    fn workspace_create_accepts_a_dash_leading_label_via_equals_form() {
        let sandbox = Sandbox::new();
        let created = payload(
            sandbox
                .call(&[
                    "workspace",
                    "create",
                    "--label=--help",
                    "--cwd",
                    "/project",
                    "--no-focus",
                ])
                .unwrap(),
        );
        assert_eq!(created["result"]["workspace"]["label"], "--help");
    }

    #[test]
    fn pane_rename_and_agent_prompt_accept_end_of_options_before_dash_values() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        sandbox
            .call(&["pane", "rename", "--", &pane, "--focus"])
            .unwrap();
        assert_eq!(
            payload(sandbox.call(&["pane", "get", &pane]).unwrap())["result"]["pane"]["label"],
            "--focus"
        );
        sandbox
            .call(&[
                "agent",
                "start",
                "test-agent",
                "--kind",
                "codex",
                "--pane",
                &pane,
            ])
            .unwrap();
        sandbox
            .call(&["agent", "prompt", "--wait", "--", "test-agent", "--help"])
            .unwrap();
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "test-agent"]).unwrap())["result"]["agent"]
                ["agent_status"],
            "done"
        );
    }

    /// Card `20261103-b-0511f0`: `argv::agent_start` moves NAME after
    /// `--kind`/`--pane`/`--` once it looks like a flag. This fake must
    /// read that shape identically to the ordinary one, including the
    /// `-- NAME NATIVE_ARGS` run forwarding native args this fake never
    /// interprets.
    #[test]
    fn agent_start_accepts_a_dash_leading_name_after_end_of_options() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        let started = payload(
            sandbox
                .call(&[
                    "agent",
                    "start",
                    "--kind",
                    "codex",
                    "--pane",
                    &pane,
                    "--",
                    "--help",
                    "--model",
                    "test-model",
                ])
                .unwrap(),
        );
        assert_eq!(started["result"]["agent"]["name"], "--help");
        assert_eq!(started["result"]["agent"]["agent_status"], "idle");
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "--help"]).unwrap())["result"]["agent"]
                ["pane_id"],
            pane
        );
    }

    /// Card `20261103-b-0511f0`: `argv::agent_rename` protects both
    /// positionals with `push_positionals`, so either a dash-leading
    /// target or a dash-leading name can arrive after a single `--`.
    #[test]
    fn agent_rename_accepts_dash_leading_target_or_name_after_end_of_options() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        sandbox.call(&["pane", "run", &pane, "exit 99"]).unwrap();
        sandbox
            .call(&["agent", "rename", "--", &pane, "--kind"])
            .unwrap();
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "--kind"]).unwrap())["result"]["agent"]
                ["pane_id"],
            pane
        );
    }

    /// The command-routed fallback inside `rename_agent` (no pre-existing
    /// agent at `target`) must survive a dash-leading name too, since it
    /// builds its own internal `agent start` argv.
    #[test]
    fn agent_rename_command_route_fallback_accepts_a_dash_leading_name() {
        let sandbox = Sandbox::new();
        let pane = split(&sandbox);
        sandbox.call(&["agent", "rename", &pane, "--kind"]).unwrap();
        assert_eq!(
            payload(sandbox.call(&["agent", "get", "--kind"]).unwrap())["result"]["agent"]
                ["pane_id"],
            pane
        );
    }
}

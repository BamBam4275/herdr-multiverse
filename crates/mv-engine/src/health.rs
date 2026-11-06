//! Account/seat health checks feeding `doctor` and `check`.
//!
//! Pane-text classification and read-only live scanning through the shared
//! Herdr subprocess layer (LIMIT/RATE/CONTEXT/DEAD/DIALOG).
//!
//! The evaluation *order* below is ported from `spec-seats-quotas.md`'s
//! Health section (LIMIT/RATE/CONTEXT, then DEAD against the last event,
//! then DIALOG; progress lines suppress DEAD). LIMIT and RATE delegate to the ported, fixture-anchored
//! `crate::limits::classify`/`crate::blocking_prompt::classify_prompt` —
//! exactly Python's `classify_visible_text`. Dialog/menu detection reuses
//! `runner::attention_reason` and the vendor prompt classifier. DEAD uses
//! the connection-loss vocabulary already ported by `sweep_nudges`, checked
//! only against the last event after input-bar chrome is removed.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use regex::Regex;

use crate::blocking_prompt::{self, PromptAudience};
use crate::limits;

/// Python health's `MISSING` — the printed stand-in when a pane has no
/// project, task, or seat. Keep the em dash; callers parse this token.
const MISSING: &str = "—";

/// Vendors `limits::classify` actually registers. An empty kind walks this
/// list so a LIMIT/RATE banner is never dropped for want of a seat.
const REGISTERED_KINDS: [&str; 3] = ["claude", "codex", "grok"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneClass {
    Limit,
    Rate,
    Context,
    Dead,
    Dialog,
    Unreadable,
}

impl PaneClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "LIMIT",
            Self::Rate => "RATE",
            Self::Context => "CONTEXT",
            Self::Dead => "DEAD",
            Self::Dialog => "DIALOG",
            Self::Unreadable => "UNREADABLE",
        }
    }
}

/// `PaneProblem.as_line`: `"{cls} {pane} {project} {task} {seat}"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProblem {
    pub class: PaneClass,
    pub pane: String,
    pub project: String,
    pub task: String,
    pub seat: String,
}

impl PaneProblem {
    pub fn as_line(&self) -> String {
        format!(
            "{} {} {} {} {}",
            self.class.as_str(),
            self.pane,
            self.project,
            self.task,
            self.seat
        )
    }
}

/// How many non-blank content lines of a pane's visible text are
/// considered when classifying it.
pub const VISIBLE_TAIL_LINES: usize = 7;

/// Non-blank, alnum-containing lines only — border/cursor-fill rows are
/// excluded so a real trailing sentence is not pushed out of the tail
/// window by blank-looking box-drawing, preserving line order.
pub fn tail_content_lines(text: &str, n: usize) -> Vec<&str> {
    let content: Vec<&str> = text
        .lines()
        .filter(|line| line.chars().any(|c| c.is_ascii_alphanumeric()))
        .collect();
    let start = content.len().saturating_sub(n);
    content[start..].to_vec()
}

fn progress_line_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^\s*[^\sA-Za-z0-9]{1,2}\s*[A-Za-z]+…\s*\(\d+m\s*\d+s|^\s*Working\s*\(")
            .expect("static regex compiles")
    })
}

fn input_bar_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)shift\+tab to cycle|esc to interrupt|^[>\s│|─—\-]*$")
            .expect("static regex compiles")
    })
}

fn stuck_pane_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)reconnecting|idle timeout|connection lost|ENOTFOUND|stopped arriving|network error|transport error|api error")
            .expect("static regex compiles")
    })
}

/// `limits::classify` rules that are a spent quota, not a rolling 429.
/// Codex out-of-credits is `BreakerKind::Rolling` (card `20260915-b-fe8000`
/// in the frozen Python reference), so `kind` alone cannot tell LIMIT from
/// RATE — only the rule name can. Mirrors Python's `_LIMIT_RULES`/
/// `_is_limit_rule`.
fn is_limit_rule(rule: &str) -> bool {
    rule == "codex_usage_limit_reached"
        || rule == "grok_weekly_limit_reached"
        || rule.contains("usage_limit")
        || rule.contains("weekly_limit")
}

/// Mirrors Python's `_RATE_RULES`/`_is_rate_rule`.
fn is_rate_rule(rule: &str) -> bool {
    rule == "codex_retry_limit_exceeded"
        || rule == "claude_rate_limit_error"
        || rule.contains("rate_limit")
        || rule.contains("retry_limit")
}

fn context_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)exceeds the available context|context (window|length) exceeded|prompt is too long")
            .expect("static regex compiles")
    })
}

/// LIMIT/RATE via the ported, fixture-anchored classifiers this module's own
/// doc comment has always promised (`limits::classify`'s `Verdict.rule`,
/// `blocking_prompt::classify_prompt`'s `PromptAudience::Operator` — the
/// vendor-exhaustion rules a plain banner can land outside `classify`'s own
/// non-interactive branch, same as Python's `classify_visible_text`).
/// CONTEXT uses Python health's own regex. Dialogs are considered after DEAD.
/// `kind` is the Herdr agent kind (`"claude"`/`"codex"`/`"grok"`). An
/// unrecognised kind still fail-opens to `None`; an empty kind tries every
/// registered vendor so a LIMIT/RATE banner is never missed for want of a
/// seat (the scan's own naming/dispatch lookup is what fills `kind` in
/// production — this fallback is the last line when that lookup has nothing).
fn limit_rate_or_context(text: &str, kind: &str) -> Option<PaneClass> {
    if kind.is_empty() {
        for candidate in REGISTERED_KINDS {
            if let Some(class) = limit_rate_or_context_for_kind(text, candidate) {
                return Some(class);
            }
        }
        return None;
    }
    limit_rate_or_context_for_kind(text, kind)
}

fn limit_rate_or_context_for_kind(text: &str, kind: &str) -> Option<PaneClass> {
    let verdict = limits::classify(text, kind, None);
    if verdict.as_ref().is_some_and(|v| is_limit_rule(&v.rule)) {
        return Some(PaneClass::Limit);
    }
    if blocking_prompt::classify_prompt(text, kind)
        .is_some_and(|prompt| prompt.audience == PromptAudience::Operator)
    {
        return Some(PaneClass::Limit);
    }
    if verdict.as_ref().is_some_and(|v| is_rate_rule(&v.rule)) {
        return Some(PaneClass::Rate);
    }
    if context_re().is_match(text) {
        return Some(PaneClass::Context);
    }
    None
}

/// Classify a pane's visible-tail content (already reduced to at most
/// [`VISIBLE_TAIL_LINES`] content lines by [`tail_content_lines`]). `kind` is
/// the Herdr agent kind owning this pane — see `limit_rate_or_context`.
///
/// LIMIT/RATE/CONTEXT are checked first against the joined tail. DEAD
/// checks only the last event after input-bar chrome is removed; a progress
/// line is a later event even when its cosmetic verb says "Reconnecting".
/// Dialog/menu detection follows DEAD, matching Python's precedence.
pub fn classify_visible_text(tail_lines: &[&str], kind: &str) -> Option<PaneClass> {
    let joined = tail_lines.join("\n");

    if let Some(class) = limit_rate_or_context(&joined, kind) {
        return Some(class);
    }

    if let Some(last) = tail_lines
        .iter()
        .rev()
        .find(|l| !input_bar_re().is_match(l))
    {
        if !progress_line_re().is_match(last) && stuck_pane_re().is_match(last) {
            return Some(PaneClass::Dead);
        }
    }

    if crate::runner::attention_reason(&joined).is_some()
        || blocking_prompt::classify_prompt(&joined, kind).is_some()
    {
        return Some(PaneClass::Dialog);
    }
    None
}

/// One routing-log task, used to attribute a live pane the way Python
/// health's `_logged_by_agent_and_pane` does: latest dispatch/start record
/// keyed by agent name, else by pane id.
#[derive(Clone)]
struct LoggedTask {
    project: String,
    task_id: String,
    account: String,
}

/// Drop `agent_name`'s trailing all-digit instance suffix (`-2`, `-3`, …)
/// the way Python `naming.strip_instance_suffix` does, so the last remaining
/// token is the seat short rather than the collision counter.
fn strip_instance_suffix<'a>(parts: &'a [&'a str]) -> &'a [&'a str] {
    if parts.len() > 1
        && parts
            .last()
            .is_some_and(|last| last.chars().all(|c| c.is_ascii_digit()))
    {
        &parts[..parts.len() - 1]
    } else {
        parts
    }
}

/// Python `_seat_from_name`: last hyphen token after the instance suffix,
/// mapped to a configured account name, else the short itself, else MISSING.
fn seat_from_name(config: &mv_config::load::Config, name: &str) -> String {
    let parts: Vec<&str> = name.split('-').collect();
    let parts = strip_instance_suffix(&parts);
    if parts.len() < 3 {
        return MISSING.to_owned();
    }
    let short = parts[parts.len() - 1];
    for account in config.accounts.values() {
        if account.short == short {
            return account.name.clone();
        }
    }
    if short.is_empty() {
        MISSING.to_owned()
    } else {
        short.to_owned()
    }
}

/// Python `_kind_for`: the named account's kind, else the kind of the seat
/// parsed from the agent name. Empty when neither is a configured account.
fn kind_for<'a>(
    config: &'a mv_config::load::Config,
    account_name: &str,
    agent_name: &str,
) -> &'a str {
    if !account_name.is_empty() && account_name != MISSING {
        if let Some(account) = config.accounts.get(account_name) {
            return account.kind.as_str();
        }
    }
    let seat = seat_from_name(config, agent_name);
    if seat != MISSING {
        if let Some(account) = config.accounts.get(&seat) {
            return account.kind.as_str();
        }
    }
    ""
}

/// Python `_cwd_of`: `foreground_cwd`, else `cwd`.
fn cwd_of(agent: &serde_json::Value) -> Option<&Path> {
    let pick = |key: &str| {
        agent
            .get(key)
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(Path::new)
    };
    pick("foreground_cwd").or_else(|| pick("cwd"))
}

/// Python `_project_and_task_from_cwd`: longest-prefix configured project,
/// then the first path component under `.multiverse/worktrees`.
fn project_and_task_from_cwd(
    config: &mv_config::load::Config,
    cwd: Option<&Path>,
) -> (String, String) {
    let Some(cwd) = cwd else {
        return (MISSING.to_owned(), MISSING.to_owned());
    };
    let mut matches: Vec<(&str, &Path)> = Vec::new();
    for project in config.projects.values() {
        let Some(path) = project.path.value() else {
            continue;
        };
        let root = Path::new(path);
        if cwd == root || cwd.starts_with(root) {
            matches.push((project.name.as_str(), root));
        }
    }
    let Some((name, root)) = matches
        .into_iter()
        .max_by_key(|(_, path)| path.components().count())
    else {
        return (MISSING.to_owned(), MISSING.to_owned());
    };
    let lane_root = root.join(".multiverse").join("worktrees");
    if cwd.starts_with(&lane_root) && cwd != lane_root {
        if let Ok(rel) = cwd.strip_prefix(&lane_root) {
            if let Some(task) = rel
                .components()
                .next()
                .and_then(|part| part.as_os_str().to_str())
                .filter(|part| !part.is_empty())
            {
                return (name.to_owned(), task.to_owned());
            }
        }
    }
    (name.to_owned(), MISSING.to_owned())
}

/// Python `_logged_by_agent_and_pane`: latest routing-log task per agent
/// name and per pane id, walked in `project_order` so a later project's
/// overwrite matches Python's declaration-order walk.
fn logged_by_agent_and_pane(
    config: &mv_config::load::Config,
) -> (HashMap<String, LoggedTask>, HashMap<String, LoggedTask>) {
    let mut by_agent = HashMap::new();
    let mut by_pane = HashMap::new();
    let names: Vec<&String> = if config.project_order.is_empty() {
        config.projects.keys().collect()
    } else {
        config.project_order.iter().collect()
    };
    for name in names {
        let Some(project) = config.projects.get(name) else {
            continue;
        };
        let Some(path) = project.path.value() else {
            continue;
        };
        for task in mv_state::tasks::sweep_tasks(Path::new(path)) {
            let logged = LoggedTask {
                project: project.name.clone(),
                task_id: if task.task_id.is_empty() {
                    MISSING.to_owned()
                } else {
                    task.task_id
                },
                account: task.account.unwrap_or_default(),
            };
            if let Some(agent) = task.agent.filter(|s| !s.is_empty()) {
                by_agent.insert(agent, logged.clone());
            }
            if let Some(pane) = task.pane.filter(|s| !s.is_empty()) {
                by_pane.insert(pane, logged);
            }
        }
    }
    (by_agent, by_pane)
}

/// Read-only live scan through the shared, bounded Herdr subprocess layer.
/// Listing failures remain errors rather than being reported as a clean fleet.
pub fn scan_live_panes(config: &mv_config::load::Config) -> Result<Vec<PaneProblem>, String> {
    scan_with(config, &mut crate::runner::ProcessHerdr)
}

fn scan_with(
    config: &mv_config::load::Config,
    herdr: &mut impl crate::runner::Herdr,
) -> Result<Vec<PaneProblem>, String> {
    use mv_herdr::argv;
    use std::time::Duration;

    let timeout = Duration::from_secs(30);
    let result = herdr
        .call(&argv::agent_list(), timeout)
        .map_err(|e| e.to_string())?;
    if !result.ok() {
        return Err(format!("herdr agent list failed: {}", result.message()));
    }
    let agents = result
        .payload
        .as_ref()
        .and_then(|value| value.get("result"))
        .and_then(|value| value.get("agents"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "herdr agent list returned no agents array".to_owned())?;
    let hub_names = config
        .projects
        .get(mv_state::activity::HUB_PROJECT_NAME)
        .zip(config.accounts.values().find(|account| account.interactive))
        .map(|(hub, account)| {
            [
                crate::fleet_start::nexus_agent_name(
                    &hub.name,
                    &account.short,
                    hub.short.as_deref().unwrap_or(""),
                )
                .ok(),
                crate::fleet_start::nexus_legacy_agent_name(
                    &hub.name,
                    &account.short,
                    hub.short.as_deref().unwrap_or(""),
                )
                .ok(),
            ]
        })
        .unwrap_or_default();
    let (by_agent, by_pane) = logged_by_agent_and_pane(config);
    let mut problems = Vec::new();
    for agent in agents {
        let name = agent
            .get("name")
            .and_then(serde_json::Value::as_str)
            .or_else(|| agent.get("pane_id").and_then(serde_json::Value::as_str))
            .ok_or_else(|| {
                "herdr agent list returned an agent without a name or pane".to_owned()
            })?;
        if hub_names.iter().flatten().any(|hub| hub == name) {
            continue;
        }
        let target = agent
            .get("pane_id")
            .and_then(serde_json::Value::as_str)
            .filter(|pane| !pane.is_empty())
            .unwrap_or(name);
        let pane_label = target;
        let logged = (!name.is_empty())
            .then(|| by_agent.get(name))
            .flatten()
            .or_else(|| by_pane.get(pane_label));
        let (project, task, seat) = match logged {
            Some(logged) => {
                let seat = if logged.account.is_empty() {
                    seat_from_name(config, name)
                } else {
                    logged.account.clone()
                };
                (logged.project.clone(), logged.task_id.clone(), seat)
            }
            None => {
                let (project, task) = project_and_task_from_cwd(config, cwd_of(agent));
                (project, task, seat_from_name(config, name))
            }
        };
        let kind = kind_for(config, &seat, name);
        let visible = herdr
            .call(&argv::agent_read(target, Some("visible"), None), timeout)
            .ok()
            .filter(|result| result.ok());
        let class = match visible {
            Some(visible) => classify_visible_text(
                &tail_content_lines(&visible.stdout, VISIBLE_TAIL_LINES),
                kind,
            ),
            None => Some(PaneClass::Unreadable),
        };
        let Some(class) = class else { continue };
        problems.push(PaneProblem {
            class,
            pane: pane_label.to_owned(),
            project,
            task,
            seat,
        });
    }
    Ok(problems)
}

/// Serialize the historical top-level array, escaping every string field.
pub fn problems_json(problems: &[PaneProblem]) -> String {
    let values: Vec<_> = problems
        .iter()
        .map(|problem| {
            serde_json::json!({
                "class": problem.class.as_str(), "pane": problem.pane,
                "project": problem.project, "task": problem.task, "seat": problem.seat,
            })
        })
        .collect();
    serde_json::to_string(&values).expect("health report serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scripted {
        replies: std::collections::VecDeque<mv_herdr::envelope::CommandResult>,
        calls: Vec<Vec<String>>,
    }

    impl crate::runner::Herdr for Scripted {
        fn call(
            &mut self,
            command: &mv_herdr::argv::HerdrCommand,
            _timeout: std::time::Duration,
        ) -> Result<mv_herdr::envelope::CommandResult, crate::runner::RunnerError> {
            self.calls.push(command.argv[1..].to_vec());
            Ok(self.replies.pop_front().expect("unexpected Herdr call"))
        }
    }

    fn reply(stdout: &str) -> mv_herdr::envelope::CommandResult {
        mv_herdr::envelope::CommandResult {
            argv: Vec::new(),
            returncode: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            payload: serde_json::from_str(stdout).ok(),
            protocol_error: None,
        }
    }

    fn config() -> (tempfile::TempDir, mv_config::load::Config) {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n",
            root.path().to_str().unwrap()
        );
        let config =
            mv_config::load::load_config_str(&content, std::path::Path::new("config.toml"))
                .unwrap();
        (root, config)
    }

    /// Like [`config`], plus one `codex` account whose default `short`
    /// ("sb") matches the `-sb` suffix `scan_reads_visible_text_without_
    /// writing_to_panes` already dispatches under, so that scan can resolve
    /// a `kind` for its pane and `limits::classify` has a vendor to classify
    /// against.
    fn config_with_codex_seat() -> (tempfile::TempDir, mv_config::load::Config) {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "[accounts.sb]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = {:?}\n\n[projects.sandbox]\npath = {:?}\n",
            root.path().join("codex-home").to_str().unwrap(),
            root.path().to_str().unwrap()
        );
        let config =
            mv_config::load::load_config_str(&content, std::path::Path::new("config.toml"))
                .unwrap();
        (root, config)
    }

    #[test]
    fn empty_listing_is_clean_and_malformed_listing_is_an_error() {
        for (listing, clean) in [
            (r#"{"result":{"agents":[]}}"#, true),
            (r#"{"result":{}}"#, false),
        ] {
            let mut herdr = Scripted {
                replies: [reply(listing)].into(),
                calls: Vec::new(),
            };
            let (_root, config) = config();
            let result = scan_with(&config, &mut herdr);
            if clean {
                assert!(result.unwrap().is_empty());
            } else {
                assert!(result.unwrap_err().contains("agents array"));
            }
            assert_eq!(herdr.calls, vec![vec!["agent", "list"]]);
        }
    }

    #[test]
    fn scan_reads_visible_text_without_writing_to_panes() {
        let (root, config) = config_with_codex_seat();
        let listing = serde_json::json!({"result":{"agents":[{"name":"sandbox-scou-sb", "pane_id":"w1:p1", "cwd":root.path().join(".multiverse/worktrees/lane-1")} ]}}).to_string();
        let mut herdr = Scripted {
            replies: [
                reply(&listing),
                reply(
                    "Usage limit reached\nRequest a limit increase from your owner to continue using codex.",
                ),
            ]
            .into(),
            calls: Vec::new(),
        };
        let problems = scan_with(&config, &mut herdr).unwrap();
        assert_eq!(
            problems,
            vec![PaneProblem {
                class: PaneClass::Limit,
                pane: "w1:p1".into(),
                project: "sandbox".into(),
                task: "lane-1".into(),
                seat: "sb".into(),
            }]
        );
        assert_eq!(
            herdr.calls,
            vec![
                vec!["agent", "list"],
                vec!["agent", "read", "w1:p1", "--source", "visible"]
            ]
        );
        let json: serde_json::Value = serde_json::from_str(&problems_json(&problems)).unwrap();
        assert_eq!(json[0]["class"], "LIMIT");
        assert_eq!(problems_json(&[]), "[]");
        assert!(herdr.replies.is_empty());
    }

    #[test]
    fn failed_listing_is_an_error() {
        let mut failed = reply("");
        failed.returncode = 1;
        failed.stderr = "offline".into();
        let mut herdr = Scripted {
            replies: [failed].into(),
            calls: Vec::new(),
        };
        let (_root, config) = config();
        assert!(scan_with(&config, &mut herdr)
            .unwrap_err()
            .contains("offline"));
    }

    #[test]
    fn failed_read_is_unreadable_and_later_panes_are_still_scanned() {
        let listing = r#"{"result":{"agents":[{"name":"first","pane_id":"w1:p1"},{"name":"second","pane_id":"w1:p2"}]}}"#;
        let mut failed = reply("");
        failed.returncode = 1;
        failed.stderr = "offline".into();
        let mut herdr = Scripted {
            replies: [reply(listing), failed, reply("Connection lost")].into(),
            calls: Vec::new(),
        };
        let (_root, config) = config();
        let problems = scan_with(&config, &mut herdr).unwrap();
        assert_eq!(problems.len(), 2);
        assert_eq!(problems[0].class.as_str(), "UNREADABLE");
        assert_eq!(problems[1].class, PaneClass::Dead);
        assert!(herdr.replies.is_empty());
    }

    #[test]
    fn python_dead_context_and_menu_classification() {
        for (text, expected) in [
            ("Connection lost, reconnecting...", Some(PaneClass::Dead)),
            ("Connection lost\nRecovered and continuing", None),
            (
                "Connection lost\nbypass permissions on (shift+tab to cycle)",
                Some(PaneClass::Dead),
            ),
            ("Connection lost\nWorking (3s)", None),
            ("Do you trust this folder?", Some(PaneClass::Dialog)),
            (
                "Choose a plan\nEnter to select · Esc to cancel",
                Some(PaneClass::Dialog),
            ),
            ("prompt is too long", Some(PaneClass::Context)),
            (
                "request exceeds the available context",
                Some(PaneClass::Context),
            ),
            ("context length exceeded", Some(PaneClass::Context)),
            ("A generic panic or exception in quoted code", None),
            ("API error\nEnter to select", Some(PaneClass::Dialog)),
        ] {
            assert_eq!(classify_text(text, "codex"), expected, "{text}");
        }
    }

    #[test]
    fn tail_content_lines_drops_blank_and_border_rows() {
        let text = "real line one\n\n───────\nreal line two\nreal line three";
        let tail = tail_content_lines(text, 7);
        assert_eq!(
            tail,
            vec!["real line one", "real line two", "real line three"]
        );
    }

    #[test]
    fn tail_content_lines_caps_at_n_keeping_the_most_recent() {
        let text = (1..=10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail_content_lines(&text, 3);
        assert_eq!(tail, vec!["line 8", "line 9", "line 10"]);
    }

    #[test]
    fn clean_pane_text_classifies_as_none() {
        let tail = ["everything is fine", "still working"];
        assert_eq!(classify_visible_text(&tail, "codex"), None);
    }

    // The two cases below used to be hand-crafted text matched by this
    // module's own loose, invented LIMIT/RATE regexes rather than the ported
    // `limits::classify`/`blocking_prompt::classify_prompt` this module's
    // doc comment always claimed to delegate to (card `20261103-b-566e90`:
    // those ported functions had no caller). A pane merely *saying*
    // "Usage limit reached for this account" or "429 rate limit, retry in
    // 30s" is exactly the free-text prose `limits::classify`'s own anchored
    // regexes exist to reject (see `prose_discussing_rate_limits_does_not_
    // classify` in `limits.rs`); the shapes below are the real anchors
    // instead, inline rather than from a fixture file.
    #[test]
    fn codex_exhaustion_banner_in_tail_classifies_as_limit() {
        let tail = [
            "working on the task",
            "Usage limit reached",
            "Request a limit increase from your owner to continue using codex.",
        ];
        assert_eq!(
            classify_visible_text(&tail, "codex"),
            Some(PaneClass::Limit)
        );
    }

    #[test]
    fn claude_rate_limit_error_json_in_tail_classifies_as_rate() {
        let tail = [
            "calling the API",
            r#"{"type": "rate_limit_error", "message": "Retry in 30s."}"#,
        ];
        assert_eq!(
            classify_visible_text(&tail, "claude"),
            Some(PaneClass::Rate)
        );
    }

    #[test]
    fn trust_prompt_classifies_as_dialog() {
        let tail = ["opening the project", "Do you trust this folder?"];
        assert_eq!(
            classify_visible_text(&tail, "codex"),
            Some(PaneClass::Dialog)
        );
    }

    // --- Real captured pane text (card `20261102-b-d8e4ac`), carried over
    // from the frozen Python reference's own `tests/fixtures/rate-limit/`
    // and sanitized of personal paths/project names — see
    // `tests/fixtures/rate-limit/README.md`. Assertions mirror
    // `test_health.py`'s own pins against the same captures. --------------

    const CODEX_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-out-of-credits.txt");
    const CODEX_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-rolling-429.txt");
    const CODEX_MODEL_SWITCH: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt");
    const CLAUDE_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/claude-rolling-429.txt");
    const GROK_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-reached.txt");
    const GROK_GAUGE: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt");
    const GROK_LIVE_PANE: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-live-pane.txt");

    fn classify_text(text: &str, kind: &str) -> Option<PaneClass> {
        classify_visible_text(&tail_content_lines(text, VISIBLE_TAIL_LINES), kind)
    }

    #[test]
    fn limit_class_from_captured_billing_fixtures() {
        assert_eq!(
            classify_text(CODEX_EXHAUSTED, "codex"),
            Some(PaneClass::Limit)
        );
        assert_eq!(
            classify_text(GROK_EXHAUSTED, "grok"),
            Some(PaneClass::Limit)
        );
    }

    #[test]
    fn limit_class_from_live_grok_weekly_limit_pane() {
        // Card `20261022-b-de4b39`: a real capture, not a hand-quoted one —
        // the dialog sits in its own bordered box with a nav-hint line and
        // Herdr's own pane-footer line trailing it.
        assert_eq!(
            classify_text(GROK_LIVE_PANE, "grok"),
            Some(PaneClass::Limit)
        );
    }

    #[test]
    fn rate_class_from_captured_429_fixtures() {
        assert_eq!(
            classify_text(CLAUDE_ROLLING, "claude"),
            Some(PaneClass::Rate)
        );
        assert_eq!(classify_text(CODEX_ROLLING, "codex"), Some(PaneClass::Rate));
    }

    #[test]
    fn claude_429_is_rate_not_dead() {
        // The fixture also says "API Error", which `stuck_pane_re` matches.
        // RATE must win over DEAD.
        assert_ne!(
            classify_text(CLAUDE_ROLLING, "claude"),
            Some(PaneClass::Dead)
        );
    }

    #[test]
    fn dialog_class_from_model_switch_fixture() {
        assert_eq!(
            classify_text(CODEX_MODEL_SWITCH, "codex"),
            Some(PaneClass::Dialog)
        );
    }

    #[test]
    fn live_gauge_never_classifies_despite_sharing_weekly_limit_wording() {
        // Grok's live percentage gauge shares the phrase "Weekly limit"
        // with the exhaustion dialog but must never trip on its own.
        assert_eq!(classify_text(GROK_GAUGE, "grok"), None);
    }

    #[test]
    fn spinner_verb_that_echoes_stuck_vocabulary_is_not_dead() {
        // A progress spinner whose own verb text happens to contain
        // stuck-pane vocabulary must not classify as DEAD.
        let tail = ["prior output", "\u{2735} Handling exception... (18m 42s"];
        assert_ne!(classify_visible_text(&tail, "codex"), Some(PaneClass::Dead));
    }

    #[test]
    fn stuck_text_as_the_last_real_line_is_dead() {
        let tail = ["doing work", "Connection lost"];
        assert_eq!(classify_visible_text(&tail, "codex"), Some(PaneClass::Dead));
    }

    #[test]
    fn stuck_vocabulary_buried_mid_tail_is_not_dead_once_a_newer_line_follows() {
        let tail = ["Connection lost", "recovered, continuing normally"];
        assert_ne!(classify_visible_text(&tail, "codex"), Some(PaneClass::Dead));
    }

    #[test]
    fn input_bar_chrome_is_skipped_when_finding_the_last_content_line() {
        let tail = ["Transport error", "> ", "│"];
        assert_eq!(classify_visible_text(&tail, "codex"), Some(PaneClass::Dead));
    }

    #[test]
    fn pane_problem_as_line_matches_the_fixed_field_order() {
        let problem = PaneProblem {
            class: PaneClass::Limit,
            pane: "pane-1".to_string(),
            project: "proj".to_string(),
            task: "task-1".to_string(),
            seat: "claude-1".to_string(),
        };
        assert_eq!(problem.as_line(), "LIMIT pane-1 proj task-1 claude-1");
    }

    fn config_with_colliding_shorts() -> (tempfile::TempDir, mv_config::load::Config) {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "[accounts.alpha]\nkind = \"claude\"\nshort = \"b\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = {claude:?}\n\n\
             [accounts.zulu]\nkind = \"codex\"\nshort = \"sb\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = {codex:?}\n\n\
             [projects.sandbox]\npath = {path:?}\n",
            claude = root.path().join("claude-home"),
            codex = root.path().join("codex-home"),
            path = root.path(),
        );
        let config =
            mv_config::load::load_config_str(&content, std::path::Path::new("config.toml"))
                .unwrap();
        (root, config)
    }

    #[test]
    fn scan_uses_the_last_hyphen_token_not_a_shorter_suffix() {
        let (root, config) = config_with_colliding_shorts();
        let listing = serde_json::json!({"result":{"agents":[{
            "name":"sandbox-buil-sb-2",
            "pane_id":"w1:p1",
            "cwd": root.path().join(".multiverse/worktrees/20261101-builder-abcdef")
        }]}})
        .to_string();
        let mut herdr = Scripted {
            replies: [reply(&listing), reply(CODEX_ROLLING)].into(),
            calls: Vec::new(),
        };
        let problems = scan_with(&config, &mut herdr).unwrap();
        assert_eq!(
            problems,
            vec![PaneProblem {
                class: PaneClass::Rate,
                pane: "w1:p1".into(),
                project: "sandbox".into(),
                task: "20261101-builder-abcdef".into(),
                seat: "zulu".into(),
            }]
        );
    }

    #[test]
    fn scan_prefers_the_dispatch_record_over_cwd_basename() {
        let (root, config) = config_with_colliding_shorts();
        let state = root.path().join(".multiverse");
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(
            state.join("routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20261101-builder-intoee\t\
             project=sandbox\trole=builder\taccount=zulu\tpool=codex:t\toutcome=chain-primary\t\
             reason=\tagent=renamed-pane\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20261101-builder-intoee\t\
             pane=w1:p1\n",
        )
        .unwrap();
        let owner_lane = root
            .path()
            .join(".multiverse/worktrees/20261101-builder-owner1");
        let listing = serde_json::json!({"result":{"agents":[{
            "name":"renamed-pane",
            "pane_id":"w1:p1",
            "cwd": owner_lane
        }]}})
        .to_string();
        let mut herdr = Scripted {
            replies: [reply(&listing), reply(CODEX_ROLLING)].into(),
            calls: Vec::new(),
        };
        let problems = scan_with(&config, &mut herdr).unwrap();
        assert_eq!(
            problems,
            vec![PaneProblem {
                class: PaneClass::Rate,
                pane: "w1:p1".into(),
                project: "sandbox".into(),
                task: "20261101-builder-intoee".into(),
                seat: "zulu".into(),
            }]
        );
    }

    #[test]
    fn empty_kind_still_classifies_a_rate_limit_banner() {
        let tail = tail_content_lines(CODEX_ROLLING, VISIBLE_TAIL_LINES);
        assert_eq!(
            classify_visible_text(&tail, ""),
            Some(PaneClass::Rate),
            "a RATE banner must not fail open when kind is empty"
        );
    }
}

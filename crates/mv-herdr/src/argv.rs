//! Argv builders for every Herdr surface: agent, pane, workspace,
//! tab, worktree. Includes the `native_args` effort table and the
//! Codex update-check suppression flag.
//!
//! Ported from `herdr.py`. Every builder resolves [`herdr_binary`] on
//! every call rather than once at import, so a dry-run and a real
//! dispatch built in the same process cannot disagree about which
//! binary they mean, and a long-lived process picks up a changed
//! `HERDR_BIN_PATH` without a restart.
//!
//! Fleet-policy builders that depend on config or worktree state
//! (the Grok/Claude deny lists, Codex trust, the pane-label and
//! temporary-project helpers) are deliberately not here: this crate
//! sits below `mv-config` and `mv-state` in the workspace DAG, so
//! anything that needs those types belongs in `mv-engine` instead.

use std::path::Path;

use serde_json::Value;

/// Maximum time the shared runner gives a Herdr subprocess by default.
/// A caller may pass a longer, documented timeout for a deliberately
/// slower operation.
pub const DEFAULT_TIMEOUT_SECONDS: f64 = 30.0;

/// Pane/agent reads are diagnostics and prompt-state probes, not a way
/// to stream an unbounded terminal transcript. Keep only the newest
/// portion, where Herdr's status prompts and the current composer live.
pub const MAX_PANE_READ_BYTES: usize = 2 * 1024 * 1024;

/// Cap on every other command's stdout, and on every command's stderr.
/// A JSON listing (`agent list`, `pane list`, `workspace list`) or an
/// error message is expected to stay well under this; a hostile or
/// upgraded Herdr that prints far more than that is refused with a
/// typed error rather than read in full, which could otherwise exhaust
/// memory before the reply is ever parsed (card `20261103-b-57c876`).
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// A herdr invocation, as an argv vector.
///
/// Kept as a vector rather than a string so nothing is ever
/// shell-quoted into existence by accident; [`HerdrCommand::shell`]
/// renders a copy-pasteable form for the dry-run only.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HerdrCommand {
    pub argv: Vec<String>,
    /// What this command is for, shown in the dry-run.
    pub purpose: String,
    /// Text fed to the subprocess's stdin, or `None` for inherited/empty
    /// stdin. The shared seam for a command whose input cannot go on
    /// argv without leaking it, rather than each caller inventing its
    /// own stdin-capable subprocess wrapper.
    pub stdin: Option<String>,
    /// True for commands whose JSON a caller parses (listing, lookup,
    /// pane split, workspace/tab create). The runner treats a zero-exit
    /// non-JSON reply as a protocol error for these alone: `pane run`,
    /// `agent prompt`, and text-format reads legitimately print plain
    /// text.
    pub expects_json: bool,
}

impl HerdrCommand {
    /// Build a command with no purpose, stdin, or JSON expectation set.
    pub fn new(argv: Vec<String>) -> Self {
        Self {
            argv,
            ..Default::default()
        }
    }

    /// Copy-pasteable shell form. For display; never executed.
    pub fn shell(&self) -> String {
        shell_join(&self.argv)
    }
}

/// Join `parts` into a copy-pasteable shell command, matching Python's
/// `shlex.join`: each part quoted with `shlex_quote`, then space-joined.
/// Public because policy launch-wrapper builders above `mv-herdr` in the
/// workspace DAG (`mv-engine::trust::claude_orchestrator_launch`) need the
/// same quoting [`HerdrCommand::shell`] already uses, for an argv vector
/// that is never built into a full `HerdrCommand`.
pub fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shlex_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Quote `value` the way POSIX shells require, matching Python's
/// `shlex.quote`: left bare when every character is already safe,
/// single-quoted (with embedded quotes escaped) otherwise.
fn shlex_quote(value: &str) -> String {
    let is_safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c));
    if is_safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

fn command(argv: Vec<String>, purpose: impl Into<String>) -> HerdrCommand {
    HerdrCommand {
        argv,
        purpose: purpose.into(),
        stdin: None,
        expects_json: false,
    }
}

fn json_command(argv: Vec<String>, purpose: impl Into<String>) -> HerdrCommand {
    HerdrCommand {
        expects_json: true,
        ..command(argv, purpose)
    }
}

fn path_str(path: impl AsRef<Path>) -> String {
    path.as_ref().to_string_lossy().into_owned()
}

fn push_env_flags(argv: &mut Vec<String>, env: &[(&str, &str)]) {
    for (key, value) in env {
        argv.push("--env".into());
        argv.push(format!("{key}={value}"));
    }
}

/// A token clap (and most POSIX parsers) would treat as an option rather
/// than a value if it appeared bare in argv. `-` alone is stdin/stdout,
/// not a flag.
fn looks_like_flag(value: &str) -> bool {
    value.starts_with('-') && value != "-"
}

/// `--flag value` unless `value` itself looks like a flag, in which case
/// `--flag=value` so clap cannot consume the value as a new option.
fn push_valued_flag(argv: &mut Vec<String>, flag: &str, value: impl Into<String>) {
    let value = value.into();
    if looks_like_flag(&value) {
        argv.push(format!("{flag}={value}"));
    } else {
        argv.push(flag.to_string());
        argv.push(value);
    }
}

/// Append positionals. If any token looks like a flag, insert `--` first
/// so a clap parser cannot consume it as an option. One `--` covers the
/// whole run: a second `--` after end-of-options would itself become a
/// positional.
fn push_positionals(argv: &mut Vec<String>, values: &[&str]) {
    if values.iter().copied().any(looks_like_flag) {
        argv.push("--".into());
    }
    for value in values {
        argv.push((*value).into());
    }
}

fn format_amount(amount: f64) -> String {
    let rendered = amount.to_string();
    if rendered.contains('.') || rendered.contains('e') || rendered.contains('E') {
        rendered
    } else {
        format!("{rendered}.0")
    }
}

/// Resolve the token or path used to invoke Herdr.
///
/// Prefers `HERDR_BIN_PATH` — the exact path Herdr itself passes to any
/// process it invokes — falling back to the bare `herdr` token, found
/// by `PATH` lookup. A blank or whitespace-only value is treated as
/// unset rather than as a command named `""`.
///
/// Read from the environment on every call, not cached: a long-lived
/// process must be able to pick up a changed value without a restart.
pub fn herdr_binary() -> String {
    resolve_herdr_binary(std::env::var("HERDR_BIN_PATH").ok())
}

fn resolve_herdr_binary(raw: Option<String>) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value,
        _ => "herdr".to_string(),
    }
}

/// How each kind takes a reasoning-effort setting. Every entry was read
/// from the tool, none invented:
///
/// * `claude` — `--effort <level>`
/// * `grok` — `--reasoning-effort <level>`
/// * `codex` — `-c model_reasoning_effort=<level>`
fn effort_args(kind: &str, level: &str) -> Option<Vec<String>> {
    match kind {
        "claude" => Some(vec!["--effort".into(), level.into()]),
        "grok" => Some(vec!["--reasoning-effort".into(), level.into()]),
        "codex" => Some(vec!["-c".into(), format!("model_reasoning_effort={level}")]),
        _ => None,
    }
}

/// Codex's documented `--config <key=value>` override is the launch-time
/// equivalent of setting this in `config.toml`. Keep it on every
/// dispatched Codex seat so a stale CLI cannot park a lane while trying
/// to upgrade itself.
const CODEX_DISABLE_UPDATE_ON_STARTUP: [&str; 2] = ["-c", "check_for_update_on_startup=false"];

/// Native CLI arguments for `agent start ... -- <args>`.
///
/// Native arguments go only after `--`; this is the one place that
/// builds them, so the dry-run card and the real dispatch cannot
/// disagree about what the agent actually started with. An unknown
/// `kind` gets `--model` alone rather than a guessed effort flag: a
/// wrong flag is a startup failure, a missing one is merely a default.
pub fn native_args(model: &str, kind: &str, effort: Option<&str>) -> Vec<String> {
    let mut args = vec!["--model".to_string(), model.to_string()];
    if let Some(level) = effort {
        if let Some(extra) = effort_args(kind, level) {
            args.extend(extra);
        }
    }
    if kind == "codex" {
        args.extend(
            CODEX_DISABLE_UPDATE_ON_STARTUP
                .iter()
                .map(|s| s.to_string()),
        );
    }
    args
}

/// Split a pane, with the spawn environment attached.
///
/// `--env` is where account selection actually happens: it sets the
/// variable on the Herdr *server*, which is what actually launches the
/// agent, rather than on the short-lived client process that only sends
/// a socket request.
///
/// `current` selects `--current` (split the pane resolved from
/// `HERDR_PANE_ID`) when no explicit `pane` is given; `pane` splits a
/// *named* pane instead, which is what lets a second specialist stack
/// below the first. `focus` defaults to `false` at the call site: nothing
/// is more annoying than an agent stealing the cursor.
pub fn pane_split(
    cwd: impl AsRef<Path>,
    env: &[(&str, &str)],
    pane: Option<&str>,
    current: bool,
    direction: Option<&str>,
    focus: bool,
) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "pane".into(), "split".into()];
    if let Some(pane) = pane {
        argv.push("--pane".into());
        argv.push(pane.into());
    } else if current {
        argv.push("--current".into());
    }
    argv.push("--direction".into());
    argv.push(direction.unwrap_or("right").into());
    argv.push("--cwd".into());
    argv.push(path_str(cwd));
    push_env_flags(&mut argv, env);
    argv.push(if focus {
        "--focus".into()
    } else {
        "--no-focus".into()
    });
    json_command(
        argv,
        "create the pane, carrying the account's config-dir variable",
    )
}

/// Create a workspace, with a working directory and spawn environment.
///
/// The multi-project primitive: unlike `pane split --current`, this
/// needs no `HERDR_PANE_ID`, so a launcher can bring up a project without
/// being inside any Herdr pane at all.
pub fn workspace_create(
    label: &str,
    cwd: impl AsRef<Path>,
    env: &[(&str, &str)],
    focus: bool,
) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "workspace".into(), "create".into()];
    push_valued_flag(&mut argv, "--label", label);
    push_valued_flag(&mut argv, "--cwd", path_str(cwd));
    push_env_flags(&mut argv, env);
    argv.push(if focus {
        "--focus".into()
    } else {
        "--no-focus".into()
    });
    json_command(argv, format!("create a workspace for {label}"))
}

/// List panes, optionally within one workspace.
pub fn pane_list(workspace: Option<&str>) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "pane".into(), "list".into()];
    if let Some(workspace) = workspace {
        argv.push("--workspace".into());
        argv.push(workspace.into());
    }
    json_command(argv, "see which panes are already open")
}

/// Read one pane's current label and occupant metadata.
pub fn pane_get(pane_id: &str) -> HerdrCommand {
    json_command(
        vec![herdr_binary(), "pane".into(), "get".into(), pane_id.into()],
        format!("verify pane {pane_id}"),
    )
}

/// Set a pane label used to bind task-owned scratch panes.
pub fn pane_rename(pane_id: &str, label: &str) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "pane".into(), "rename".into()];
    push_positionals(&mut argv, &[pane_id, label]);
    command(argv, format!("name pane {pane_id}"))
}

/// Where every pane in a tab actually sits, as rectangles. Scoped to the
/// pane's own tab: panes in another tab are not in the layout being
/// reasoned about.
pub fn pane_layout(pane_id: &str) -> HerdrCommand {
    command(
        vec![
            herdr_binary(),
            "pane".into(),
            "layout".into(),
            "--pane".into(),
            pane_id.into(),
        ],
        "see where the open panes sit, to place the next one",
    )
}

/// Move one of `pane`'s dividers. `amount` is a fraction of the
/// containing split's total width, not a column count, and may be
/// negative to shrink rather than grow.
pub fn pane_resize(pane: &str, direction: &str, amount: f64) -> HerdrCommand {
    command(
        vec![
            herdr_binary(),
            "pane".into(),
            "resize".into(),
            "--pane".into(),
            pane.into(),
            "--direction".into(),
            direction.into(),
            "--amount".into(),
            format_amount(amount),
        ],
        format!("resize pane {pane} toward an equal share"),
    )
}

/// Close a workspace and everything in it. Destructive: closing a pane
/// is the only way an agent ends, so this ends every agent in the
/// workspace and takes their scrollback with them.
pub fn workspace_close(workspace_id: &str) -> HerdrCommand {
    command(
        vec![
            herdr_binary(),
            "workspace".into(),
            "close".into(),
            workspace_id.into(),
        ],
        format!("close workspace {workspace_id}"),
    )
}

/// List workspaces, so a launcher can tell new from already-open.
pub fn workspace_list() -> HerdrCommand {
    json_command(
        vec![herdr_binary(), "workspace".into(), "list".into()],
        "list open workspaces",
    )
}

/// List tabs, optionally within one workspace.
pub fn tab_list(workspace: Option<&str>) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "tab".into(), "list".into()];
    if let Some(workspace) = workspace {
        argv.push("--workspace".into());
        argv.push(workspace.into());
    }
    json_command(argv, "see which tabs are already open")
}

/// Create a tab without moving the user's focus.
pub fn tab_create(
    workspace: &str,
    cwd: impl AsRef<Path>,
    label: &str,
    env: &[(&str, &str)],
    focus: bool,
) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "tab".into(), "create".into()];
    push_valued_flag(&mut argv, "--workspace", workspace);
    push_valued_flag(&mut argv, "--cwd", path_str(cwd));
    push_valued_flag(&mut argv, "--label", label);
    push_env_flags(&mut argv, env);
    argv.push(if focus {
        "--focus".into()
    } else {
        "--no-focus".into()
    });
    json_command(argv, format!("create the {label} tab"))
}

/// The create-if-absent decision for a workspace's dashboard tab.
///
/// Takes the tab listing the caller already fetched via [`tab_list`] and
/// returns the one command that makes a `label` tab exist, or `None`
/// when one already does — idempotency lives here, not at the call
/// site.
pub fn ensure_dashboard_tab(
    existing_tabs: &[Value],
    workspace: &str,
    cwd: impl AsRef<Path>,
    label: Option<&str>,
) -> Option<HerdrCommand> {
    let label = label.unwrap_or("dashboard");
    let already_exists = existing_tabs
        .iter()
        .any(|tab| tab.get("label").and_then(Value::as_str) == Some(label));
    if already_exists {
        return None;
    }
    Some(tab_create(workspace, cwd, label, &[], false))
}

/// Start a named agent in an existing pane.
///
/// `agent start` never creates layout — it requires a pane already at an
/// interactive shell prompt. Native CLI arguments go after `--`.
pub fn agent_start(
    name: &str,
    kind: &str,
    pane: &str,
    timeout_ms: Option<u64>,
    agent_args: &[String],
) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "agent".into(), "start".into()];
    // NAME is a positional read before any flag; a dash-leading name sent
    // in that position is indistinguishable from `--kind`/`--pane`/
    // `--help`. When that is possible, put every flag before `--` (as
    // `agent_prompt` already does for its own name/text positionals) so
    // NAME can only be read back as the positional once option parsing
    // has ended, with any native args following in the same end-of-options
    // run.
    if looks_like_flag(name) {
        argv.push("--kind".into());
        argv.push(kind.into());
        argv.push("--pane".into());
        argv.push(pane.into());
        if let Some(timeout_ms) = timeout_ms {
            argv.push("--timeout".into());
            argv.push(timeout_ms.to_string());
        }
        argv.push("--".into());
        argv.push(name.into());
        argv.extend(agent_args.iter().cloned());
    } else {
        argv.push(name.into());
        argv.push("--kind".into());
        argv.push(kind.into());
        argv.push("--pane".into());
        argv.push(pane.into());
        if let Some(timeout_ms) = timeout_ms {
            argv.push("--timeout".into());
            argv.push(timeout_ms.to_string());
        }
        if !agent_args.is_empty() {
            argv.push("--".into());
            argv.extend(agent_args.iter().cloned());
        }
    }
    command(argv, format!("start {kind} as agent {name:?} in that pane"))
}

/// Send a prompt to a named agent.
///
/// Defaults to `wait=false`: the runner keeps its own two-part gate over
/// this flag, so the default exists to keep a new call site from
/// inheriting semantics this codebase deliberately avoids.
pub fn agent_prompt(name: &str, text: &str, wait: bool, timeout_ms: Option<u64>) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "agent".into(), "prompt".into()];
    // Flags before `--` so a dash-leading name or brief cannot be parsed
    // as `--wait`/`--timeout`/`--help`, and so those flags are not
    // themselves swallowed as positionals after end-of-options.
    if looks_like_flag(name) || looks_like_flag(text) {
        if wait {
            argv.push("--wait".into());
        }
        if let Some(timeout_ms) = timeout_ms {
            push_valued_flag(&mut argv, "--timeout", timeout_ms.to_string());
        }
        argv.push("--".into());
        argv.push(name.into());
        argv.push(text.into());
    } else {
        argv.push(name.into());
        argv.push(text.into());
        if wait {
            argv.push("--wait".into());
        }
        if let Some(timeout_ms) = timeout_ms {
            argv.push("--timeout".into());
            argv.push(timeout_ms.to_string());
        }
    }
    command(argv, "hand the agent its brief")
}

/// Send literal key names to a named agent.
pub fn agent_send_keys(name: &str, keys: &[&str]) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "agent".into(), "send-keys".into()];
    let mut positionals = vec![name];
    positionals.extend_from_slice(keys);
    push_positionals(&mut argv, &positionals);
    command(argv, "press keys in the agent's input box")
}

/// Give a name to an agent Herdr detected but did not start. `target`
/// may be a pane id, which is what makes the seat-command launch path
/// workable: an agent launched by running a command in a pane is
/// detected with its kind and state, but with no name, and a nameless
/// agent cannot be prompted or polled.
pub fn agent_rename(target: &str, name: &str) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "agent".into(), "rename".into()];
    push_positionals(&mut argv, &[target, name]);
    command(argv, "name an agent Herdr detected rather than started")
}

/// Read one agent's state. The board is a cache; this is the truth.
pub fn agent_get(name: &str) -> HerdrCommand {
    json_command(
        vec![herdr_binary(), "agent".into(), "get".into(), name.into()],
        "reconcile board state against Herdr",
    )
}

/// List live agents in one call, cheaper than reconciling every row
/// against `agent get`.
pub fn agent_list() -> HerdrCommand {
    json_command(
        vec![herdr_binary(), "agent".into(), "list".into()],
        "enumerate live agents in one call",
    )
}

/// Block until an agent reaches one of `until`.
///
/// This command alone is not a completion signal: it is satisfied by
/// the state the agent is already in, not by a transition into it, so
/// issued straight after `agent prompt` it can return instantly,
/// matching the pre-prompt state before the agent has begun. Use this
/// only as a cheap first block, corroborated by `state_change_seq`
/// advancing.
pub fn agent_wait(name: &str, until: &[&str], timeout_ms: Option<u64>) -> HerdrCommand {
    let mut argv = vec![herdr_binary(), "agent".into(), "wait".into(), name.into()];
    for state in until {
        argv.push("--until".into());
        argv.push((*state).into());
    }
    if let Some(timeout_ms) = timeout_ms {
        argv.push("--timeout".into());
        argv.push(timeout_ms.to_string());
    }
    command(argv, "block until the agent settles (corroboration only)")
}

/// Read an agent's terminal output. `source` defaults to `visible`
/// deliberately: `recent`/`recent-unwrapped` mean scrollback, and return
/// empty on a pane whose output has not yet scrolled — which is every
/// freshly dispatched agent. Never scrape a pane for the result; this is
/// for diagnostics when a dispatch goes wrong.
pub fn agent_read(name: &str, source: Option<&str>, lines: Option<u32>) -> HerdrCommand {
    let mut argv = vec![
        herdr_binary(),
        "agent".into(),
        "read".into(),
        name.into(),
        "--source".into(),
        source.unwrap_or("visible").into(),
    ];
    if let Some(lines) = lines {
        argv.push("--lines".into());
        argv.push(lines.to_string());
    }
    command(argv, "read the agent's screen (diagnostics)")
}

/// Run a shell command in a pane, in the shell the agent will start
/// from. Used for exactly one thing: clearing inherited auth variables
/// before `agent start`, which `--env` cannot do (an empty value still
/// counts as set, so the variable must actually be unset). Because the
/// agent is started in this same shell, state changed here is inherited
/// by the agent.
pub fn pane_run(pane_id: &str, script: &str) -> HerdrCommand {
    command(
        vec![
            herdr_binary(),
            "pane".into(),
            "run".into(),
            pane_id.into(),
            script.into(),
        ],
        "clear inherited auth vars so the seat's own login is used",
    )
}

/// Read ordinary terminal output from one explicitly named pane. Separate
/// from [`agent_read`]: a scratch pane has no Herdr agent name.
pub fn pane_read(pane_id: &str, source: Option<&str>, lines: Option<u32>) -> HerdrCommand {
    let mut argv = vec![
        herdr_binary(),
        "pane".into(),
        "read".into(),
        pane_id.into(),
        "--source".into(),
        source.unwrap_or("recent-unwrapped").into(),
    ];
    if let Some(lines) = lines {
        argv.push("--lines".into());
        argv.push(lines.to_string());
    }
    command(argv, format!("read the scratch pane {pane_id}"))
}

/// Shell to unset `variables`, safe when they are not set.
pub fn clear_auth_script(variables: &[&str]) -> String {
    format!("unset {}", variables.join(" "))
}

/// Close a pane, terminating whatever occupies it. This is how an agent
/// ends: there is no `herdr agent stop`, so teardown is abrupt by
/// construction — the pane is removed from underneath a running process.
pub fn pane_close(pane_id: &str) -> HerdrCommand {
    command(
        vec![
            herdr_binary(),
            "pane".into(),
            "close".into(),
            pane_id.into(),
        ],
        "tear the pane down (the only way to end an agent)",
    )
}

/// The `HERDR_ENV` guardrail, confirmed by the bundled skill. Prevents
/// an agent in an ordinary terminal from controlling a session it does
/// not own.
pub fn guard_script() -> &'static str {
    r#"[ "$HERDR_ENV" = "1" ] || { echo "not inside herdr" >&2; exit 1; }"#
}

/// Whether we are running inside a Herdr-managed pane. `environ`
/// overrides the process environment when given, for tests.
pub fn in_herdr_pane(environ: Option<&std::collections::HashMap<String, String>>) -> bool {
    match environ {
        Some(map) => map.get("HERDR_ENV").map(String::as_str) == Some("1"),
        None => std::env::var("HERDR_ENV")
            .map(|value| value == "1")
            .unwrap_or(false),
    }
}

/// Test-only helpers shared with `run.rs`: both files build
/// [`HerdrCommand`]s through [`herdr_binary`], which reads the real
/// process environment, and `cargo test` runs on multiple threads by
/// default — so every test that depends on `HERDR_BIN_PATH`'s value
/// must serialize on the same lock.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Holds the shared lock and restores `HERDR_BIN_PATH` to whatever
    /// it was before this guard was created, for as long as the guard
    /// is alive.
    pub struct HerdrBinPathGuard {
        _lock: MutexGuard<'static, ()>,
        previous: Option<String>,
    }

    impl HerdrBinPathGuard {
        /// Hold the lock with `HERDR_BIN_PATH` unset, regardless of
        /// whatever the ambient shell (possibly a real Herdr pane) had
        /// set it to.
        pub fn unset() -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var("HERDR_BIN_PATH").ok();
            std::env::remove_var("HERDR_BIN_PATH");
            Self {
                _lock: lock,
                previous,
            }
        }

        /// Hold the lock with `HERDR_BIN_PATH` set to `value`.
        pub fn set(value: &str) -> Self {
            let lock = ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let previous = std::env::var("HERDR_BIN_PATH").ok();
            std::env::set_var("HERDR_BIN_PATH", value);
            Self {
                _lock: lock,
                previous,
            }
        }
    }

    impl Drop for HerdrBinPathGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var("HERDR_BIN_PATH", value),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::HerdrBinPathGuard;
    use super::*;

    // --- HERDR_BIN_PATH resolution ----------------------------------

    #[test]
    fn resolve_herdr_binary_defaults_to_the_bare_token() {
        assert_eq!(resolve_herdr_binary(None), "herdr");
    }

    #[test]
    fn resolve_herdr_binary_prefers_an_explicit_path() {
        assert_eq!(
            resolve_herdr_binary(Some("/opt/herdr/bin/herdr".to_string())),
            "/opt/herdr/bin/herdr"
        );
    }

    #[test]
    fn resolve_herdr_binary_treats_blank_as_unset() {
        for blank in ["", "   ", "\t\n"] {
            assert_eq!(resolve_herdr_binary(Some(blank.to_string())), "herdr");
        }
    }

    #[test]
    fn herdr_binary_is_reread_on_every_call() {
        // Each guard must be dropped before the next is taken: `let`
        // shadowing alone does not drop the earlier binding until the
        // end of this function, which would self-deadlock on the
        // non-reentrant `ENV_LOCK`.
        {
            let _guard = HerdrBinPathGuard::unset();
            assert_eq!(herdr_binary(), "herdr");
        }
        {
            let _guard = HerdrBinPathGuard::set("/custom/herdr");
            assert_eq!(herdr_binary(), "/custom/herdr");
        }
        {
            let _guard = HerdrBinPathGuard::unset();
            assert_eq!(herdr_binary(), "herdr");
        }
    }

    #[test]
    fn all_builders_route_through_the_one_resolution_point() {
        let _guard = HerdrBinPathGuard::set("/custom/herdr");
        assert_eq!(
            pane_split("/repo", &[], None, true, None, false).argv[0],
            "/custom/herdr"
        );
        assert_eq!(
            workspace_create("x", "/tmp", &[], false).argv[0],
            "/custom/herdr"
        );
        assert_eq!(pane_list(None).argv[0], "/custom/herdr");
        assert_eq!(workspace_close("w1").argv[0], "/custom/herdr");
        assert_eq!(workspace_list().argv[0], "/custom/herdr");
        assert_eq!(
            agent_start("x", "claude", "w1:p2", None, &[]).argv[0],
            "/custom/herdr"
        );
        assert_eq!(
            agent_prompt("x", "go", false, None).argv[0],
            "/custom/herdr"
        );
        assert_eq!(agent_rename("t", "n").argv[0], "/custom/herdr");
        assert_eq!(agent_get("x").argv[0], "/custom/herdr");
        assert_eq!(agent_list().argv[0], "/custom/herdr");
        assert_eq!(agent_wait("x", &[], None).argv[0], "/custom/herdr");
        assert_eq!(agent_read("x", None, None).argv[0], "/custom/herdr");
        assert_eq!(pane_run("p1", "script").argv[0], "/custom/herdr");
        assert_eq!(pane_close("p1").argv[0], "/custom/herdr");
        assert_eq!(pane_resize("p1", "right", 0.1).argv[0], "/custom/herdr");
    }

    // --- pane split -------------------------------------------------

    #[test]
    fn pane_split_carries_env() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_split(
            "/repo",
            &[("CLAUDE_CONFIG_DIR", "/dir")],
            None,
            true,
            None,
            false,
        );
        assert!(command.argv.contains(&"--env".to_string()));
        assert!(command.argv.contains(&"CLAUDE_CONFIG_DIR=/dir".to_string()));
    }

    #[test]
    fn pane_split_defaults_to_no_focus() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_split("/repo", &[], None, true, None, false);
        assert!(command.argv.contains(&"--no-focus".to_string()));
    }

    #[test]
    fn pane_split_multiple_env_vars_each_get_a_flag() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_split("/repo", &[("A", "1"), ("B", "2")], None, true, None, false);
        assert_eq!(
            command.argv.iter().filter(|part| *part == "--env").count(),
            2
        );
    }

    #[test]
    fn pane_read_targets_a_named_pane_without_agent_resolution() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_read("w1:p2", None, None);
        assert_eq!(&command.argv[..4], &["herdr", "pane", "read", "w1:p2"]);
        assert!(command.argv.contains(&"--source".to_string()));
    }

    #[test]
    fn pane_get_and_rename_target_the_pane_directly() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(pane_get("w1:p2").argv, ["herdr", "pane", "get", "w1:p2"]);
        assert_eq!(
            pane_rename("w1:p2", "scratch-task").argv,
            ["herdr", "pane", "rename", "w1:p2", "scratch-task"]
        );
    }

    #[test]
    fn agent_start_puts_native_args_after_a_double_dash() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_start(
            "x",
            "claude",
            "w1:p2",
            None,
            &["--model".to_string(), "some-model".to_string()],
        );
        let dash = command.argv.iter().position(|part| part == "--").unwrap();
        let model = command
            .argv
            .iter()
            .position(|part| part == "--model")
            .unwrap();
        assert!(dash < model);
    }

    #[test]
    fn codex_native_args_disable_update_on_startup() {
        assert_eq!(
            native_args("gpt-5", "codex", None),
            vec![
                "--model",
                "gpt-5",
                "-c",
                "check_for_update_on_startup=false"
            ]
        );
    }

    #[test]
    fn native_args_applies_the_effort_table_per_kind() {
        assert_eq!(
            native_args("opus", "claude", Some("high")),
            vec!["--model", "opus", "--effort", "high"]
        );
        assert_eq!(
            native_args("grok-4", "grok", Some("low")),
            vec!["--model", "grok-4", "--reasoning-effort", "low"]
        );
        assert_eq!(
            native_args("gpt-5", "codex", Some("high")),
            vec![
                "--model",
                "gpt-5",
                "-c",
                "model_reasoning_effort=high",
                "-c",
                "check_for_update_on_startup=false",
            ]
        );
    }

    #[test]
    fn native_args_with_an_unknown_kind_gets_model_alone() {
        assert_eq!(
            native_args("some-model", "mastracode", Some("high")),
            vec!["--model", "some-model"]
        );
    }

    // --- HerdrCommand -------------------------------------------------

    #[test]
    fn command_stdin_defaults_to_none() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(agent_list().stdin, None);
    }

    #[test]
    fn command_carries_an_explicit_stdin() {
        let mut command = HerdrCommand::new(vec!["curl".into(), "-K".into(), "-".into()]);
        command.stdin = Some("url = \"x\"\n".to_string());
        assert_eq!(command.stdin.as_deref(), Some("url = \"x\"\n"));
    }

    #[test]
    fn command_shell_form_is_quoted() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_prompt("x", "read the brief and go", false, None);
        assert!(command.shell().contains("'read the brief and go'"));
    }

    #[test]
    fn agent_prompt_defaults_to_wait_false() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_prompt("x", "read the brief and go", false, None);
        assert!(!command.argv.contains(&"--wait".to_string()));
    }

    #[test]
    fn agent_send_keys_preserves_targets_and_key_arguments() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_send_keys("builder", &["Enter"]).argv,
            ["herdr", "agent", "send-keys", "builder", "Enter"]
        );
        assert_eq!(
            agent_send_keys("--help", &["Escape", "Enter"]).argv,
            [
                "herdr",
                "agent",
                "send-keys",
                "--",
                "--help",
                "Escape",
                "Enter"
            ]
        );
        assert_eq!(
            agent_send_keys("builder", &["--help"]).argv,
            ["herdr", "agent", "send-keys", "--", "builder", "--help"]
        );
    }

    // --- reconciliation commands --------------------------------------

    #[test]
    fn agent_get_targets_one_agent() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_get("explorer").argv,
            ["herdr", "agent", "get", "explorer"]
        );
    }

    #[test]
    fn agent_list_exists_as_a_cheaper_reconcile() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(agent_list().argv, ["herdr", "agent", "list"]);
    }

    #[test]
    fn agent_wait_builds_repeated_until_flags() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_wait("explorer", &["idle", "done"], Some(1000));
        assert_eq!(
            command.argv,
            [
                "herdr",
                "agent",
                "wait",
                "explorer",
                "--until",
                "idle",
                "--until",
                "done",
                "--timeout",
                "1000"
            ]
        );
    }

    #[test]
    fn agent_wait_without_options_is_bare() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_wait("explorer", &[], None).argv,
            ["herdr", "agent", "wait", "explorer"]
        );
    }

    #[test]
    fn agent_read_defaults_to_visible() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_read("explorer", None, None);
        assert!(command.argv.contains(&"--source".to_string()));
        assert_eq!(command.argv.last(), Some(&"visible".to_string()));
    }

    #[test]
    fn agent_read_takes_a_line_count() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_read("explorer", None, Some(40));
        assert_eq!(&command.argv[command.argv.len() - 2..], &["--lines", "40"]);
    }

    #[test]
    fn pane_close_is_the_teardown_command() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            pane_close("w1Q:p5").argv,
            ["herdr", "pane", "close", "w1Q:p5"]
        );
    }

    // --- workspace / tab ----------------------------------------------

    #[test]
    fn workspace_create_matches_the_measured_invocation() {
        let _guard = HerdrBinPathGuard::unset();
        let command = workspace_create(
            "mv-probe",
            "/tmp/mv-live",
            &[("GROK_HOME", "/home/example/.grok")],
            false,
        );
        assert_eq!(
            &command.argv[..5],
            &["herdr", "workspace", "create", "--label", "mv-probe"]
        );
        assert!(command.argv.contains(&"--cwd".to_string()));
        assert!(command.argv.contains(&"/tmp/mv-live".to_string()));
        assert!(command.argv.contains(&"--env".to_string()));
        assert!(command
            .argv
            .contains(&"GROK_HOME=/home/example/.grok".to_string()));
    }

    #[test]
    fn a_workspace_never_steals_the_cursor_by_default() {
        let _guard = HerdrBinPathGuard::unset();
        assert!(workspace_create("x", "/tmp", &[], false)
            .argv
            .contains(&"--no-focus".to_string()));
        assert!(workspace_create("x", "/tmp", &[], true)
            .argv
            .contains(&"--focus".to_string()));
    }

    #[test]
    fn workspace_create_needs_no_pane_id() {
        let _guard = HerdrBinPathGuard::unset();
        let command = workspace_create("x", "/tmp", &[], false);
        assert!(!command.argv.contains(&"--current".to_string()));
        assert!(!command.argv.iter().any(|part| part.contains("PANE_ID")));
    }

    #[test]
    fn dashboard_tab_helpers_preserve_focus_and_scope() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            tab_list(Some("w1")).argv,
            ["herdr", "tab", "list", "--workspace", "w1"]
        );
        let command = tab_create("w1", "/tmp/project", "dashboard", &[], false);
        assert_eq!(
            command.argv,
            [
                "herdr",
                "tab",
                "create",
                "--workspace",
                "w1",
                "--cwd",
                "/tmp/project",
                "--label",
                "dashboard",
                "--no-focus"
            ]
        );
    }

    #[test]
    fn ensure_dashboard_tab_is_idempotent_and_swappable() {
        let _guard = HerdrBinPathGuard::unset();
        let existing = vec![serde_json::json!({"label": "dashboard"})];
        assert!(ensure_dashboard_tab(&existing, "w1", "/tmp/project", None).is_none());

        let command = ensure_dashboard_tab(&[], "w1", "/tmp/project", None).unwrap();
        assert_eq!(
            command.argv,
            [
                "herdr",
                "tab",
                "create",
                "--workspace",
                "w1",
                "--cwd",
                "/tmp/project",
                "--label",
                "dashboard",
                "--no-focus"
            ]
        );
    }

    // --- pane resize ----------------------------------------------------

    #[test]
    fn pane_resize_builds_the_measured_flags() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_resize("w1Q:p5", "right", 0.1);
        assert_eq!(
            command.argv,
            [
                "herdr",
                "pane",
                "resize",
                "--pane",
                "w1Q:p5",
                "--direction",
                "right",
                "--amount",
                "0.1"
            ]
        );
    }

    #[test]
    fn pane_resize_amount_can_be_negative() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_resize("w1Q:p5", "right", -0.05);
        assert_eq!(command.argv.last(), Some(&"-0.05".to_string()));
    }

    // --- guard / environment --------------------------------------------

    #[test]
    fn guard_script_checks_herdr_env() {
        assert!(guard_script().contains("HERDR_ENV"));
    }

    #[test]
    fn in_herdr_pane_reads_the_given_map() {
        let mut set = std::collections::HashMap::new();
        set.insert("HERDR_ENV".to_string(), "1".to_string());
        assert!(in_herdr_pane(Some(&set)));

        let mut unset = std::collections::HashMap::new();
        unset.insert("HERDR_ENV".to_string(), "0".to_string());
        assert!(!in_herdr_pane(Some(&unset)));

        assert!(!in_herdr_pane(Some(&std::collections::HashMap::new())));
    }

    #[test]
    fn clear_auth_script_unsets_every_variable() {
        assert_eq!(clear_auth_script(&["FOO", "BAR"]), "unset FOO BAR");
    }

    // --- dash-leading titles / briefs must never become flags ----------

    #[test]
    fn a_dash_leading_workspace_title_is_an_option_value_not_a_flag() {
        let _guard = HerdrBinPathGuard::unset();
        for title in ["--help", "-n", "--focus", "--label"] {
            let command = workspace_create(title, "/tmp", &[], false);
            assert!(
                command
                    .argv
                    .iter()
                    .any(|part| part == &format!("--label={title}")),
                "title {title:?} must ride on --label=..., got {:?}",
                command.argv
            );
            assert!(
                !command.argv.iter().any(|part| part == title),
                "title {title:?} must not appear as a bare argv token, got {:?}",
                command.argv
            );
        }
    }

    #[test]
    fn a_newline_in_a_workspace_title_stays_one_argv_element() {
        let _guard = HerdrBinPathGuard::unset();
        let title = "line1\n--injected";
        let command = workspace_create(title, "/tmp", &[], false);
        assert_eq!(
            command
                .argv
                .iter()
                .filter(|part| part.contains('\n'))
                .count(),
            1
        );
        assert!(!command
            .argv
            .iter()
            .any(|part| part.as_str() == "--injected"));
        assert!(command.shell().contains("'line1\n--injected'"));
    }

    #[test]
    fn a_dash_leading_tab_title_is_an_option_value_not_a_flag() {
        let _guard = HerdrBinPathGuard::unset();
        let command = tab_create("w1", "/tmp", "--help", &[], false);
        assert!(command.argv.iter().any(|part| part == "--label=--help"));
        assert!(!command.argv.iter().any(|part| part == "--help"));
    }

    #[test]
    fn a_dash_leading_pane_label_is_protected_by_end_of_options() {
        let _guard = HerdrBinPathGuard::unset();
        let command = pane_rename("w1:p2", "--focus");
        assert_eq!(
            command.argv,
            ["herdr", "pane", "rename", "--", "w1:p2", "--focus"]
        );
    }

    #[test]
    fn an_ordinary_pane_rename_does_not_insert_end_of_options() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            pane_rename("w1:p2", "scratch-task").argv,
            ["herdr", "pane", "rename", "w1:p2", "scratch-task"]
        );
    }

    #[test]
    fn a_dash_leading_rename_target_or_name_is_protected_by_end_of_options() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_rename("--help", "n").argv,
            ["herdr", "agent", "rename", "--", "--help", "n"]
        );
        assert_eq!(
            agent_rename("w1:p2", "--kind").argv,
            ["herdr", "agent", "rename", "--", "w1:p2", "--kind"]
        );
    }

    #[test]
    fn an_ordinary_agent_rename_does_not_insert_end_of_options() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_rename("w1:p2", "sandbox-nexus").argv,
            ["herdr", "agent", "rename", "w1:p2", "sandbox-nexus"]
        );
    }

    #[test]
    fn a_dash_leading_agent_name_cannot_be_parsed_as_a_flag() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_start("--kind", "claude", "w1:p2", None, &[]);
        assert_eq!(
            command.argv,
            ["herdr", "agent", "start", "--kind", "claude", "--pane", "w1:p2", "--", "--kind"]
        );
    }

    #[test]
    fn a_dash_leading_agent_name_keeps_native_args_after_the_same_end_of_options() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_start(
            "--help",
            "claude",
            "w1:p2",
            Some(1000),
            &["--model".to_string(), "some-model".to_string()],
        );
        assert_eq!(
            command.argv,
            [
                "herdr",
                "agent",
                "start",
                "--kind",
                "claude",
                "--pane",
                "w1:p2",
                "--timeout",
                "1000",
                "--",
                "--help",
                "--model",
                "some-model"
            ]
        );
    }

    #[test]
    fn an_ordinary_agent_start_keeps_the_measured_shape() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_start("sandbox-nexus", "claude", "w1:p2", None, &[]).argv,
            [
                "herdr",
                "agent",
                "start",
                "sandbox-nexus",
                "--kind",
                "claude",
                "--pane",
                "w1:p2"
            ]
        );
    }

    #[test]
    fn a_dash_leading_brief_cannot_be_parsed_as_a_prompt_flag() {
        let _guard = HerdrBinPathGuard::unset();
        let command = agent_prompt("x", "--wait", false, None);
        assert_eq!(
            command.argv,
            ["herdr", "agent", "prompt", "--", "x", "--wait"]
        );
        let waited = agent_prompt("x", "--help", true, Some(1000));
        assert_eq!(
            waited.argv,
            [
                "herdr",
                "agent",
                "prompt",
                "--wait",
                "--timeout",
                "1000",
                "--",
                "x",
                "--help"
            ]
        );
    }

    #[test]
    fn an_ordinary_brief_keeps_the_measured_prompt_shape() {
        let _guard = HerdrBinPathGuard::unset();
        assert_eq!(
            agent_prompt("sandbox-nexus", "Read the brief.", false, None).argv,
            [
                "herdr",
                "agent",
                "prompt",
                "sandbox-nexus",
                "Read the brief."
            ]
        );
    }

    #[test]
    fn unicode_names_and_briefs_pass_through_argv_unaltered() {
        let _guard = HerdrBinPathGuard::unset();
        let name = "scout-日本語";
        let brief = "read «brief.md» — café";
        let command = agent_prompt(name, brief, false, None);
        assert_eq!(command.argv[3], name);
        assert_eq!(command.argv[4], brief);
        let renamed = pane_rename("w1:p2", "scratch-é");
        assert_eq!(renamed.argv.last().map(String::as_str), Some("scratch-é"));
    }
}

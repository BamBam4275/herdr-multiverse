//! `multiverse dispatch`: scope and run a task on a worker seat.
//!
//! The real planner/selector live in `mv-engine::{dispatch, selector}`;
//! [`RealEngine`] is the glue that reads `mv-config`'s `config.toml`,
//! builds the chain `mv-engine::selector::Seat`s it names, walks it
//! through `mv_engine::dispatch::plan_dispatch`, and renders the result
//! into the exact card/refusal text the conformance scenarios pin.
//! [`RealEngine::dispatch`] (task `20261102-fixer-393efb`) is the live
//! `--yes` path: it re-resolves the same plan through
//! [`RealEngine::resolve`] and hands it to a [`RealDispatchStore`] —
//! the production `mv_engine::runner::DispatchStore` over `mv-state`
//! (worktrees, routing log, task dirs) and `mv_engine::brief` — which
//! `mv_engine::runner::dispatch` then drives against the real
//! `mv_engine::runner::ProcessHerdr`. See `RealDispatchStore`'s own doc
//! comment and this task's `result.md` for what is, and is not, ported to
//! full Python parity. Agent names use the shared 32-character naming
//! budget; task-id collision rebind, including the predecessor supersede
//! a collision retry runs (card `20261105-b-28cb36`), is ported short of
//! Python's own race-narrowing second dirty check immediately before the
//! predecessor's pane close (`runner.py:330-369`) — see this task's
//! `result.md`.
//!
//! Review IDs are resolved against this project's routing log and forwarded
//! to the planner's author-vendor exclusion rule. The dry-run includes the
//! prompt, brief, environment, predicted paths and launch commands; `--full`
//! includes role/stage prompt bodies. Preview and launch share brief
//! construction and title derivation. New agent names use the current role
//! codes and the shared naming budget, including collision instances.
//!
//! One deliberate scope choice remains, made because this lane's
//! conformance group (`dispatch-*`) replays scenarios recorded from
//! Python, which never had the role rename:
//!
//! - **Config reading accepts the retired role keys.** `mv_config::load`
//!   already refuses a `config.toml` with `[roles.fixer]`/`fixer = […]`
//!   outright ("run the config migrator"), and nothing in this checkout
//!   calls `mv_config::migrate::migrate_role_keys` yet. [`load_config`]
//!   calls it itself, in memory only (never writing the rewritten
//!   document back to disk) — a reader's job, not a writer's, consistent
//!   with this project's "history readers still accept old on-disk names"
//!   rule. Every scenario's `config.toml` uses the retired keys
//!   (`explorer`, `fixer`), so this is required for any of them to load
//!   at all.
//!
//! **The `--dry-run` card/refusal text and a real dispatch now render the
//! same, current name** (`role        Scout`, "every account in the
//! Builder chain…", `Role::display_name()`/`Role::config_key()`) — release
//! policy (operator decision, task `20261103-fixer-fa9c70`): no aliases,
//! and old names are read from on-disk history and translated, never
//! written or shown in new output, dry-run included. [`RealEngine::resolve`]
//! no longer branches on whether it is resolving for a preview or a real
//! launch; the task id it mints, the predicted and real agent name
//! ([`role_short`]), the routing log's `role=` field
//! ([`RealDispatchStore::record_dispatch`]), the dispatch reason, and
//! every rendered card/refusal string all use the role's current name on
//! both paths. This used to differ — the dry-run card rendered Python's
//! retired, capitalized name byte-for-byte via a `legacy_display_name`
//! helper this task removed — because the `dispatch-*` conformance
//! scenarios were recorded against that literal old-name text; the fix
//! moves that bridge to replay time instead:
//! `conformance/role_translate.translate_expect_text` now rewrites the
//! handful of fixed positions Rust's own card/refusal renderer produces a
//! role word from (never a history read) old->new before a scenario's
//! recorded `expect` text is compared against Rust's output — see that
//! module's doc comment and `conformance/README.md`'s "Role rename"
//! section. History readers (`mv_state`, `commands::tasks`) already accept
//! both spellings and are unaffected either way.
//!
//! See this task's `result.md` for why the remaining config-reading choice
//! stays scoped to this file, not fixed at the source (`mv-config::load`/
//! `roles`). `commands::accounts`'s
//! `limit`/`lend` write paths (`mv_engine::seatstate::set_cap`/`set_lend`)
//! now cover `dispatch-breaker-open`, `dispatch-capacity-cap`, and
//! `dispatch-lent-off-fallback` — the trip/limit/lend state they each set
//! up now actually lands in `seat_from_account`'s reads. The remaining
//! `dispatch-*` scenario this group does not reach needs `--role fixer`,
//! refused by `commands::mod`'s `WORKER_ROLES` check before this file sees
//! the request.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use clap::ArgMatches;
use sha2::{Digest, Sha256};

use mv_config::load::{
    self, Account, Config, FallbackPolicy as ConfigFallbackPolicy, Project, RoleSettings,
};
use mv_config::roles::Role;
use mv_core::redact::redact_text;
use mv_engine::breaker;
use mv_engine::dispatch as engine_dispatch;
use mv_engine::limits;
use mv_engine::mv_herdr::{
    argv,
    envelope::{AgentState, CommandResult},
};
use mv_engine::runner::{self, DispatchStore, Herdr as _, PreparedLaunch, RunnerError};
use mv_engine::selector::{self, LocalLivenessProbe as _, Seat};

use crate::exit::Failure;

/// Python's `AUTH_VARS_TO_CLEAR` (`config.py:297-300`): auth tokens the
/// orchestrator may have inherited that would otherwise shadow a worker's
/// own subscription login. Only relevant for a non-command-wrapper launch
/// (Claude always goes through [`PreparedLaunch::command`] instead); kept
/// local to this file rather than added to `mv-config`, which is out of
/// this task's write scope.
const AUTH_VARS_TO_CLEAR: &[&str] = &[
    "CLAUDE_CODE_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
];

/// Where `RealDispatchStore::lock_fleet` keeps the fleet-wide dispatch
/// lock, beside the global config/state dir — Python's
/// `FLEET_LOCK_FILENAME` (`runner.py:1223`).
const FLEET_LOCK_FILENAME: &str = "dispatch.lock";

/// Bound on how long `[projects.<name>].lane_setup` may run before
/// `setup_lane` kills it and fails the dispatch — critic finding 8: this
/// used to have no bound at all, so a hung `npm ci` blocked a dispatch
/// forever with a live pane. Python has its own `LANE_SETUP_TIMEOUT_SECONDS`
/// constant (not available to read against for this task; AGENTS.md
/// restricts the frozen Python reference to tasks that explicitly permit
/// it); this value is this port's own choice, not a ported byte-parity
/// constant.
const LANE_SETUP_TIMEOUT: Duration = Duration::from_secs(300);

/// Everything this command needs from the dispatch planner/runner, kept
/// narrow and local rather than reaching into `mv-engine`'s stub modules.
pub trait Engine {
    fn plan(&self, request: &Request) -> Result<Plan, EngineError>;
    fn dispatch(&self, request: &Request, plan: Plan) -> Result<Plan, EngineError>;
}

/// The caller-facing request, built from the flags this lane owns:
/// `--role`, `--into`, `--input`, `--scope`, `--exclude-vendor`, `--force`,
/// `--title`, `--reviewing`, `--full`, `--column`, `--timeout`,
/// `--wait-for-seat`, `--wait-for-seat-seconds`, `--dry-run`, `--commits`.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub project_path: String,
    pub task: String,
    pub role: Option<String>,
    pub into: Option<String>,
    pub inputs: Vec<String>,
    pub scope: Vec<String>,
    pub exclude_vendor: Vec<String>,
    pub reviewing: Vec<String>,
    /// Repeatable `--commits SHA`, oldest first. Only applied with exactly
    /// one `--reviewing` id; otherwise ignored after a warning.
    pub commits: Vec<String>,
    pub full: bool,
    pub column: Option<String>,
    pub timeout_seconds: f64,
    pub force: bool,
    pub title: Option<String>,
    pub wait_for_seat: bool,
    pub wait_for_seat_seconds: Option<f64>,
    /// `--wait`: block for the dispatched agent to settle with a usable
    /// result before returning, instead of reporting where to look and
    /// returning immediately — Python `cli.py:3791-3864`. Distinct from
    /// `wait_for_seat`, which blocks *before* a pane exists for a free
    /// seat; this blocks *after* dispatch, for the agent's own turn.
    pub wait: bool,
    pub dry_run: bool,
    /// `--no-reconcile`: skip the pre-admission fleet-wide convergence pass
    /// (reconcile the dispatching project's own settled-but-unrecorded
    /// tasks, reclaim every configured project's stale idle/done panes) —
    /// Python `cli.py:3539,3452-3543`.
    pub no_reconcile: bool,
    /// The global `--config PATH` override, threaded through so
    /// [`load_config`] resolves the same path every other subcommand does
    /// (`commands::mod::config_override`) instead of only ever honoring
    /// `$MULTIVERSE_CONFIG`/the default (smoke finding 2).
    pub config_override: Option<String>,
}

/// A planned dispatch, rendered by `--dry-run` and, once started, returned
/// again as the outcome of a real dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub role: String,
    pub seat: String,
    pub task_id: String,
    /// The full `--dry-run` report, pre-rendered by [`RealEngine::plan`]
    /// (or a fake, in tests) — `run` prints it verbatim rather than
    /// re-deriving a summary from `role`/`seat`/`task_id`.
    pub dry_run_report: String,
    /// "reconciled"/"reclaimed"/"superseded"/"lease-expired" lines from the
    /// pre-admission fleet convergence pass, pre-formatted (each already
    /// starts with its own two-space indent) and printed before the
    /// `Dispatched …` announcement — Python `cli.py:3547-3692`. Always
    /// empty under `--no-reconcile` or for a council seat.
    pub pre_dispatch_notes: Vec<String>,
    /// A writer's worktree path, set only by a real (non-dry-run)
    /// [`RealEngine::dispatch`] — `None` for the `--dry-run` preview and
    /// for a read-only role, which has no lane. Python `cli.py:3776-3780`
    /// prints this and `branch` unconditionally, not only under `--wait`.
    pub worktree: Option<String>,
    pub branch: Option<String>,
    /// Backlog ids the goal named and this dispatch linked/failed to link —
    /// Python's `_record_dispatch_backlog_links` (`cli.py:14118-14144`),
    /// called only at a real dispatch. Always empty on the `--dry-run`
    /// preview, which creates nothing to link.
    pub linked_backlog: Vec<String>,
    pub backlog_link_failures: Vec<(String, String)>,
    /// The selected account's vendor and model, for the `Dispatched … (harness/model)`
    /// announcement — Python `cli.py:3759-3764`. Unused (empty) on the
    /// `--dry-run` preview, which prints `dry_run_report` instead.
    pub harness: String,
    pub model: String,
    /// The pane's working directory: a writer's own lane, or the shared
    /// checkout for a read-only role — Python's `card.cwd`, printed as
    /// `Dispatched … in {cwd}`.
    pub cwd: String,
    /// The started agent's name and the pane it runs in — Python's
    /// `live.card.agent`/`live.pane_id` (`cli.py:3764`).
    pub agent: String,
    pub pane: String,
    /// Where the task's `status` file lives, printed only when `--wait`
    /// was not given — Python's `paths_in(card, ...).status`
    /// (`cli.py:3780-3781`).
    pub status_path: String,
    /// Set only when `--wait` was given and a real dispatch started: the
    /// settled turn's own terminal status, or `None` when the agent settled
    /// without ever writing a usable `status`/`result.md` pair — Python's
    /// `live.status is None` branch (`cli.py:3793-3806`). `result_path` is
    /// always the path `result.md` would be at, named in the "no status"
    /// message either way.
    pub wait_outcome: Option<WaitOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitOutcome {
    pub status: Option<mv_state::tasks::TaskStatus>,
    /// Raw Herdr `agent_status` at settle — Python's `live.settled.state`
    /// (`cli.py:3792`). `None` when `wait_for_result` timed out before a
    /// settled snapshot. Both fields, because they answer different
    /// questions and can disagree: `settled_state` is what Herdr saw,
    /// `status` is what the agent wrote.
    pub settled_state: Option<AgentState>,
    pub result_path: String,
    pub status_path: String,
    /// Q15: what a read-only role changed in the shared checkout during its
    /// turn, that it should not have — always empty for a writer, which has
    /// its own worktree instead. Python `runner.py:1104-1131`.
    pub scope_violations: Vec<String>,
    /// Q21: artifacts this task's own `revise` file asks someone to
    /// change — Python's `read_revisions` (`runner.py:1136-1153`). Always
    /// empty when the task wrote nothing there, which is the normal case.
    pub revisions: Vec<String>,
    /// A writer's own lane: its porcelain status (`Some("")` clean, `Some`
    /// non-empty dirty, `None` unreadable) and the handback instructions —
    /// Python's `worktrees.uncommitted_changes`/`handback`
    /// (`worktrees.py:2022-2109`). `None` for a read-only role, which has
    /// no lane of its own.
    pub lane_handback: Option<LaneHandback>,
}

/// A writer's own lane, reported only once `--wait` has settled —
/// `dirty` is the lane's own porcelain status (`None` when git could not
/// be read, never read as clean), `handback` is Python's own `handback`
/// instruction lines (`worktrees.py:2022-2070`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneHandback {
    pub dirty: Option<String>,
    pub handback: Vec<String>,
}

// `Unimplemented` only ever returns `Refused`; the rest of this surface is
// real but unreachable until a real `Engine` lands, same as `exit::Failure`.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// The fleet halt is active and `--force` was not given. Carries the
    /// full refusal line (`DispatchError::FleetHaltActive`'s `Display`) so
    /// `run` can recognize this specific refusal and skip the warnings
    /// Python never reaches once it has already exited on the halt check
    /// (`cli.py:3277-3283`, before `planner.plan` or either warning print).
    FleetHalted(String),
    /// Project/config could not be resolved — `EXIT_NEEDS_CONFIG`, prefixed
    /// with "config error:" (a malformed/missing `config.toml`, not a
    /// refusal whose wording already stands on its own).
    ProjectResolution(String),
    /// `EXIT_NEEDS_CONFIG`, with no prefix added: project-path resolution,
    /// an unconfigured chain, or a `ChainExhaustedError` whose message is
    /// already complete on its own.
    NeedsConfig(String),
    /// Everything else planning or the runner can refuse — `EXIT_ERROR`:
    /// unresolved placeholders, invalid role/selection, exhausted chains,
    /// a missing Claude wrapper, an unsafe `--into`, an invalid reviewer
    /// target, or a runner-side dispatch failure.
    Refused(String),
    /// `role` exists but phase 1 never dispatches it as a task — only
    /// `council` today. Python's `RoleNotImplementedError`
    /// (`roles.py:275-306`), mapped the same way: `EXIT_ERROR`, not
    /// `EXIT_NEEDS_CONFIG` — the role is spelled correctly, it is simply
    /// not a thing `dispatch` can start.
    RoleNotImplemented(String),
}

fn to_failure(error: EngineError) -> Failure {
    match error {
        EngineError::FleetHalted(message) => Failure::Operation(message),
        EngineError::ProjectResolution(message) => Failure::Config(message),
        EngineError::NeedsConfig(message) => Failure::AccountSelection(message),
        EngineError::Refused(message) => Failure::Operation(message),
        EngineError::RoleNotImplemented(message) => Failure::RoleNotImplemented(message),
    }
}

fn commits_warning(commits: &[String], reviewing: &[String]) -> Option<String> {
    if commits.is_empty() || reviewing.len() == 1 {
        None
    } else {
        Some(format!(
            "warning: --commits only applies with exactly one --reviewing id ({} given) — ignored.",
            reviewing.len()
        ))
    }
}

/// Python `cli.py:3361-3368`: Critic without `--reviewing`/`--exclude-vendor`
/// still plans (and then refuses), but the missing author-vendor is made
/// visible first. Found via `conformance/diff_fuzz.py` seed 9000.
fn critic_vendor_warning(
    role: Option<&str>,
    reviewing: &[String],
    exclude_vendor: &[String],
) -> Option<String> {
    let role = role?;
    if role != "critic" && role != "reviewer" {
        return None;
    }
    if !reviewing.is_empty() || !exclude_vendor.is_empty() {
        return None;
    }
    Some(
        "warning: dispatching Critic without --reviewing or --exclude-vendor — no author vendor was supplied, so vendor exclusion is not applied and review may land on the author's own vendor.".to_owned(),
    )
}

/// Basenames cited as `` `.multiverse/input/<name>` `` in `text` — Python's
/// `_cited_inputs`/`_CITED_INPUT_RE` (`cli.py:310-325`). Only a path inside a
/// single Markdown code span counts, same as Python: a bare mention
/// explaining the mechanism (this file's own doc comments, or a brief
/// describing the trap) must not trip the refusal. `name` may not contain a
/// further `/` or whitespace, mirroring the flat, basename-keyed
/// `.multiverse/input/` layout `--input` actually copies into.
fn cited_inputs(text: &str) -> Vec<String> {
    const PREFIX: &str = "`.multiverse/input/";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(PREFIX) {
        let after_prefix = &rest[start + PREFIX.len()..];
        let Some(end) = after_prefix.find('`') else {
            break;
        };
        let name = &after_prefix[..end];
        if !name.is_empty() && !name.contains(['/', ' ', '\t', '\n', '\r']) {
            out.push(name.to_string());
        }
        rest = &after_prefix[end + 1..];
    }
    out
}

/// Refuses a brief that cites `` `.multiverse/input/<name>` `` with nothing
/// that will actually land there — Python's `CitedInputMissingError`/
/// `_check_cited_inputs` (`cli.py:295-381`, backlog card `20260914-b-811e56`:
/// three real dispatches cited such a path with nothing behind it). Called
/// before anything is created, same as Python's placement ahead of
/// `DispatchPlanner.plan`.
///
/// A writing role gets its own worktree, and only the basenames actually
/// passed to `--input` land inside it — `inputs` here are those raw
/// `--input` values, not yet validated to exist (that happens separately,
/// in [`render_card`]/[`RealDispatchStore::prepare`]); a cited name not
/// among their basenames is "cited but not staged" regardless. A read-only
/// role has no worktree and runs in the shared checkout at `project_root`
/// directly; a cited name is only good there if a file with that basename
/// already exists on disk under `project_root/.multiverse/input/`.
fn check_cited_inputs(
    task: &str,
    role_display: &str,
    project_root: &Path,
    write_scope: &str,
    inputs: &[String],
) -> Result<(), EngineError> {
    let cited = cited_inputs(task);
    if cited.is_empty() {
        return Ok(());
    }
    if write_scope != "none" {
        let staged: BTreeSet<&str> = inputs
            .iter()
            .filter_map(|raw| Path::new(raw).file_name().and_then(|n| n.to_str()))
            .collect();
        for name in &cited {
            if !staged.contains(name.as_str()) {
                let staged_list = if staged.is_empty() {
                    "none".to_string()
                } else {
                    staged.iter().copied().collect::<Vec<_>>().join(", ")
                };
                return Err(EngineError::Refused(format!(
                    "the brief cites `.multiverse/input/{name}`, but no --input landed at that basename (staged: {staged_list}). --input copies under the source file's own basename — pass --input <a path ending in {name}>, or drop the citation."
                )));
            }
        }
    } else {
        let shared_dir = project_root.join(".multiverse").join("input");
        for name in &cited {
            let candidate = shared_dir.join(name);
            if !candidate.is_file() {
                return Err(EngineError::Refused(format!(
                    "the brief cites `.multiverse/input/{name}`, but {role_display} is read-only and runs in the shared checkout at {} — {} does not exist there. A file staged into a writing role's own worktree is invisible to a read-only role; put the file at that path in the shared checkout instead, or drop the citation.",
                    project_root.display(),
                    candidate.display()
                )));
            }
        }
    }
    Ok(())
}

/// Python `cli.py:3308-3334`: `--input` only ever lands in a writer's own
/// worktree, so it is meaningless for a read-only role, which already runs
/// in the shared checkout where every path it would name is one it can open
/// directly — refused, the louder and checkable answer. `--scope` is
/// likewise rendered only in a writer's brief (`Brief.render`'s `## Write
/// scope` section), so it is not refused — nothing fails to copy — but
/// warned: the paths named never reach the agent. Returns the warning text
/// rather than printing it, so the caller ([`RealEngine::plan`]) can print
/// it exactly once even though [`RealEngine::resolve`] itself runs twice on
/// a real (non-dry-run) dispatch.
fn check_read_only_flags(
    request: &Request,
    role_display: &str,
    write_scope: &str,
) -> Result<Option<String>, EngineError> {
    if write_scope != "none" {
        return Ok(None);
    }
    if !request.inputs.is_empty() {
        return Err(EngineError::Refused(format!(
            "--input is for a writing role; {role_display} is read-only and runs in the shared checkout, where it can already open any of these paths directly. Drop --input, or dispatch a writing role."
        )));
    }
    if request.scope.is_empty() {
        return Ok(None);
    }
    let noun = if request.scope.len() == 1 {
        "this path"
    } else {
        "these paths"
    };
    Ok(Some(format!(
        "warning: --scope is for a writing role; {role_display} is read-only, so its brief never renders a Write scope section and {noun} will not reach the agent. Drop --scope, or dispatch a writing role."
    )))
}

/// The one shape a minted backlog item id ever has (`backlog._mint_item_id`
/// / `cli.py:14047-14055`'s own `_ITEM_ID_SHAPE_RE`): an 8-digit date,
/// `-b-`, six lowercase hex digits.
fn is_backlog_item_id_shape(s: &str) -> bool {
    // Indexed as bytes throughout, never sliced as `str`, so an input with
    // multi-byte characters can never land this on a non-char-boundary split.
    let bytes = s.as_bytes();
    bytes.len() == 17
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && &bytes[8..11] == b"-b-"
        && bytes[11..]
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
}

/// Backlog ids `goal` names — Python's `_ITEM_ID_SHAPE_RE.findall(goal)`
/// (`cli.py:14123`). That regex is compiled **without** `re.MULTILINE`, so
/// `^`/`$` anchor to the start/end of the whole string (allowing at most one
/// trailing newline), not to any embedded line — against free-form goal
/// prose, `findall` therefore only ever matches when the *entire* goal is a
/// single backlog id, never when one is merely mentioned inside it.
/// Confirmed directly against the frozen reference rather than assumed, and
/// ported as this exact quirk rather than the more generally useful "find
/// any id anywhere in the text": this task's charter is behavioral parity,
/// not improving on it.
fn item_id_candidates(goal: &str) -> Vec<String> {
    let trimmed = goal.strip_suffix('\n').unwrap_or(goal);
    if is_backlog_item_id_shape(trimmed) {
        vec![trimmed.to_string()]
    } else {
        Vec::new()
    }
}

/// Links every known backlog id named in a dispatch brief's goal — Python's
/// `_record_dispatch_backlog_links` (`cli.py:14118-14144`). Called only at a
/// real (non-dry-run) dispatch, after the agent has actually started
/// (mirroring `record_reviewed`'s own "only once a slot is granted" rule —
/// a dry-run preview creates nothing to link).
fn record_dispatch_backlog_links(
    project_root: &Path,
    goal: &str,
    task_id: &str,
) -> (Vec<String>, Vec<(String, String)>) {
    let candidates = item_id_candidates(goal);
    if candidates.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let items = mv_state::backlog::read_items(project_root, false);
    let known: HashSet<&str> = items.iter().map(|item| item.item.as_str()).collect();
    let mut linked = Vec::new();
    let mut failures = Vec::new();
    for item_id in candidates {
        if !known.contains(item_id.as_str()) {
            continue;
        }
        match mv_state::backlog::link_task(project_root, &item_id, task_id, None) {
            Ok(true) => linked.push(item_id),
            Ok(false) => {}
            Err(error) => failures.push((item_id, error.to_string())),
        }
    }
    (linked, failures)
}

/// What `git status --porcelain` says about `root`, as a set of lines — port
/// of Python's `worktrees.tree_state` (`worktrees.py:2658-2675`). `None`
/// when git cannot answer (not a repository, or unreadable), which the
/// caller must treat as "could not tell", never as "nothing changed" —
/// reading it as clean would report every read-only dispatch into an
/// unreadable tree as well-behaved. A small, self-contained duplicate of
/// Python's plumbing (a single `git status --porcelain` call, nothing
/// lane-specific) rather than a new `mv-state::worktrees` export, the same
/// choice `mv_engine::lane_diff` already made for its own minimal git
/// reads — see that module's doc comment.
fn tree_state(root: &Path) -> Option<HashSet<String>> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect(),
    )
}

/// What a read-only role changed that it should not have (Q15) — Python's
/// `runner.scope_violations` (`runner.py:1104-1131`). Reported, not
/// prevented: a before/after `tree_state` diff works identically across
/// every vendor and cannot wrongly block real work, at the cost of only
/// catching the violation after the fact. `None` on either side (an
/// unreadable tree) yields nothing rather than a false all-clear.
fn scope_violations(
    write_scope: &str,
    before: Option<&HashSet<String>>,
    after: Option<&HashSet<String>>,
) -> Vec<String> {
    if write_scope != "none" {
        return Vec::new();
    }
    let (Some(before), Some(after)) = (before, after) else {
        return Vec::new();
    };
    let mut violations: Vec<String> = after.difference(before).cloned().collect();
    violations.sort();
    violations
}

/// Q21: artifacts `task_dir`'s own `revise` file asks someone to change —
/// Python's `read_revisions` (`runner.py:1136-1153`). Absence is the
/// normal case (empty, not an error); blank lines are dropped and the
/// agent's own order is kept. Nothing here validates that a named path
/// exists — the agent names what it read, possibly in a lane this
/// dispatch cannot see, and inventing a check that rejected those would
/// silently drop exactly the findings worth keeping.
/// Scout-gaps #27: whether a just-started dispatch should advance the
/// diff-scope "last reviewed" marker — Python's own gate
/// (`runner.py:2706-2712`): a Critic, dispatched `--reviewing` at least one
/// lane, under `[review_policy] scope = "diff"`. A `full`-scope policy (or
/// any other role) has nothing to mark — the next round would always see
/// the whole history anyway.
fn should_record_reviewed(role: Role, reviewing: &[String], review_scope: &str) -> bool {
    role == Role::Critic && !reviewing.is_empty() && review_scope == "diff"
}

fn read_revisions(task_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(task_dir.join("revise")) else {
        return Vec::new();
    };
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect()
}

fn many(args: &ArgMatches, name: &str) -> Vec<String> {
    args.get_many::<String>(name)
        .map(|values| values.map(String::from).collect())
        .unwrap_or_default()
}

fn one(args: &ArgMatches, name: &str) -> Option<String> {
    args.get_one::<String>(name).cloned()
}

fn request_from(args: &ArgMatches) -> Request {
    Request {
        project_path: one(args, "path").unwrap_or_default(),
        task: one(args, "task").unwrap_or_default(),
        role: one(args, "role"),
        into: one(args, "into"),
        inputs: many(args, "input"),
        scope: many(args, "scope"),
        exclude_vendor: many(args, "exclude-vendor"),
        reviewing: many(args, "reviewing"),
        commits: many(args, "commits"),
        full: args.get_flag("full"),
        column: one(args, "column"),
        timeout_seconds: one(args, "timeout")
            .and_then(|s| s.parse().ok())
            .unwrap_or(900.0),
        force: args.get_flag("force"),
        title: one(args, "title"),
        wait_for_seat: args.get_flag("wait-for-seat"),
        wait_for_seat_seconds: one(args, "wait-for-seat-seconds")
            .and_then(|raw| raw.parse::<f64>().ok()),
        wait: args.get_flag("wait"),
        dry_run: args.get_flag("dry-run"),
        no_reconcile: args.get_flag("no-reconcile"),
        config_override: one(args, "config"),
    }
}

/// Run `dispatch` against any [`Engine`]. `commands::run`'s `dispatch` match
/// arm calls [`run_default`], which binds [`RealEngine`].
pub fn run(args: &ArgMatches, engine: &dyn Engine, out: &mut dyn Write) -> Result<(), Failure> {
    let request = request_from(args);
    let dry_run = request.dry_run;
    let planned = engine.plan(&request);
    // Python checks the fleet halt first and exits immediately
    // (`cli.py:3277-3283`), before either warning below is ever printed —
    // reaching them at all already means this dispatch was not halted.
    // `commands::dispatch::EngineError::FleetHalted` carries that same
    // refusal, so a halted fleet short-circuits here too, with neither
    // warning printed first (conformance seeds 50017/50021/50022/60026
    // caught the warning leaking ahead of the refusal).
    if let Err(EngineError::FleetHalted(_)) = &planned {
        return Err(to_failure(planned.unwrap_err()));
    }
    if let Some(message) = commits_warning(&request.commits, &request.reviewing) {
        eprintln!("{message}");
    }
    if let Some(message) = critic_vendor_warning(
        request.role.as_deref(),
        &request.reviewing,
        &request.exclude_vendor,
    ) {
        eprintln!("{message}");
    }
    let plan = planned.map_err(to_failure)?;
    if dry_run {
        // `render_card`'s own report already ends in a single `\n`; Python's
        // `print(render_dry_run(...))` adds a second one on top of its own
        // trailing blank line, so the dry-run card always ends with a blank
        // line on screen (conformance seeds 9001-9004).
        let _ = writeln!(out, "{}", plan.dry_run_report);
        return Ok(());
    }
    let outcome = engine.dispatch(&request, plan).map_err(to_failure)?;
    // Python `cli.py:3547-3692`: the pre-admission convergence pass runs,
    // and prints, before `_run_dispatch`'s own announcement.
    for note in &outcome.pre_dispatch_notes {
        let _ = writeln!(out, "{note}");
    }
    // Python `cli.py:3759-3764`: announced only once the runner granted a
    // slot. A refused dispatch never reaches here, so this never shows a
    // task-shaped line with no pane behind it.
    let _ = writeln!(
        out,
        "Dispatched {} on {} ({}/{}) in {}",
        outcome.role, outcome.seat, outcome.harness, outcome.model, outcome.cwd
    );
    let _ = writeln!(out, "  agent {} in pane {}", outcome.agent, outcome.pane);
    // Python `cli.py:3759-3764`: linked only once the runner granted a slot,
    // printed right after the dispatch announcement.
    for item_id in &outcome.linked_backlog {
        let _ = writeln!(
            out,
            "  backlog {item_id} linked to task {}",
            outcome.task_id
        );
    }
    for (item_id, failure) in &outcome.backlog_link_failures {
        let _ = writeln!(
            out,
            "  backlog {item_id} link failed ({failure}); follow-up remains"
        );
    }
    // Python `cli.py:3778-3780`: printed unconditionally, not only under
    // `--wait` — a writer's output that nobody can find is the same as no
    // output.
    if let (Some(worktree), Some(branch)) = (&outcome.worktree, &outcome.branch) {
        let _ = writeln!(out, "  worktree {worktree}");
        let _ = writeln!(out, "  branch   {branch}");
    }
    if outcome.wait_outcome.is_none() {
        // Python `cli.py:3781-3785`: the prompt has landed and the agent is
        // running; real work takes 30-70 minutes, and blocking here gains
        // nothing the card, worktree and brief did not already give the
        // caller. Report where to look instead of waiting to see what
        // happens.
        let _ = writeln!(out, "  task {}", outcome.task_id);
        let _ = writeln!(out, "  status file {}", outcome.status_path);
        return Ok(());
    }
    // Python `cli.py:3791-3864`: only entered under `--wait`. Everything
    // this prints goes to `out`, same as Python's own `print(..., file=out)`
    // calls here — a non-DONE/no-status outcome is reported the same way
    // `Failure::Reported` documents: already written to stdout, only the
    // exit code still needs to cross the boundary.
    if let Some(wait) = &outcome.wait_outcome {
        // Python `cli.py:3792`: printed before the status-file check, from
        // the snapshot `wait_for_turn` returned. Absent when the wait
        // timed out without a settled snapshot.
        if let Some(state) = wait.settled_state {
            let _ = writeln!(out, "  settled as {state}");
        }
        match wait.status {
            None => {
                let _ = writeln!(
                    out,
                    "  no status file at {}\n  The agent settled without recording an outcome. \
                     That is either a crash or a protocol miss; the pane may still be open — \
                     read it before drawing a conclusion.",
                    wait.status_path
                );
                return Err(Failure::Reported(crate::exit::ERROR));
            }
            Some(status) => {
                let _ = writeln!(out, "  status {status}");
                // Q15, Python `cli.py:3808-3821`: louder than the revision
                // notice below it — a breach of the contract the brief
                // stated, not a suggestion. Reported, never reverted.
                if !wait.scope_violations.is_empty() {
                    let _ = writeln!(
                        out,
                        "\n  SCOPE VIOLATION — {} thing(s) changed in the shared checkout during this read-only turn:\n{}\n  Nothing was reverted. Check them before anything else commits them.",
                        wait.scope_violations.len(),
                        wait.scope_violations
                            .iter()
                            .map(|line| format!("    {line}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                // Q21, Python `cli.py:3836-3846`: a reviewer saying "right,
                // except for one sentence" is neither pass nor fail — this
                // does not act on it, only stops the finding being
                // invisible.
                if !wait.revisions.is_empty() {
                    let _ = writeln!(
                        out,
                        "\n  THIS RESULT ASKS FOR {} CHANGE(S):\n{}\n  Nothing here has acted on that — read the result and decide.",
                        wait.revisions.len(),
                        wait.revisions
                            .iter()
                            .map(|path| format!("    {path}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    );
                }
                // Python `cli.py:3848-3864`: §8 leaves the merge to a human
                // on purpose, but not knowing *how* is not the same as
                // choosing not to.
                if let Some(lane) = &wait.lane_handback {
                    if let Some(dirty) = &lane.dirty {
                        if !dirty.is_empty() {
                            let _ = writeln!(
                                out,
                                "  UNCOMMITTED changes in the worktree:\n{}",
                                dirty
                                    .lines()
                                    .map(|l| format!("    {l}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            );
                        }
                    }
                    let _ = writeln!(out);
                    for line in &lane.handback {
                        let _ = writeln!(out, "  {line}");
                    }
                }
                if status == mv_state::tasks::TaskStatus::Done {
                    let _ = writeln!(out, "  result {}", wait.result_path);
                    return Ok(());
                }
                return Err(Failure::Reported(crate::exit::ERROR));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------
// RealEngine: config -> chain -> selection -> rendered card
// ---------------------------------------------------------------------

/// Title-cases `word` ("execute" -> "Execute"). Used for the card's
/// `column` field, the stage name — not a role, so never touched by the
/// role rename.
fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// Read `config.toml` at the path `cli_override`/`$MULTIVERSE_CONFIG`/the
/// default would resolve to — the same precedence `commands::mod::
/// load_config` gives every other subcommand's `--config` flag (smoke
/// finding 2: this function used to hardcode `None` here, so `dispatch
/// --config PATH` silently fell back to the environment/default instead) —
/// tolerating retired `[roles.<old>]` / `<old> = […]` chain keys by
/// migrating them in memory first — `mv_config::load::load_config_str`
/// refuses those outright otherwise. Never writes the migrated document
/// back; see this file's module doc.
fn load_config(cli_override: Option<&str>) -> Result<Config, EngineError> {
    let path = load::resolve_config_path_from_env(cli_override.map(Path::new)).map_err(|e| {
        EngineError::ProjectResolution(format!("could not resolve config path: {e}"))
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| {
        EngineError::ProjectResolution(format!(
            "{}: could not read config (see config.example.toml, or set MULTIVERSE_CONFIG): {e}",
            path.display()
        ))
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|e| {
        EngineError::ProjectResolution(format!("{}: malformed TOML: {e}", path.display()))
    })?;
    mv_config::migrate::migrate_role_keys(&mut doc);
    load::load_config_str(&doc.to_string(), &path)
        .map_err(|e| EngineError::ProjectResolution(e.message().to_string()))
}

/// `target`, made absolute against `cwd` and canonicalized (following
/// symlinks, unlike `mv_config::load::resolve_config_path` — this is a
/// project *path*, not a config path; see `spec-config.md` §C).
fn resolve_user_path(raw: &str, cwd: &Path) -> Result<PathBuf, String> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    std::fs::canonicalize(&candidate)
        .map_err(|e| format!("{raw:?} does not resolve to a usable path: {e}"))
}

/// `config.project_for_path` (longest-configured-root-prefix match):
/// every `[projects.<name>]` whose own (canonicalized) `path` is `target`
/// or an ancestor of it is a candidate; the deepest one wins, so a project
/// nested inside another beats the outer one.
fn resolve_project<'c>(config: &'c Config, target: &Path) -> Option<(&'c str, PathBuf)> {
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

fn no_project_message(target: &Path, config: &Config) -> String {
    let mut names: Vec<&str> = config.projects.keys().map(String::as_str).collect();
    names.sort_unstable();
    let bullets = names
        .iter()
        .map(|n| format!("  - {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "\nno configured project contains {}.\nConfigured projects:\n{bullets}\nAdd a [projects.<name>] table with this path, and a chain for the role you want to dispatch.",
        target.display()
    )
}

/// `task_id_for` (`dispatch.py:410-457`), narrowed to this lane's needs: no
/// seat suffix (advisory either way — conformance only needs *a* stable,
/// `<ID>`-shaped token, not byte parity with Python's digest). Hashes
/// `project_path + NUL + role + NUL + task` with SHA-256, keeps the first
/// six hex characters, and prefixes the UTC date. `role_key` is the
/// role's current config key on every path — a `--dry-run` card and a
/// real dispatch mint the same shape of id (see [`RealEngine::resolve`]).
fn task_id_for(project_root: &Path, role_key: &str, task: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_root.as_os_str().as_encoded_bytes());
    hasher.update([0u8]);
    hasher.update(role_key.as_bytes());
    hasher.update([0u8]);
    hasher.update(task.as_bytes());
    let digest = hasher.finalize();
    let hex6 = digest[..3]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let today = time::OffsetDateTime::now_utc().date();
    format!(
        "{:04}{:02}{:02}-{role_key}-{hex6}",
        today.year(),
        u8::from(today.month()),
        today.day()
    )
}

/// Critic finding 5: `task_id_for` is a pure hash of (project, role, task
/// text, UTC date) — a second dispatch of identical material on the same
/// day mints the exact same id. Finds the first of `base`, `base-2`,
/// `base-3`, … (`unique_agent_name`'s own scheme, reused here for the same
/// reason: small, bounded, and already proven) that `taken` reports as
/// free, under whatever lock the caller already holds.
fn rebind_free_task_id(
    base: &str,
    mut taken: impl FnMut(&str) -> Result<bool, RunnerError>,
) -> Result<String, RunnerError> {
    if !taken(base)? {
        return Ok(base.to_string());
    }
    for attempt in 2..100 {
        let candidate = format!("{base}-{attempt}");
        if !taken(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(RunnerError::Refused(format!(
        "no free task id for {base:?}"
    )))
}

/// `AccountLookup` over a loaded [`Config`] — a newtype rather than `impl
/// AccountLookup for Config` because both the trait and the type are
/// foreign to this crate.
struct ConfigAccounts<'a>(&'a Config);

impl engine_dispatch::AccountLookup for ConfigAccounts<'_> {
    fn all_vendors(&self) -> Vec<String> {
        self.0.accounts.values().map(|a| a.vendor.clone()).collect()
    }
    fn all_account_names(&self) -> Vec<String> {
        self.0.accounts.keys().cloned().collect()
    }
    fn all_kinds(&self) -> Vec<String> {
        self.0.accounts.values().map(|a| a.kind.clone()).collect()
    }
    fn vendor_exists(&self, raw: &str) -> bool {
        self.0.accounts.values().any(|a| a.vendor == raw)
    }
    fn vendor_of_account(&self, raw: &str) -> Option<String> {
        self.0.accounts.get(raw).map(|a| a.vendor.clone())
    }
    fn vendors_of_kind(&self, raw: &str) -> Vec<String> {
        self.0
            .accounts
            .values()
            .filter(|a| a.kind == raw)
            .map(|a| a.vendor.clone())
            .collect()
    }
}

/// Read only this project's dispatch history; planning never creates an index.
struct ProjectRouting<'a> {
    project: &'a str,
    root: &'a Path,
}
impl engine_dispatch::RoutingLookup for ProjectRouting<'_> {
    fn dispatch_for_task(
        &self,
        project: &str,
        task_id: &str,
    ) -> Option<engine_dispatch::LoggedDispatch> {
        if project != self.project {
            return None;
        }
        let log = std::fs::read_to_string(self.root.join(".multiverse/routing.log")).ok()?;
        log.lines()
            .filter_map(|line| mv_state::log::parse(line).ok())
            .rfind(|record| {
                record.event() == "dispatch"
                    && record.fields.get("task").is_some_and(|id| id == task_id)
            })
            .map(|record| engine_dispatch::LoggedDispatch {
                account: record.fields.get("account").cloned(),
            })
    }
}

/// No scenario in this lane dispatches the reviewer role; a real 24h
/// routing-log count is a different lane's wiring.
struct ZeroReviewBudget;
impl engine_dispatch::ReviewBudget for ZeroReviewBudget {
    fn reviewer_dispatch_count(&self, _project: &str, _now: Instant) -> u32 {
        0
    }
}

/// The real fleet halt marker: `commands::halt` already owns reading and
/// writing it for `multiverse halt on|off|status` and `doctor`/`sweep`'s
/// own status lines (`commands::halt::read_halt_state`); this reuses that
/// exact reader rather than a second parser for the same file. `state.at`
/// already carries the recorded wall-clock timestamp (the same string
/// `doctor`/`sweep` print) — `DispatchError::FleetHaltActive`'s `Display`
/// renders it exactly as Python's `_fleet_halt_line` does
/// (`cli.py:3241-3246`), so it is passed through rather than discarded.
struct RealHalt<'a> {
    state_dir: &'a Path,
}
impl engine_dispatch::HaltQuery for RealHalt<'_> {
    fn read(&self) -> Result<Option<engine_dispatch::FleetHalt>, String> {
        let state = mv_state::halt::read_halt_state(self.state_dir);
        if !state.halted {
            return Ok(None);
        }
        Ok(Some(engine_dispatch::FleetHalt {
            reason: state
                .reason
                .unwrap_or_else(|| "no reason given".to_string()),
            since: state.at,
        }))
    }
}

/// One live `agent list` call, folded into the set of pane ids Herdr
/// currently reports — the liveness check [`breaker::StoreBreakerQuery`]
/// needs to decide whether an expired half-open trial's pane is still
/// around to reclaim. `None` (Herdr unreadable) reads as "every pane is
/// live": [`breaker::with_active_trial`]'s own doc comment requires a
/// liveness-check failure to fail open rather than silently expiring a
/// trial on a flaky call, the same fail-safe direction `live_open_counts`
/// already takes for seat caps.
fn live_pane_ids() -> Option<HashSet<String>> {
    let mut herdr = runner::ProcessHerdr;
    let result = herdr
        .call(&argv::agent_list(), Duration::from_secs(10))
        .ok()?;
    let agents = result
        .ok()
        .then(|| {
            result
                .payload
                .as_ref()
                .and_then(|p| p["result"]["agents"].as_array().cloned())
        })
        .flatten()?;
    Some(
        agents
            .iter()
            .filter_map(|a| a["pane_id"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// The real [`selector::BreakerQuery`] (card `20261102-b-02df4c`): backed
/// by `mv_engine::breaker::StoreBreakerQuery` over the real
/// `breaker-state.json`/`gauge-state.json` stores, rather than this file's
/// earlier hand-rolled "any entry at all" stand-in — `StoreBreakerQuery`
/// already carries the wall-clock-to-`Instant` bridge
/// ([`breaker::ClockBridge`]), the gauge-triggered reopen check, and
/// half-open trial-claim tracking that comment used to flag as missing.
/// `gauge_for_pool`/`is_pane_live` are the two Herdr/state reads
/// `StoreBreakerQuery` takes as injected closures rather than owning
/// itself; both are lazy — `is_pane_live` only ever calls Herdr when a
/// pool is actually half-open with an expired trial to re-check, not on
/// every status read.
///
/// `pub(super)`: `commands::council`'s own `RealEngine::seat` calls this
/// too, so a council's roster and an ordinary dispatch read FENCED/
/// AVAILABLE off the exact same decision rather than council keeping a
/// second, independently-drifting `BreakerQuery` impl.
pub(super) fn real_breaker(
    config_dir: &Path,
) -> breaker::StoreBreakerQuery<
    '_,
    impl Fn(&str) -> Option<breaker::GaugeReading> + '_,
    impl Fn(&str) -> bool,
> {
    breaker::StoreBreakerQuery {
        config_dir,
        clock: breaker::ClockBridge::now(),
        gauge_for_pool: move |pool: &str| {
            mv_state::gauge::load(config_dir)
                .get(pool)
                .and_then(breaker::gauge_reading_from_entry)
        },
        is_pane_live: |pane: &str| live_pane_ids().is_none_or(|live| live.contains(pane)),
    }
}

fn to_selector_policy(policy: ConfigFallbackPolicy) -> selector::FallbackPolicy {
    match policy {
        ConfigFallbackPolicy::Degrade => selector::FallbackPolicy::Degrade,
        ConfigFallbackPolicy::DegradeOnce => selector::FallbackPolicy::DegradeOnce,
        ConfigFallbackPolicy::Queue => selector::FallbackPolicy::Queue,
        ConfigFallbackPolicy::Warn => selector::FallbackPolicy::Warn,
        // `Shrink` only has meaning for a whole-batch council selection,
        // which this file does not build; approximated as `Degrade` until
        // council dispatch lands. Not exercised by this lane's scenarios.
        ConfigFallbackPolicy::Shrink => selector::FallbackPolicy::Degrade,
    }
}

/// `_reorder_chain_by_preference` (`accounts.py:890-930`): float any
/// `accounts prefer`red seat ahead of its same-`kind` siblings, without
/// disturbing a differently-kinded seat's position or a kind's own
/// unpreferred relative order. Confined to each kind's own slots in
/// `chain` rather than one global sort, exactly as the Python original
/// partitions — a preference recorded for `claude` must never move where
/// a `codex` seat sits. Purely local state (`seatstate::is_preferred`,
/// already read by nothing in this file before now); no live Herdr call.
pub(crate) fn reorder_chain_by_preference(
    chain: &[String],
    accounts: &BTreeMap<String, Account>,
    config_dir: &Path,
) -> Vec<String> {
    let mut result = chain.to_vec();
    let mut positions_by_kind: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, name) in chain.iter().enumerate() {
        positions_by_kind
            .entry(accounts[name].kind.as_str())
            .or_default()
            .push(index);
    }
    for positions in positions_by_kind.values() {
        if positions.len() < 2 {
            continue;
        }
        let names: Vec<&String> = positions.iter().map(|&i| &chain[i]).collect();
        let (preferred, rest): (Vec<&String>, Vec<&String>) = names
            .iter()
            .partition(|name| mv_engine::seatstate::is_preferred(config_dir, name));
        if preferred.is_empty() {
            continue;
        }
        for (&index, name) in positions.iter().zip(preferred.into_iter().chain(rest)) {
            result[index] = name.clone();
        }
    }
    result
}

/// `project_slots_grace_counts`'s open-task reading, narrowed to this
/// lane's local-only scope: a task this project dispatched but whose
/// `routing.log` has no `event=result` yet (`SweepTask::recorded`) is
/// still occupying a slot, the same "in progress" count `multiverse
/// sweep` itself reports as `in_progress_before`. No live Herdr call —
/// unlike Python's `concurrency_or_raise` (which folds this in as a lag
/// correction over a live `agent list`), this is the sole source here;
/// see this task's `result.md` for why a live reconciliation was judged
/// out of this file's scope.
fn open_task_count(project_root: &Path) -> u32 {
    mv_state::tasks::sweep_tasks(project_root)
        .iter()
        .filter(|task| !task.recorded)
        .count() as u32
}

/// Fleet-wide open count (every configured project whose `path` resolves),
/// plus each *other* project's own floor gap (`min_slots` not yet met by
/// its open count) for `FleetCapacityInputs::other_floor_gaps`.
fn fleet_open_counts(config: &Config, current_project: &str) -> (u32, Vec<(String, u32)>) {
    let mut fleet_open = 0u32;
    let mut other_floor_gaps = Vec::new();
    for (name, project) in &config.projects {
        let Some(raw) = project.path.value() else {
            continue;
        };
        let Ok(root) = std::fs::canonicalize(raw) else {
            continue;
        };
        let open = open_task_count(&root);
        fleet_open += open;
        if name != current_project {
            let gap = project.min_slots.saturating_sub(open);
            if gap > 0 {
                other_floor_gaps.push((name.clone(), gap));
            }
        }
    }
    (fleet_open, other_floor_gaps)
}

/// Whether a Herdr `agent_status` counts as "open" for a seat cap —
/// Python's `_OPEN_AGENT_STATES` (`accounts.py:623-624`): only
/// `working`/`blocked` hold a slot. `unknown` is excluded on purpose, by
/// Python's own stated rationale — "not evidence of anything, open or
/// otherwise", the same reason `is_terminal_for_turn` excludes it — so a
/// flaky or just-restarted Herdr never holds a seat's cap hostage on a
/// guess. Critic finding 3 (card `20261105-b-5e4d10`): this used to also
/// match `unknown`, overcounting against a cap until the status resolved.
fn is_open_agent_status(status: &str) -> bool {
    matches!(status, "working" | "blocked")
}

/// Every agent `name` Herdr's listing reports, with its raw
/// `agent_status` — Python's `status_by_name` (`accounts.py:816-819`),
/// which [`grace_open_counts`] consults to tell "Herdr already calls this
/// one open" (already counted by [`live_seat_counts`]) from "Herdr has not
/// caught up on it yet".
fn agent_status_by_name(result: &CommandResult) -> HashMap<String, String> {
    let mut statuses = HashMap::new();
    let Some(agents) = result
        .payload
        .as_ref()
        .and_then(|p| p["result"]["agents"].as_array())
    else {
        return statuses;
    };
    for agent in agents {
        if let (Some(name), Some(status)) = (agent["name"].as_str(), agent["agent_status"].as_str())
        {
            statuses.insert(name.to_string(), status.to_string());
        }
    }
    statuses
}

/// Latest routing-log task per Herdr agent name, across every configured
/// project — Python's `latest` dict (`accounts.py:769-781`). A name is
/// reissued once its previous occupant is no longer live (`naming.py`), so
/// several routing-log records sharing one reissued name must collapse to
/// the newest dispatch — the one whose own account this lane's counts
/// should answer about, not whichever record happened to be read first.
/// Ordered by [`mv_state::tasks::SweepTask::dispatch_ordinal`] (the log's
/// own append order), never by `sweep_tasks`'s returned order, which is an
/// alphabetical task-id sort with no chronology of its own. Critic finding
/// 4 (card `20261105-b-5e4d10`).
fn latest_tasks_by_agent(config: &Config) -> HashMap<String, mv_state::tasks::SweepTask> {
    let mut latest: HashMap<String, mv_state::tasks::SweepTask> = HashMap::new();
    for project in config.projects.values() {
        let Some(raw) = project.path.value() else {
            continue;
        };
        let Ok(root) = std::fs::canonicalize(raw) else {
            continue;
        };
        let mut tasks = mv_state::tasks::sweep_tasks(&root);
        tasks.sort_by_key(|task| task.dispatch_ordinal);
        for task in tasks {
            if let Some(agent) = task.agent.clone() {
                latest.insert(agent, task);
            }
        }
    }
    latest
}

/// Drop a trailing all-digit instance suffix (`-2`, `-3`, …) the way
/// Python `naming.strip_instance_suffix` does, so the last remaining
/// token is the seat short rather than the collision counter.
fn strip_instance_suffix<'a>(parts: &'a [&'a str]) -> &'a [&'a str] {
    if parts.len() > 1
        && parts
            .last()
            .is_some_and(|last| !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()))
    {
        &parts[..parts.len() - 1]
    } else {
        parts
    }
}

/// Python `accounts.py:784-804`: today's config account for a routing-log
/// task. Prefer `task.account` when that name still exists; if the seat
/// was renamed (or the recorded name was removed), recover it from the
/// agent name's trailing short via `account_by_short`. A name that is not
/// a configured short, or an agent name too short to parse as
/// `<slug>-<role>-<seat>`, contributes nothing — the fallback is a lookup,
/// not a guess from a trailing segment that happens to match.
pub(crate) fn resolve_task_account<'a>(
    task: &mv_state::tasks::SweepTask,
    config: &'a Config,
) -> Option<&'a Account> {
    if let Some(name) = task.account.as_deref() {
        if let Some(account) = config.accounts.get(name) {
            return Some(account);
        }
    }
    let agent = task.agent.as_deref()?;
    let parts: Vec<&str> = agent.split('-').collect();
    let parts = strip_instance_suffix(&parts);
    if parts.len() < 3 {
        return None;
    }
    let short = parts[parts.len() - 1];
    if short.is_empty() {
        return None;
    }
    config
        .accounts
        .values()
        .find(|account| !account.short.is_empty() && account.short == short)
}

/// Fold one `agent list` reply into open-agent counts, attributed to the
/// account that actually dispatched each pane — not read off the agent's
/// own name (Python's `open_agent_counts`, `accounts.py:769-800`: "An
/// agent's account is not read off its own name ... but off the dispatch
/// record that started it"). `latest` is `name` keyed, deduped to the
/// newest dispatch of that name ([`latest_tasks_by_agent`]); an agent name
/// not found there at all — a hand-started pane, the human's own
/// interactive seat, an agent from a project no longer configured — still
/// contributes to no seat's count, exactly Python's stated behavior,
/// rather than guessing a seat from a trailing name segment that happens
/// to match one. Critic finding 5 (card `20261105-b-5e4d10`). A dispatch
/// whose `account=` is no longer a config key (the seat was renamed)
/// still counts, recovered from the agent name's trailing short — Python's
/// `account_by_short` (`accounts.py:784-804`).
fn live_seat_counts(
    statuses: &HashMap<String, String>,
    latest: &HashMap<String, mv_state::tasks::SweepTask>,
    config: &Config,
) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for (name, status) in statuses {
        if !is_open_agent_status(status) {
            continue;
        }
        let Some(task) = latest.get(name) else {
            continue;
        };
        let Some(account) = resolve_task_account(task, config) else {
            continue;
        };
        *counts.entry(account.short.clone()).or_insert(0) += 1;
    }
    counts
}

/// `_STARTED_GRACE_SECONDS`/`_started_recently`/`_dispatched_recently`
/// (`accounts.py:623-663`), applied the way `open_agent_counts`
/// (`accounts.py:686-862`) applies them: credit an account's open count
/// for an unrecorded task Herdr's own listing does not (yet) call open —
/// either because the listing has not caught up right after a dispatch
/// (card `20261013-fixer-0d6e50`: two dispatches 17 seconds apart both
/// passed a cap-1 seat's check because the first pane, already live but
/// not yet prompted, counted as nothing), or because a pane briefly reads
/// `idle` before its first prompt registers (card `20261021-fixer-f43d02`).
///
/// Scoped fleet-wide, the same projects [`fleet_open_counts`] walks — a
/// task's pane can be live under any project's routing log. `task.account`
/// is today's config account name when that key still exists; otherwise
/// Python's `account_by_short` rename fallback ([`resolve_task_account`])
/// recovers the live short from the agent name, so a seat that was
/// renamed after the dispatch was recorded still holds its cap.
///
/// `latest` is [`latest_tasks_by_agent`]'s own per-name dedup — several
/// routing-log records sharing one reissued agent name (a name is
/// reissued once its previous occupant is no longer live) must credit the
/// grace window at most once, not once per record. Critic finding 4
/// (card `20261105-b-5e4d10`): before this, every unrecorded task in every
/// project's `sweep_tasks` was walked directly, so two such records
/// sharing a reissued name could both credit the same seat.
///
/// A task already counted open by `statuses` (so already present in
/// [`live_seat_counts`]'s own tally) is skipped here — this only ever adds
/// the grace credit [`live_seat_counts`] could not have already given it,
/// never double-counts one open pane.
fn grace_open_counts(
    config: &Config,
    statuses: &HashMap<String, String>,
    latest: &HashMap<String, mv_state::tasks::SweepTask>,
    now: time::OffsetDateTime,
) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for task in latest.values() {
        if task.recorded {
            continue;
        }
        let Some(account) = resolve_task_account(task, config) else {
            continue;
        };
        let already_open = task
            .agent
            .as_deref()
            .and_then(|agent| statuses.get(agent))
            .is_some_and(|status| is_open_agent_status(status));
        if already_open {
            continue;
        }
        let grace = time::Duration::seconds(account.startup_grace_seconds.unwrap_or(600).into());
        let started_open = match task.started_at {
            Some(started_at) => now - started_at <= grace,
            None => task
                .dispatched_at
                .is_some_and(|dispatched_at| now - dispatched_at <= grace),
        };
        if started_open {
            *counts.entry(account.short.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// One live `agent list` call, folded into per-seat open counts —
/// Python's `default_open_agent_counts` (`accounts.py:864-886`): `None`
/// when Herdr could not be asked at all (card `20260927-b-8b6e34`: that
/// must read as "at cap" for a capped seat, never as zero open), `Some`
/// otherwise, even if no seat in it is actually open. [`grace_open_counts`]
/// is folded in here so every caller sees the post-start grace window
/// without having to ask for it separately.
fn live_open_counts(config: &Config) -> Option<HashMap<String, u32>> {
    let mut herdr = runner::ProcessHerdr;
    let result = herdr
        .call(&argv::agent_list(), Duration::from_secs(10))
        .ok()?;
    if !result.ok() {
        return None;
    }
    let statuses = agent_status_by_name(&result);
    let latest = latest_tasks_by_agent(config);
    let mut counts = live_seat_counts(&statuses, &latest, config);
    for (short, grace_count) in
        grace_open_counts(config, &statuses, &latest, time::OffsetDateTime::now_utc())
    {
        *counts.entry(short).or_insert(0) += grace_count;
    }
    Some(counts)
}

/// One chain entry's config, joined with the live seatstate reads
/// `mv_engine::seatstate` already exposes (cap/lend, `unavailable_until`
/// still not parsed — no scenario in this lane's conformance group sets
/// it) and, for a *capped* seat only, a live per-seat open-agent count
/// (card `20261102-b-15ee38`) folded from `live_counts` — `None` there
/// (Herdr could not be asked, or was never asked because nothing in this
/// chain is capped) reads as "unreadable", same as before this card.
/// `live_counts` is computed once per `plan`/`dispatch` call, outside this
/// function, so a chain of several capped seats costs one Herdr round
/// trip rather than one per seat — Python's own `_open_counts` caching for
/// the same reason.
fn seat_from_account(
    account: &Account,
    config_dir: &Path,
    live_counts: &Option<HashMap<String, u32>>,
) -> Seat {
    let cap = mv_engine::seatstate::get_cap(config_dir, &account.name);
    let open_count = cap.and_then(|_| {
        live_counts
            .as_ref()
            .map(|counts| counts.get(&account.short).copied().unwrap_or(0))
    });
    Seat {
        name: account.name.clone(),
        vendor: account.vendor.clone(),
        pool: account.pool.clone(),
        kind: account.kind.clone(),
        available: account.available,
        unavailable_reason: account.unavailable_reason.clone(),
        unavailable_until: None,
        lent: mv_engine::seatstate::is_lent(config_dir, &account.name),
        // Selector still sees a model here: chain validation only checks
        // `[roles.<role>.models]` when that table exists. A miss is turned
        // into `NeedsConfig` by [`model_for_account`], not a panic.
        has_model: true,
        roles_allowed: account
            .roles_allowed
            .as_ref()
            .map(|roles| roles.iter().map(|r| r.config_key().to_string()).collect()),
        max_brief_chars: account.max_brief_chars.map(|v| v as usize),
        has_command: !account.command.trim().is_empty(),
        cap,
        open_count,
        quota_primary_percent: None,
        quota_secondary_percent: None,
        local_alive: (account.vendor == "local")
            .then(|| selector::HttpLocalLivenessProbe.probe(&account.name, &account.health_url)),
    }
}

/// Account `model`, else `[roles.<role>.models.<kind>]`. `[roles.<role>]`
/// is optional at load — `validate_chain_entry` only checks models when
/// that table exists — so a miss is a config error, never a panic.
fn model_for_account(
    account: &Account,
    role_settings: Option<&RoleSettings>,
    role_display: &str,
) -> Result<String, EngineError> {
    if let Some(model) = &account.model {
        return Ok(model.clone());
    }
    role_settings
        .and_then(|settings| settings.models.get(&account.kind).cloned())
        .ok_or_else(|| {
            EngineError::NeedsConfig(format!(
                "{role_display} has no model for kind {:?}",
                account.kind
            ))
        })
}

fn render_chain_exhausted(
    err: &selector::ChainExhaustedError,
    role_display: &str,
    project: &str,
) -> String {
    let bullets = err
        .skipped
        .iter()
        .map(|s| format!("  - {}: {}", s.seat, s.reason))
        .collect::<Vec<_>>()
        .join("\n");
    let mut message = format!(
        "\nevery account in the {role_display} chain for '{project}' is unavailable:\n{bullets}"
    );
    if err.policy == Some(selector::FallbackPolicy::DegradeOnce) {
        message.push_str(&format!(
            "\n\n{role_display}'s fallback policy is degrade_once, which accepts only one \
             step down the chain before refusing. That step is already spent on the seats \
             above, so any seat further down the chain was never tried."
        ));
    }
    message.push('\n');
    message.push_str(
        "Multiverse will not route to an account outside the chain (route by path, never by quota).",
    );
    // Python `cli.py:14929` prints AccountSelectionError as
    // `print(f"\n{exc}\n")`, which adds a trailing blank line after this
    // body. The leading `\n` is already in the message; this trailing `\n`
    // is the other half. Found via `conformance/diff_fuzz.py` seed 9000.
    message.push('\n');
    message
}

fn to_engine_error(
    error: engine_dispatch::DispatchError,
    role_display: &str,
    project: &str,
) -> EngineError {
    match error {
        engine_dispatch::DispatchError::ChainExhausted(chain_err) => {
            EngineError::NeedsConfig(render_chain_exhausted(&chain_err, role_display, project))
        }
        // A fleet halt is `EXIT_ERROR` (1), matching `commands::mod::
        // authorization`'s own fixed halt refusal and Python's
        // `cli.py:3277-3283` — never `EXIT_NEEDS_CONFIG` (3), which is for
        // a config/selection problem, not an operator-imposed pause.
        // `EngineError::FleetHalted` keeps the real reason and "use
        // --force" wording `DispatchError`'s own `Display` already
        // renders (rather than discarding it for a generic message) and
        // lets `run` recognize this specific refusal to skip the
        // warnings Python never reaches once it has exited on its own
        // halt check, ahead of both of them.
        halt @ (engine_dispatch::DispatchError::FleetHaltActive { .. }
        | engine_dispatch::DispatchError::FleetHaltUnreadable { .. }) => {
            EngineError::FleetHalted(halt.to_string())
        }
        other => EngineError::NeedsConfig(other.to_string()),
    }
}

/// A minimal JSON string literal for `value` — `mv-cli` has no production
/// `serde_json` dependency (it is a dev-dependency only, for tests), and
/// widening `Cargo.toml` to add one is outside this file's write scope, so
/// `record_dispatch`'s `title=` field (critic finding 9) is escaped by hand
/// rather than by pulling in a JSON library for one field. Quotes,
/// backslashes, and the standard short escapes are covered; any other
/// control character falls back to `\u00XX`. Unlike Python's
/// `json.dumps(..., ensure_ascii=True)` default, a non-ASCII character is
/// left as UTF-8 rather than `\uXXXX`-escaped — still valid JSON, just not
/// byte-identical to the Python oracle's own encoding.
fn json_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

const CARD_RULE: &str =
    "──────────────────────────────────────────────────────────────────────────";

fn section(out: &mut String, title: &str) {
    out.push_str(&format!("\n{title}\n{CARD_RULE}\n"));
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("  {line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn wrap_prompt(prompt: &str) -> String {
    let mut out = String::new();
    let mut column = 0;
    for word in prompt.split_whitespace() {
        if column > 0 {
            if column + 1 + word.chars().count() > 72 {
                out.push('\n');
                column = 0;
            } else {
                out.push(' ');
                column += 1;
            }
        }
        for c in word.chars() {
            if column == 72 {
                out.push('\n');
                column = 0;
            }
            out.push(c);
            column += 1;
        }
    }
    indent(&out)
}

fn task_prompt(task_dir: &Path) -> String {
    format!("Read `{}` and execute it. Follow its write scope exactly. Write your findings to `{}` and then write `done`, `blocked`, or `failed` to `{}`.",
        task_dir.join("brief.md").display(), task_dir.join("result.md").display(), task_dir.join("status").display())
}

/// The complete Python plan-card structure. All work here is prediction or
/// local reading: no store preparation, pane calls, copies, or log writes.
fn render_card(resolved: &Resolved, request: &Request) -> Result<String, EngineError> {
    let refused = |e: RunnerError| EngineError::Refused(e.to_string());
    let mut store = RealDispatchStore::new(resolved.clone(), request);
    store.isolated = store.write_scope != "none";
    let cwd = if let Some(into) = &request.into {
        if !store.isolated {
            return Err(EngineError::Refused(format!(
                "--into requires a writing role; {} has write scope \"none\"",
                resolved.role_display
            )));
        }
        if !mv_state::paths::is_valid_task_id(into) {
            return Err(EngineError::Refused(format!(
                "invalid --into task id {into:?}"
            )));
        }
        let lane = resolved
            .project_root
            .join(".multiverse/worktrees")
            .join(into);
        if !lane.join(".git").is_file() {
            return Err(EngineError::Refused(format!(
                "--into {into:?}: no existing lane at {}",
                lane.display()
            )));
        }
        lane
    } else if store.isolated {
        resolved
            .project_root
            .join(".multiverse/worktrees")
            .join(&resolved.task_id)
    } else {
        resolved.project_root.clone()
    };
    let mut inputs = Vec::new();
    let mut context = Vec::new();
    let mut seen = HashSet::new();
    let invocation_cwd =
        std::env::current_dir().map_err(|e| EngineError::Refused(e.to_string()))?;
    for raw in &request.inputs {
        let source = resolve_user_path(raw, &invocation_cwd).map_err(EngineError::Refused)?;
        let basename = source
            .file_name()
            .ok_or_else(|| EngineError::Refused(format!("--input {raw:?}: no file name")))?;
        if !source.is_file() || !seen.insert(basename.to_owned()) {
            return Err(EngineError::Refused(format!(
                "--input {raw:?}: not a file or duplicate basename"
            )));
        }
        let relative = PathBuf::from(".multiverse/inputs")
            .join(&resolved.task_id)
            .join(basename);
        context.push(format!("Input file: `{}` — copied in from {} before you started; read it at that path, not the original.", relative.display(), source.display()));
        inputs.push((source, cwd.join(relative)));
    }
    let brief = store
        .build_brief(&cwd, context, &resolved.role_display)
        .map_err(refused)?;
    let account = &store.account;
    let project = &resolved.config.projects[&resolved.project_name];
    let name = runner::naming::agent_name(
        &resolved.project_name,
        role_short(resolved.role),
        None,
        &account.short,
        project.short.as_deref().unwrap_or(""),
    )
    .map_err(refused)?;
    let paths = cwd.join(".multiverse/tasks").join(&resolved.task_id);
    let prompt = task_prompt(&paths);
    let title = resolved_title(request)?;
    let mut out = format!("DRY RUN — nothing was created. No pane, no agent, no files, no log entry.\n\nCARD  {}\n{CARD_RULE}\n", resolved.task_id);
    out.push_str(&format!("  role        {}\n  column      {}  (stage, not a role — spec §12)\n  harness     {}{}\n  model       {}\n  account     {}   (pool: {})\n  cwd         {}\n  agent       {}\n  write scope {}\n", resolved.role_display, capitalize(&store.column), account.kind,
        if account.kind == "grok" { "   ⚠ no state integration available — heuristic detection only" } else { "" },
        store.model, account.name, account.pool, cwd.display(), name, store.write_scope));
    if store.isolated {
        for path in &request.scope {
            out.push_str(&format!("  scope       {path}\n"));
        }
    }
    if let Some(into) = &request.into {
        out.push_str(&format!("  into        {into}  (adopts that task's existing lane — `git worktree add` is not run)\n"));
    }
    section(
        &mut out,
        "ACCOUNT SELECTION  (spec §7 — explicit and logged, never inferred)",
    );
    out.push_str(&format!("  project     {}  →  {}\n  chain       {}\n  selected    {}\n  outcome     {}\n  reason      {}\n", resolved.project_name, resolved.project_root.display(), resolved.chain_names.join(" → "), account.name, store.outcome, store.reason));
    for note in &resolved.exclusion_notes {
        out.push_str(&format!("  excluded    {note}\n"));
    }
    for skip in &resolved.selection.skipped {
        out.push_str(&format!("  skipped     {}: {}\n", skip.seat, skip.reason));
    }
    out.push_str(&format!(
        "  env         {}={}\n",
        account.config_dir_var,
        account.config_dir.as_raw()
    ));
    section(
        &mut out,
        "WOULD LOG  (not written — routing.log is untouched on a dry run)",
    );
    // A real dispatch's routing-log line always leads with a timestamp and
    // trails with `branch=` (`RealDispatchStore::record_dispatch` below) —
    // this preview previously omitted both. `branch=` is always empty here
    // regardless of write scope: no worktree exists yet on a dry run, same
    // as Python's own `card.selection.log_line(task=..., title=...)` call,
    // which never passes a `branch` either.
    let when = mv_core::time::now_stamp();
    out.push_str(&format!("  {when}\tevent=dispatch\ttask={}\tproject={}\trole={}\taccount={}\tpool={}\toutcome={}\treason={}", resolved.task_id, resolved.project_name, resolved.minted_role_key, account.name, account.pool, store.outcome, store.reason));
    if !title.is_empty() {
        out.push_str(&format!("\ttitle={}", json_quote(&title)));
    }
    out.push_str("\tbranch=");
    out.push('\n');
    section(&mut out, "PROMPT  (what the agent would receive)");
    out.push_str(&wrap_prompt(&prompt));
    out.push('\n');
    section(&mut out, "WOULD CREATE  (paths only — nothing was written)");
    out.push_str(&format!(
        "  {}\n  {}      (written by the agent)\n  {}         (written by the agent)\n",
        paths.join("brief.md").display(),
        paths.join("result.md").display(),
        paths.join("status").display()
    ));
    for (source, dest) in inputs {
        out.push_str(&format!(
            "  {}   (copied from {})\n",
            dest.display(),
            source.display()
        ));
    }
    section(&mut out, "BRIEF  (content of brief.md)");
    out.push_str(&indent(brief.render_with(request.full).trim_end()));
    out.push('\n');
    if !request.full {
        out.push_str("\n  Plus, appended to brief.md and omitted here for length:\n");
        for (label, loaded) in [
            (
                "role",
                mv_engine::prompts::role_prompt(resolved.role.config_key()),
            ),
            ("stage", mv_engine::prompts::column_prompt(&store.column)),
        ] {
            let loaded = loaded.map_err(|e| EngineError::Refused(e.to_string()))?;
            out.push_str(&format!(
                "    {label:<6} {}  ({} lines)\n",
                loaded.source.display(),
                loaded.line_count()
            ));
        }
        out.push_str("  Re-run with --full to print them inline.\n");
    }
    section(&mut out, "WOULD RUN");
    let native = argv::native_args(&store.model, &account.kind, store.effort.as_deref());
    let env = [(account.config_dir_var.as_str(), account.config_dir.as_raw())];
    let mut commands = vec![argv::pane_split(
        &cwd,
        &env,
        None,
        false,
        Some("right"),
        false,
    )];
    let launch = PreparedLaunch {
        project_root: resolved.project_root.clone(),
        cwd: cwd.clone(),
        task_id: resolved.task_id.clone(),
        agent_name: name.clone(),
        role: resolved.minted_role_key.into(),
        kind: account.kind.clone(),
        model: store.model.clone(),
        seat: account.name.clone(),
        brief: String::new(),
        native_args: native.clone(),
        environment: Vec::new(),
        clear_auth: Vec::new(),
        command: None,
    };
    commands.push(argv::pane_rename(
        "<new-pane>",
        &runner::pane_label(&launch),
    ));
    let cd = format!("cd {}", shell_quote(&cwd.to_string_lossy()));
    if !account.command.trim().is_empty() {
        let mut words = Vec::new();
        let mut in_prefix = true;
        for word in account.command.split_whitespace() {
            if in_prefix {
                if let Some((key, value)) = word.split_once('=') {
                    if !key.is_empty()
                        && key.chars().enumerate().all(|(i, c)| {
                            c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                        })
                    {
                        words.push(format!("{key}={}", shell_quote(value)));
                        continue;
                    }
                }
            }
            in_prefix = false;
            words.push(shell_quote(word));
        }
        words.extend(native.iter().map(|s| shell_quote(s)));
        commands.push(argv::pane_run(
            "<new-pane>",
            &format!("{cd} && {}", words.join(" ")),
        ));
        commands.push(argv::agent_rename("<new-pane>", &name));
    } else {
        commands.push(argv::pane_run(
            "<new-pane>",
            &format!("{cd} && {}", argv::clear_auth_script(AUTH_VARS_TO_CLEAR)),
        ));
        commands.push(argv::agent_start(
            &name,
            &account.kind,
            "<new-pane>",
            Some(60_000),
            &native,
        ));
    }
    commands.push(argv::agent_prompt(&name, &prompt, false, None));
    for command in commands {
        out.push_str(&format!(
            "  # {}\n  {}\n\n",
            command.purpose,
            command.shell()
        ));
    }
    out.push_str(&format!(
        "  # wait_for_turn locally (timeout: {} seconds)\n",
        request.timeout_seconds
    ));
    out.push_str("  Note: the account's config-dir variable rides on `pane split --env`,\n  not on the `agent start` client process. Spec §6 has this wrong —\n  see VERIFICATION.md A4. The client only sends a socket request; the\n  agent is started by the herdr server, which does not inherit the\n  client's environment.\n");
    let mut unresolved = Vec::new();
    if account.config_dir.is_placeholder() {
        unresolved.push(format!(
            "accounts.{}.config_dir = {}",
            account.name,
            account.config_dir.as_raw()
        ));
    }
    if mv_config::placeholder::is_placeholder(&store.model) {
        unresolved.push(if account.model.is_some() {
            format!("accounts.{}.model = {}", account.name, store.model)
        } else {
            format!(
                "roles.{}.models.{} = {}",
                resolved.minted_role_key, account.kind, store.model
            )
        });
    }
    if mv_config::placeholder::is_placeholder(&account.kind) {
        unresolved.push(format!("accounts.{}.kind = {}", account.name, account.kind));
    }
    if unresolved.is_empty() {
        section(&mut out, "READY");
        out.push_str("  Every value is resolved. This card is dispatchable.\n");
    } else {
        section(&mut out, "UNRESOLVED  (config values still to fill in)");
        for item in unresolved {
            out.push_str(&format!("  {item}\n"));
        }
        out.push_str("\n  This card cannot be dispatched for real until these are set.\n  Multiverse will not guess them (spec §0). They are printed\n  verbatim above so you can see exactly what reaches the agent.\n");
    }
    Ok(out)
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn resolved_title(request: &Request) -> Result<String, EngineError> {
    let title = request.title.clone().unwrap_or_else(|| {
        request
            .task
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(120)
            .collect()
    });
    if title.contains(['\t', '\n', '\r']) {
        return Err(EngineError::Refused(
            "--title must not contain a tab or newline".into(),
        ));
    }
    Ok(redact_text(&title))
}

/// The column (stage) a dispatched role's agent is told to work from —
/// Python's `roles.py:86-100` + `dispatch.py:1689,1503-1505` table,
/// carried over onto the current role names: a reading role (Scout,
/// Archivist, Watcher) gets Explore; a writing role (Builder, Artisan)
/// gets Execute; a reviewing role (Critic, Seer) and Council get Review.
/// Critic finding 3: this file used to hand every live brief
/// `column_prompt("execute")` unconditionally, so a Scout was told to
/// implement and a Critic was handed the Execute stage contract.
fn stage_for_role(role: Role) -> &'static str {
    match role {
        Role::Scout | Role::Archivist | Role::Watcher => "explore",
        Role::Builder | Role::Artisan => "execute",
        Role::Critic | Role::Seer | Role::Council => "review",
        // Never dispatched as a task (`role_not_implemented` refuses it
        // first); named so this match stays exhaustive against a future
        // role rather than silently defaulting one.
        Role::Nexus => "execute",
    }
}

/// Every role `dispatch` can actually start as a task — every
/// [`crate::args::WORKER_ROLES`] key except `council`, which Python's
/// `DISPATCHABLE_ROLES` (`roles.py:170-179`) also excludes (alongside
/// `orchestrator`, already refused upstream of this file: `nexus` is not
/// in `WORKER_ROLES` at all, so `commands::mod::validate` turns it into a
/// generic "unknown role" before this module ever sees it).
fn dispatchable_roles() -> Vec<&'static str> {
    let mut roles: Vec<&'static str> = crate::args::WORKER_ROLES
        .iter()
        .copied()
        .filter(|r| *r != "council")
        .collect();
    roles.sort_unstable();
    roles
}

/// Python's `RoleNotImplementedError` message for `role` (`roles.py:275
/// -306`), narrowed to the one role this port ever reaches with (`council`
/// — see [`dispatchable_roles`]).
fn role_not_implemented(role_display: &str) -> EngineError {
    EngineError::RoleNotImplemented(format!(
        "{role_display} cannot be dispatched as a task. Council fans out to several agents \
         across DISTINCT vendors and synthesizes them; a chain is an ordered fallback, so a \
         Council card would dispatch one agent and call the result a council. Dispatchable: {}.",
        dispatchable_roles().join(", ")
    ))
}

/// Everything [`RealEngine::plan`] and [`RealEngine::dispatch`] both need,
/// resolved once from a [`Request`] — config through to the chosen seat.
/// Factored out so a real dispatch rechecks admission against *fresh*
/// fleet state (`RealDispatchStore::recheck_admission`, under the fleet
/// lock) rather than trusting this same, now-stale, plan-time answer —
/// while still sharing every other resolution step (config load, project/
/// role lookup, chain build, capacity inputs, selection) with the
/// `--dry-run` card so the two paths cannot drift apart.
///
/// `role_display`/`minted_role_key` hold the role's current display
/// name/key (`Role::display_name()`/`Role::config_key()`) on every path —
/// the `--dry-run` card and a real dispatch render the same name; see
/// this file's module doc.
#[derive(Clone)]
struct Resolved {
    config: Config,
    config_dir: PathBuf,
    project_name: String,
    project_root: PathBuf,
    role: Role,
    role_display: String,
    minted_role_key: &'static str,
    chain_names: Vec<String>,
    task_id: String,
    selection: selector::Selection,
    model: String,
    exclusion_notes: Vec<String>,
    /// Set when `--scope` was given for a read-only role — Python
    /// `cli.py:3320-3334`. [`RealEngine::plan`] prints it exactly once;
    /// [`RealEngine::dispatch`] does not re-print it on its own, separate
    /// `resolve()` call for a real dispatch.
    scope_warning: Option<String>,
}

/// The real [`Engine`]: reads `config.toml`, resolves the project and
/// role, walks the chain through `mv_engine::dispatch::plan_dispatch`, and
/// renders a `--dry-run` card. See this file's module doc for the
/// remaining retired-role-name scope choice, and `result.md` for the
/// scenarios this does not reach.
pub struct RealEngine;

impl RealEngine {
    /// Shared by [`RealEngine::plan`]'s `--dry-run` card and
    /// [`RealEngine::dispatch`]'s real launch: both resolve the same way
    /// now, so `role_display`/`minted_role_key` on the returned
    /// [`Resolved`] (and the task id minted from it) always use the
    /// role's current name — task `20261103-fixer-fa9c70` (this file's
    /// module doc; previously the `--dry-run` card alone used the
    /// retired name, task `20261102-fixer-e1e2c3`).
    fn resolve(&self, request: &Request) -> Result<Resolved, EngineError> {
        // Resolved before `config.toml` is ever read: a path that does not
        // exist refuses on that fact alone, with no dependency on — or
        // need to load — whatever config happens to be on this machine.
        let cwd = std::env::current_dir().map_err(|e| {
            EngineError::Refused(format!("could not read the current directory: {e}"))
        })?;
        let target =
            resolve_user_path(&request.project_path, &cwd).map_err(EngineError::Refused)?;

        let config = load_config(request.config_override.as_deref())?;
        let config_dir = load::config_state_dir_from_env(&config.source_path);

        let Some((project_name, project_root)) = resolve_project(&config, &target) else {
            return Err(EngineError::NeedsConfig(no_project_message(
                &target, &config,
            )));
        };
        let project_name = project_name.to_string();

        // `--role` is new input, not a history read: `commands::mod::
        // validate`'s `WORKER_ROLES` check already refuses a retired name
        // before this ever runs in production, but `Role::from_config_key`
        // (not `::from_any_config_key`) is the one that belongs here on its
        // own terms — this file's `Engine` can be driven directly (tests,
        // a future caller), and a write surface must refuse a retired role
        // name itself rather than depend on an upstream check it does not
        // own (card `20261102-b-47a7b3`).
        let role = match &request.role {
            Some(raw) => {
                Role::from_config_key(raw).map_err(|e| EngineError::NeedsConfig(e.to_string()))?
            }
            None => Role::Scout,
        };
        let minted_role_key = role.config_key();
        let role_display = role.display_name().to_string();

        // Card 20261102-b-3b5de0: a council is seated and convened through
        // `multiverse council`, never dispatched as a single-seat task —
        // refused here, before a chain is even looked up, the same point
        // Python's `planner.plan` raises `RoleNotImplementedError` from.
        if role == Role::Council {
            return Err(role_not_implemented(&role_display));
        }

        let project: &Project = &config.projects[&project_name];
        let chain_names = project.chains.get(&role).cloned().ok_or_else(|| {
            EngineError::NeedsConfig(format!(
                "no chain configured for role {role_display:?} in project {project_name:?}"
            ))
        })?;
        let chain_names = reorder_chain_by_preference(&chain_names, &config.accounts, &config_dir);

        // Card 20261102-b-15ee38: one live `agent list` call, made only
        // when at least one chain entry is actually capped — see
        // `live_open_counts`/`seat_from_account`'s own doc comments.
        let live_counts = chain_names
            .iter()
            .any(|name| mv_engine::seatstate::get_cap(&config_dir, name).is_some())
            .then(|| live_open_counts(&config))
            .flatten();
        let seats: Vec<Seat> = chain_names
            .iter()
            .map(|name| seat_from_account(&config.accounts[name], &config_dir, &live_counts))
            .collect();

        let role_settings = config.roles.get(&role);
        let on_exhausted = role_settings
            .map(|r| r.on_exhausted)
            .unwrap_or(ConfigFallbackPolicy::Degrade);
        let write_scope = role_settings
            .map(|r| r.write_scope.as_str())
            .unwrap_or("none");

        let scope_warning = check_read_only_flags(request, &role_display, write_scope)?;
        check_cited_inputs(
            &request.task,
            &role_display,
            &project_root,
            write_scope,
            &request.inputs,
        )?;

        let task_id = task_id_for(&project_root, minted_role_key, &request.task);

        let review_policy = engine_dispatch::ReviewPolicy {
            batch_max_lanes: config.review_policy.batch_max_lanes as usize,
            min_quota_percent: Some(config.review_policy.min_quota_percent as f64),
            budget_per_window: config.review_policy.budget_per_window,
        };
        let (fleet_open, other_floor_gaps) = fleet_open_counts(&config, &project_name);
        let capacity = engine_dispatch::FleetCapacityInputs {
            max_concurrent: config.max_concurrent,
            fleet_open,
            project_open: open_task_count(&project_root),
            project_min_slots: project.min_slots,
            project_max_slots: project.max_slots,
            other_floor_gaps,
            batch_count: 1,
        };
        // Card 20261013-b-531383 / `cli.py:3401-3405`: a dry run never
        // waits on a capped seat regardless of `--wait-for-seat`; a real
        // dispatch opts in only when the operator asked, via either the
        // bare flag or an explicit `--wait-for-seat-seconds`.
        let wait_for_capacity =
            !request.dry_run && (request.wait_for_seat || request.wait_for_seat_seconds.is_some());
        let req = engine_dispatch::DispatchRequest {
            role: minted_role_key,
            reviewer_role: Role::Critic.config_key(),
            nexus_role: Role::Nexus.config_key(),
            project: &project_name,
            task_id: &task_id,
            task_len_chars: request.task.chars().count(),
            reviewing: &request.reviewing,
            explicit_exclude_vendor: &request.exclude_vendor,
            chain: &seats,
            policy: to_selector_policy(on_exhausted),
            review_policy,
            fleet_min_quota_percent: Some(config.dispatch_min_quota_percent as f64),
            wait_for_capacity,
            capacity_timeout: Some(
                request
                    .wait_for_seat_seconds
                    .unwrap_or(selector::DEFAULT_SEAT_WAIT_TIMEOUT_SECS),
            ),
            force: request.force,
            dry_run: request.dry_run,
        };

        let breaker = real_breaker(&config_dir);
        let halt = RealHalt {
            state_dir: &config_dir,
        };
        let routing = ProjectRouting {
            project: &project_name,
            root: &project_root,
        };
        let exclusions = engine_dispatch::author_exclusions(
            &project_name,
            &request.reviewing,
            &request.exclude_vendor,
            &routing,
            &ConfigAccounts(&config),
        )
        .map_err(|e| to_engine_error(e, &role_display, &project_name))?;
        let selection = engine_dispatch::plan_dispatch(
            &req,
            &routing,
            &ConfigAccounts(&config),
            &ZeroReviewBudget,
            &capacity,
            &halt,
            &breaker,
            Instant::now(),
        )
        .map_err(|e| to_engine_error(e, &role_display, &project_name))?;
        // `breaker` is an opaque `impl Trait` (closures capturing
        // `&config_dir`); the borrow checker conservatively extends such a
        // type's borrow to the end of scope unless it is dropped
        // explicitly, which would otherwise conflict with moving
        // `config_dir` into `Resolved` below.
        drop(breaker);

        // scout2-gaps.md row #30 / Python `dispatch.py:1677-1684`: checked
        // right after an account is actually selected, before anything
        // else about this dispatch is built — `card_from_selection`'s own
        // placement. A project with a backlog item a human answered and
        // nobody closed refuses every dispatch (dry-run included, since
        // `plan` and `dispatch` both call this), until `backlog done
        // <item> --evidence ...` closes it; there is no `--force`-style
        // escape, matching `AnsweredItemsUnclosedError`'s own docstring.
        let unclosed = engine_dispatch::unclosed_answered_items(&project_root);
        if !unclosed.is_empty() {
            return Err(EngineError::NeedsConfig(
                engine_dispatch::answered_items_unclosed_message(&project_name, &unclosed),
            ));
        }

        let selected_account = &config.accounts[&selection.seat.name];
        let model = model_for_account(selected_account, role_settings, &role_display)?;

        Ok(Resolved {
            config,
            config_dir,
            project_name,
            project_root,
            role,
            role_display,
            minted_role_key,
            chain_names,
            task_id,
            selection,
            model,
            exclusion_notes: exclusions.notes,
            scope_warning,
        })
    }
}

impl Engine for RealEngine {
    fn plan(&self, request: &Request) -> Result<Plan, EngineError> {
        let resolved = self.resolve(request)?;
        if let Some(warning) = &resolved.scope_warning {
            eprintln!("{warning}");
        }
        let report = render_card(&resolved, request)?;

        Ok(Plan {
            role: resolved.minted_role_key.to_string(),
            seat: resolved.selection.seat.name.clone(),
            task_id: resolved.task_id,
            dry_run_report: report,
            pre_dispatch_notes: Vec::new(),
            worktree: None,
            branch: None,
            linked_backlog: Vec::new(),
            backlog_link_failures: Vec::new(),
            // Unused: the `--dry-run` preview prints `dry_run_report` and
            // returns before any of these fields are read.
            harness: String::new(),
            model: String::new(),
            cwd: String::new(),
            agent: String::new(),
            pane: String::new(),
            status_path: String::new(),
            wait_outcome: None,
        })
    }

    /// Builds a [`RealDispatchStore`] from the same resolution `plan`
    /// used, and calls `mv_engine::runner::dispatch` on it — Python's
    /// `cli.py:3728` calling `DispatchRunner.dispatch` on the card
    /// `DispatchPlanner.plan` just built. `_plan` (the `--dry-run` answer
    /// from a possibly-earlier moment) is intentionally not reused for the
    /// real launch: admission, the chosen seat, and even the chain's
    /// shape can have moved on since, and [`resolve`](RealEngine::resolve)
    /// — plus `RealDispatchStore::recheck_admission` under the fleet lock
    /// — is what re-answers all of that against *now*, not then.
    fn dispatch(&self, request: &Request, _plan: Plan) -> Result<Plan, EngineError> {
        // Critic finding 9: a title is validated before anything is
        // created — `record_dispatch`'s routing-log line is tab-delimited,
        // so an embedded tab or newline would corrupt it or a later field.
        if let Some(title) = &request.title {
            if title.contains(['\t', '\n', '\r']) {
                return Err(EngineError::Refused(
                    "--title must not contain a tab or newline".into(),
                ));
            }
        }

        let resolved = self.resolve(request)?;
        let role_display = resolved.role_display.clone();
        let minted_role_key = resolved.minted_role_key;
        let seat_name = resolved.selection.seat.name.clone();

        // Critic finding 1 / card 20261102-b-0a4062: Python refuses
        // `--into` outright for a non-writer (`LaneAdoptionNotWritingError`)
        // rather than silently proceeding in the shared checkout a reader
        // would otherwise use — there is no lane for a reader to adopt.
        if request.into.is_some() {
            let write_scope = resolved
                .config
                .roles
                .get(&resolved.role)
                .map(|r| r.write_scope.as_str())
                .unwrap_or("none");
            if write_scope == "none" {
                return Err(EngineError::Refused(format!(
                    "--into requires a writing role; {role_display} has write scope \"none\""
                )));
            }
        }

        let mut store = RealDispatchStore::new(resolved, request);
        let mut herdr = runner::ProcessHerdr;
        let current_pane = std::env::var("HERDR_PANE_ID").unwrap_or_default();
        let timing = runner::Timing::default();

        // Python `runner.py:2642-2645`: taken before the agent exists, so
        // anything that appears afterwards is its doing. Only a read-only
        // role runs here at all — a writer gets its own worktree instead.
        let tree_before = (store.write_scope == "none")
            .then(|| tree_state(&store.project_root))
            .flatten();

        match runner::dispatch(&mut store, &mut herdr, &current_pane, &timing) {
            Ok(dispatched) => {
                let worktree = store
                    .isolated
                    .then(|| dispatched.launch.cwd.display().to_string());
                let branch = store.isolated.then(|| store.branch.clone());
                let (linked_backlog, backlog_link_failures) = record_dispatch_backlog_links(
                    &store.project_root,
                    &request.task,
                    &dispatched.launch.task_id,
                );
                // Scout-gaps #27, card `20261105-b-5e4d10`: Python's own
                // placement — after the agent has actually started, never
                // earlier (card `20261015-fixer-ea95a9`). A Critic that
                // failed to start never reaches this `Ok` arm, so a doomed
                // dispatch cannot advance the marker past work nothing
                // read. `--commits`' last entry, when given, names exactly
                // what was reviewed — which may end before the branch's
                // live tip — so it is stamped directly rather than the tip,
                // the same reasoning `record_reviewed` itself documents.
                if should_record_reviewed(
                    store.role,
                    &store.reviewing,
                    &store.config.review_policy.scope,
                ) {
                    let reviewed_commit = request.commits.last().map(String::as_str);
                    for lane_task_id in &store.reviewing {
                        mv_state::worktrees::record_reviewed(
                            &store.project_root,
                            lane_task_id,
                            reviewed_commit,
                        );
                    }
                }
                // Python `cli.py:3791-3864`: `--wait` blocks here, after the
                // pane exists, for the agent's own turn. `settled_state` is
                // `live.settled.state` — the raw Herdr agent_status at
                // settle — returned alongside the status file by
                // `wait_for_result` (shared with `mv_engine::council`).
                let wait_outcome = request.wait.then(|| {
                    let task_dir = dispatched
                        .launch
                        .cwd
                        .join(".multiverse/tasks")
                        .join(&dispatched.launch.task_id);
                    let timeout = Duration::from_secs_f64(request.timeout_seconds.max(0.0));
                    let (status, settled_state) = match runner::wait_for_result(
                        &mut herdr,
                        &dispatched,
                        timeout,
                        timing.poll,
                    ) {
                        Ok((status, state)) => (status, Some(state)),
                        Err(_) => (None, None),
                    };
                    // Python `runner.py:2906-2907`: `close_pane_when_done`
                    // defaults true and no caller turns it off, so a `--wait`
                    // that settles with a usable `done` result always closes
                    // the agent's pane and tidies the row behind it, rather
                    // than leaving a finished pane open until a later `tasks
                    // --reclaimable` sweeps it up.
                    if status == Some(mv_state::tasks::TaskStatus::Done) {
                        runner::teardown(&mut herdr, &dispatched.pane, &current_pane);
                    }
                    let tree_after = tree_state(&store.project_root);
                    let violations = scope_violations(
                        &store.write_scope,
                        tree_before.as_ref(),
                        tree_after.as_ref(),
                    );
                    let revisions = read_revisions(&task_dir);
                    // Python `runner.py:2869-2877`: the dispatch's own
                    // completion record, written the moment the wait
                    // observes a settled turn with a usable status — see
                    // `record_wait_result`'s own doc comment for why this is
                    // never gated by `--no-reconcile` and never skipped in
                    // favor of a later reconciler's guess.
                    if let (Some(status), Some(state)) = (status, settled_state) {
                        store.record_wait_result(
                            &dispatched.launch.task_id,
                            status,
                            state,
                            revisions.len(),
                            &dispatched.pane,
                        );
                    }
                    // Scout-gaps #4 remainder: a writer's own lane gets the
                    // same uncommitted-changes/handback report Python prints
                    // unconditionally once a lane exists — `lane_task_id`'s
                    // own scheme (`mv_state::tasks::lane_task_id`), since an
                    // `--into` adoption's branch names its *owner's* lane,
                    // never this task's own id.
                    let lane_handback = store.isolated.then(|| {
                        let lane_task = store
                            .branch
                            .strip_prefix("multiverse/")
                            .unwrap_or(&store.branch);
                        LaneHandback {
                            dirty: mv_state::worktrees::uncommitted_changes(&dispatched.launch.cwd),
                            handback: mv_state::worktrees::handback(
                                &store.project_root,
                                lane_task,
                                &store.branch,
                                &dispatched.launch.cwd,
                            ),
                        }
                    });
                    WaitOutcome {
                        status,
                        settled_state,
                        result_path: task_dir.join("result.md").display().to_string(),
                        status_path: task_dir.join("status").display().to_string(),
                        scope_violations: violations,
                        revisions,
                        lane_handback,
                    }
                });
                Ok(Plan {
                    role: minted_role_key.to_string(),
                    seat: dispatched.launch.seat.clone(),
                    task_id: dispatched.launch.task_id.clone(),
                    dry_run_report: String::new(),
                    pre_dispatch_notes: std::mem::take(&mut store.reconcile_notes),
                    worktree,
                    branch,
                    linked_backlog,
                    backlog_link_failures,
                    harness: dispatched.launch.kind.clone(),
                    model: dispatched.launch.model.clone(),
                    cwd: dispatched.launch.cwd.display().to_string(),
                    agent: dispatched.launch.agent_name.clone(),
                    pane: dispatched.pane.clone(),
                    status_path: dispatched
                        .launch
                        .cwd
                        .join(".multiverse/tasks")
                        .join(&dispatched.launch.task_id)
                        .join("status")
                        .display()
                        .to_string(),
                    wait_outcome,
                })
            }
            // Critic finding 2 / the smoke finding's exit-code bug: a
            // trust/approval dialog is already recorded as
            // `failed-to-start` by `runner::dispatch`'s own cleanup, and
            // the prompt is never sent — the agent did not actually start.
            // Python records the same way and raises; this used to map
            // `NeedsAttention` to `Ok` so the CLI printed a successful
            // `Dispatched …` line and exited 0 anyway. It is now reported
            // like any other failed start: non-zero exit, with the pane
            // named (via `RunnerError`'s own `Display`) so a human knows
            // where to confirm the dialog before retrying.
            Err(error) => Err(EngineError::Refused(format!(
                "dispatch: {role_display} on {seat_name}: {error}"
            ))),
        }
    }
}

// ---------------------------------------------------------------------
// RealDispatchStore: the production mv_engine::runner::DispatchStore
// adapter over mv-state (routing log, task dirs, briefs, worktrees).
// ---------------------------------------------------------------------

/// Current role codes, on both the `--dry-run` preview and a real
/// dispatch — the preview used to preserve Python's retired codes
/// (`expl`/`fix`/…) for the same reason the card's `role` field did; see
/// this file's module doc.
fn role_short(role: Role) -> &'static str {
    match role {
        Role::Nexus => "nexu",
        Role::Scout => "scou",
        Role::Builder => "buil",
        Role::Archivist => "arch",
        Role::Critic => "crit",
        Role::Seer => "seer",
        Role::Council => "coun",
        Role::Artisan => "arti",
        Role::Watcher => "watc",
    }
}

/// The dispatch record's human-readable `reason` field — Python's
/// `Selection.reason` property (`accounts.py:178-190`).
pub(super) fn dispatch_reason(
    selection: &selector::Selection,
    role_display: &str,
    project_name: &str,
) -> String {
    if selection.skipped.is_empty() {
        format!("first entry in {role_display} chain for {project_name}")
    } else {
        let skipped = selection
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.seat, s.reason))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "entry {} in {role_display} chain for {project_name}; skipped {skipped}",
            selection.skipped.len() + 1
        )
    }
}

/// Either lock `DispatchStore::lock_fleet`/`lock_project` can hand back —
/// a fleet-wide `flock` or the project's own routing-log lock. Opaque to
/// `mv_engine::runner::dispatch`, which only ever holds and drops it — both
/// variants carry a live RAII guard that unlocks on `Drop` and are
/// otherwise never read.
#[allow(dead_code)]
pub(super) enum StoreGuard {
    Fleet(mv_core::lock::LockGuard),
    Project(mv_state::log::ProjectLock),
}

/// The production [`DispatchStore`]: everything [`PreparedLaunch`] needs,
/// resolved once by [`RealEngine::resolve`] and carried here for
/// `mv_engine::runner::dispatch` to call back into while the fleet/project
/// locks it manages are held. See this file's module doc and `result.md`
/// for what is and is not ported to full Python parity.
///
/// `pub(super)` (task `20261102-fixer-62c3b7`): `commands::council`'s own
/// `RealEngine` builds one of these per seated councillor (`convene` needs
/// N concurrent `DispatchStore`s, one per active seat, where `dispatch`
/// only ever needs one) via [`RealDispatchStore::for_seat`], reusing this
/// type's whole `DispatchStore` impl — worktree/no-worktree handling,
/// routing-log lines, lane setup, the real halt/capacity rechecks — rather
/// than a second adapter that could drift from this one.
pub(super) struct RealDispatchStore {
    config: Config,
    config_dir: PathBuf,
    project_name: String,
    project_root: PathBuf,
    role: Role,
    account: Account,
    model: String,
    task_id: String,
    /// `task_id` as first minted, before any collision-suffix rebind —
    /// Python's own `card.task_id_base` (`dispatch.py`), finalized once at
    /// construction and never touched again. [`Self::collision_rebind`]
    /// always rebinds from this, not from `task_id`, so a second rebind
    /// (`recheck_admission`'s early one, then `prepare`'s authoritative
    /// one) lands on the same candidate rather than drifting further on
    /// every call (card `20261105-b-28cb36`).
    task_id_base: String,
    task_text: String,
    reviewing: Vec<String>,
    column: String,
    inputs: Vec<String>,
    scope: Vec<String>,
    write_scope: String,
    effort: Option<String>,
    skills: Vec<String>,
    lane_setup: Option<String>,
    force: bool,
    outcome: &'static str,
    reason: String,
    /// Explicit `--title`, or the goal's first 120 characters, validated before a
    /// worktree or task dir ever exists (critic finding 9) and carried as
    /// the routing log's optional `title=` field.
    title: Option<String>,
    /// `--into <TASK_ID>`: adopt that lane's already-existing worktree
    /// instead of carving a fresh one from HEAD (critic finding 1, card
    /// `20261102-b-0a4062`). Refused at [`RealEngine::dispatch`] before
    /// this store is even built when the role has no write scope.
    into: Option<String>,
    /// Set by [`prepare`](DispatchStore::prepare) once it knows whether
    /// this launch got its own worktree.
    isolated: bool,
    /// The worktree's branch name, or empty for a reader (never given a
    /// lane) — the routing log's `branch=` field is always written, empty
    /// or not (`accounts.py:240-253`).
    branch: String,
    /// Set by [`setup_lane`](DispatchStore::setup_lane) on its own
    /// failure, so [`record_failed_to_start`](DispatchStore::record_failed_to_start)
    /// can record `state=lane-setup-failed` instead of the generic
    /// `state=failed-to-start` (critic finding 8).
    lane_setup_failed: bool,
    /// The pool and token [`RealEngine::resolve`]'s selection actually
    /// claimed a half-open breaker trial under, if any — carried so
    /// [`release_trial`](DispatchStore::release_trial) can release the
    /// exact trial this dispatch holds on a failed start (card
    /// `20261102-b-02df4c`).
    trial: Option<(String, String)>,
    /// `--no-reconcile`: skip the pre-admission fleet-wide convergence pass
    /// [`DispatchStore::recheck_admission`] otherwise runs — Python
    /// `cli.py:3539` (`args.no_reconcile`). `false` (reconcile runs) for a
    /// council seat, which has no such flag of its own.
    no_reconcile: bool,
    /// `--commits SHA...`, oldest-first — [`Self::review_scope`]'s own
    /// explicit diff override, only meaningful with exactly one
    /// `reviewing` lane (Python `dispatch.py:1001`). Always empty for a
    /// council seat, which has no `--commits` of its own.
    commits: Vec<String>,
    /// Pre-dispatch "reconciled"/"reclaimed"/"superseded"/"lease-expired"
    /// lines [`recheck_admission`](DispatchStore::recheck_admission) fills
    /// in — Python `cli.py:3547-3692`, printed by `cmd_dispatch` before its
    /// own `_run_dispatch` call. Always empty for a council seat, which
    /// skips the pass entirely (`FleetLock::AlreadyHeld`).
    reconcile_notes: Vec<String>,
}

impl RealDispatchStore {
    fn new(resolved: Resolved, request: &Request) -> Self {
        let role_settings = resolved.config.roles.get(&resolved.role);
        let write_scope = role_settings
            .map(|r| r.write_scope.clone())
            .unwrap_or_else(|| "none".to_string());
        let effort = if resolved.role == Role::Critic {
            Some(resolved.config.review_policy.effort.clone())
        } else {
            role_settings.and_then(|r| r.effort.clone())
        };
        let skills = role_settings.map(|r| r.skills.clone()).unwrap_or_default();
        let lane_setup = resolved
            .config
            .projects
            .get(&resolved.project_name)
            .and_then(|p| p.lane_setup.clone());
        let outcome = if resolved.selection.is_fallback() {
            "chain-fallback"
        } else {
            "chain-primary"
        };
        let reason = dispatch_reason(
            &resolved.selection,
            &resolved.role_display,
            &resolved.project_name,
        );
        let account = resolved.config.accounts[&resolved.selection.seat.name].clone();

        Self {
            config: resolved.config,
            config_dir: resolved.config_dir,
            project_name: resolved.project_name,
            project_root: resolved.project_root,
            role: resolved.role,
            account,
            model: resolved.model,
            task_id_base: resolved.task_id.clone(),
            task_id: resolved.task_id,
            task_text: request.task.clone(),
            reviewing: request.reviewing.clone(),
            commits: request.commits.clone(),
            column: request
                .column
                .clone()
                .unwrap_or_else(|| stage_for_role(resolved.role).into()),
            inputs: request.inputs.clone(),
            scope: request.scope.clone(),
            write_scope,
            effort,
            skills,
            lane_setup,
            force: request.force,
            outcome,
            reason,
            title: resolved_title(request).ok().filter(|s| !s.is_empty()),
            into: request.into.clone(),
            isolated: false,
            branch: String::new(),
            lane_setup_failed: false,
            no_reconcile: request.no_reconcile,
            trial: resolved
                .selection
                .trial_token
                .clone()
                .map(|token| (resolved.selection.seat.pool.clone(), token)),
            reconcile_notes: Vec::new(),
        }
    }

    /// Build a store for one council seat, already resolved by
    /// `commands::council`'s own `RealEngine` (config, project, account,
    /// model, task id, brief text) — a council seat has no `--into`/
    /// `--scope`/`--force`/`--wait-for-seat` of its own, so this does not
    /// take a `dispatch::Request` the way [`RealDispatchStore::new`] does.
    ///
    /// `is_fallback`: Python's `Selection.outcome` is `chain-fallback`
    /// whenever seating this account skipped at least one earlier chain
    /// entry first (`accounts.py:1546-1549`) — true of every council seat
    /// but, in practice, the very first one, since each later seat's own
    /// walk restarts from the top of the chain with every already-seated
    /// vendor excluded, and those earlier entries are themselves skipped
    /// (`SkipCause.VENDOR_EXCLUDED`) before this seat's account is reached.
    /// Previously hardcoded `"chain-primary"` here on the mistaken premise
    /// that council "never falls back within the one seat it tries per
    /// vendor" — true of a single seat's own walk, but council seats run
    /// several walks against the *same* chain, and it is walks after the
    /// first whose own outcome this missed (card `<this task>`, scout2-
    /// gaps.md row #30's accompanying council divergence).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn for_seat(
        config: Config,
        config_dir: PathBuf,
        project_name: String,
        project_root: PathBuf,
        role: Role,
        account: Account,
        model: String,
        task_id: String,
        task_text: String,
        reason: String,
        is_fallback: bool,
    ) -> Self {
        let role_settings = config.roles.get(&role);
        let write_scope = role_settings
            .map(|r| r.write_scope.clone())
            .unwrap_or_else(|| "none".to_string());
        let effort = if role == Role::Critic {
            Some(config.review_policy.effort.clone())
        } else {
            role_settings.and_then(|r| r.effort.clone())
        };
        let skills = role_settings.map(|r| r.skills.clone()).unwrap_or_default();
        let lane_setup = config
            .projects
            .get(&project_name)
            .and_then(|p| p.lane_setup.clone());
        Self {
            config,
            config_dir,
            project_name,
            project_root,
            role,
            account,
            model,
            task_id_base: task_id.clone(),
            task_id,
            task_text,
            reviewing: Vec::new(),
            commits: Vec::new(),
            column: stage_for_role(role).into(),
            inputs: Vec::new(),
            scope: Vec::new(),
            write_scope,
            effort,
            skills,
            lane_setup,
            force: false,
            outcome: if is_fallback {
                "chain-fallback"
            } else {
                "chain-primary"
            },
            reason,
            title: None,
            into: None,
            isolated: false,
            branch: String::new(),
            lane_setup_failed: false,
            trial: None,
            no_reconcile: false,
            reconcile_notes: Vec::new(),
        }
    }

    /// The `## What to review` section of a Critic's `--reviewing` card —
    /// port of Python's `_render_reviewer_scope_section` (card
    /// `20261102-b-82436e`: the diff's own `base`/`stat_text`/`truncated`
    /// metadata, plus the `require_cross_vendor_for` risk-question block;
    /// task `20261106-fixer-fbf55c` then ported the `--commits`/`rereview`-
    /// aware re-diffing Python's own docstring describes under "EXACT
    /// INPUTS"). The lane's own `## review guide` section is still not
    /// ported — out of this task's scope.
    fn review_scope(&self) -> Result<String, RunnerError> {
        if self.reviewing.is_empty() {
            return Ok(String::new());
        }
        let policy = &self.config.review_policy;
        let mut parts = vec!["## What to review".to_string()];
        if policy.scope == "diff" {
            parts.push("Read ONLY the files each lane's diff below touches, plus any direct caller you name yourself — do not explore the rest of the repo. That is what keeps a cross-vendor review cheap on a scarce seat.".into());
            if !policy.run_tests {
                parts.push("The orchestrator has already run the mutations and the full suite for the work below — do NOT run tests or the suite yourself; read and reason only.".into());
            }
            if self.project_root.join(".multiverse/codemap.md").is_file() {
                parts.push("Project codemap: `.multiverse/codemap.md` — read it directly if you need orientation; not inlined here.".into());
            }
        }
        // Python `dispatch.py:1001`: an explicit `--commits` override is
        // only meaningful when it can name a single lane unambiguously —
        // `commits_warning` (`run`, above) already told the operator it is
        // ignored otherwise, so this mirrors that same one-lane guard
        // rather than re-deriving a different answer here.
        let explicit_commits: &[String] = if self.reviewing.len() == 1 {
            &self.commits
        } else {
            &[]
        };
        for task in &self.reviewing {
            parts.push(format!("\n### Lane {task}"));
            if policy.scope == "diff" {
                let (diff, diff_note): (mv_engine::lane_diff::LaneDiff, &'static str) =
                    if !explicit_commits.is_empty() {
                        (
                            mv_engine::lane_diff::commit_set_diff(
                                &self.project_root,
                                explicit_commits,
                                policy.max_diff_lines,
                            ),
                            "against `--commits`'s own explicit list",
                        )
                    } else {
                        // Python `dispatch.py:1016-1025`: a lane already
                        // reviewed once (`last_reviewed_commit` finds a
                        // mark) under `rereview = "diff"` gets only its own
                        // fix-round commits since that mark — not the whole
                        // lane again. First review, or `rereview = "full"`,
                        // still gets the full `lane_diff` against the
                        // lane's own cut point.
                        let since =
                            mv_state::worktrees::last_reviewed_commit(&self.project_root, task);
                        match since {
                            Some(since) if policy.rereview == "diff" => {
                                let fix_commits = mv_engine::lane_diff::fix_round_commits(
                                    &self.project_root,
                                    task,
                                    &since,
                                )
                                .map_err(|e| {
                                    RunnerError::Refused(format!("--reviewing {task:?}: {e}"))
                                })?;
                                (
                                    mv_engine::lane_diff::commit_set_diff(
                                        &self.project_root,
                                        &fix_commits,
                                        policy.max_diff_lines,
                                    ),
                                    "against this lane's own fix-round commits since its last \
                                     review — not the whole lane again",
                                )
                            }
                            _ => (
                                mv_engine::lane_diff::lane_diff(
                                    &self.project_root,
                                    task,
                                    policy.max_diff_lines,
                                )
                                .map_err(|e| {
                                    RunnerError::Refused(format!("--reviewing {task:?}: {e}"))
                                })?,
                                "against this lane's own cut point",
                            ),
                        }
                    };
                parts.push(format!("Diffed {diff_note}, from `{}`.", diff.base));
                parts.push("\nTouched files:".into());
                if diff.touched_files.is_empty() {
                    parts.push("- (none)".into());
                }
                for path in &diff.touched_files {
                    parts.push(format!("- {path}"));
                }
                parts.push("\nDiff stat:".into());
                parts.push("```".into());
                parts.push(if diff.stat_text.is_empty() {
                    "(empty)".to_string()
                } else {
                    diff.stat_text.clone()
                });
                parts.push("```".into());
                let truncation_note = if diff.truncated {
                    format!(
                        " (truncated at {} lines — read the rest from the lane's own branch if you need it)",
                        policy.max_diff_lines
                    )
                } else {
                    String::new()
                };
                parts.push(format!("\nUnified diff{truncation_note}:"));
                parts.push(format!(
                    "```diff\n{}\n```",
                    redact_text(if diff.body_text.is_empty() {
                        "(empty)"
                    } else {
                        diff.body_text.trim_end()
                    })
                ));
            } else {
                let branch = mv_state::worktrees::branch_name(task)
                    .map_err(|e| RunnerError::Refused(e.to_string()))?;
                parts.push(format!("`review_scope = \"repo\"` — explore the repository as needed. This lane's work is on branch `{branch}`."));
            }
        }
        let questions = mv_engine::risk_paths::risk_questions(&policy.require_cross_vendor_for);
        if !questions.is_empty() {
            parts.push("\nRisk questions this policy calls out:".into());
            for question in questions {
                parts.push(format!("- {question}"));
            }
        }
        Ok(parts.join("\n"))
    }

    fn build_brief(
        &self,
        cwd: &Path,
        context: Vec<String>,
        role_display: &str,
    ) -> Result<mv_engine::brief::Brief, RunnerError> {
        // Critic finding 7: a missing role/stage/skills prompt file used to
        // be swallowed by `.unwrap_or_default()`, so a bad prompts
        // directory still started a pane with an empty contract instead of
        // failing the dispatch at plan time — `mv_engine::prompts`'s own
        // `MissingPromptError` already exists precisely so this does not
        // have to be guessed from an empty string.
        let role_prompt = mv_engine::prompts::role_prompt(self.role.config_key())
            .map_err(|e| RunnerError::Refused(format!("role prompt: {e}")))?
            .body();
        let stage_prompt = mv_engine::prompts::column_prompt(&self.column)
            .map_err(|e| RunnerError::Refused(format!("stage prompt: {e}")))?
            .body();
        let skill_names: Vec<&str> = self.skills.iter().map(String::as_str).collect();
        let skills_prompt = mv_engine::skills::render(&skill_names)
            .map_err(|e| RunnerError::Refused(format!("skills: {e}")))?;

        let mut brief = mv_engine::brief::Brief::new(
            self.task_id.clone(),
            role_display,
            self.account.name.clone(),
            cwd.to_path_buf(),
            self.task_text.clone(),
        );
        brief.isolated = self.isolated;
        brief.context = context;
        brief.write_scope = self.write_scope.clone();
        brief.scope = self.scope.clone();
        brief.result_path = format!(".multiverse/tasks/{}/result.md", self.task_id);
        brief.status_path = format!(".multiverse/tasks/{}/status", self.task_id);
        brief.revise_path = format!(".multiverse/tasks/{}/revise", self.task_id);
        brief.role_prompt = role_prompt;
        brief.stage_prompt = stage_prompt;
        brief.skills_prompt = skills_prompt;

        // Critic finding 6: definition-of-done and constraints were left
        // empty, rendering as the brief template's own "(not specified)"/
        // "- (none)" placeholders on every live dispatch. An observable
        // DoD always names this task's own result/status paths; a
        // read-only role additionally leads its constraints with the rule
        // that actually governs it, ahead of the two standing bullets
        // every dispatch gets.
        brief.definition_of_done = format!(
            "`{}` contains the findings, and `{}` contains `done`.",
            brief.result_path, brief.status_path
        );
        let mut constraints = Vec::new();
        if self.write_scope == "none" {
            constraints.push(
                "**Read-only.** Do not create, modify, or delete any file except \
                 the result and status paths named above."
                    .to_string(),
            );
        }
        constraints.push("Do not refactor beyond the stated scope.".to_string());
        constraints.push(
            "Reference paths and line numbers; do not paste file contents into the result."
                .to_string(),
        );
        brief.constraints = constraints;

        // Review metadata is shared by the preview and the dispatched brief.
        if self.role == Role::Critic {
            brief.review_lanes = self.reviewing.clone();
            brief.review_scope_section = self.review_scope()?;
            let rp = &self.config.review_policy;
            brief.review_policy = format!(
                "effort = {:?}\nscope = {:?}\nmax_diff_lines = {}\ntimeout_minutes = {}\n\
                 batch_max_lanes = {}\nmin_quota_percent = {}\nrun_tests = {}\n\
                 require_cross_vendor_for = {:?}",
                rp.effort,
                rp.scope,
                rp.max_diff_lines,
                rp.timeout_minutes,
                rp.batch_max_lanes,
                rp.min_quota_percent,
                rp.run_tests,
                rp.require_cross_vendor_for,
            );
        }

        Ok(brief)
    }

    fn task_dir(&self, cwd: &Path) -> PathBuf {
        cwd.join(".multiverse").join("tasks").join(&self.task_id)
    }

    /// The collision-rebind check [`prepare`](DispatchStore::prepare) has
    /// always used — an isolated writer's candidate is taken when its
    /// branch already exists, a reader's when its task directory already
    /// exists in the shared checkout — pulled apart so
    /// [`DispatchStore::recheck_admission`] can also run it, early and
    /// read-only, under the fleet lock alone (card `20261105-b-28cb36`,
    /// finding 5's remainder). `--into` never rebinds: it deliberately
    /// reuses an *existing* lane's branch under a brand-new id, so there is
    /// no collision to resolve. Calling this twice on the same free
    /// candidate is idempotent (`rebind_free_task_id`'s own `taken(base)`
    /// check returns it unchanged), which is what makes `prepare`'s own
    /// later call, under the project lock, a safe authoritative re-check
    /// rather than a second independent rebind.
    fn collision_rebind(&self, base: &str) -> Result<String, RunnerError> {
        if self.into.is_some() {
            return Ok(base.to_string());
        }
        if self.write_scope != "none" {
            let worktrees = mv_state::worktrees::Worktrees::new(&self.project_root)
                .map_err(|e| RunnerError::Refused(format!("worktree: {e}")))?;
            rebind_free_task_id(base, |candidate| {
                worktrees
                    .branch_exists(candidate)
                    .map_err(|e| RunnerError::Refused(format!("worktree: {e}")))
            })
        } else {
            let tasks_dir = self.project_root.join(".multiverse/tasks");
            rebind_free_task_id(base, |candidate| Ok(tasks_dir.join(candidate).exists()))
        }
    }

    fn append_routing(&self, line: &str) -> Result<(), RunnerError> {
        mv_state::log::ProjectLog::new(&self.project_root)
            .append(line)
            .map_err(|e| RunnerError::Refused(format!("routing log: {e}")))?;
        Ok(())
    }

    /// Python `accounts.append_outcome_log`, called from
    /// `DispatchRunner.dispatch`'s own `--wait` leg (`runner.py:2869-2877`)
    /// the instant its own wait observes a settled turn with a usable
    /// status file. Deliberately unconditional — never gated by
    /// `--no-reconcile` (which only gates the pre-admission hygiene pass in
    /// [`reconcile_and_reclaim_fleet`]) and never skipped because a poorer
    /// record already exists: this is the authoritative result, from the
    /// runner that actually watched the turn settle, not a reconciler's
    /// later guess (`state=reconciled`, no pane, revisions=0) — Python's own
    /// doc comment on `append_outcome_log` calls exactly that scenario "exactly
    /// backwards". `state` is the raw Herdr `agent_status` at settle
    /// (`live.settled.state`), distinct from `status`, the agent's own
    /// status file — same two fields `WaitOutcome` already carries for
    /// `--wait`'s own printed report. Best-effort like `record_started`: a
    /// logging failure here must not turn an otherwise-successful `--wait`
    /// into a reported error.
    fn record_wait_result(
        &self,
        task_id: &str,
        status: mv_state::tasks::TaskStatus,
        state: AgentState,
        revisions: usize,
        pane: &str,
    ) {
        let when = mv_core::time::now_stamp();
        let mut line = format!(
            "{when}\tevent=result\ttask={task_id}\tstatus={status}\tstate={state}\trevisions={revisions}"
        );
        if !pane.is_empty() {
            line.push_str(&format!("\tpane={pane}"));
        }
        let _ = self.append_routing(&line);
    }
}

/// Closes every reclaimable pane `reclaimable_panes` found at `root`,
/// re-confirming each one is still live, under its own task's agent, and
/// ready for input, immediately before closing it — the same "confirm
/// right before mutation" posture `commands::tasks::reclaimable_and_report`
/// already takes for `tasks --reclaimable --yes` (`reclaim_pane_is_safe`),
/// and `sweep_project`'s own second listing (`mv_state::tasks`). A pane id
/// is reusable: a bare pane-id match against a possibly-stale `list()`
/// snapshot can agree with an unrelated agent that now occupies the
/// recorded task's old pane, and closing on that match alone destroys
/// that agent's session instead of merely freeing a slot nobody needs
/// anymore (critic-grok3.md row 1, card `20261105-b-28cb36`). Never
/// reconciles (`reconcile_settled`) or converges leases for `root`: a
/// foreign project must never be mutated beyond this reclaim predicate by
/// a dispatch whose own admission can still be refused afterward (Python's
/// "finding 2", `cli.py:3463-3470`).
///
/// The `stale` half of `reclaimable_panes`'s return (a recorded, clean
/// task whose pane is already gone — nothing here to close) is
/// deliberately left unwritten: Python's own pre-dispatch pass discards it
/// too (`_candidates, _ = _reclaimable_panes(...)`, `cli.py:3600`;
/// `candidates, _stale = reclaimable_panes(...)`, `cli.py:2699`). Only the
/// explicit `tasks --reclaimable --yes` operator command
/// (`commands::tasks::reclaimable_and_report`) records that bookkeeping —
/// a `dispatch` call must never write it as a side effect, recorded or
/// not, of trying to admit a different task.
fn reclaim_stale_panes(root: &Path, agents: &mut impl mv_state::tasks::AgentPanes) -> usize {
    let (candidates, _stale) = mv_state::tasks::reclaimable_panes(root, agents);
    if candidates.is_empty() {
        return 0;
    }
    // A failed probe is "nothing known live", never "nothing is live" —
    // the same posture `AgentPanes::list_checked`'s own doc comment
    // requires, and the same read `supersede_collision_predecessor` and
    // `sweep_project` already give it.
    let Some(live_now) = agents.list_checked() else {
        return 0;
    };
    let mut reclaimed = 0;
    for candidate in &candidates {
        let Some(current) = live_now.iter().find(|a| a.pane_id == candidate.pane) else {
            continue;
        };
        if candidate.agent.as_deref().unwrap_or("") != current.name.as_deref().unwrap_or("") {
            continue;
        }
        if !current
            .status
            .is_some_and(mv_engine::mv_herdr::envelope::AgentState::is_ready_for_input)
        {
            continue;
        }
        if agents.close(&candidate.pane) {
            reclaimed += 1;
        }
    }
    reclaimed
}

/// `{account name: minutes}` for every account with its own
/// `awaiting_lease_minutes_override` — [`mv_state::tasks::expire_awaiting`]'s
/// per-account lease, consulted before `config.awaiting_lease_minutes`.
/// Mirrors Python `cli.py:_awaiting_lease_overrides`; duplicated here
/// (rather than shared with `commands::sweep`'s own private helper of the
/// same shape) since this file's write scope does not extend to
/// `sweep.rs`.
fn lease_overrides(config: &Config) -> HashMap<String, u32> {
    config
        .accounts
        .values()
        .filter_map(|account| {
            account
                .awaiting_lease_minutes_override
                .map(|minutes| (account.name.clone(), minutes))
        })
        .collect()
}

/// Python `cli.py:3452-3543`: before admitting a new pane anywhere, converge
/// every configured project's recorded state and free anything already
/// safely reclaimable. One `self.project_name` — the one actually being
/// dispatched into — gets the full pass (reconcile its own
/// settled-but-unrecorded tasks, auto-supersede/lease-expire whatever is
/// still genuinely AWAITING past `awaiting_lease_minutes`, then reclaim);
/// every other configured project gets the reclaim leg only. Skipped
/// entirely by `--no-reconcile`. `require_live: true` mirrors Python's own
/// `if live is None: return` — a Herdr probe that failed must never be
/// read as grounds to lease-expire or auto-supersede anything.
///
/// The collision-suffixed-retry predecessor supersede
/// (`supersede_open_redispatch`) Python runs even under `--no-reconcile` is
/// deliberately not folded into this function — it runs from
/// [`DispatchStore::recheck_admission`] directly, before this function is
/// even called, via [`supersede_collision_predecessor`] (card
/// `20261105-b-28cb36`), since it needs the rebound `task_id` and must run
/// unconditionally on `--no-reconcile`, which this function's own caller
/// skips calling entirely. Also unlike Python's single fleet-wide
/// `agent list`, each project here gets its own fresh listing
/// (`AgentPanes::list`/`reclaimable_panes` each call it internally) rather
/// than one shared snapshot — more Herdr calls, not a correctness
/// difference.
fn reconcile_and_reclaim_fleet(
    config: &Config,
    dispatching_project: &str,
    dispatching_root: &Path,
    agents: &mut impl mv_state::tasks::AgentPanes,
) -> Vec<String> {
    let overrides = lease_overrides(config);
    let outcome = mv_state::tasks::reconcile_project(
        dispatching_root,
        mv_state::tasks::ReconcileRequest {
            only_task: None,
            close_panes: false,
            expire_stale: true,
            lease_minutes: config.awaiting_lease_minutes,
            lease_overrides: &overrides,
            now: mv_core::time::now_utc(),
            require_live: true,
        },
        agents,
    );
    let reclaimed = reclaim_stale_panes(dispatching_root, agents);
    // Python `cli.py:3669-3692`: reconciled, then reclaimed, then
    // superseded, then lease-expired — in that order, each only when its
    // own count is nonzero.
    let mut notes = Vec::new();
    if !outcome.recorded.is_empty() {
        notes.push(format!(
            "  reconciled {} outcome(s) in {dispatching_project} before dispatching",
            outcome.recorded.len()
        ));
    }
    if reclaimed > 0 {
        notes.push(format!(
            "  reclaimed {reclaimed} pane(s) in {dispatching_project} before dispatching"
        ));
    }
    if !outcome.superseded.is_empty() {
        notes.push(format!(
            "  superseded {} stale record(s) in {dispatching_project} before dispatching",
            outcome.superseded.len()
        ));
    }
    if outcome.expired > 0 {
        notes.push(format!(
            "  lease-expired {} task(s) in {dispatching_project} before dispatching",
            outcome.expired
        ));
    }
    for (name, project) in &config.projects {
        if name == dispatching_project {
            continue;
        }
        let Some(raw) = project.path.value() else {
            continue;
        };
        let Ok(root) = std::fs::canonicalize(raw) else {
            continue;
        };
        let reclaimed = reclaim_stale_panes(&root, agents);
        if reclaimed > 0 {
            notes.push(format!(
                "  reclaimed {reclaimed} pane(s) in {name} before dispatching"
            ));
        }
    }
    notes
}

/// Port of Python's `dispatch.redispatch_predecessor_ids`
/// (`dispatch.py:549-567`): every id a collision-suffixed `task_id` could
/// have superseded, nearest suffix first, so a live `base-2` lane is
/// preferred over an older unsuffixed `base` lane. `()` (here, an empty
/// `Vec`) whenever `task_id` is not itself a `base-N` suffix of `base`, or
/// `N < 2` — `rebind_free_task_id` never mints `base-0`/`base-1`, so
/// neither has a predecessor to settle.
fn redispatch_predecessor_ids(task_id: &str, base: &str) -> Vec<String> {
    let Some(suffix) = task_id
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return Vec::new();
    };
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return Vec::new();
    }
    let Ok(instance) = suffix.parse::<u64>() else {
        return Vec::new();
    };
    if instance < 2 {
        return Vec::new();
    }
    let mut ids: Vec<String> = (2..instance).rev().map(|n| format!("{base}-{n}")).collect();
    ids.push(base.to_string());
    ids
}

/// Port of Python's `runner.supersede_open_redispatch` (`runner.py:222-
/// 372`), called from [`DispatchStore::recheck_admission`] once a collision
/// rebind has actually happened (`task_id != task_id_base`): settle and
/// close a still-live predecessor lane before this new, collision-suffixed
/// dispatch is admitted, so a redispatch of identical material never
/// leaves two panes open for the same lineage.
///
/// Safety gates sit in front of the close, using checks this codebase
/// already established elsewhere rather than inventing new ones (card
/// `20261105-b-28cb36`, "SAFETY FIRST"):
///
/// - **Wrong occupant, never closed.** A pane id is reusable: finding the
///   predecessor's recorded pane still listed is not evidence that the
///   predecessor is still *in* it. Every match against `live`/`fresh` below
///   also requires the live agent's name to equal the predecessor's own
///   recorded `agent=` (`SweepTask.agent`), exactly as
///   [`mv_state::tasks::reclaimable_panes`]'s sibling confirmations do —
///   never a bare pane-id match (review finding 1).
/// - **Dirty lane, never closed.** [`mv_state::worktrees::uncommitted_changes`]
///   — the same probe [`Worktrees::adopt`](mv_state::worktrees::Worktrees::adopt)'s
///   own clean check is built on — refuses the new dispatch outright
///   (mirrors Python's `RedispatchRefusedError`) rather than closing a pane
///   over uncommitted work. An unreadable tree (git itself failed) refuses
///   the same way as a dirty one: "could not tell" is never read as "safe
///   to close", matching Python's own `except WorktreeError`.
/// - **Unrescued output, never closed.** [`mv_state::tasks::lanes_holding_unrescued_output`]
///   — the exact check `commands::stop` already refuses to close a pane
///   over — is consulted before the close here too.
/// - **Working/blocked/unknown pane, never closed.** Only a pane Herdr
///   reports ready for input (`AgentState::is_ready_for_input`, the same
///   gate `worktrees::occupant_refusal` already uses for an `--into`
///   occupant) is closed. A predecessor that is still genuinely working is
///   left exactly alone: no outcome recorded, no refusal raised, nothing
///   closed — the new dispatch simply proceeds alongside it, since the
///   only thing this function's `--no-reconcile`-surviving contract
///   promises is that it never *destroys* a live predecessor.
///
/// All four gates above run twice: once on the first listing, to decide
/// whether this predecessor is even worth pursuing, and a second time,
/// fresh, immediately before the close — both re-checks (identity,
/// readiness, dirty, unrescued output) under the single project lock taken
/// just before them. A human prompting the pane, or another process
/// dirtying the lane or writing output, between the first snapshot and
/// here must abort the close rather than race it; mirrors Python's own
/// second, race-narrowing `uncommitted_changes` check immediately before
/// the close itself (`runner.py:330-369`), broadened to cover every gate
/// this port added, not just the one Python re-checks (review finding 2).
///
/// The close itself only runs once lifecycle evidence
/// (`append_supersede_if_absent`/`record_stranded_closure`) has actually
/// been written — an append that errors refuses the whole call rather than
/// closing a pane with nothing on record to explain why. `agents.close`'s
/// own boolean is checked too: a close that fails after evidence was
/// written refuses the call (never a silent `Ok(())`), because the
/// predecessor is still live and the new dispatch must not be admitted
/// alongside it (review finding 3).
///
/// Card `20261105-b-bfc75b` ("codex re-review", beyond Python parity):
/// pane id plus agent name, both read from one `list`/`list_checked`
/// snapshot, is not *durable* identity — a pane id is reusable and (same
/// role/seat) an agent name is reissuable by a later, unrelated dispatch —
/// and the readiness snapshot above can still go stale in the gap between
/// the last check and the `pane close` call itself (Python has the same
/// window; this port cannot close it to zero without either a naming
/// change that would break cross-implementation parity, or an atomic
/// close-if-name primitive Herdr itself does not expose). What this port
/// *can* do, and now does: every confirmation from the two-gates-above
/// discovery onward asks Herdr directly for the predecessor's own name
/// ([`mv_state::tasks::AgentPanes::confirm_live`], backed by `agent get
/// <name>` rather than another `agent list` scan), carries its
/// `state_change_seq` forward as a same-session witness (a continuing
/// session's own counter never goes backwards, so a lower value later is
/// proof of a *different* session wearing the same name), and runs that
/// confirmation as the very last read before the write and the close —
/// after, not before, the slower dirty/unrescued-output rechecks — so the
/// window between "confirmed" and "closed" is as narrow as this client can
/// make it.
fn supersede_collision_predecessor(
    root: &Path,
    task_id: &str,
    task_id_base: &str,
    agents: &mut impl mv_state::tasks::AgentPanes,
) -> Result<(), RunnerError> {
    let predecessor_ids = redispatch_predecessor_ids(task_id, task_id_base);
    if predecessor_ids.is_empty() {
        return Ok(());
    }
    // A failed probe is "nothing known live", never "nothing is live" —
    // same posture `AgentPanes::list_checked`'s own doc comment requires.
    // Mirrors Python's own `if live_agents is None: return None`.
    let Some(live) = agents.list_checked() else {
        return Ok(());
    };

    let tasks = mv_state::tasks::sweep_tasks(root);
    let by_id: HashMap<&str, &mv_state::tasks::SweepTask> = tasks
        .iter()
        .map(|task| (task.task_id.as_str(), task))
        .collect();
    let same_occupant = |task: &mv_state::tasks::SweepTask,
                         agent: &mv_state::tasks::LiveAgentStatus| {
        agent.name.as_deref().unwrap_or("") == task.agent.as_deref().unwrap_or("")
    };
    let predecessor = predecessor_ids.iter().find_map(|candidate| {
        let task = *by_id.get(candidate.as_str())?;
        let pane = task.pane.as_deref()?;
        let live_agent = live.iter().find(|agent| agent.pane_id == pane)?;
        if !same_occupant(task, live_agent) {
            return None;
        }
        Some((task, live_agent.clone()))
    });
    let Some((predecessor, live_agent)) = predecessor else {
        return Ok(());
    };
    let ready = |agent: &mv_state::tasks::LiveAgentStatus| {
        agent
            .status
            .is_some_and(mv_engine::mv_herdr::envelope::AgentState::is_ready_for_input)
    };
    if !ready(&live_agent) {
        return Ok(());
    }

    // Card 20261105-b-bfc75b: pane id + agent name read from one `list`/
    // `list_checked` snapshot is not durable identity across time — a
    // pane id is reusable, and (same role/seat) an agent name is
    // reissuable by a later, unrelated dispatch. From here on, every
    // further confirmation asks Herdr directly for this exact name
    // ([`mv_state::tasks::AgentPanes::confirm_live`]) rather than
    // re-deriving identity from another bare listing, and carries
    // `state_change_seq` forward as a same-session witness: a continuing
    // session's own counter never goes backwards, so a lower value at the
    // close-time recheck below is proof this is a *different* session now
    // wearing the same name, not a stale read of the one found here.
    let agent_name = live_agent.name.clone().unwrap_or_default();
    let Some(baseline) = agents.confirm_live(&live_agent.pane_id, &agent_name) else {
        return Ok(());
    };
    if !baseline.ready {
        return Ok(());
    }

    let lane = mv_state::tasks::lane_task_id(predecessor).to_string();
    let lane_root = root.join(".multiverse/worktrees").join(&lane);
    let check_dirty = |lane_root: &Path| -> Result<(), RunnerError> {
        if !lane_root.is_dir() {
            return Ok(());
        }
        match mv_state::worktrees::uncommitted_changes(lane_root) {
            None => Err(RunnerError::Refused(format!(
                "cannot safely re-dispatch over {:?}: could not inspect its lane {}. \
                 Resume or rescue the old task first.",
                predecessor.task_id,
                lane_root.display()
            ))),
            Some(status) if !status.is_empty() => Err(RunnerError::Refused(format!(
                "refusing to re-dispatch {task_id:?}: predecessor {:?}'s lane {} has \
                 uncommitted changes. Resume or rescue the old task first; nothing was \
                 closed.",
                predecessor.task_id,
                lane_root.display()
            ))),
            Some(_) => Ok(()),
        }
    };
    let check_unrescued = || -> Result<(), RunnerError> {
        let held = mv_state::tasks::lanes_holding_unrescued_output(root)
            .map_err(|e| RunnerError::Refused(format!("checking lane output: {e}")))?;
        if held.contains(&lane) {
            return Err(RunnerError::Refused(format!(
                "refusing to re-dispatch {task_id:?}: predecessor {:?}'s lane {lane} holds \
                 unrescued task output. Resume or rescue the old task first; nothing was closed.",
                predecessor.task_id
            )));
        }
        Ok(())
    };
    check_dirty(&lane_root)?;
    check_unrescued()?;

    // Finding 2: re-verify every gate immediately before the close, under
    // the project lock that now also guards the write — a stale first
    // snapshot is never trusted as the authority for a destructive close.
    let _project_lock = mv_state::log::ProjectLog::new(root)
        .lock()
        .map_err(|e| RunnerError::Refused(format!("project lock: {e}")))?;

    // Card 20261105-b-bfc75b's close-time TOCTOU: dirty and
    // unrescued-output are re-checked first — both can take real wall
    // time (a git subprocess, a filesystem walk) — so the identity/
    // readiness/same-session confirmation below is the very last read
    // before the write and the close, not before these slower ones. This
    // is as narrow as this client can make the window without an atomic
    // close-if-name primitive in Herdr itself; Python has the same
    // residual window (card's own note).
    check_dirty(&lane_root)?;
    check_unrescued()?;
    let Some(confirmed) = agents.confirm_live(&live_agent.pane_id, &agent_name) else {
        return Ok(());
    };
    if !confirmed.ready {
        return Ok(());
    }
    if let (Some(base_seq), Some(fresh_seq)) =
        (baseline.state_change_seq, confirmed.state_change_seq)
    {
        if fresh_seq < base_seq {
            // A different session is now answering to the same name in
            // the same pane — not the predecessor we found above.
            return Ok(());
        }
    }

    let reason = format!(
        "superseded by re-dispatch {task_id:?}; its live pane {:?} was closed",
        live_agent.pane_id
    );
    if predecessor.recorded {
        let supersedes = predecessor.outcome_ordinal.ok_or_else(|| {
            RunnerError::Refused(format!(
                "refusing to re-dispatch {task_id:?}: predecessor {:?} is recorded with no \
                 supersedable outcome; nothing was closed.",
                predecessor.task_id
            ))
        })?;
        let status = predecessor
            .status
            .unwrap_or(mv_state::tasks::TaskStatus::Failed);
        mv_state::tasks::append_supersede_if_absent(
            root,
            &predecessor.task_id,
            supersedes,
            status.as_str(),
            mv_state::tasks::SUPERSEDED_REDISPATCHED,
            &reason,
            predecessor.revisions,
            None,
        )
        .map_err(|e| {
            RunnerError::Refused(format!(
                "refusing to re-dispatch {task_id:?}: could not record predecessor {:?} \
                 superseded: {e}; nothing was closed.",
                predecessor.task_id
            ))
        })?;
    } else {
        let status = predecessor
            .file_status
            .unwrap_or(mv_state::tasks::TaskStatus::Failed);
        if !mv_state::tasks::record_stranded_closure(
            root,
            &predecessor.task_id,
            status,
            mv_state::tasks::SUPERSEDED_REDISPATCHED,
            predecessor.pane.as_deref(),
        ) {
            return Err(RunnerError::Refused(format!(
                "refusing to re-dispatch {task_id:?}: could not record predecessor {:?} \
                 superseded; nothing was closed.",
                predecessor.task_id
            )));
        }
    }

    if !agents.close(&live_agent.pane_id) {
        return Err(RunnerError::Refused(format!(
            "refusing to re-dispatch {task_id:?}: predecessor {:?} was recorded superseded but \
             its live pane {:?} could not be closed. Investigate the pane manually before \
             retrying; the new dispatch must not be admitted alongside a predecessor that may \
             still be live.",
            predecessor.task_id, live_agent.pane_id
        )));
    }
    Ok(())
}

impl DispatchStore for RealDispatchStore {
    type Guard = StoreGuard;

    fn lock_fleet(&mut self) -> Result<StoreGuard, RunnerError> {
        let path = self.config_dir.join(FLEET_LOCK_FILENAME);
        let guard = mv_core::lock::acquire(&path, "fleet", mv_core::lock::LOCK_ACQUIRE_TIMEOUT)
            .map_err(|e| RunnerError::Refused(format!("fleet lock: {e}")))?;
        Ok(StoreGuard::Fleet(guard))
    }

    fn lock_project(&mut self) -> Result<StoreGuard, RunnerError> {
        let guard = mv_state::log::ProjectLog::new(&self.project_root)
            .lock()
            .map_err(|e| RunnerError::Refused(format!("project lock: {e}")))?;
        Ok(StoreGuard::Project(guard))
    }

    /// Finding 5's fresh recheck: current fleet/project capacity, then —
    /// only for a capped seat — a fresh live open-count probe for that one
    /// seat, failing closed (at-cap) when Herdr cannot be asked. Also runs
    /// the pre-admission fleet-wide reconcile/reclaim pass first (Python
    /// `cli.py:3452-3543`), unless `--no-reconcile` was given.
    ///
    /// Before any of that: finding 5's remainder (card `20261105-b-28cb36`).
    /// `prepare` is the only place that ever rebinds a colliding `task_id`,
    /// and it only runs later, under the project lock `runner::dispatch`
    /// takes around it — too late to know, here, whether *this* dispatch is
    /// itself a collision retry. Python does not have this problem because
    /// `card.task_id` is finalized long before any lock
    /// (`cli.py:3502`/`3563`). Rebinding here too, under the fleet lock
    /// alone (no project lock exists yet, so this is sequential with
    /// `prepare`'s own later acquire, never nested), recovers the same
    /// ordering: once the real id is known, a still-live predecessor under
    /// an earlier suffix (or the bare base) is settled and its pane closed
    /// — [`supersede_collision_predecessor`] — before this new pane is ever
    /// admitted. Runs even under `--no-reconcile`, matching Python's own
    /// `cli.py:3502-3525`, which this port's broader reconcile/reclaim pass
    /// below still skips under that flag.
    fn recheck_admission(&mut self) -> Result<(), RunnerError> {
        if self.into.is_none() {
            self.task_id = self.collision_rebind(&self.task_id_base)?;
            if self.task_id != self.task_id_base {
                let mut agents = mv_state::tasks::HerdrAgents;
                supersede_collision_predecessor(
                    &self.project_root,
                    &self.task_id,
                    &self.task_id_base,
                    &mut agents,
                )?;
            }
        }

        if !self.no_reconcile {
            let mut agents = mv_state::tasks::HerdrAgents;
            self.reconcile_notes = reconcile_and_reclaim_fleet(
                &self.config,
                &self.project_name,
                &self.project_root,
                &mut agents,
            );
        }
        let (fleet_open, other_floor_gaps) = fleet_open_counts(&self.config, &self.project_name);
        let project = &self.config.projects[&self.project_name];
        let capacity = engine_dispatch::FleetCapacityInputs {
            max_concurrent: self.config.max_concurrent,
            fleet_open,
            project_open: open_task_count(&self.project_root),
            project_min_slots: project.min_slots,
            project_max_slots: project.max_slots,
            other_floor_gaps,
            batch_count: 1,
        };
        engine_dispatch::check_fleet_capacity(&capacity)
            .map_err(|e| RunnerError::Refused(e.to_string()))?;

        if let Some(cap) = mv_engine::seatstate::get_cap(&self.config_dir, &self.account.name) {
            let open_now = match live_open_counts(&self.config) {
                Some(counts) => counts.get(&self.account.short).copied().unwrap_or(0),
                // Herdr unreadable reads as at-cap, the same fail-closed
                // posture `seat_from_account` already takes at plan time.
                None => cap,
            };
            if open_now >= cap {
                return Err(RunnerError::Refused(format!(
                    "seat {:?} is now at its open-agent cap ({open_now}/{cap} open)",
                    self.account.name
                )));
            }
        }
        Ok(())
    }

    /// Allocate/verify the lane (a fresh worktree for a writer, the shared
    /// checkout for a reader), mint the agent name, and render the brief —
    /// everything [`PreparedLaunch`] carries. Runs under both locks.
    fn prepare(&mut self) -> Result<PreparedLaunch, RunnerError> {
        self.isolated = self.write_scope != "none";
        let cwd = if self.isolated {
            let worktrees = mv_state::worktrees::Worktrees::new(&self.project_root)
                .map_err(|e| RunnerError::Refused(format!("worktree: {e}")))?;

            // Critic finding 5: `task_id` is a pure hash of (project, role,
            // task text, UTC date), so a second `--yes` dispatch of
            // identical material on the same day mints the exact same id —
            // and would otherwise fail outright on the owner branch
            // `create` already refused once before. Rebind to the next
            // free numeric suffix instead, under the project lock this
            // runs inside (`runner::dispatch` holds it through `prepare`),
            // the same way `unique_agent_name` already rebinds a colliding
            // agent name. Does not apply to `--into`, which deliberately
            // reuses an *existing* lane's branch under a brand-new id.
            //
            // Authoritative re-check, not the only one: `recheck_admission`
            // (finding 5's remainder, card `20261105-b-28cb36`) already ran
            // this same [`Self::collision_rebind`] under the fleet lock
            // alone, before any project lock existed, so it could settle a
            // live predecessor first. Rebinding again here, from the
            // original `task_id_base` and under the project lock this
            // `prepare` call actually runs inside, is what makes that
            // earlier rebind safe to trust — idempotent in the common case,
            // a fresh answer if the candidate was claimed in between.
            if self.into.is_none() {
                self.task_id = self.collision_rebind(&self.task_id_base)?;
            }

            // Critic finding 1 / card 20261102-b-0a4062: `--into` continues
            // this task in an already-existing writer's lane (its worktree,
            // its branch, its uncommitted work) instead of always carving a
            // fresh one from HEAD. `Engine::dispatch` already refused this
            // for a reader before this store was even built, so reaching
            // here with `into: Some(_)` means a writer asked to adopt.
            // `Worktrees::adopt` is the same check `multiverse worktrees
            // prune` already trusts to judge a lane reusable: registered,
            // not live, no occupant task still unsettled.
            let lane = match &self.into {
                Some(into) => {
                    let mut observer =
                        mv_state::worktrees::HerdrLaneObserver::new(worktrees.project());
                    worktrees
                        .adopt(into, &mut observer)
                        .map_err(|e| RunnerError::Refused(format!("--into {into:?}: {e}")))?
                }
                None => worktrees
                    .create(&self.task_id, "HEAD")
                    .map_err(|e| RunnerError::Refused(format!("worktree create: {e}")))?,
            };
            self.branch = lane.branch;
            lane.path
        } else {
            // Critic finding 5, reader side: the collision is an
            // already-occupied task directory in the shared checkout
            // rather than a git branch. Same authoritative-re-check
            // relationship with `recheck_admission`'s early rebind as the
            // isolated branch above.
            self.task_id = self.collision_rebind(&self.task_id_base)?;
            self.project_root.clone()
        };

        // Critic finding 4: `--input` used to be copied nowhere — the
        // brief just listed the operator's own original paths, which an
        // isolated writer cannot read (its own module doc forbids reading
        // outside the lane) and a reader would otherwise follow straight
        // out to wherever the operator happened to be standing. Each input
        // is copied into `.multiverse/inputs/<task_id>/`, namespaced by
        // task id so two unrelated dispatches' same-named input can never
        // collide in a reader's shared, never-pruned checkout — deliberately
        // *not* inside `.multiverse/tasks/<task_id>/`, which `write_brief`
        // (`mv-engine::runner`) still requires to be freshly created on
        // every dispatch as its own redispatch-collision signal; creating
        // it early here would defeat that check. The brief references each
        // copy's relative path instead of the original. Two `--input`
        // basenames colliding is refused up front, before anything is
        // copied.
        let context = if self.inputs.is_empty() {
            Vec::new()
        } else {
            let invocation_cwd = std::env::current_dir().map_err(|e| {
                RunnerError::Refused(format!("could not read the current directory: {e}"))
            })?;
            let inputs_dir = cwd.join(".multiverse").join("inputs").join(&self.task_id);
            std::fs::create_dir_all(&inputs_dir)
                .map_err(|e| RunnerError::Refused(format!("--input: {e}")))?;
            std::fs::set_permissions(&inputs_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| RunnerError::Refused(format!("--input: {e}")))?;
            let mut seen = HashSet::new();
            let mut context = Vec::with_capacity(self.inputs.len());
            for raw in &self.inputs {
                let source = resolve_user_path(raw, &invocation_cwd)
                    .map_err(|e| RunnerError::Refused(format!("--input {raw:?}: {e}")))?;
                let basename = source
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| {
                        RunnerError::Refused(format!("--input {raw:?} has no usable file name"))
                    })?
                    .to_string();
                if !seen.insert(basename.clone()) {
                    return Err(RunnerError::Refused(format!(
                        "--input {raw:?}: another --input already uses the basename {basename:?}"
                    )));
                }
                std::fs::copy(&source, inputs_dir.join(&basename))
                    .map_err(|e| RunnerError::Refused(format!("--input {raw:?}: {e}")))?;
                context.push(format!(".multiverse/inputs/{}/{basename}", self.task_id));
            }
            context
        };

        let config_dir_value = self.account.config_dir.value().ok_or_else(|| {
            RunnerError::Refused(format!(
                "account {:?} has an unresolved config_dir placeholder",
                self.account.name
            ))
        })?;
        let environment = vec![(
            self.account.config_dir_var.clone(),
            config_dir_value.to_string(),
        )];
        let command =
            (!self.account.command.trim().is_empty()).then(|| self.account.command.clone());
        let clear_auth = if command.is_some() {
            Vec::new()
        } else {
            AUTH_VARS_TO_CLEAR.iter().map(|s| s.to_string()).collect()
        };
        let mut native_args =
            argv::native_args(&self.model, &self.account.kind, self.effort.as_deref());
        // Card 20261105-b-50b425: every dispatched claude seat — an
        // ordinary dispatch's own `new()` and a council seat's `for_seat()`
        // alike, since both share this one `prepare` — gets the same
        // trust/permission shape Python's `Card.agent_args` always attaches
        // to a claude harness (`--permission-mode bypassPermissions` plus
        // the Oracle's `--disallowedTools` rules). Before this, only a
        // claude-kind orchestrator launch (`mv_engine::trust::
        // claude_orchestrator_launch`, wired into `commands::start`) ever
        // carried any of this; a dispatched specialist's launch args
        // stopped at `--model`/`--effort`.
        if self.account.kind == "claude" {
            // `resolve_home_dir` (not `mv_config::load::home_dir_from_env`'s
            // `/` fallback): the result below anchors every deny-list rule
            // `claude_specialist_launch_args` builds (`~/.ssh/**`,
            // `~/.gitconfig`, ...), so an unresolved home must refuse the
            // launch rather than silently protect `/.ssh/**` instead of the
            // real one (card `20261106-reviewer-abf805`'s finding).
            let home = mv_config::load::resolve_home_dir().ok_or_else(|| {
                RunnerError::Refused(
                    "cannot resolve a home directory for the claude seat (no $HOME and no \
                     passwd entry for the current user) — refusing rather than writing \
                     deny-list rules anchored at /"
                        .to_string(),
                )
            })?;
            let directives_root = mv_engine::directives::directives_root(&self.config_dir);
            native_args.extend(mv_engine::trust::claude_specialist_launch_args(
                Path::new(config_dir_value),
                &home,
                &directives_root,
                &self.write_scope,
                &self.project_root,
            ));
        }
        let short = self
            .config
            .projects
            .get(&self.project_name)
            .and_then(|p| p.short.as_deref())
            .unwrap_or("");
        let agent_name = runner::naming::agent_name(
            &self.project_name,
            role_short(self.role),
            None,
            &self.account.short,
            short,
        )?;

        let brief = self.build_brief(&cwd, context, self.role.display_name())?;

        Ok(PreparedLaunch {
            project_root: self.project_root.clone(),
            cwd,
            task_id: self.task_id.clone(),
            agent_name,
            role: self.role.config_key().to_string(),
            kind: self.account.kind.clone(),
            model: self.model.clone(),
            seat: self.account.name.clone(),
            brief: brief.render(),
            native_args,
            environment,
            clear_auth,
            command,
        })
    }

    /// Best-effort in-flight reservation: Python also folds a routing
    /// log's own not-yet-resolved dispatches into this set
    /// (`runner.py:2464`), which this adapter does not — `mv_engine::
    /// runner::dispatch` already unions whatever this returns with a live
    /// `agent list`, so an empty set here only means one less layer of
    /// protection against naming a brand-new agent the same as one whose
    /// pane has not shown up in that listing yet, not an incorrect one.
    fn unique_agent_name(
        &self,
        _base: &str,
        taken: &HashSet<String>,
    ) -> Result<String, RunnerError> {
        let short = self
            .config
            .projects
            .get(&self.project_name)
            .and_then(|p| p.short.as_deref())
            .unwrap_or("");
        runner::naming::unique_agent_name(
            &self.project_name,
            role_short(self.role),
            &self.account.short,
            short,
            taken,
        )
    }

    fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
        Ok(HashSet::new())
    }

    /// `role=` is this task's own current name (task `20261102-fixer-e1e2c3`:
    /// new dispatches are new records and use the new role names; a
    /// history reader like `commands::tasks`/`mv_state::tasks` still
    /// accepts a retired name from a routing log written before this).
    ///
    /// Critic finding 9: `title=` (JSON-escaped, immediately before
    /// `branch=`) used to be dropped entirely, so `--title` never reached
    /// the log or the board.
    fn record_dispatch(&mut self, launch: &PreparedLaunch) -> Result<(), RunnerError> {
        let when = mv_core::time::now_stamp();
        let mut line = format!(
            "{when}\tevent=dispatch\ttask={task}\tproject={project}\trole={role}\taccount={account}\t\
             pool={pool}\toutcome={outcome}\treason={reason}\tagent={agent}",
            task = self.task_id,
            project = self.project_name,
            role = self.role.config_key(),
            account = self.account.name,
            pool = self.account.pool,
            outcome = self.outcome,
            reason = self.reason,
            agent = launch.agent_name,
        );
        if let Some(title) = &self.title {
            line.push_str(&format!("\ttitle={}", json_quote(title)));
        }
        line.push_str(&format!("\tbranch={}", self.branch));
        self.append_routing(&line)
    }

    fn check_halt_before_pane_creation(&mut self) -> Result<(), RunnerError> {
        // Python's `DispatchRunner.dispatch` (`runner.py`) never reads the
        // fleet halt at all — both of its halt checks live in `cli.py`'s
        // `dispatch`-specific `cmd_dispatch`/`_run_dispatch`, which
        // `council.py` never calls on its way to the same shared runner
        // (`council.py:637`: `DispatchRunner().dispatch(...)` directly). A
        // council seat refusing here (conformance seed 50003) is this
        // check applying a dispatch-only rule to a command Python never
        // gates on halt at all.
        if self.role == Role::Council {
            return Ok(());
        }
        let halt = RealHalt {
            state_dir: &self.config_dir,
        };
        engine_dispatch::check_halt_before_pane_creation(&halt, self.force)
            .map_err(|e| RunnerError::Refused(e.to_string()))
    }

    fn record_started(
        &mut self,
        launch: &PreparedLaunch,
        pane: &str,
        started_at: SystemTime,
    ) -> Result<(), RunnerError> {
        let when = mv_core::time::format_seconds_utc(time::OffsetDateTime::from(started_at));
        let line = format!(
            "{when}\tevent=started\ttask={}\tpane={pane}",
            launch.task_id
        );
        self.append_routing(&line)
    }

    /// `state=lane-setup-failed` (critic finding 8), not the generic
    /// `failed-to-start`, when [`setup_lane`](DispatchStore::setup_lane)
    /// is what actually failed — it runs after the agent has already
    /// started, so conflating the two states would read as a start/prompt
    /// miss in the log when the agent in fact came up fine and only the
    /// configured setup command failed.
    fn record_failed_to_start(
        &mut self,
        launch: &PreparedLaunch,
        pane: &str,
    ) -> Result<(), RunnerError> {
        let when = mv_core::time::now_stamp();
        let state = if self.lane_setup_failed {
            "lane-setup-failed"
        } else {
            "failed-to-start"
        };
        let mut line = format!(
            "{when}\tevent=result\ttask={}\tstatus=failed\tstate={state}\trevisions=0",
            launch.task_id
        );
        if !pane.is_empty() {
            line.push_str(&format!("\tpane={pane}"));
        }
        self.append_routing(&line)
    }

    /// Trip this dispatch's own seat's pool breaker on a settled/failed
    /// pane's vendor-exhaustion verdict — the production wiring
    /// `DispatchStore::trip_breaker`'s own doc comment calls for.
    /// `self.account` is the exact seat `RealDispatchStore::new`/`for_seat`
    /// resolved this dispatch onto, so its `pool`/`vendor` are used rather
    /// than anything re-derived from `launch`. `breaker::trip` derives its
    /// own cooldown from `verdict.kind` and writes `pane_text` as evidence
    /// under `self.project_root` itself; a failure here (including the
    /// vendor `"local"` refusal, which `limits::classify` never produces a
    /// verdict for in the first place) is surfaced as an `Err` for the
    /// caller to warn on, never a panic.
    fn trip_breaker(
        &mut self,
        launch: &PreparedLaunch,
        verdict: &limits::Verdict,
        pane_text: &str,
        now: time::OffsetDateTime,
    ) -> Result<bool, RunnerError> {
        let _ = launch;
        breaker::trip(
            &self.config_dir,
            &self.account.pool,
            now,
            breaker::TripRequest {
                vendor: &self.account.vendor,
                kind: verdict.kind,
                rule: &verdict.rule,
                caller: "dispatch",
                evidence_path: None,
                pane_text: Some(pane_text),
                project_root: Some(&self.project_root),
                manual: false,
            },
        )
        .map_err(|e| RunnerError::Refused(format!("trip breaker: {e}")))?;
        Ok(true)
    }

    /// Runs `[projects.<name>].lane_setup` for a writer lane only — Python
    /// never runs lane setup in the shared checkout a reader uses, since
    /// it exists to prepare a *fresh* worktree. Synchronous, POSIX `sh`,
    /// per `AGENTS.md` — no new async runtime, just a poll loop over
    /// `Child::try_wait` bounded by [`LANE_SETUP_TIMEOUT`] (critic finding
    /// 8), with stdout/stderr drained concurrently on their own threads so
    /// a chatty command cannot deadlock on a full pipe while this thread is
    /// only polling for exit. The combined, redacted output always lands in
    /// `lane-setup.log` beside the brief — even on a timeout kill — for the
    /// same reason a failed dispatch's pane stays open: evidence of what
    /// went wrong, with credentials scrubbed before they ever touch disk.
    fn setup_lane(&mut self, launch: &PreparedLaunch) -> Result<(), RunnerError> {
        if !self.isolated {
            return Ok(());
        }
        let Some(command) = self.lane_setup.clone().filter(|c| !c.trim().is_empty()) else {
            return Ok(());
        };
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .current_dir(&launch.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                self.lane_setup_failed = true;
                RunnerError::Refused(format!("lane_setup: {e}"))
            })?;
        let mut stdout_pipe = child.stdout.take().expect("piped stdout");
        let mut stderr_pipe = child.stderr.take().expect("piped stderr");
        let stdout_reader = thread::spawn(move || -> Vec<u8> {
            let mut buffer = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut buffer);
            buffer
        });
        let stderr_reader = thread::spawn(move || -> Vec<u8> {
            let mut buffer = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buffer);
            buffer
        });

        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {}
                Err(e) => {
                    self.lane_setup_failed = true;
                    return Err(RunnerError::Refused(format!("lane_setup: {e}")));
                }
            }
            if start.elapsed() >= LANE_SETUP_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(100));
        };

        let stdout = stdout_reader.join().unwrap_or_default();
        let stderr = stderr_reader.join().unwrap_or_default();
        let mut log = redact_text(&String::from_utf8_lossy(&stdout)).into_bytes();
        log.push(b'\n');
        log.extend_from_slice(redact_text(&String::from_utf8_lossy(&stderr)).as_bytes());
        let log_path = self.task_dir(&launch.cwd).join("lane-setup.log");
        let _ = std::fs::write(&log_path, log);

        let Some(status) = status else {
            self.lane_setup_failed = true;
            return Err(RunnerError::Refused(format!(
                "lane_setup did not finish within {LANE_SETUP_TIMEOUT:?}; see {}",
                log_path.display()
            )));
        };
        if !status.success() {
            self.lane_setup_failed = true;
            return Err(RunnerError::Refused(format!(
                "lane_setup exited {:?}; see {}",
                status.code(),
                log_path.display()
            )));
        }
        Ok(())
    }

    /// Releases the exact half-open trial `RealEngine::resolve`'s
    /// selection claimed on this dispatch's behalf (card
    /// `20261102-b-02df4c`), if any — `self.trial` is only `Some` when
    /// `Selection::trial_token` actually came back set, i.e. the chosen
    /// seat's pool was half-open and this dispatch is the one holding the
    /// trial. Best effort, like every other cleanup step here: a storage
    /// failure must not replace the dispatch error that caused this to run
    /// (`runner::dispatch`'s own `store.warn` path surfaces it instead).
    fn release_trial(&mut self) -> Result<(), RunnerError> {
        let Some((pool, token)) = &self.trial else {
            return Ok(());
        };
        breaker::release_trial(&self.config_dir, pool, &self.task_id, token)
            .map_err(|e| RunnerError::Refused(format!("release trial: {e}")))?;
        Ok(())
    }

    fn warn(&mut self, message: &str) {
        eprintln!("warning: {message}");
    }
}

pub fn run_default(args: &ArgMatches) -> Result<(), Failure> {
    run(args, &RealEngine, &mut std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;
    use engine_dispatch::HaltQuery;

    struct Fake {
        plan: Result<Plan, EngineError>,
        dispatch: Result<Plan, EngineError>,
    }

    impl Engine for Fake {
        fn plan(&self, _request: &Request) -> Result<Plan, EngineError> {
            self.plan.clone()
        }

        fn dispatch(&self, _request: &Request, _plan: Plan) -> Result<Plan, EngineError> {
            self.dispatch.clone()
        }
    }

    fn plan() -> Plan {
        Plan {
            role: "builder".into(),
            seat: "claude".into(),
            task_id: "20261102-builder-abc123".into(),
            dry_run_report: "Would dispatch builder on claude (20261102-builder-abc123).\n".into(),
            pre_dispatch_notes: Vec::new(),
            worktree: None,
            branch: None,
            linked_backlog: Vec::new(),
            backlog_link_failures: Vec::new(),
            harness: "claude".into(),
            model: "claude-test".into(),
            cwd: "/repo".into(),
            agent: "repo-buil-cl".into(),
            pane: "w1:p1".into(),
            status_path: "/repo/.multiverse/tasks/20261102-builder-abc123/status".into(),
            wait_outcome: None,
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("dispatch").unwrap().clone()
    }

    /// Backlog `20261108-b-bdd00b`: every test that can reach
    /// `mv_config::load::config_state_dir_from_env` — directly, through
    /// `RealEngine::resolve`'s `real_breaker`/gauge/seat-state reads, or
    /// through [`RealDispatchStore::trip_breaker`]'s `limits::Verdict`
    /// handling — must hold one of these instead of a bare
    /// `HerdrEnvGuard::unreachable()`. On a dev machine `AGENTS.md`'s own
    /// dispatched-lane env sets `$MULTIVERSE_STATE_DIR`/
    /// `$HERDR_PLUGIN_STATE_DIR`, which `config_state_dir_from_env`
    /// checks before any tempdir default; without this, these tests read
    /// and wrote the live fleet's `breaker-state.json`/`seat-state.json`
    /// (card `20261103-b-a5c59f`'s `trip_breaker` test left a real
    /// `codex:test`/`grok:test` breaker entry behind this way).
    ///
    /// Wraps [`HerdrEnvGuard`] — rather than taking `herdr_env::lock()` a
    /// second time — so a test that also needs `HERDR_BIN_PATH` pointed
    /// at a nonexistent binary gets that for free, and the one lock
    /// acquisition still serializes every env-mutating test in this
    /// binary, state-dir vars included.
    struct StateDirEnvGuard {
        _herdr: HerdrEnvGuard,
        _dir: tempfile::TempDir,
        previous_state_dir: Option<std::ffi::OsString>,
        previous_plugin_state_dir: Option<std::ffi::OsString>,
        previous_config: Option<std::ffi::OsString>,
    }

    impl StateDirEnvGuard {
        fn new() -> Self {
            let herdr = HerdrEnvGuard::unreachable();
            let dir = tempfile::tempdir().unwrap();
            let previous_state_dir = std::env::var_os("MULTIVERSE_STATE_DIR");
            let previous_plugin_state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
            let previous_config = std::env::var_os("MULTIVERSE_CONFIG");
            std::env::set_var("MULTIVERSE_STATE_DIR", dir.path());
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", dir.path());
            std::env::remove_var("MULTIVERSE_CONFIG");
            Self {
                _herdr: herdr,
                _dir: dir,
                previous_state_dir,
                previous_plugin_state_dir,
                previous_config,
            }
        }
    }

    impl Drop for StateDirEnvGuard {
        fn drop(&mut self) {
            match self.previous_state_dir.take() {
                Some(value) => std::env::set_var("MULTIVERSE_STATE_DIR", value),
                None => std::env::remove_var("MULTIVERSE_STATE_DIR"),
            }
            match self.previous_plugin_state_dir.take() {
                Some(value) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", value),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            match self.previous_config.take() {
                Some(value) => std::env::set_var("MULTIVERSE_CONFIG", value),
                None => std::env::remove_var("MULTIVERSE_CONFIG"),
            }
        }
    }

    #[test]
    fn request_from_captures_this_lanes_flags() {
        let args = matches(&[
            "dispatch",
            "proj",
            "task text",
            "--yes",
            "--role",
            "builder",
            "--into",
            "lane-a",
            "--input",
            "a.md",
            "--input",
            "b.md",
            "--scope",
            "src",
            "--reviewing",
            "lane-a",
            "--reviewing",
            "lane-b",
            "--full",
            "--column",
            "Review",
            "--timeout",
            "45",
            "--exclude-vendor",
            "codex",
            "--force",
            "--title",
            "title",
            "--wait-for-seat",
            "--wait-for-seat-seconds",
            "12.5",
            "--wait",
        ]);
        let request = request_from(&args);
        assert_eq!(request.project_path, "proj");
        assert_eq!(request.task, "task text");
        assert_eq!(request.role.as_deref(), Some("builder"));
        assert_eq!(request.into.as_deref(), Some("lane-a"));
        assert_eq!(request.inputs, vec!["a.md", "b.md"]);
        assert_eq!(request.scope, vec!["src"]);
        assert_eq!(request.exclude_vendor, vec!["codex"]);
        assert_eq!(request.reviewing, vec!["lane-a", "lane-b"]);
        assert!(request.commits.is_empty());
        assert!(request.full);
        assert_eq!(request.column.as_deref(), Some("Review"));
        assert_eq!(request.timeout_seconds, 45.0);
        assert!(request.force);
        assert_eq!(request.title.as_deref(), Some("title"));
        assert!(request.wait_for_seat);
        assert_eq!(request.wait_for_seat_seconds, Some(12.5));
        assert!(request.wait);
        assert!(!request.dry_run);
    }

    #[test]
    fn request_from_captures_repeatable_commits() {
        let args = matches(&[
            "dispatch",
            "proj",
            "task",
            "--reviewing",
            "lane-a",
            "--commits",
            "abc",
            "--commits",
            "def",
        ]);
        let request = request_from(&args);
        assert_eq!(request.commits, vec!["abc", "def"]);
        assert!(commits_warning(&request.commits, &request.reviewing).is_none());
        assert_eq!(
            commits_warning(&["abc".into()], &[]).unwrap(),
            "warning: --commits only applies with exactly one --reviewing id (0 given) — ignored."
        );
        assert_eq!(
            commits_warning(&["abc".into()], &["a".into(), "b".into()]).unwrap(),
            "warning: --commits only applies with exactly one --reviewing id (2 given) — ignored."
        );
    }

    #[test]
    fn critic_without_reviewing_or_exclude_vendor_warns_like_the_python_oracle() {
        // `conformance/diff_fuzz.py` seed 9000: Python prints this warning
        // on stderr before the ReviewerVendorUnknown refusal.
        assert_eq!(
            critic_vendor_warning(Some("critic"), &[], &[]).as_deref(),
            Some(
                "warning: dispatching Critic without --reviewing or --exclude-vendor — no author vendor was supplied, so vendor exclusion is not applied and review may land on the author's own vendor."
            )
        );
        assert!(critic_vendor_warning(Some("critic"), &["lane".into()], &[]).is_none());
        assert!(critic_vendor_warning(Some("builder"), &[], &[]).is_none());
    }

    #[test]
    fn cited_inputs_only_matches_inside_a_backtick_span() {
        assert_eq!(
            cited_inputs("read `.multiverse/input/scout-gaps.md` before you start"),
            vec!["scout-gaps.md".to_string()]
        );
        // A bare mention — no backticks — is prose describing the mechanism,
        // not a citation (this brief's own text does exactly this).
        assert!(cited_inputs("cites .multiverse/input/<name> with nothing behind it").is_empty());
        // A directory mention, with no basename after it, is not a citation.
        assert!(cited_inputs("see `.multiverse/input/` for staged files").is_empty());
        assert_eq!(
            cited_inputs("`.multiverse/input/a.md` and `.multiverse/input/b.md`"),
            vec!["a.md".to_string(), "b.md".to_string()]
        );
    }

    #[test]
    fn check_read_only_flags_refuses_input_and_warns_on_scope() {
        let mut request = Request {
            project_path: String::new(),
            task: String::new(),
            role: None,
            into: None,
            inputs: vec!["a.md".into()],
            scope: vec![],
            exclude_vendor: vec![],
            reviewing: vec![],
            commits: vec![],
            full: false,
            column: None,
            timeout_seconds: 900.0,
            force: false,
            title: None,
            wait_for_seat: false,
            wait_for_seat_seconds: None,
            wait: false,
            dry_run: false,
            no_reconcile: false,
            config_override: None,
        };
        assert!(matches!(
            check_read_only_flags(&request, "Scout", "none"),
            Err(EngineError::Refused(message)) if message.contains("--input is for a writing role")
        ));
        // A writing role is never refused or warned, regardless of flags.
        assert_eq!(
            check_read_only_flags(&request, "Builder", "declared").unwrap(),
            None
        );
        request.inputs.clear();
        request.scope = vec!["src".into()];
        let warning = check_read_only_flags(&request, "Scout", "none")
            .unwrap()
            .unwrap();
        assert!(warning.contains("warning: --scope is for a writing role"));
        assert!(warning.contains("this path"));
        request.scope.push("tests".into());
        let warning = check_read_only_flags(&request, "Scout", "none")
            .unwrap()
            .unwrap();
        assert!(warning.contains("these paths"));
    }

    #[test]
    fn check_cited_inputs_refuses_a_citation_nothing_stages() {
        let dir = tempfile::tempdir().unwrap();
        // Writing role: a citation not among the staged --input basenames
        // refuses, even though a same-named file exists on disk elsewhere.
        let error = check_cited_inputs(
            "read `.multiverse/input/spec.md` first",
            "Builder",
            dir.path(),
            "declared",
            &["other.md".to_string()],
        )
        .unwrap_err();
        assert!(matches!(&error, EngineError::Refused(m) if m.contains("no --input landed")));
        check_cited_inputs(
            "read `.multiverse/input/spec.md` first",
            "Builder",
            dir.path(),
            "declared",
            &["/tmp/anything/spec.md".to_string()],
        )
        .unwrap();

        // Read-only role: a citation is only good if the shared checkout
        // already has that basename under `.multiverse/input/`.
        let error = check_cited_inputs(
            "read `.multiverse/input/spec.md` first",
            "Scout",
            dir.path(),
            "none",
            &[],
        )
        .unwrap_err();
        assert!(matches!(&error, EngineError::Refused(m) if m.contains("read-only")));
        let shared = dir.path().join(".multiverse").join("input");
        std::fs::create_dir_all(&shared).unwrap();
        std::fs::write(shared.join("spec.md"), "spec").unwrap();
        check_cited_inputs(
            "read `.multiverse/input/spec.md` first",
            "Scout",
            dir.path(),
            "none",
            &[],
        )
        .unwrap();

        // No citation at all is always fine.
        check_cited_inputs("no citation here", "Scout", dir.path(), "none", &[]).unwrap();
    }

    #[test]
    fn item_id_candidates_only_matches_a_goal_that_is_exactly_one_id() {
        assert_eq!(
            item_id_candidates("20261105-b-1cb911"),
            vec!["20261105-b-1cb911".to_string()]
        );
        // A single trailing newline is tolerated, same as Python's `$`.
        assert_eq!(
            item_id_candidates("20261105-b-1cb911\n"),
            vec!["20261105-b-1cb911".to_string()]
        );
        // Embedded in prose — the common case for a real dispatch goal —
        // never matches: Python's regex has no `re.MULTILINE`, so `^`/`$`
        // anchor the whole string, not a line within it.
        assert!(item_id_candidates("Card 20261105-b-1cb911 — fix the gap").is_empty());
        assert!(item_id_candidates("20261105-b-1cb911\nmore text").is_empty());
        assert!(item_id_candidates("20261105-b-zzzzzz").is_empty());
        assert!(item_id_candidates("").is_empty());
    }

    #[test]
    fn record_dispatch_backlog_links_only_links_a_goal_that_is_exactly_a_known_id() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        let item = mv_state::backlog::add(
            dir.path(),
            "a finding",
            "sandbox",
            "because",
            None,
            None,
            None,
            "P2",
            None,
        )
        .unwrap();

        let (linked, failures) =
            record_dispatch_backlog_links(dir.path(), &item.item, "20261102-fixer-abc123");
        assert_eq!(linked, vec![item.item.clone()]);
        assert!(failures.is_empty());

        // Mentioned inside prose, the same id links nothing — ported quirk.
        let goal = format!("see {} for context", item.item);
        let (linked, failures) =
            record_dispatch_backlog_links(dir.path(), &goal, "20261102-fixer-def456");
        assert!(linked.is_empty());
        assert!(failures.is_empty());

        // Id-shaped but naming nothing in this project's backlog: no-op,
        // not a failure.
        let (linked, failures) =
            record_dispatch_backlog_links(dir.path(), "20261102-b-ffffff", "20261102-fixer-ghijkl");
        assert!(linked.is_empty());
        assert!(failures.is_empty());
    }

    #[test]
    fn tree_state_reads_porcelain_status_and_is_none_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(tree_state(dir.path()).is_none());
        git(dir.path(), &["init", "-q", "-b", "main"]);
        assert_eq!(tree_state(dir.path()), Some(HashSet::new()));
        std::fs::write(dir.path().join("untracked.txt"), "hi").unwrap();
        let state = tree_state(dir.path()).unwrap();
        assert_eq!(state.len(), 1);
        assert!(state.iter().next().unwrap().contains("untracked.txt"));
    }

    #[test]
    fn scope_violations_only_fires_for_a_read_only_role_with_both_snapshots() {
        let mut before = HashSet::new();
        before.insert("?? old.txt".to_string());
        let mut after = before.clone();
        after.insert("?? new.txt".to_string());

        assert_eq!(
            scope_violations("none", Some(&before), Some(&after)),
            vec!["?? new.txt".to_string()]
        );
        // A writer is never checked — it has its own worktree instead.
        assert!(scope_violations("declared", Some(&before), Some(&after)).is_empty());
        // An unreadable tree on either side yields "could not tell", not a
        // false all-clear.
        assert!(scope_violations("none", None, Some(&after)).is_empty());
        assert!(scope_violations("none", Some(&before), None).is_empty());
    }

    #[test]
    fn wait_reports_a_scope_violation_for_a_read_only_turn() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Done),
            settled_state: None,
            result_path: "/repo/result.md".into(),
            status_path: "/repo/status".into(),
            scope_violations: vec!["M  src/main.rs".into()],
            revisions: Vec::new(),
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("SCOPE VIOLATION — 1 thing(s) changed"));
        assert!(printed.contains("    M  src/main.rs"));
        assert!(printed.contains("Nothing was reverted"));
    }

    /// Scout-gaps #4 remainder (card `20261105-b-5e4d10`): `--wait` must
    /// surface a result's own `revise` file, not only its terminal status.
    #[test]
    fn wait_prints_a_revisions_notice_when_the_result_asks_for_changes() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Done),
            settled_state: None,
            result_path: "/repo/result.md".into(),
            status_path: "/repo/status".into(),
            scope_violations: Vec::new(),
            revisions: vec!["docs/spec.md".into(), "README.md".into()],
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("THIS RESULT ASKS FOR 2 CHANGE(S)"));
        assert!(printed.contains("    docs/spec.md"));
        assert!(printed.contains("    README.md"));
        assert!(printed.contains("Nothing here has acted on that"));
    }

    /// Scout-gaps #4 remainder: a writer's lane gets the uncommitted-changes
    /// report and handback instructions, not only the bare status line.
    #[test]
    fn wait_prints_uncommitted_changes_and_handback_for_a_writer() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Done),
            settled_state: None,
            result_path: "/repo/.multiverse/worktrees/t/result.md".into(),
            status_path: "/repo/.multiverse/worktrees/t/status".into(),
            scope_violations: Vec::new(),
            revisions: Vec::new(),
            lane_handback: Some(LaneHandback {
                dirty: Some("?? scratch.txt".into()),
                handback: vec![
                    "To review what the agent did:".into(),
                    "    git -C /repo diff HEAD...multiverse/t".into(),
                    "To land it:".into(),
                    "    git -C /repo merge multiverse/t".into(),
                ],
            }),
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("UNCOMMITTED changes in the worktree:"));
        assert!(printed.contains("    ?? scratch.txt"));
        assert!(printed.contains("  To land it:"));
        assert!(printed.contains("    git -C /repo merge multiverse/t"));
    }

    #[test]
    fn read_revisions_trims_blank_lines_and_keeps_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("revise"),
            "docs/spec.md\n\n  README.md  \n\n",
        )
        .unwrap();
        assert_eq!(
            read_revisions(dir.path()),
            vec!["docs/spec.md".to_string(), "README.md".to_string()]
        );
        assert!(read_revisions(&dir.path().join("no-such-dir")).is_empty());
    }

    /// Scout-gaps #27: only a Critic dispatched `--reviewing` at least one
    /// lane, under diff-scope policy, should advance the marker.
    #[test]
    fn should_record_reviewed_requires_critic_reviewing_lanes_and_diff_scope() {
        let lanes = vec!["20261101-fixer-abcdef".to_string()];
        assert!(should_record_reviewed(Role::Critic, &lanes, "diff"));
        assert!(!should_record_reviewed(Role::Critic, &lanes, "full"));
        assert!(!should_record_reviewed(Role::Critic, &[], "diff"));
        assert!(!should_record_reviewed(Role::Builder, &lanes, "diff"));
    }

    struct FakeFleetAgents {
        live: Vec<mv_state::tasks::LiveAgentStatus>,
        closed: Vec<String>,
    }

    impl FakeFleetAgents {
        /// `(pane, agent name)`, reported idle — the shape the identity +
        /// readiness confirmation both `reclaim_stale_panes` and
        /// `supersede_collision_predecessor` now require before closing
        /// (critic-grok3.md row 1; supersede-review.md findings 1-2).
        fn idle(pane_id: &str, name: &str) -> mv_state::tasks::LiveAgentStatus {
            mv_state::tasks::LiveAgentStatus {
                pane_id: pane_id.to_string(),
                name: Some(name.to_string()),
                status: Some(mv_engine::mv_herdr::envelope::AgentState::Idle),
            }
        }
    }

    impl mv_state::tasks::AgentPanes for FakeFleetAgents {
        fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
            self.live
                .iter()
                .map(|agent| mv_state::tasks::LiveAgent {
                    pane_id: agent.pane_id.clone(),
                })
                .collect()
        }

        fn close(&mut self, pane_id: &str) -> bool {
            self.live.retain(|a| a.pane_id != pane_id);
            self.closed.push(pane_id.to_string());
            true
        }

        fn list_checked(&mut self) -> Option<Vec<mv_state::tasks::LiveAgentStatus>> {
            Some(self.live.clone())
        }
    }

    #[test]
    fn reconcile_and_reclaim_fleet_reconciles_its_own_project_only_and_reclaims_every_others() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("a");
        let root_b = dir.path().join("b");
        std::fs::create_dir_all(root_a.join(".multiverse")).unwrap();
        std::fs::create_dir_all(root_b.join(".multiverse")).unwrap();

        // Project a (dispatching into): a settled-but-unrecorded task with
        // no live pane — `reconcile_settled`'s own candidate shape.
        std::fs::write(
            root_a.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tbranch=main\trole=fixer\n",
        )
        .unwrap();
        std::fs::create_dir_all(root_a.join(".multiverse/tasks/20260101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            root_a.join(".multiverse/tasks/20260101-fixer-aaaaaa/status"),
            "done",
        )
        .unwrap();
        std::fs::write(
            root_a.join(".multiverse/tasks/20260101-fixer-aaaaaa/result.md"),
            "# Result\ndone.\n",
        )
        .unwrap();

        // Project b (every other configured project): already recorded
        // done, clean lane, pane still live — `reclaimable_panes`'s own
        // "reclaimable" shape (mirrors
        // `reclaimable_panes_lists_a_live_recorded_clean_pane` in
        // `mv-state/src/tasks.rs`).
        std::fs::write(
            root_b.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-bbbbbb\tagent=mv-fixer-b\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-bbbbbb\tstatus=done\tstate=done\trevisions=0\tpane=w2:p1\n",
        )
        .unwrap();

        let config_toml = format!(
            "[accounts.author]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}\"\npool = \"codex:test\"\nshort = \"cx\"\n\
             [projects.a]\npath = \"{}\"\n[projects.a.chains]\nbuilder = [\"author\"]\n\
             [projects.b]\npath = \"{}\"\n[projects.b.chains]\nbuilder = [\"author\"]\n\
             [roles.builder]\nwrite_scope = \"declared\"\nskills = []\n[roles.builder.models]\ncodex = \"gpt-test\"\n",
            dir.path().join("account").display(),
            root_a.display(),
            root_b.display(),
        );
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config_toml).unwrap();
        let config = load_config(Some(config_path.to_str().unwrap())).unwrap();

        let mut agents = FakeFleetAgents {
            live: vec![FakeFleetAgents::idle("w2:p1", "mv-fixer-b")],
            closed: Vec::new(),
        };
        reconcile_and_reclaim_fleet(&config, "a", &root_a, &mut agents);

        let log_a = std::fs::read_to_string(root_a.join(".multiverse/routing.log")).unwrap();
        assert!(
            log_a.contains("state=reconciled"),
            "project a's own settled-but-unrecorded task should be reconciled: {log_a}"
        );
        assert_eq!(
            agents.closed,
            vec!["w2:p1".to_string()],
            "project b's reclaimable pane should be closed even though it is not the \
             dispatching project"
        );
        let log_b = std::fs::read_to_string(root_b.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log_b.contains("state=reconciled"),
            "a foreign project must only be reclaimed, never reconciled: {log_b}"
        );
    }

    /// critic-grok3.md row 1 / card `20261105-b-28cb36`: a pane id is
    /// reusable. Recording task `aaaaaa`'s old pane `w1:p1` does not prove
    /// task `aaaaaa`'s own agent is still in it — closing on the bare pane
    /// id alone would destroy whatever unrelated agent now occupies it.
    #[test]
    fn reclaim_stale_panes_never_closes_a_reused_pane_under_an_unrelated_agent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a");
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tagent=mv-fixer-a\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();

        // The pane id is still live, but under a different, unrelated
        // agent — the old occupant already exited and something else
        // reused the id.
        let mut agents = FakeFleetAgents {
            live: vec![FakeFleetAgents::idle("w1:p1", "unrelated-agent")],
            closed: Vec::new(),
        };
        reclaim_stale_panes(&root, &mut agents);
        assert!(
            agents.closed.is_empty(),
            "a reused pane under an unrelated agent must never be closed: {:?}",
            agents.closed
        );
    }

    /// The safe case the pass above exists for: the task's own dispatched
    /// agent is still live, idle, in its recorded pane.
    #[test]
    fn reclaim_stale_panes_closes_a_pane_confirmed_idle_under_the_same_agent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("a");
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-fixer-aaaaaa\tagent=mv-fixer-a\tbranch=main\trole=fixer\n\
             2026-01-01T00:01:00+00:00\tevent=result\ttask=20260101-fixer-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();

        let mut agents = FakeFleetAgents {
            live: vec![FakeFleetAgents::idle("w1:p1", "mv-fixer-a")],
            closed: Vec::new(),
        };
        reclaim_stale_panes(&root, &mut agents);
        assert_eq!(agents.closed, vec!["w1:p1".to_string()]);
    }

    /// Card `20261105-b-6c2b34` / fuzz seed 40003: Python's own pre-dispatch
    /// pass (`cli.py:3452-3543`) also runs `converge_stale` — auto-supersede
    /// then lease-expire — for the dispatching project, not just
    /// `tasklog.reconcile`. A stranded AWAITING task (dispatched long before
    /// `awaiting_lease_minutes`, no live pane, no status file) must be
    /// recorded `failed`/abandoned here, before the new dispatch is ever
    /// admitted — mirroring the `event=result ... lease expired after 60m`
    /// line the Python oracle writes in that seed.
    #[test]
    fn reconcile_and_reclaim_fleet_lease_expires_a_stranded_awaiting_task() {
        let dir = tempfile::tempdir().unwrap();
        let root_a = dir.path().join("a");
        std::fs::create_dir_all(root_a.join(".multiverse")).unwrap();
        std::fs::write(
            root_a.join(".multiverse/routing.log"),
            "2000-01-01T00:00:00+00:00\tevent=dispatch\ttask=20000101-fixer-aaaaaa\tproject=a\trole=fixer\taccount=author\tpool=codex:test\toutcome=chain-primary\treason=\tbranch=main\n",
        )
        .unwrap();

        let config_toml = format!(
            "[accounts.author]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}\"\npool = \"codex:test\"\nshort = \"cx\"\n\
             [projects.a]\npath = \"{}\"\n[projects.a.chains]\nbuilder = [\"author\"]\n\
             [roles.builder]\nwrite_scope = \"declared\"\nskills = []\n[roles.builder.models]\ncodex = \"gpt-test\"\n",
            dir.path().join("account").display(),
            root_a.display(),
        );
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config_toml).unwrap();
        let config = load_config(Some(config_path.to_str().unwrap())).unwrap();

        let mut agents = FakeFleetAgents {
            live: Vec::new(),
            closed: Vec::new(),
        };
        reconcile_and_reclaim_fleet(&config, "a", &root_a, &mut agents);

        let log = std::fs::read_to_string(root_a.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("state=abandoned: pane gone, no status file, lease expired after 60m"),
            "the dispatching project's own stranded AWAITING task must be lease-expired \
             before the new dispatch is admitted: {log}"
        );
    }

    #[test]
    fn resolve_refuses_input_for_a_read_only_role_before_planning() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, mut request) = parity_fixture("scout");
        let input = tempfile::NamedTempFile::new().unwrap();
        request.inputs.push(input.path().display().to_string());
        let Err(error) = RealEngine.resolve(&request) else {
            panic!("expected a refusal");
        };
        assert!(matches!(
            error,
            EngineError::Refused(m) if m.contains("--input is for a writing role")
        ));
    }

    #[test]
    fn resolve_refuses_a_cited_input_nothing_stages() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, mut request) = parity_fixture("builder");
        request.task = "read `.multiverse/input/spec.md` and follow it".into();
        let Err(error) = RealEngine.resolve(&request) else {
            panic!("expected a refusal");
        };
        assert!(matches!(
            error,
            EngineError::Refused(m) if m.contains("the brief cites `.multiverse/input/spec.md`")
        ));
        let named = _dir.path().join("spec.md");
        std::fs::write(&named, "spec").unwrap();
        request.inputs.push(named.display().to_string());
        RealEngine.resolve(&request).unwrap();
    }

    #[test]
    fn resolve_allows_a_scope_warning_through_for_a_read_only_role() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, mut request) = parity_fixture("scout");
        request.scope = vec!["src".into()];
        let resolved = RealEngine.resolve(&request).unwrap();
        assert_eq!(
            resolved.scope_warning.as_deref(),
            Some(
                "warning: --scope is for a writing role; Scout is read-only, so its brief never renders a Write scope section and this path will not reach the agent. Drop --scope, or dispatch a writing role."
            )
        );
    }

    fn parity_fixture(role: &str) -> (tempfile::TempDir, Request) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let config_dir = dir.path().join("account");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&config_dir).unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(
            &config,
            format!(
                r#"
[accounts.author]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "{}"
pool = "codex:test"
short = "cx"
[accounts.other]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "{}"
pool = "grok:test"
short = "gr"
[projects.sandbox]
path = "{}"
short = "sb"
[projects.sandbox.chains]
scout = ["author", "other"]
builder = ["author", "other"]
critic = ["author", "other"]
[roles.scout]
write_scope = "none"
skills = []
[roles.scout.models]
codex = "gpt-test"
grok = "grok-test"
[roles.builder]
write_scope = "declared"
effort = "high"
skills = []
[roles.builder.models]
codex = "gpt-test"
grok = "grok-test"
[roles.critic]
write_scope = "none"
skills = []
[roles.critic.models]
codex = "gpt-test"
grok = "grok-test"
[review_policy]
review_scope = "repo"
"#,
                config_dir.display(),
                config_dir.display(),
                project.display()
            ),
        )
        .unwrap();
        let args = matches(&[
            "dispatch",
            project.to_str().unwrap(),
            "map the repo",
            "--role",
            role,
            "--dry-run",
            "--config",
            config.to_str().unwrap(),
        ]);
        (dir, request_from(&args))
    }

    #[test]
    fn reviewing_reads_only_the_projects_log_and_excludes_all_author_vendors() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, mut request) = parity_fixture("critic");
        let state = Path::new(&request.project_path).join(".multiverse");
        std::fs::create_dir_all(&state).unwrap();
        let log = state.join("routing.log");
        let contents = "2020-01-01T00:00:00Z\tevent=dispatch\ttask=20200101-fixer-aaaaaa\taccount=author\trole=fixer\n";
        std::fs::write(&log, contents).unwrap();
        request.reviewing = vec!["20200101-fixer-aaaaaa".into()];
        let resolved = RealEngine.resolve(&request).unwrap();
        assert_eq!(resolved.selection.seat.name, "other");
        assert_eq!(resolved.selection.skipped.len(), 1);
        assert!(resolved.exclusion_notes[0].contains("--reviewing"));
        request.exclude_vendor = vec!["codex".into()];
        let composed = RealEngine.resolve(&request).unwrap();
        assert_eq!(composed.exclusion_notes.len(), 1);
        assert_eq!(composed.selection.seat.name, "other");
        let report = RealEngine.plan(&request).unwrap().dry_run_report;
        assert!(report.contains("  excluded    "));
        assert!(report.contains("### Lane 20200101-fixer-aaaaaa"));
        assert!(report.contains("## Lane 20200101-fixer-aaaaaa"));
        assert_eq!(std::fs::read_to_string(&log).unwrap(), contents);
        assert_eq!(std::fs::read_dir(&state).unwrap().count(), 1);
        request.reviewing.push("20200101-fixer-bbbbbb".into());
        let error = RealEngine.plan(&request).unwrap_err();
        assert!(format!("{error:?}").contains("20200101-fixer-bbbbbb"));
        request.reviewing.clear();
        request.exclude_vendor.clear();
        assert!(RealEngine.plan(&request).is_err());
    }

    /// scout2-gaps.md row #30 / Python `dispatch.py:1677-1684`
    /// (`AnsweredItemsUnclosedError`, armed by `backlog answer`'s own
    /// `by="human"` annotation): a project with an answered-but-unclosed
    /// backlog item refuses every dispatch of it, dry-run included, since
    /// both `plan` and `dispatch` resolve through the same `resolve`.
    #[test]
    fn an_answered_unclosed_backlog_item_refuses_both_the_dry_run_and_the_real_dispatch() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, request) = parity_fixture("scout");
        let project_root = Path::new(&request.project_path);
        let item = mv_state::backlog::ask(
            project_root,
            "ship it?",
            &["yes".to_string(), "no".to_string()],
            1,
            "sandbox",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        mv_state::backlog::append_annotation_if_absent(
            project_root,
            &item.item,
            "human",
            "go ahead",
            None,
        )
        .unwrap();

        let error = RealEngine.plan(&request).unwrap_err();
        match error {
            EngineError::NeedsConfig(message) => {
                assert!(message.contains(&item.item), "{message}");
                assert!(message.contains("answered and never closed"), "{message}");
                assert!(message.contains("backlog done"), "{message}");
            }
            other => panic!("expected NeedsConfig, got {other:?}"),
        }
        assert_eq!(to_failure(RealEngine.plan(&request).unwrap_err()).code(), 3);

        let error = RealEngine.dispatch(&request, plan()).unwrap_err();
        assert!(matches!(error, EngineError::NeedsConfig(_)));

        // An item merely `asked`, with no human reply yet, must not gate —
        // that one is waiting on the human's own response time, not the
        // project's (`_unclosed_answered_items`'s own docstring).
        let unanswered = mv_state::backlog::ask(
            project_root,
            "still pending?",
            &["yes".to_string(), "no".to_string()],
            1,
            "sandbox",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        let _ = unanswered;
        mv_state::backlog::done(project_root, &item.item, "closed in review", None).unwrap();
        assert!(RealEngine.plan(&request).is_ok());
    }

    #[test]
    fn dry_run_renders_all_plan_sections_and_creates_nothing() {
        let _state_dir = StateDirEnvGuard::new();
        let (dir, mut request) = parity_fixture("builder");
        request.scope = vec!["src/**".into(), "README.md".into()];
        request.title = Some("Plan title".into());
        let input = dir.path().join("evidence.txt");
        std::fs::write(&input, "evidence").unwrap();
        request.inputs.push(input.display().to_string());
        let plan = RealEngine.plan(&request).unwrap();
        let report = &plan.dry_run_report;
        let cwd = std::fs::canonicalize(&request.project_path)
            .unwrap()
            .join(".multiverse/worktrees")
            .join(&plan.task_id);
        assert!(report.starts_with(&format!("DRY RUN — nothing was created. No pane, no agent, no files, no log entry.\n\nCARD  {}\n{CARD_RULE}\n  role        Builder\n  column      Execute  (stage, not a role — spec §12)\n", plan.task_id)));
        assert!(report.contains("  agent       sb-buil-cx\n  write scope declared\n  scope       src/**\n  scope       README.md\n"));
        assert!(report.contains(&format!("  cwd         {}\n", cwd.display())));
        let mut prior = 0;
        for heading in [
            "ACCOUNT SELECTION  (",
            "WOULD LOG  (",
            "PROMPT  (",
            "WOULD CREATE  (",
            "BRIEF  (",
            "WOULD RUN\n",
            "READY\n",
        ] {
            let position = report
                .find(heading)
                .unwrap_or_else(|| panic!("missing {heading}"));
            assert!(position > prior);
            prior = position;
        }
        for expected in [
            "title=\"Plan title\"",
            "brief.md",
            "result.md      (written by the agent)",
            "status         (written by the agent)",
            "evidence.txt   (copied from",
            "# Task ",
            "## Goal",
            "map the repo",
            "role-builder.md",
            "column-execute.md",
            "pane split",
            "--env CODEX_HOME=",
            "agent start sb-buil-cx",
            "agent prompt sb-buil-cx",
            "model_reasoning_effort",
            "Re-run with --full",
        ] {
            assert!(report.contains(expected), "missing {expected}");
        }
        request.full = true;
        let full = RealEngine.plan(&request).unwrap().dry_run_report;
        assert!(!full.contains("Plus, appended to brief.md"));
        assert!(full.len() > report.len());
        assert!(!Path::new(&request.project_path)
            .join(".multiverse")
            .exists());
        assert_eq!(std::fs::read_to_string(input).unwrap(), "evidence");
    }

    #[test]
    fn live_agent_naming_honors_the_project_short_and_rebudgets_collisions() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, request) = parity_fixture("builder");
        let resolved = RealEngine.resolve(&request).unwrap();
        let store = RealDispatchStore::new(resolved, &request);
        let taken = HashSet::from(["sb-buil-cx".into(), "sb-buil-cx-2".into()]);
        assert_eq!(
            store.unique_agent_name("sb-buil-cx", &taken).unwrap(),
            "sb-buil-cx-3"
        );
    }

    /// Card `20261103-b-a5c59f`: `DispatchStore::trip_breaker` used to fall
    /// through to the trait's own no-op default on `RealDispatchStore`, so a
    /// vendor-exhaustion verdict was correctly classified but never actually
    /// fenced the seat's pool. This drives the real adapter's own
    /// `trip_breaker` (not a parallel harness) and checks the two durable
    /// side effects `mv_engine::breaker::trip` owns: a tripped
    /// `BreakerStatus` for the seat's pool, and a redacted evidence file
    /// under the project's `.multiverse/evidence/`.
    #[test]
    fn trip_breaker_fences_the_seats_pool_and_writes_evidence_through_the_real_store() {
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, request) = parity_fixture("builder");
        let resolved = RealEngine.resolve(&request).unwrap();
        let config_dir = resolved.config_dir.clone();
        let project_root = resolved.project_root.clone();
        let pool = resolved.selection.seat.pool.clone();
        let mut store = RealDispatchStore::new(resolved, &request);
        let launch = PreparedLaunch {
            project_root: project_root.clone(),
            cwd: project_root.clone(),
            task_id: "task".into(),
            agent_name: "agent".into(),
            role: "builder".into(),
            kind: "codex".into(),
            model: "gpt-test".into(),
            seat: "author".into(),
            brief: String::new(),
            native_args: Vec::new(),
            environment: Vec::new(),
            clear_auth: Vec::new(),
            command: None,
        };
        let verdict = limits::Verdict {
            vendor: "codex".into(),
            rule: "codex_usage_limit_reached".into(),
            kind: breaker::BreakerKind::Rolling,
            reset_at: None,
            interactive: true,
        };
        let now = time::OffsetDateTime::now_utc();
        let tripped = store
            .trip_breaker(&launch, &verdict, "usage limit reached banner", now)
            .unwrap();
        assert!(tripped);

        let status = breaker::pool_status(&config_dir, &pool);
        assert!(status.open);
        assert_eq!(status.rule.as_deref(), Some("codex_usage_limit_reached"));
        assert_eq!(status.vendor.as_deref(), Some("codex"));
        assert_eq!(status.caller.as_deref(), Some("dispatch"));

        let evidence_dir = project_root.join(".multiverse/evidence");
        let mut entries: Vec<_> = std::fs::read_dir(&evidence_dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert_eq!(
            std::fs::read_to_string(entries.remove(0)).unwrap(),
            "usage limit reached banner"
        );
    }

    #[test]
    fn routing_lookup_uses_the_latest_dispatch_and_rejects_another_project() {
        use engine_dispatch::RoutingLookup;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".multiverse")).unwrap();
        std::fs::write(dir.path().join(".multiverse/routing.log"), "old\tevent=dispatch\ttask=lane\taccount=old\nnew\tevent=dispatch\ttask=lane\taccount=new\nnew\tevent=dispatch\ttask=missing-account\n").unwrap();
        let routing = ProjectRouting {
            project: "sandbox",
            root: dir.path(),
        };
        assert_eq!(
            routing
                .dispatch_for_task("sandbox", "lane")
                .unwrap()
                .account
                .as_deref(),
            Some("new")
        );
        assert!(routing.dispatch_for_task("elsewhere", "lane").is_none());
        assert!(routing.dispatch_for_task("sandbox", "unknown").is_none());
        assert!(routing
            .dispatch_for_task("sandbox", "missing-account")
            .unwrap()
            .account
            .is_none());
        assert_eq!(
            std::fs::read_dir(dir.path().join(".multiverse"))
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn unresolved_dry_run_names_only_the_selected_seats_placeholders() {
        // An ambient state-dir override could skip `author` and select
        // `other`'s resolved model.
        let _state_dir = StateDirEnvGuard::new();
        let (_dir, request) = parity_fixture("scout");
        let path = Path::new(request.config_override.as_deref().unwrap());
        let config = std::fs::read_to_string(path)
            .unwrap()
            .replace("gpt-test", "<<PLACEHOLDER: model>>");
        std::fs::write(path, config).unwrap();
        let report = RealEngine.plan(&request).unwrap().dry_run_report;
        assert!(report.contains("UNRESOLVED  (config values still to fill in)"));
        assert!(report.contains("roles.scout.models.codex = <<PLACEHOLDER: model>>"));
        assert!(!report.contains("\nREADY\n"));
        assert!(!Path::new(&request.project_path)
            .join(".multiverse")
            .exists());
    }

    #[test]
    fn dry_run_plans_but_never_starts_the_agent() {
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["dispatch", "proj", "task", "--dry-run"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Would dispatch builder on claude (20261102-builder-abc123).\n\n"
        );
    }

    #[test]
    fn a_real_dispatch_prints_the_dispatched_line() {
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(plan()),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Dispatched builder on claude (claude/claude-test) in /repo\n\
             \x20 agent repo-buil-cl in pane w1:p1\n\
             \x20 task 20261102-builder-abc123\n\
             \x20 status file /repo/.multiverse/tasks/20261102-builder-abc123/status\n"
        );
    }

    #[test]
    fn a_real_dispatch_prints_backlog_links_and_the_lane_unconditionally() {
        let mut dispatched = plan();
        dispatched.worktree = Some("/repo/.multiverse/worktrees/20261102-builder-abc123".into());
        dispatched.branch = Some("multiverse/20261102-builder-abc123".into());
        dispatched.linked_backlog = vec!["20261102-b-aaaaaa".into()];
        dispatched.backlog_link_failures =
            vec![("20261102-b-bbbbbb".into(), "item is terminal".into())];
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "Dispatched builder on claude (claude/claude-test) in /repo\n\
             \x20 agent repo-buil-cl in pane w1:p1\n\
             \x20 backlog 20261102-b-aaaaaa linked to task 20261102-builder-abc123\n\
             \x20 backlog 20261102-b-bbbbbb link failed (item is terminal); follow-up remains\n\
             \x20 worktree /repo/.multiverse/worktrees/20261102-builder-abc123\n\
             \x20 branch   multiverse/20261102-builder-abc123\n\
             \x20 task 20261102-builder-abc123\n\
             \x20 status file /repo/.multiverse/tasks/20261102-builder-abc123/status\n"
        );
    }

    #[test]
    fn wait_prints_the_result_path_and_exits_zero_on_done() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Done),
            settled_state: None,
            result_path: "/repo/.multiverse/tasks/20261102-builder-abc123/result.md".into(),
            status_path: "/repo/.multiverse/tasks/20261102-builder-abc123/status".into(),
            scope_violations: Vec::new(),
            revisions: Vec::new(),
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(printed.contains("  status done\n"));
        assert!(printed
            .ends_with("  result /repo/.multiverse/tasks/20261102-builder-abc123/result.md\n"));
    }

    /// Python `cli.py:3792`: `--wait` prints `settled as {live.settled.state}`
    /// — the raw Herdr `agent_status` at settle — before the status-file
    /// line. Both can disagree: Herdr's label is not the agent's own
    /// status file.
    #[test]
    fn wait_prints_settled_as_the_raw_herdr_agent_status() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Done),
            settled_state: Some(AgentState::Done),
            result_path: "/repo/.multiverse/tasks/20261102-builder-abc123/result.md".into(),
            status_path: "/repo/.multiverse/tasks/20261102-builder-abc123/status".into(),
            scope_violations: Vec::new(),
            revisions: Vec::new(),
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("  settled as done\n"),
            "missing settled-as line: {printed:?}"
        );
        let settled_at = printed.find("  settled as done\n").unwrap();
        let status_at = printed.find("  status done\n").unwrap();
        assert!(
            settled_at < status_at,
            "Python prints settled as before the status-file line"
        );
    }

    #[test]
    fn wait_exits_nonzero_on_a_blocked_or_failed_settle_with_no_extra_stderr() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: Some(mv_state::tasks::TaskStatus::Blocked),
            settled_state: None,
            result_path: "/repo/result.md".into(),
            status_path: "/repo/status".into(),
            scope_violations: Vec::new(),
            revisions: Vec::new(),
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 1);
        // `Failure::Reported` prints nothing of its own — Python's "no
        // status"/non-done cases are reported entirely on stdout.
        assert_eq!(error.to_string(), "");
        assert!(String::from_utf8(out)
            .unwrap()
            .contains("  status blocked\n"));
    }

    #[test]
    fn wait_reports_an_ambiguous_settle_with_no_status_file() {
        let mut dispatched = plan();
        dispatched.wait_outcome = Some(WaitOutcome {
            status: None,
            settled_state: Some(AgentState::Done),
            result_path: "/repo/result.md".into(),
            status_path: "/repo/.multiverse/tasks/20261102-builder-abc123/status".into(),
            scope_violations: Vec::new(),
            revisions: Vec::new(),
            lane_handback: None,
        });
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Ok(dispatched),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes", "--wait"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 1);
        let printed = String::from_utf8(out).unwrap();
        assert!(
            printed.contains("  settled as done\n"),
            "Python cli.py:3792 prints live.settled.state before the missing-status line: {printed:?}"
        );
        let settled_at = printed.find("  settled as done\n").unwrap();
        let missing_at = printed
            .find("  no status file at /repo/.multiverse/tasks/20261102-builder-abc123/status")
            .expect("missing-status line");
        assert!(
            settled_at < missing_at,
            "settled as must precede the no-status-file line"
        );
        assert!(printed.contains("settled without recording an outcome"));
    }

    /// Critic finding 2 / the smoke finding's exit-code bug: a trust dialog
    /// (or any other needs-attention pane) is a failed start, not a
    /// successful dispatch — the routing log already says
    /// `failed-to-start` and the prompt was never sent, so the CLI must
    /// not print `Dispatched …` and exit 0 for it. This test used to pin
    /// exactly that wrong behavior (`Plan::trust_pane`, now removed); a
    /// real `Engine::dispatch` now reports a needs-attention pane the same
    /// way it reports any other failed start: `Err`, non-zero exit,
    /// pane named in the message.
    #[test]
    fn a_trust_dialog_refuses_with_a_nonzero_exit_and_names_the_pane() {
        let fake = Fake {
            plan: Ok(plan()),
            dispatch: Err(EngineError::Refused(
                "dispatch: Builder on claude: agent example-builder in pane w1:p2 needs \
                 attention: trust dialog"
                    .into(),
            )),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 1);
        assert!(error.to_string().contains("needs attention"));
        assert!(error.to_string().contains("w1:p2"));
        assert!(String::from_utf8(out).unwrap().is_empty());
    }

    #[test]
    fn a_fleet_halt_refuses_with_the_exact_line_and_exit_one() {
        let fake = Fake {
            plan: Err(EngineError::FleetHalted(
                "dispatch refused: fleet halt is active; use --force".into(),
            )),
            dispatch: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(
            error.to_string(),
            "dispatch refused: fleet halt is active; use --force"
        );
        assert_eq!(error.code(), 1);
    }

    #[test]
    fn an_unresolved_project_needs_configuration_not_a_refusal() {
        let fake = Fake {
            plan: Err(EngineError::ProjectResolution("no such project".into())),
            dispatch: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 3);
    }

    #[test]
    fn needs_config_also_maps_to_exit_three_with_no_added_prefix() {
        let fake = Fake {
            plan: Err(EngineError::NeedsConfig(
                "no configured project contains /x".into(),
            )),
            dispatch: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["dispatch", "proj", "task", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 3);
        assert_eq!(error.to_string(), "no configured project contains /x");
    }

    #[test]
    fn json_quote_escapes_quotes_backslashes_and_control_characters() {
        assert_eq!(json_quote("plain"), "\"plain\"");
        assert_eq!(json_quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_quote("line\nbreak\ttab"), "\"line\\nbreak\\ttab\"");
        assert_eq!(json_quote("\u{01}"), "\"\\u0001\"");
    }

    /// Critic finding 9: `--title` must be refused outright (before any
    /// config is even loaded, let alone a worktree or task dir created) if
    /// it carries a tab or newline, since `record_dispatch`'s routing-log
    /// line is tab-delimited.
    #[test]
    fn a_title_with_an_embedded_tab_or_newline_is_refused_before_resolving_config() {
        for bad in ["bad\ttitle", "bad\ntitle", "bad\rtitle"] {
            let request = Request {
                project_path: "proj".into(),
                task: "task".into(),
                role: None,
                into: None,
                inputs: Vec::new(),
                scope: Vec::new(),
                exclude_vendor: Vec::new(),
                reviewing: Vec::new(),
                commits: Vec::new(),
                full: false,
                column: None,
                timeout_seconds: 900.0,
                force: false,
                title: Some(bad.into()),
                wait_for_seat: false,
                wait_for_seat_seconds: None,
                wait: false,
                dry_run: false,
                no_reconcile: false,
                config_override: None,
            };
            let error = RealEngine.dispatch(&request, plan()).unwrap_err();
            assert!(matches!(error, EngineError::Refused(_)), "{bad:?}");
        }
    }

    #[test]
    fn capitalize_title_cases_the_first_letter_only() {
        assert_eq!(capitalize("execute"), "Execute");
        assert_eq!(capitalize("review"), "Review");
        assert_eq!(capitalize(""), "");
    }

    #[test]
    fn task_id_matches_the_conformance_harness_id_shape() {
        let id = task_id_for(Path::new("/tmp/project"), "explorer", "map the repo");
        // `conformance/normalize.py`'s `_ID_RE`.
        let re = regex_lite(&id);
        assert!(re, "{id:?} does not match the <ID> shape");
    }

    /// A tiny hand check standing in for the Python regex
    /// `\b\d{8}-[a-z][a-z0-9]*(?:-[a-z0-9]+)?-[0-9a-f]{6}\b`, so this test
    /// does not need a regex dependency this crate does not otherwise use.
    fn regex_lite(id: &str) -> bool {
        let parts: Vec<&str> = id.splitn(3, '-').collect();
        let [date, role, hex] = parts.as_slice() else {
            return false;
        };
        date.len() == 8
            && date.chars().all(|c| c.is_ascii_digit())
            && !role.is_empty()
            && role
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
            && hex.len() == 6
            && hex.chars().all(|c| c.is_ascii_hexdigit())
    }

    #[test]
    fn resolve_project_picks_the_deepest_matching_root() {
        let outer = tempfile::tempdir().unwrap();
        let inner = outer.path().join("nested");
        std::fs::create_dir_all(&inner).unwrap();
        let toml = format!(
            "[projects.outer]\npath = \"{}\"\n[projects.inner]\npath = \"{}\"\n",
            outer.path().display(),
            inner.display()
        );
        let config_path = outer.path().join("config.toml");
        let config = load::load_config_str(&toml, &config_path).unwrap();

        let target = inner.join("dir");
        std::fs::create_dir_all(&target).unwrap();
        let (name, root) = resolve_project(&config, &std::fs::canonicalize(&target).unwrap())
            .expect("inner should match");
        assert_eq!(name, "inner");
        assert_eq!(root, std::fs::canonicalize(&inner).unwrap());
    }

    #[test]
    fn resolve_project_returns_none_outside_every_configured_root() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let toml = format!("[projects.p]\npath = \"{}\"\n", project.display());
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();

        assert!(resolve_project(&config, &std::fs::canonicalize(&outside).unwrap()).is_none());
    }

    #[test]
    fn reorder_chain_by_preference_floats_a_preferred_seat_within_its_own_kind() {
        let toml = "\
[accounts.codex-a]\nkind = \"codex\"\nshort = \"ca\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"codex:a\"\n\
[accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: b>>\"\npool = \"codex:b\"\n\
[accounts.claude-a]\nkind = \"claude\"\nshort = \"cl\"\ncommand = \"wrap\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: c>>\"\npool = \"claude:a\"\n";
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let config_dir = config_dir.path();
        let mut seats = mv_state::seatstate_store::SeatMap::new();
        seats.insert(
            "codex-b".to_string(),
            mv_state::seatstate_store::SeatEntry {
                prefer: Some(true),
                ..Default::default()
            },
        );
        mv_state::seatstate_store::save(config_dir, &seats).unwrap();

        let chain = vec![
            "codex-a".to_string(),
            "claude-a".to_string(),
            "codex-b".to_string(),
        ];
        let reordered = reorder_chain_by_preference(&chain, &config.accounts, config_dir);
        assert_eq!(reordered, vec!["codex-b", "claude-a", "codex-a"]);
    }

    #[test]
    fn reorder_chain_by_preference_leaves_an_unpreferred_chain_untouched() {
        let toml = "\
[accounts.codex-a]\nkind = \"codex\"\nshort = \"ca\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"codex:a\"\n\
[accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: b>>\"\npool = \"codex:b\"\n";
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = dir.path().join("state");

        let chain = vec!["codex-a".to_string(), "codex-b".to_string()];
        let reordered = reorder_chain_by_preference(&chain, &config.accounts, &config_dir);
        assert_eq!(reordered, chain);
    }

    #[test]
    fn open_task_count_reads_an_unrecorded_dispatch_as_open_and_a_result_as_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".multiverse/tasks/20250101-builder-aaaaaa")).unwrap();
        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-builder-aaaaaa\tproject=sandbox\trole=builder\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tbranch=main\n",
        )
        .unwrap();
        assert_eq!(open_task_count(root), 1);

        std::fs::write(
            root.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-builder-aaaaaa\tproject=sandbox\trole=builder\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tbranch=main\n\
             2020-01-01T01:00:00+00:00\tevent=result\ttask=20250101-builder-aaaaaa\tstatus=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        )
        .unwrap();
        assert_eq!(open_task_count(root), 0);
    }

    #[test]
    fn fleet_open_counts_sums_every_resolvable_project_and_flags_other_floor_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().join("here");
        let there = dir.path().join("there");
        std::fs::create_dir_all(&here).unwrap();
        std::fs::create_dir_all(&there).unwrap();
        std::fs::create_dir_all(there.join(".multiverse/tasks/20250101-builder-bbbbbb")).unwrap();
        std::fs::write(
            there.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-builder-bbbbbb\tproject=there\trole=builder\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.here]\npath = \"{}\"\n[projects.there]\npath = \"{}\"\nmin_slots = 2\n",
            here.display(),
            there.display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();

        let (fleet_open, other_floor_gaps) = fleet_open_counts(&config, "here");
        assert_eq!(fleet_open, 1);
        assert_eq!(other_floor_gaps, vec![("there".to_string(), 1)]);

        let (_, no_gaps) = fleet_open_counts(&config, "there");
        assert!(
            no_gaps.is_empty(),
            "the dispatched project's own floor is never listed as an 'other' gap"
        );
    }

    /// Card `20261105-fixer-9d070c`: before this task, a seat's open count
    /// came only from Herdr's own `agent list` reply (`live_seat_counts`),
    /// with no fallback for a pane Herdr has not caught up on yet — exactly
    /// the race `accounts.py`'s own `_started_recently` grace window exists
    /// to close (two dispatches landing seconds apart on a cap-1 seat, both
    /// reading it as free). A task whose `event=started` line is recent and
    /// whose agent is entirely absent from Herdr's listing must still count
    /// as open; the same task well past its grace window must not.
    #[test]
    fn grace_open_counts_credits_an_unrecorded_task_herdr_has_not_caught_up_on() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-se\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            project.display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let statuses = HashMap::new();
        let latest = latest_tasks_by_agent(&config);

        let just_after_started = time::macros::datetime!(2020-01-01 00:05:30 UTC);
        let counts = grace_open_counts(&config, &statuses, &latest, just_after_started);
        assert_eq!(counts.get("se"), Some(&1), "within the grace window");

        let long_after_started = time::macros::datetime!(2020-01-01 01:00:00 UTC);
        let counts = grace_open_counts(&config, &statuses, &latest, long_after_started);
        assert_eq!(
            counts.get("se"),
            None,
            "past the grace window, this is back to whatever Herdr's listing says"
        );

        // Already counted by `live_seat_counts` (Herdr lists it `working`):
        // the grace window must not double-count it.
        let mut listed = HashMap::new();
        listed.insert("mv-fixer-1-se".to_string(), "working".to_string());
        let counts = grace_open_counts(&config, &listed, &latest, just_after_started);
        assert_eq!(
            counts.get("se"),
            None,
            "already open per Herdr's own listing — grace adds nothing more"
        );
    }

    /// Critic finding 3, card `20261105-b-5e4d10`: Python's
    /// `_OPEN_AGENT_STATES` is `(WORKING, BLOCKED)` only — `UNKNOWN` is
    /// deliberately excluded, "not evidence of anything, open or
    /// otherwise". A pane Herdr reports `unknown` (a flaky/just-restarted
    /// Herdr) must not hold a seat's cap hostage.
    #[test]
    fn is_open_agent_status_excludes_unknown() {
        assert!(is_open_agent_status("working"));
        assert!(is_open_agent_status("blocked"));
        assert!(!is_open_agent_status("unknown"));
        assert!(!is_open_agent_status("idle"));
        assert!(!is_open_agent_status("done"));
    }

    /// Critic finding 4, card `20261105-b-5e4d10`: a reissued agent name
    /// (the previous occupant is no longer live, so `naming.py`'s scheme
    /// reissues it) can leave two unrecorded routing-log records — one
    /// stale, one current — sharing one `agent=` name. `grace_open_counts`
    /// must credit the seat once per *name*, not once per record, matching
    /// Python's own `latest` dict (`accounts.py:769-781`).
    #[test]
    fn grace_open_counts_dedups_latest_task_per_reissued_agent_name() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-bbbbbb")).unwrap();
        // Two unrecorded dispatches reuse the same agent name
        // (`mv-fixer-1-se`) — the second (`bbbbbb`) is the live one; the
        // first (`aaaaaa`)'s pane already died without ever being recorded.
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-se\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n\
             2020-01-01T00:10:00+00:00\tevent=dispatch\ttask=20250101-fixer-bbbbbb\tproject=p\trole=fixer\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-se\tbranch=main\n\
             2020-01-01T00:11:00+00:00\tevent=started\ttask=20250101-fixer-bbbbbb\tpane=w2:p1\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            project.display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let statuses = HashMap::new();
        let latest = latest_tasks_by_agent(&config);
        assert_eq!(
            latest.get("mv-fixer-1-se").map(|t| t.task_id.as_str()),
            Some("20250101-fixer-bbbbbb"),
            "the newer dispatch (higher dispatch_ordinal) wins the name"
        );

        let within_grace = time::macros::datetime!(2020-01-01 00:11:30 UTC);
        let counts = grace_open_counts(&config, &statuses, &latest, within_grace);
        assert_eq!(
            counts.get("se"),
            Some(&1),
            "one reissued name must credit the seat once, not once per stale record"
        );
    }

    /// Critic finding 5, card `20261105-b-5e4d10`: an agent's account is
    /// attributed from the dispatch record that started it (`task.account`
    /// off the routing log), never guessed from a trailing `-<short>`
    /// segment of its live name — Python's `open_agent_counts`
    /// (`accounts.py:769-800`). A pane whose name happens to end in a
    /// configured short code, but was never dispatched by this lane (a
    /// human's own interactive seat, a hand-started pane), must not count
    /// against that seat.
    #[test]
    fn live_seat_counts_attributes_by_dispatch_record_not_name_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            // Dispatched under account "seat" (short "se"), but its own
            // agent name ends in "-zz" — a short this config does not
            // define at all, so a name-suffix guess would find nothing.
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-zz\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            project.display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let latest = latest_tasks_by_agent(&config);

        let mut statuses = HashMap::new();
        statuses.insert("mv-fixer-1-zz".to_string(), "working".to_string());
        let counts = live_seat_counts(&statuses, &latest, &config);
        assert_eq!(
            counts.get("se"),
            Some(&1),
            "attributed via the dispatch record's account, not the name's own -zz suffix"
        );

        // A pane whose name ends in a *real* configured short, but that
        // this lane never dispatched (so it has no routing-log record at
        // all), must contribute to no seat's count.
        let mut unrecorded = HashMap::new();
        unrecorded.insert("a-human-pane-se".to_string(), "working".to_string());
        let counts = live_seat_counts(&unrecorded, &latest, &config);
        assert_eq!(
            counts.get("se"),
            None,
            "a name not found in any project's log must not be guessed from its own suffix"
        );
    }

    /// Python `open_agent_counts` (`accounts.py:798-804`): when the
    /// dispatch record's `account=` is no longer a config key (the seat
    /// was renamed), recover it from the agent name's trailing short
    /// after `strip_instance_suffix`, via `account_by_short`. A recorded
    /// task under the old name must still hold the live seat's cap.
    #[test]
    fn live_seat_counts_recovers_a_renamed_account_from_the_agent_name_short() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-se\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            project.display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let latest = latest_tasks_by_agent(&config);

        let mut statuses = HashMap::new();
        statuses.insert("mv-fixer-1-se".to_string(), "working".to_string());
        let counts = live_seat_counts(&statuses, &latest, &config);
        assert_eq!(
            counts.get("se"),
            Some(&1),
            "a task recorded under a since-renamed account name must still count via the agent-name short"
        );

        // Instance suffix (`-2`) is stripped before the short is read,
        // matching `naming.strip_instance_suffix`.
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-se-2\tbranch=main\n",
        )
        .unwrap();
        let latest = latest_tasks_by_agent(&config);
        let mut suffixed = HashMap::new();
        suffixed.insert("mv-fixer-se-2".to_string(), "working".to_string());
        let counts = live_seat_counts(&suffixed, &latest, &config);
        assert_eq!(
            counts.get("se"),
            Some(&1),
            "the instance suffix is not the short; strip it first"
        );

        // A trailing token that is not a configured short still contributes
        // nothing — the fallback is a lookup, not a guess.
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-zz\tbranch=main\n",
        )
        .unwrap();
        let latest = latest_tasks_by_agent(&config);
        let mut unknown_short = HashMap::new();
        unknown_short.insert("mv-fixer-1-zz".to_string(), "working".to_string());
        let counts = live_seat_counts(&unknown_short, &latest, &config);
        assert_eq!(
            counts.get("se"),
            None,
            "an unknown name-suffix must not be attributed to a live seat"
        );
    }

    /// Same rename fallback as [`live_seat_counts`], applied to the
    /// startup-grace window (`grace_open_counts`): an unrecorded task
    /// whose `account=` is gone from config still credits the live short
    /// while Herdr has not caught up.
    #[test]
    fn grace_open_counts_recovers_a_renamed_account_from_the_agent_name_short() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks/20250101-fixer-aaaaaa")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-fixer-aaaaaa\tproject=p\trole=fixer\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=mv-fixer-1-se\tbranch=main\n\
             2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-fixer-aaaaaa\tpane=w1:p1\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            project.display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let statuses = HashMap::new();
        let latest = latest_tasks_by_agent(&config);
        let just_after_started = time::macros::datetime!(2020-01-01 00:05:30 UTC);
        let counts = grace_open_counts(&config, &statuses, &latest, just_after_started);
        assert_eq!(
            counts.get("se"),
            Some(&1),
            "grace must credit the live short when the recorded account name is gone"
        );
    }

    /// Critic finding 3 / card `20261102-b-47a7b3`: `--role` is new input
    /// (`mv_config::roles::Role::from_config_key`'s own doc comment), so a
    /// retired name must be refused by `RealEngine` itself, not only by the
    /// `commands::mod::validate` `WORKER_ROLES` check upstream of it —
    /// `conformance/role_translate.py` already rewrites every recorded
    /// scenario's `--role fixer`-style argv to the current name before it
    /// reaches Rust, so this file's own parsing was the one place still
    /// using `::from_any_config_key` (a history-reader, not a write-surface,
    /// parse) for a CLI flag.
    #[test]
    fn dispatch_role_flag_refuses_a_retired_name() {
        let _state_dir = StateDirEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!("[projects.p]\npath = \"{}\"\n", project.display()),
        )
        .unwrap();
        let request = Request {
            project_path: project.display().to_string(),
            task: "task text".into(),
            role: Some("fixer".into()),
            into: None,
            inputs: Vec::new(),
            scope: Vec::new(),
            exclude_vendor: Vec::new(),
            reviewing: Vec::new(),
            commits: Vec::new(),
            full: false,
            column: None,
            timeout_seconds: 900.0,
            force: false,
            title: None,
            wait_for_seat: false,
            wait_for_seat_seconds: None,
            wait: false,
            dry_run: true,
            no_reconcile: false,
            config_override: Some(config_path.display().to_string()),
        };
        let error = RealEngine.plan(&request).unwrap_err();
        match error {
            EngineError::NeedsConfig(message) => assert!(message.contains("fixer")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn real_halt_reads_the_shared_halt_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let halt = RealHalt {
            state_dir: dir.path(),
        };
        assert_eq!(halt.read(), Ok(None));

        std::fs::write(
            dir.path().join("halt.state"),
            "true\nmaintenance\n2026-11-02T00:00:00+00:00\n",
        )
        .unwrap();
        let active = halt.read().unwrap().expect("halt is active");
        assert_eq!(active.reason, "maintenance");
        assert_eq!(
            active.since.as_deref(),
            Some("2026-11-02T00:00:00+00:00"),
            "the recorded wall-clock timestamp must reach DispatchError::FleetHaltActive's \
             Display, not an Instant with no on-disk representation"
        );
    }

    /// Conformance seed 50003: Python's `DispatchRunner.dispatch` (the
    /// runner both `dispatch` and `council` share) never reads the fleet
    /// halt at all — only `cli.py`'s `dispatch`-specific `cmd_dispatch`/
    /// `_run_dispatch` do, and `council.py` calls the shared runner
    /// directly, bypassing both. A council seat refusing on halt here
    /// applies a dispatch-only rule to a command Python exempts entirely.
    #[test]
    fn a_halted_fleet_refuses_a_dispatch_seat_before_pane_creation_but_exempts_a_council_seat() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_dir.path().join("halt.state"),
            "true\nfuzz maintenance\n2026-11-02T00:00:00+00:00\n",
        )
        .unwrap();

        let mut dispatch_store = store_for(
            &project,
            config_dir.path(),
            Role::Scout,
            "20261102-scout-ffffff",
        );
        let error = dispatch_store
            .check_halt_before_pane_creation()
            .unwrap_err();
        assert!(error.to_string().contains("fleet halted since"), "{error}");

        let mut council_store = store_for(
            &project,
            config_dir.path(),
            Role::Council,
            "20261102-council-ffffff",
        );
        assert!(council_store.check_halt_before_pane_creation().is_ok());
    }

    // --- --into lane adoption (critic finding 1, card 20261102-b-0a4062) --

    fn git(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn seat_account(config_dir: &Path) -> Account {
        let toml = format!(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}\"\npool = \"codex:test\"\n",
            config_dir.display()
        );
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        config.accounts["seat"].clone()
    }

    #[test]
    fn into_adopts_the_existing_lanes_worktree_instead_of_creating_a_fresh_one() {
        let _herdr = HerdrEnvGuard::unreachable();
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );

        let worktrees = mv_state::worktrees::Worktrees::new(&project).unwrap();
        let existing = worktrees.create("20261101-builder-aaaaaa", "HEAD").unwrap();
        std::fs::write(existing.path.join("earlier-work.txt"), "earlier work\n").unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let account = seat_account(config_dir.path());
        let empty_config_dir = tempfile::tempdir().unwrap();
        let empty_config =
            load::load_config_str("", &empty_config_dir.path().join("config.toml")).unwrap();
        let mut store = RealDispatchStore::for_seat(
            empty_config,
            config_dir.path().to_path_buf(),
            "p".into(),
            project.clone(),
            Role::Builder,
            account,
            "test-model".into(),
            "20261102-builder-bbbbbb".into(),
            "continue the earlier work".into(),
            "test".into(),
            false,
        );
        store.write_scope = "declared".into();
        store.into = Some("20261101-builder-aaaaaa".into());

        let launch = store.prepare().unwrap();
        assert_eq!(launch.cwd, existing.path);
        assert!(launch.cwd.join("earlier-work.txt").exists());
        assert_eq!(launch.task_id, "20261102-builder-bbbbbb");
    }

    // --- card 20261105-b-50b425: a claude seat's trust args, from `prepare` -

    #[test]
    fn prepare_gives_a_claude_seat_the_same_trust_args_whether_dispatch_or_council_built_it() {
        // `for_seat` is `RealDispatchStore`'s own council constructor, but
        // `prepare` is the one method both it and `new`'s ordinary dispatch
        // share — so a claude seat built either way must come out of this
        // same `prepare` call with Python's `Card.agent_args` claude-branch
        // shape (`--permission-mode bypassPermissions` plus
        // `--disallowedTools` and its rules), not just the bare
        // `--model`/`--effort` native args.
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.seat]\nkind = \"claude\"\nshort = \"cl\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"{}\"\n\
             pool = \"claude:test\"\ncommand = \"fake-claude\"\n",
            config_dir.path().join("claude-home").display(),
        );
        let account_config_dir = tempfile::tempdir().unwrap();
        let config =
            load::load_config_str(&toml, &account_config_dir.path().join("config.toml")).unwrap();
        let account = config.accounts["seat"].clone();

        let empty_config_dir = tempfile::tempdir().unwrap();
        let empty_config =
            load::load_config_str("", &empty_config_dir.path().join("config.toml")).unwrap();
        let mut store = RealDispatchStore::for_seat(
            empty_config,
            config_dir.path().to_path_buf(),
            "p".into(),
            project.clone(),
            Role::Council,
            account,
            "claude-test".into(),
            "20261106-council-aaaaaa".into(),
            "question text".into(),
            "test".into(),
            false,
        );

        let launch = store.prepare().unwrap();
        assert_eq!(launch.command.as_deref(), Some("fake-claude"));
        assert!(launch
            .native_args
            .windows(2)
            .any(|w| w == ["--permission-mode", "bypassPermissions"]));
        let disallowed = launch
            .native_args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("--disallowedTools must be carried");
        let rules = &launch.native_args[disallowed + 1..];
        assert!(rules.iter().any(|r| r == "Bash(git push *)"));
        assert!(rules.iter().any(|r| r == "Bash(herdr)"));
        // `for_seat` with no `[roles.council]` entry in `empty_config`
        // defaults `write_scope` to `"none"` — a reader, which carries the
        // extra `Bash(rm *)` and worktrees-deny rules `trust::
        // claude_specialist_launch_args` only adds for `write_scope ==
        // "none"`.
        assert!(rules.iter().any(|r| r == "Bash(rm *)"));
        assert!(rules.iter().any(|r| r.contains(".multiverse/worktrees")));
    }

    /// Guards `HOME` the same way `worktrees::herdr_env::HerdrEnvGuard`
    /// guards `HERDR_BIN_PATH` (a lock plus restore-on-drop), kept local
    /// to this test since no other test in this file touches `HOME`.
    struct HomeEnvGuard {
        previous: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl HomeEnvGuard {
        fn unset() -> Self {
            static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let previous = std::env::var_os("HOME");
            std::env::remove_var("HOME");
            Self {
                previous,
                _lock: held,
            }
        }
    }

    impl Drop for HomeEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("HOME", value),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    // --- card 20261106-reviewer-abf805: HOME-unset must anchor a claude
    // seat's deny-list rules at the passwd home, not `/` --------------

    #[test]
    fn prepare_with_home_unset_anchors_claude_deny_rules_at_the_passwd_home_not_root() {
        let _home = HomeEnvGuard::unset();
        let passwd_home = mv_config::load::resolve_home_dir()
            .expect("the test process must have a resolvable passwd home");
        assert_ne!(
            passwd_home,
            PathBuf::from("/"),
            "test environment has no real passwd home to distinguish from the bug's \
             `/` fallback"
        );

        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.seat]\nkind = \"claude\"\nshort = \"cl\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"{}\"\n\
             pool = \"claude:test\"\ncommand = \"fake-claude\"\n",
            config_dir.path().join("claude-home").display(),
        );
        let account_config_dir = tempfile::tempdir().unwrap();
        let config =
            load::load_config_str(&toml, &account_config_dir.path().join("config.toml")).unwrap();
        let account = config.accounts["seat"].clone();

        let empty_config_dir = tempfile::tempdir().unwrap();
        let empty_config =
            load::load_config_str("", &empty_config_dir.path().join("config.toml")).unwrap();
        let mut store = RealDispatchStore::for_seat(
            empty_config,
            config_dir.path().to_path_buf(),
            "p".into(),
            project.clone(),
            Role::Council,
            account,
            "claude-test".into(),
            "20261106-council-bbbbbb".into(),
            "question text".into(),
            "test".into(),
            false,
        );

        let launch = store.prepare().unwrap();
        let disallowed = launch
            .native_args
            .iter()
            .position(|a| a == "--disallowedTools")
            .expect("--disallowedTools must be carried");
        let rules = &launch.native_args[disallowed + 1..];
        let expected_ssh = format!("Edit(/{})", passwd_home.join(".ssh").join("**").display());
        let expected_gitconfig = format!("Edit(/{})", passwd_home.join(".gitconfig").display());
        assert!(rules.contains(&expected_ssh), "{rules:?}");
        assert!(rules.contains(&expected_gitconfig), "{rules:?}");
        // The bug this guards against: a root-anchored rule instead of
        // the real passwd home's.
        assert!(!rules.iter().any(|r| r == "Edit(//.ssh/**)"), "{rules:?}");
        assert!(
            !rules.iter().any(|r| r == "Edit(//.gitconfig)"),
            "{rules:?}"
        );
    }

    // --- lane_setup redaction and distinct failure state (critic finding 8) -

    #[test]
    fn lane_setup_redacts_secrets_before_writing_the_log_and_marks_lane_setup_failed() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Builder,
            "20261102-builder-eeeeee",
        );
        store.isolated = true;
        store.lane_setup = Some("echo 'Bearer sometesttoken123' >&2; exit 7".into());
        let task_dir = project.join(".multiverse/tasks/20261102-builder-eeeeee");
        std::fs::create_dir_all(&task_dir).unwrap();

        let launch = PreparedLaunch {
            project_root: project.clone(),
            cwd: project.clone(),
            task_id: "20261102-builder-eeeeee".into(),
            agent_name: "a".into(),
            role: "builder".into(),
            kind: "codex".into(),
            model: "m".into(),
            seat: "s".into(),
            brief: String::new(),
            native_args: Vec::new(),
            environment: Vec::new(),
            clear_auth: Vec::new(),
            command: None,
        };

        let error = store.setup_lane(&launch).unwrap_err();
        assert!(error.to_string().contains("lane_setup exited"));
        assert!(store.lane_setup_failed);

        let log = std::fs::read_to_string(task_dir.join("lane-setup.log")).unwrap();
        assert!(!log.contains("sometesttoken123"), "{log:?}");
        assert!(log.contains("[redacted]"), "{log:?}");

        // The distinct state `record_failed_to_start` would log.
        let state = if store.lane_setup_failed {
            "lane-setup-failed"
        } else {
            "failed-to-start"
        };
        assert_eq!(state, "lane-setup-failed");
    }

    #[test]
    fn lane_setup_is_skipped_entirely_for_a_reader() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Scout,
            "20261102-scout-ffffff",
        );
        store.isolated = false;
        store.lane_setup = Some("exit 1".into());
        let launch = PreparedLaunch {
            project_root: project.clone(),
            cwd: project.clone(),
            task_id: "20261102-scout-ffffff".into(),
            agent_name: "a".into(),
            role: "scout".into(),
            kind: "codex".into(),
            model: "m".into(),
            seat: "s".into(),
            brief: String::new(),
            native_args: Vec::new(),
            environment: Vec::new(),
            clear_auth: Vec::new(),
            command: None,
        };
        store.setup_lane(&launch).unwrap();
        assert!(!store.lane_setup_failed);
    }

    // --- task-id rebind on collision (critic finding 5) --------------------

    fn store_for(
        project: &Path,
        config_dir: &Path,
        role: Role,
        task_id: &str,
    ) -> RealDispatchStore {
        let account = seat_account(config_dir);
        let empty_config_dir = tempfile::tempdir().unwrap();
        let empty_config =
            load::load_config_str("", &empty_config_dir.path().join("config.toml")).unwrap();
        RealDispatchStore::for_seat(
            empty_config,
            config_dir.to_path_buf(),
            "p".into(),
            project.to_path_buf(),
            role,
            account,
            "test-model".into(),
            task_id.into(),
            "same task text".into(),
            "test".into(),
            false,
        )
    }

    // -- card 20261102-b-82436e: reviewer-diff metadata and risk questions --

    #[test]
    fn review_scope_renders_lane_diff_metadata_and_risk_questions_matching_python() {
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let task = "20261101-fixer-abcdef";
        git(
            &project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        git(
            &project,
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        std::fs::write(project.join("src.rs"), "fn login() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "work",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-cccccc",
        );
        store.reviewing = vec![task.into()];
        let out = store.review_scope().unwrap();

        // py-dispatch.py:1030-1034 — `Diffed {diff_note}, from \`{diff.base}\`.`
        // then the touched-files list.
        assert!(out.contains(
            "### Lane 20261101-fixer-abcdef\nDiffed against this lane's own cut point, from `"
        ));
        assert!(out.contains("`.\n\nTouched files:\n- src.rs\n"));
        // py-dispatch.py:1034-1037 — `Diff stat:` fenced block from
        // `LaneDiff.stat_text`.
        assert!(
            out.contains("\nDiff stat:\n```\nsrc.rs | 1 +\n 1 file changed, 1 insertion(+)\n```\n")
        );
        // py-dispatch.py:1038-1047 — untruncated diff gets no truncation
        // note, and the redacted unified diff.
        assert!(out.contains("\nUnified diff:\n```diff\ndiff --git a/src.rs b/src.rs\n"));
        assert!(!out.contains("truncated at"));
        // py-dispatch.py:1065-1068 / config.py's `render_risk_questions` —
        // one question per the default `require_cross_vendor_for` area, in
        // that list's own order.
        assert!(out.contains(
            "\nRisk questions this policy calls out:\n\
             - Does this change how input is validated, sanitized, or trusted?\n\
             - Does this change who is allowed to do what, or how identity is checked?\n\
             - Does this change data already on disk or in a database, and is that change reversible?\n\
             - Can this delete or discard something a human cannot get back?\n\
             - Does this touch a credential, token, or key — reading, writing, or logging one?\n\
             - Does this change what gets shipped, published, or rolled out, and to whom?"
        ));
    }

    #[test]
    fn review_scope_notes_truncation_at_the_configured_max_diff_lines() {
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let task = "20261101-fixer-abcdef";
        git(
            &project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        git(
            &project,
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        std::fs::write(project.join("many.txt"), "a\nb\nc\nd\ne\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "work",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-dddddd",
        );
        store.reviewing = vec![task.into()];
        store.config.review_policy.max_diff_lines = 2;
        let out = store.review_scope().unwrap();

        assert!(out.contains(
            "\nUnified diff (truncated at 2 lines — read the rest from the lane's own branch if you need it):\n"
        ));
    }

    // -- task 20261106-fixer-fbf55c: `--commits`/`rereview` re-diffing -----

    #[test]
    fn review_scope_with_explicit_commits_diffs_only_the_named_commits() {
        // Python `dispatch.py:1001-1014` / `worktrees.commit_set_diff`: an
        // explicit `--commits` list restricts the diff to exactly those
        // commits' own touched paths, not the whole lane since its cut
        // point — so a lane with two commits, told to diff only the
        // second, must show that commit's file and not the first's.
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let task = "20261101-fixer-abcdef";
        git(
            &project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        git(
            &project,
            &["checkout", "-q", "-b", &format!("multiverse/{task}")],
        );
        std::fs::write(project.join("a.rs"), "fn a() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit A",
            ],
        );
        let commit_a = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let commit_a = commit_a.trim().to_string();
        std::fs::write(project.join("b.rs"), "fn b() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit B",
            ],
        );
        let commit_b = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let commit_b = commit_b.trim().to_string();
        git(&project, &["checkout", "-q", "main"]);

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-cccccc",
        );
        store.reviewing = vec![task.into()];
        store.commits = vec![commit_b];
        let out = store.review_scope().unwrap();

        let expected_base = String::from_utf8(
            std::process::Command::new("git")
                .arg("-C")
                .arg(&project)
                .args(["rev-parse", "--short", &commit_a])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert!(out.contains(&format!(
            "### Lane 20261101-fixer-abcdef\nDiffed against `--commits`'s own explicit list, from `{}`.",
            expected_base.trim()
        )));
        assert!(out.contains("\nTouched files:\n- b.rs\n"));
        assert!(!out.contains("a.rs"));
        assert!(out.contains("+fn b() {}"));
        assert!(!out.contains("+fn a() {}"));
    }

    #[test]
    fn review_scope_ignores_commits_when_more_than_one_lane_is_reviewing() {
        // Python `dispatch.py:1001`: `explicit_commits = commits if
        // len(reviewing) == 1 else ()` — an explicit `--commits` list has
        // no way to say which of several lanes it names, so with more than
        // one lane in `--reviewing` it is ignored for every lane, which
        // still falls back to each lane's own full `lane_diff`.
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        for task in ["20261101-fixer-aaaaaa", "20261101-fixer-bbbbbb"] {
            git(
                &project,
                &[
                    "update-ref",
                    &format!("refs/multiverse/base/{task}"),
                    "HEAD",
                ],
            );
            git(
                &project,
                &["checkout", "-q", "-b", &format!("multiverse/{task}")],
            );
            std::fs::write(project.join(format!("{task}.rs")), "fn work() {}\n").unwrap();
            git(&project, &["add", "."]);
            git(
                &project,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t.invalid",
                    "commit",
                    "-qm",
                    "work",
                ],
            );
            git(&project, &["checkout", "-q", "main"]);
        }

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-cccccc",
        );
        store.reviewing = vec![
            "20261101-fixer-aaaaaa".into(),
            "20261101-fixer-bbbbbb".into(),
        ];
        store.commits = vec!["deadbeef".into()];
        let out = store.review_scope().unwrap();

        assert!(out.contains("Diffed against this lane's own cut point, from `"));
        assert!(!out.contains("--commits"));
    }

    #[test]
    fn review_scope_falls_back_to_fix_round_commits_when_rereview_is_diff_and_lane_was_already_reviewed(
    ) {
        // Python `dispatch.py:1016-1025`: absent `--commits`, a lane
        // `last_reviewed_commit` already has a mark for gets only its
        // fix-round commits since that mark, not the whole lane again.
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let task = "20261101-fixer-abcdef";
        git(
            &project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        let branch = format!("multiverse/{task}");
        git(&project, &["checkout", "-q", "-b", &branch]);
        std::fs::write(project.join("a.rs"), "fn a() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit A",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);
        mv_state::worktrees::record_reviewed(&project, task, None);
        git(&project, &["checkout", "-q", &branch]);
        std::fs::write(project.join("b.rs"), "fn b() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit B (fix round)",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-dddddd",
        );
        store.reviewing = vec![task.into()];
        assert_eq!(store.config.review_policy.rereview, "diff");
        let out = store.review_scope().unwrap();

        assert!(out.contains(
            "### Lane 20261101-fixer-abcdef\nDiffed against this lane's own fix-round commits \
             since its last review — not the whole lane again, from `"
        ));
        assert!(out.contains("\nTouched files:\n- b.rs\n"));
        assert!(!out.contains("a.rs"));
        assert!(out.contains("+fn b() {}"));
        assert!(!out.contains("+fn a() {}"));
    }

    #[test]
    fn review_scope_uses_the_full_lane_diff_when_rereview_is_full_even_after_a_previous_review() {
        // Python `dispatch.py:1026-1028`: `rereview = "full"` always gets
        // the whole lane_diff against the cut point, even once
        // `last_reviewed_commit` has a mark.
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let task = "20261101-fixer-abcdef";
        git(
            &project,
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        let branch = format!("multiverse/{task}");
        git(&project, &["checkout", "-q", "-b", &branch]);
        std::fs::write(project.join("a.rs"), "fn a() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit A",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);
        mv_state::worktrees::record_reviewed(&project, task, None);
        git(&project, &["checkout", "-q", &branch]);
        std::fs::write(project.join("b.rs"), "fn b() {}\n").unwrap();
        git(&project, &["add", "."]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "commit B (fix round)",
            ],
        );
        git(&project, &["checkout", "-q", "main"]);

        let config_dir = tempfile::tempdir().unwrap();
        let mut store = store_for(
            &project,
            config_dir.path(),
            Role::Critic,
            "20261102-critic-eeeeee",
        );
        store.reviewing = vec![task.into()];
        store.config.review_policy.rereview = "full".into();
        let out = store.review_scope().unwrap();

        assert!(out.contains(
            "### Lane 20261101-fixer-abcdef\nDiffed against this lane's own cut point, from `"
        ));
        assert!(out.contains("\nTouched files:\n- a.rs\n- b.rs\n"));
        assert!(out.contains("+fn a() {}"));
        assert!(out.contains("+fn b() {}"));
    }

    #[test]
    fn a_colliding_writer_task_id_rebinds_to_the_next_free_suffix() {
        let _herdr = HerdrEnvGuard::unreachable();
        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let config_dir = tempfile::tempdir().unwrap();

        let mut first = store_for(
            &project,
            config_dir.path(),
            Role::Builder,
            "20261102-builder-cccccc",
        );
        first.write_scope = "declared".into();
        let first_launch = first.prepare().unwrap();
        assert_eq!(first_launch.task_id, "20261102-builder-cccccc");

        let mut second = store_for(
            &project,
            config_dir.path(),
            Role::Builder,
            "20261102-builder-cccccc",
        );
        second.write_scope = "declared".into();
        let second_launch = second.prepare().unwrap();
        assert_eq!(second_launch.task_id, "20261102-builder-cccccc-2");
        assert_ne!(second_launch.cwd, first_launch.cwd);
    }

    #[test]
    fn a_colliding_reader_task_id_rebinds_to_the_next_free_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = tempfile::tempdir().unwrap();

        let mut first = store_for(
            &project,
            config_dir.path(),
            Role::Scout,
            "20261102-scout-dddddd",
        );
        first.write_scope = "none".into();
        let first_launch = first.prepare().unwrap();
        assert_eq!(first_launch.task_id, "20261102-scout-dddddd");
        std::fs::create_dir_all(
            project
                .join(".multiverse/tasks")
                .join(&first_launch.task_id),
        )
        .unwrap();

        let mut second = store_for(
            &project,
            config_dir.path(),
            Role::Scout,
            "20261102-scout-dddddd",
        );
        second.write_scope = "none".into();
        let second_launch = second.prepare().unwrap();
        assert_eq!(second_launch.task_id, "20261102-scout-dddddd-2");
    }

    #[test]
    fn into_is_refused_for_a_read_only_role_with_an_otherwise_valid_chain() {
        let _state_dir = StateDirEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = dir.path().join("codex-home");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\n\
                 config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}\"\npool = \"codex:test\"\n\n\
                 [projects.p]\npath = \"{}\"\n\n\
                 [projects.p.chains]\nscout = [\"seat\"]\n\n\
                 [roles.scout]\nwrite_scope = \"none\"\non_exhausted = \"degrade\"\n\n\
                 [roles.scout.models]\ncodex = \"gpt-test\"\n",
                config_dir.display(),
                project.display(),
            ),
        )
        .unwrap();
        let request = Request {
            project_path: project.display().to_string(),
            task: "task".into(),
            role: Some("scout".into()),
            into: Some("20261101-scout-aaaaaa".into()),
            inputs: Vec::new(),
            scope: Vec::new(),
            exclude_vendor: Vec::new(),
            reviewing: Vec::new(),
            commits: Vec::new(),
            full: false,
            column: None,
            timeout_seconds: 900.0,
            force: false,
            title: None,
            wait_for_seat: false,
            wait_for_seat_seconds: None,
            wait: false,
            dry_run: false,
            no_reconcile: false,
            config_override: Some(config_path.display().to_string()),
        };
        // A fully resolvable chain proves this is the `--into`/write-scope
        // refusal specifically, not a side effect of some other failure
        // (e.g. no chain configured) masking it.
        let error = RealEngine.dispatch(&request, plan()).unwrap_err();
        match error {
            EngineError::Refused(message) => assert!(message.contains("--into")),
            other => panic!("{other:?}"),
        }
    }

    fn account_without_model(dir: &std::path::Path) -> (Account, std::path::PathBuf) {
        let toml = format!(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            dir.display()
        );
        let config = load::load_config_str(&toml, &dir.join("config.toml")).unwrap();
        (config.accounts["seat"].clone(), dir.join("config.toml"))
    }

    #[test]
    fn model_for_account_prefers_the_account_model() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\n\
             model = \"from-account\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        assert_eq!(
            model_for_account(&config.accounts["seat"], None, "Scout").unwrap(),
            "from-account"
        );
    }

    #[test]
    fn model_for_account_falls_back_to_the_role_table() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n\n\
             [roles.scout]\nwrite_scope = \"none\"\n[roles.scout.models]\ncodex = \"from-role\"\n",
            dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        assert_eq!(
            model_for_account(
                &config.accounts["seat"],
                config.roles.get(&Role::Scout),
                "Scout"
            )
            .unwrap(),
            "from-role"
        );
    }

    #[test]
    fn model_for_account_without_either_source_is_needs_config() {
        let dir = tempfile::tempdir().unwrap();
        let (account, _) = account_without_model(dir.path());
        let error = model_for_account(&account, None, "Scout").unwrap_err();
        match error {
            EngineError::NeedsConfig(message) => {
                assert!(message.contains("no model for kind"), "{message}");
                assert!(message.contains("codex"), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_chain_account_without_a_role_model_table_is_needs_config() {
        let _state_dir = StateDirEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let config_dir = dir.path().join("codex-home");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\n\
                 config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}\"\npool = \"codex:test\"\n\n\
                 [projects.p]\npath = \"{}\"\n\n\
                 [projects.p.chains]\nscout = [\"seat\"]\n",
                config_dir.display(),
                project.display(),
            ),
        )
        .unwrap();
        let request = Request {
            project_path: project.display().to_string(),
            task: "task".into(),
            role: Some("scout".into()),
            into: None,
            inputs: Vec::new(),
            scope: Vec::new(),
            exclude_vendor: Vec::new(),
            reviewing: Vec::new(),
            commits: Vec::new(),
            full: false,
            column: None,
            timeout_seconds: 900.0,
            force: false,
            title: None,
            wait_for_seat: false,
            wait_for_seat_seconds: None,
            wait: false,
            dry_run: true,
            no_reconcile: false,
            config_override: Some(config_path.display().to_string()),
        };
        let error = RealEngine.plan(&request).unwrap_err();
        match &error {
            EngineError::NeedsConfig(message) => {
                assert!(message.contains("no model for kind"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(to_failure(error).code(), 3);
    }

    // --- collision-suffix predecessor supersede (card 20261105-b-28cb36) --
    //
    // SAFETY FIRST, per this task's brief: these four tests pin (a) a dirty
    // or unrescued-output predecessor lane is never closed or superseded,
    // (b) a working/blocked predecessor pane is never closed, (c) no lock
    // is taken re-entrantly across `recheck_admission`'s early rebind and
    // `prepare`'s own later one, and (d) the Python-matching supersede
    // actually happens in the clean case — before any of the other three
    // tests are trusted to mean the feature is safe.

    fn write_routing_log(root: &Path, contents: &str) {
        std::fs::create_dir_all(root.join(".multiverse")).unwrap();
        std::fs::write(root.join(".multiverse/routing.log"), contents).unwrap();
    }

    struct FakeLiveAgents {
        agents: Vec<mv_state::tasks::LiveAgentStatus>,
        /// Replaces `agents` from the second `list_checked()` call onward
        /// when set — scripts a state change between the first snapshot and
        /// the re-check immediately before close (finding 2).
        then_agents: Option<Vec<mv_state::tasks::LiveAgentStatus>>,
        /// Replaces `agents` for exactly one 1-indexed `list_checked()` call
        /// number, overriding `then_agents` for that call only — scripts a
        /// change landing between two specific calls rather than from one
        /// call onward (card `20261105-b-bfc75b`: a pane reused, or a
        /// session restarted under the same name, strictly between the
        /// baseline confirmation and the close-time recheck, with the
        /// baseline itself still seeing the original predecessor).
        at_call: HashMap<usize, Vec<mv_state::tasks::LiveAgentStatus>>,
        list_calls: usize,
        closed: Vec<String>,
        /// What `close` reports back — `false` scripts a close that fails
        /// even though the pane was a valid, confirmed target (finding 3).
        close_result: bool,
    }

    impl FakeLiveAgents {
        fn new(agents: Vec<mv_state::tasks::LiveAgentStatus>) -> Self {
            Self {
                agents,
                then_agents: None,
                at_call: HashMap::new(),
                list_calls: 0,
                closed: Vec::new(),
                close_result: true,
            }
        }

        fn then(mut self, agents: Vec<mv_state::tasks::LiveAgentStatus>) -> Self {
            self.then_agents = Some(agents);
            self
        }

        fn at_call(mut self, call: usize, agents: Vec<mv_state::tasks::LiveAgentStatus>) -> Self {
            self.at_call.insert(call, agents);
            self
        }

        fn closing_fails(mut self) -> Self {
            self.close_result = false;
            self
        }
    }

    impl mv_state::tasks::AgentPanes for FakeLiveAgents {
        fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
            self.agents
                .iter()
                .map(|agent| mv_state::tasks::LiveAgent {
                    pane_id: agent.pane_id.clone(),
                })
                .collect()
        }

        fn close(&mut self, pane_id: &str) -> bool {
            self.closed.push(pane_id.to_string());
            if self.close_result {
                self.agents.retain(|agent| agent.pane_id != pane_id);
            }
            self.close_result
        }

        fn list_checked(&mut self) -> Option<Vec<mv_state::tasks::LiveAgentStatus>> {
            self.list_calls += 1;
            if let Some(agents) = self.at_call.get(&self.list_calls) {
                return Some(agents.clone());
            }
            if self.list_calls > 1 {
                if let Some(then_agents) = &self.then_agents {
                    return Some(then_agents.clone());
                }
            }
            Some(self.agents.clone())
        }
    }

    fn idle_predecessor_log() -> &'static str {
        "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20260101-builder-aaaaaa\t\
         branch=multiverse/20260101-builder-aaaaaa\trole=builder\tagent=a1\n\
         2026-01-01T00:05:00+00:00\tevent=result\ttask=20260101-builder-aaaaaa\t\
         status=done\tstate=done\trevisions=0\tpane=w1:p1\n"
    }

    fn idle_live_agent() -> mv_state::tasks::LiveAgentStatus {
        mv_state::tasks::LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: Some("a1".into()),
            status: Some(mv_engine::mv_herdr::envelope::AgentState::Idle),
        }
    }

    /// (d): the clean case — a RECORDED, idle, live predecessor is
    /// superseded and its pane closed, mirroring Python's
    /// `runner.supersede_open_redispatch` in the ordinary case.
    #[test]
    fn supersede_collision_predecessor_closes_a_clean_idle_recorded_predecessor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]);

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert_eq!(agents.closed, vec!["w1:p1".to_string()]);
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=supersede") && log.contains("state=superseded-redispatched"),
            "{log}"
        );
    }

    /// supersede-review.md finding 1: a pane id is reusable. The
    /// predecessor's *recorded* pane being listed live is not evidence that
    /// the predecessor's own dispatched agent is still in it — a bare
    /// pane-id match would close an unrelated occupant instead.
    #[test]
    fn supersede_collision_predecessor_never_closes_a_predecessor_pane_under_an_unrelated_agent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = FakeLiveAgents::new(vec![mv_state::tasks::LiveAgentStatus {
            pane_id: "w1:p1".into(),
            name: Some("someone-else".into()),
            status: Some(mv_engine::mv_herdr::envelope::AgentState::Idle),
        }]);

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert!(
            agents.closed.is_empty(),
            "a reused pane under an unrelated agent must never be closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=supersede"),
            "no outcome may be recorded for an occupant that isn't the predecessor: {log}"
        );
    }

    /// supersede-review.md finding 2: the idle/done confirmation above is
    /// re-run, fresh, immediately before the close — a predecessor that
    /// starts working again between that snapshot and the close must abort
    /// rather than race it.
    #[test]
    fn supersede_collision_predecessor_aborts_when_the_predecessor_stops_being_ready_before_close()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]).then(vec![
            mv_state::tasks::LiveAgentStatus {
                pane_id: "w1:p1".into(),
                name: Some("a1".into()),
                status: Some(mv_engine::mv_herdr::envelope::AgentState::Working),
            },
        ]);

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert!(
            agents.closed.is_empty(),
            "a predecessor that started working again before the close must not be closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=supersede"),
            "no evidence should be written for a close aborted by the fresh re-check: {log}"
        );
    }

    /// supersede-review.md finding 3: a failed `AgentPanes::close` must
    /// refuse the call rather than silently succeed, since the predecessor
    /// may still be live and the new dispatch must not be admitted
    /// alongside it. Lifecycle evidence is still written first — the close
    /// is the last step, not a precondition for it.
    #[test]
    fn supersede_collision_predecessor_refuses_when_the_pane_close_itself_fails() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]).closing_fails();

        let error = supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap_err();

        assert!(error.to_string().contains("could not be closed"), "{error}");
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=supersede") && log.contains("state=superseded-redispatched"),
            "evidence must already be written by the time the close is attempted: {log}"
        );
    }

    /// (a): a predecessor whose lane has uncommitted changes is refused,
    /// not closed — the pane stays open and nothing is recorded, so the
    /// old task remains resumable/rescuable exactly as the brief requires.
    #[test]
    fn supersede_collision_predecessor_never_closes_a_dirty_predecessor_lane() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        write_routing_log(&root, idle_predecessor_log());
        let lane_root = root
            .join(".multiverse/worktrees")
            .join("20260101-builder-aaaaaa");
        std::fs::create_dir_all(&lane_root).unwrap();
        git(&lane_root, &["init", "-q", "-b", "main"]);
        std::fs::write(lane_root.join("dirty.txt"), "uncommitted\n").unwrap();
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]);

        let error = supersede_collision_predecessor(
            &root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap_err();

        assert!(error.to_string().contains("uncommitted changes"), "{error}");
        assert!(
            agents.closed.is_empty(),
            "a dirty predecessor lane's pane must never be closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=supersede"),
            "a refused dirty predecessor must never be recorded superseded: {log}"
        );
    }

    /// (a), the unrescued-output half: reuses
    /// [`mv_state::tasks::lanes_holding_unrescued_output`] — the exact check
    /// `commands::stop` already refuses to close a pane over — rather than
    /// inventing a second one here.
    #[test]
    fn supersede_collision_predecessor_never_closes_a_lane_holding_unrescued_output() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(
            &root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        write_routing_log(&root, idle_predecessor_log());
        let worktrees = mv_state::worktrees::Worktrees::new(&root).unwrap();
        let lane = worktrees.create("20260101-builder-aaaaaa", "HEAD").unwrap();
        // Committed, not merely written: a dirty (uncommitted) lane is
        // already refused by the earlier gate this test does not want to
        // exercise — this isolates the unrescued-output gate on its own.
        let relative = Path::new(".multiverse/tasks/20260101-builder-cccccc/result.md");
        std::fs::create_dir_all(lane.path.join(relative).parent().unwrap()).unwrap();
        std::fs::write(lane.path.join(relative), "unrescued output\n").unwrap();
        git(&lane.path, &["add", "-f", relative.to_str().unwrap()]);
        git(
            &lane.path,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "-qm",
                "unrescued result",
            ],
        );
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]);

        let error = supersede_collision_predecessor(
            &root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("unrescued task output"),
            "{error}"
        );
        assert!(
            agents.closed.is_empty(),
            "a lane holding unrescued output must never be closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("event=supersede"), "{log}");
    }

    /// (b): a predecessor pane Herdr reports as anything other than ready
    /// for input is left exactly alone — no close, no outcome recorded, no
    /// refusal. The new, collision-suffixed dispatch simply proceeds
    /// alongside it.
    #[test]
    fn supersede_collision_predecessor_never_closes_a_working_or_blocked_predecessor_pane() {
        for status in [
            mv_engine::mv_herdr::envelope::AgentState::Working,
            mv_engine::mv_herdr::envelope::AgentState::Blocked,
            mv_engine::mv_herdr::envelope::AgentState::Unknown,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write_routing_log(root, idle_predecessor_log());
            let mut agents = FakeLiveAgents::new(vec![mv_state::tasks::LiveAgentStatus {
                pane_id: "w1:p1".into(),
                name: Some("a1".into()),
                status: Some(status),
            }]);

            supersede_collision_predecessor(
                root,
                "20260101-builder-aaaaaa-2",
                "20260101-builder-aaaaaa",
                &mut agents,
            )
            .unwrap();

            assert!(
                agents.closed.is_empty(),
                "{status:?} pane must never be closed"
            );
            let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
            assert!(!log.contains("event=supersede"), "{status:?}: {log}");
        }
    }

    // --- durable identity / close-time TOCTOU hardening (card
    // 20261105-b-bfc75b) ----------------------------------------------------
    //
    // The tests above already pin that a stale *first* snapshot never
    // authorizes a close. These pin the narrower, later race the card
    // raises: the pane id being reused by an unrelated occupant, or the
    // predecessor's own session being replaced under the same name,
    // strictly *after* the baseline confirmation already matched — i.e.
    // between the two reads `confirm_live` makes, not before the first one.

    /// A pane id is reusable: if some other occupant has taken over
    /// `live_agent.pane_id` by the time of the close-time recheck — even one
    /// that happens to be ready — the close must still be refused, because
    /// `confirm_live`'s own lookup is keyed by the predecessor's *name*, and
    /// that name is no longer who answers there.
    #[test]
    fn supersede_collision_predecessor_aborts_when_the_pane_is_reused_by_a_different_name_before_close(
    ) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        // Call 1: initial discovery. Call 2: the baseline `confirm_live`
        // right after discovery — both still see the real predecessor.
        // Call 3: the close-time `confirm_live`, scripted alone via
        // `at_call` to see an unrelated, unrelated-named occupant that has
        // since taken the same pane.
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]).at_call(
            3,
            vec![mv_state::tasks::LiveAgentStatus {
                pane_id: "w1:p1".into(),
                name: Some("someone-else".into()),
                status: Some(mv_engine::mv_herdr::envelope::AgentState::Idle),
            }],
        );

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert!(
            agents.closed.is_empty(),
            "a pane reused by a different name between the baseline and the close-time \
             recheck must never be closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=supersede"),
            "no outcome may be recorded once the occupant at close time isn't the \
             predecessor: {log}"
        );
    }

    /// The same race as finding 2's own test, but landing strictly between
    /// the baseline confirmation and the close-time recheck rather than
    /// being visible at the baseline already — proves the *final* read,
    /// not just an earlier one, is what actually gates the close.
    #[test]
    fn supersede_collision_predecessor_aborts_when_readiness_is_lost_strictly_before_the_close() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = FakeLiveAgents::new(vec![idle_live_agent()]).at_call(
            3,
            vec![mv_state::tasks::LiveAgentStatus {
                pane_id: "w1:p1".into(),
                name: Some("a1".into()),
                status: Some(mv_engine::mv_herdr::envelope::AgentState::Working),
            }],
        );

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert!(
            agents.closed.is_empty(),
            "a predecessor that starts working again strictly before the close must not be \
             closed"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(!log.contains("event=supersede"), "{log}");
    }

    /// A fake whose `confirm_live` is scripted directly (not derived from
    /// `list_checked`) so it can report a `state_change_seq` — something no
    /// [`mv_state::tasks::LiveAgentStatus`]-based fake can produce, since
    /// that type deliberately carries no such field. Models a brand new
    /// Herdr session that has taken over the same pane under the identical
    /// name (same role/seat, a later, unrelated dispatch) strictly between
    /// the baseline confirmation and the close-time recheck: pane id and
    /// name both still match, but the counter resets rather than advancing.
    struct SeqRegressionAgents {
        confirm_calls: usize,
        closed: Vec<String>,
    }

    impl mv_state::tasks::AgentPanes for SeqRegressionAgents {
        fn list(&mut self) -> Vec<mv_state::tasks::LiveAgent> {
            vec![mv_state::tasks::LiveAgent {
                pane_id: "w1:p1".into(),
            }]
        }

        fn close(&mut self, pane_id: &str) -> bool {
            self.closed.push(pane_id.to_string());
            true
        }

        fn list_checked(&mut self) -> Option<Vec<mv_state::tasks::LiveAgentStatus>> {
            Some(vec![idle_live_agent()])
        }

        fn confirm_live(
            &mut self,
            pane_id: &str,
            name: &str,
        ) -> Option<mv_state::tasks::LiveConfirmation> {
            assert_eq!(pane_id, "w1:p1");
            assert_eq!(name, "a1");
            self.confirm_calls += 1;
            Some(mv_state::tasks::LiveConfirmation {
                ready: true,
                state_change_seq: Some(if self.confirm_calls == 1 { 5 } else { 3 }),
            })
        }
    }

    /// Card `20261105-b-bfc75b`: a continuing Herdr session's own
    /// `state_change_seq` never goes backwards. A lower value at the
    /// close-time recheck than at the baseline is proof a different session
    /// has taken over the same name in the same pane, even though identity
    /// and readiness both still read as a match — this must refuse the
    /// close exactly like any other occupant change, not treat the matching
    /// name/pane/readiness as sufficient on their own.
    #[test]
    fn supersede_collision_predecessor_aborts_when_state_change_seq_goes_backwards_before_close() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_routing_log(root, idle_predecessor_log());
        let mut agents = SeqRegressionAgents {
            confirm_calls: 0,
            closed: Vec::new(),
        };

        supersede_collision_predecessor(
            root,
            "20260101-builder-aaaaaa-2",
            "20260101-builder-aaaaaa",
            &mut agents,
        )
        .unwrap();

        assert!(
            agents.closed.is_empty(),
            "a state_change_seq regression must refuse the close even though name, pane, \
             and readiness all still matched"
        );
        assert_eq!(
            agents.confirm_calls, 2,
            "both the baseline and close-time reads must run"
        );
        let log = std::fs::read_to_string(root.join(".multiverse/routing.log")).unwrap();
        assert!(
            !log.contains("event=supersede"),
            "no outcome may be recorded for a session that isn't actually the predecessor: {log}"
        );
    }

    /// (c): `recheck_admission`'s early rebind-and-supersede must not leave
    /// the project lock held once it returns — `prepare`'s own later
    /// `lock_project()` call (the identical `ProjectLog::new(path).lock()`)
    /// has to find it free, sequential rather than nested. A regression
    /// that moved the supersede write back inside `prepare`'s own
    /// project-lock scope (the self-deadlock risk both prior investigations
    /// of card `20261105-b-28cb36` flagged) would fail the mid-test lock
    /// probe below even though this lock is itself thread-local-reentrant
    /// (`mv_core::lock`) and so would not actually hang.
    #[test]
    fn recheck_admission_settles_a_live_predecessor_and_frees_the_project_lock_before_prepare() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        // `.into_temp_path()` closes the `tempfile()` handle immediately:
        // an open write fd on the script (Linux only; not a race) makes
        // the later `execve` fail with `ETXTBSY` ("text file busy").
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap()
            .into_temp_path();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$1 $2\" in\n  \
                 \"agent list\") printf '%s' '{{\"result\": {{\"agents\": \
                 [{{\"pane_id\": \"w1:p1\", \"agent_status\": \"idle\", \"name\": \"a1\"}}]}}}}' ;;\n  \
                 \"agent get\") printf '%s' '{{\"result\": {{\"agent\": \
                 {{\"pane_id\": \"w1:p1\", \"agent_status\": \"idle\", \"state_change_seq\": 1}}}}}}' ;;\n  \
                 *) printf '%s' '{{\"result\": {{}}}}' ;;\nesac\n",
                marker.path().display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let _guard = HerdrEnvGuard::set(&script);

        let repo = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(repo.path()).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(
            &project,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        let worktrees = mv_state::worktrees::Worktrees::new(&project).unwrap();
        worktrees.create("20261105-builder-aaaaaa", "HEAD").unwrap();
        write_routing_log(
            &project,
            "2026-01-01T00:00:00+00:00\tevent=dispatch\ttask=20261105-builder-aaaaaa\t\
             branch=multiverse/20261105-builder-aaaaaa\trole=builder\tagent=a1\n\
             2026-01-01T00:05:00+00:00\tevent=result\ttask=20261105-builder-aaaaaa\t\
             status=done\tstate=done\trevisions=0\tpane=w1:p1\n",
        );

        let config_dir = tempfile::tempdir().unwrap();
        let config_toml = format!(
            "[accounts.author]\nkind = \"codex\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{}\"\npool = \"codex:test\"\nshort = \"cx\"\n\
             [projects.p]\npath = \"{}\"\n[projects.p.chains]\nbuilder = [\"author\"]\n\
             [roles.builder]\nwrite_scope = \"declared\"\nskills = []\n\
             [roles.builder.models]\ncodex = \"gpt-test\"\n",
            config_dir.path().join("account").display(),
            project.display(),
        );
        let config_path_dir = tempfile::tempdir().unwrap();
        let config_path = config_path_dir.path().join("config.toml");
        std::fs::write(&config_path, config_toml).unwrap();
        let config = load_config(Some(config_path.to_str().unwrap())).unwrap();
        let account = config.accounts["author"].clone();

        let mut store = RealDispatchStore::for_seat(
            config,
            config_dir.path().to_path_buf(),
            "p".into(),
            project.clone(),
            Role::Builder,
            account,
            "test-model".into(),
            "20261105-builder-aaaaaa".into(),
            "same task text".into(),
            "test".into(),
            false,
        );
        store.write_scope = "declared".into();
        store.no_reconcile = true;

        store.recheck_admission().unwrap();
        assert_eq!(store.task_id, "20261105-builder-aaaaaa-2");

        let fresh = mv_state::log::ProjectLog::new(&project)
            .lock_timeout(std::time::Duration::from_millis(200));
        assert!(
            fresh.is_ok(),
            "the project lock must be free between recheck_admission and prepare, not still \
             held by the supersede write"
        );
        drop(fresh);

        let launch = store.prepare().unwrap();
        assert_eq!(launch.task_id, "20261105-builder-aaaaaa-2");

        let log = std::fs::read_to_string(project.join(".multiverse/routing.log")).unwrap();
        assert!(
            log.contains("event=supersede") && log.contains("state=superseded-redispatched"),
            "{log}"
        );
        let calls = std::fs::read_to_string(marker.path()).unwrap();
        assert!(
            calls.lines().any(|line| line == "pane close w1:p1"),
            "{calls:?}"
        );
    }
}

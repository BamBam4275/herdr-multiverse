//! `multiverse council`: ask every seated vendor the same question.
//!
//! The real roster/convene logic lives in `mv-engine::council` — seat/plan/
//! capacity_or_raise/recheck_seat_availability/recheck_seat_caps/convene/
//! quorum_report, a full roster/quorum/convene library over `crate::runner`
//! and `crate::selector`, same as `commands::dispatch` already uses for a
//! single seat. [`RealEngine`] is the glue, in the same shape as
//! `commands::dispatch`'s own `RealEngine`: it reads `mv-config`'s
//! `config.toml`, resolves the project and its `[projects.<name>.chains.
//! council]` pool into `mv-engine::selector::Seat`s, seats a roster through
//! `mv_engine::council::ChainSelector` (the binding from that narrow pool to
//! `selector::select`), and — for a real `--yes` convene — builds one
//! `commands::dispatch::RealDispatchStore` per seated councillor
//! ([`RealDispatchStore::for_seat`], the one addition this task made to
//! that file) and runs every seat at once through `mv_engine::council::
//! convene` against `mv_engine::runner::ProcessHerdr`.
//!
//! [`refuse_without_yes`] is unchanged by this: `commands::mod`'s
//! `authorization` gate calls it in place of the generic "council requires
//! --yes" refusal, so a missing `--yes` seats the real council (reading
//! `mv_config`, same as every other command here) and prints its roster
//! before refusing — "the heaviest path in the system is never a silent
//! default" (conformance scenario `council-refusal-without-yes`). `--yes`
//! and `--dry-run` bypass this gate entirely and reach [`run_default`],
//! which now binds [`RealEngine`] rather than the old `Unimplemented` stub.
//!
//! `accounts prefer` reorders the council chain through dispatch's shared
//! helper, preserving each kind's positions. A capped seat's live open-agent
//! count is read — one `agent list` call
//! per [`RealEngine::resolve`], folded into [`seat_from_account`] the same
//! way `commands::dispatch`'s own `seat_from_account` already does, scout2
//! gap #24 (`council.py:211-276`'s live open-count vs cap, ported without
//! that sibling's post-start grace-window credit — a council's `plan`/
//! `convene` is one shot, not dispatch's repeated admission-recheck
//! cadence, so a seat this lane just started is already visible in the
//! next `agent list` listing well before a second council could be seated
//! against it). Counts are keyed by account **name** (Python
//! `open_agent_counts` / `recheck_seat_caps`), never by a trailing name
//! segment, and `unknown` is not an open state (`accounts.py:623`).

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ArgMatches;
use sha2::{Digest, Sha256};

use mv_config::load::{
    self, Account, Config, FallbackPolicy as ConfigFallbackPolicy, RoleSettings,
};
use mv_config::roles::Role;
use mv_engine::council;
use mv_engine::dispatch as engine_dispatch;
use mv_engine::mv_herdr::argv;
use mv_engine::runner;
use mv_engine::selector;
use mv_engine::selector::LocalLivenessProbe as _;

use crate::exit::Failure;

use super::dispatch::RealDispatchStore;

/// Everything this command needs from Council's seat/convene logic.
pub trait Engine {
    fn plan(&self, request: &Request) -> Result<Roster, EngineError>;
    fn convene(&self, request: &Request, roster: Roster) -> Result<Convened, EngineError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub project_path: String,
    pub question: String,
    pub dry_run: bool,
    pub timeout_seconds: f64,
}

/// One seated vendor, before it has run — scout2-gaps.md #50
/// (`cli.py:8826-8911`'s always-printed roster: account/vendor/model/
/// task_id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterSeat {
    pub account: String,
    pub vendor: String,
    pub model: String,
    pub task_id: String,
}

/// The seated accounts, one per vendor, before any seat has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roster {
    pub seats: Vec<RosterSeat>,
}

/// What became of one seat after a real convene — scout2-gaps.md #50's
/// per-seat line: `FAILED — …`, or a settled status and where its
/// `result.md` landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatOutcome {
    Answered {
        status: String,
        result_path: PathBuf,
    },
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatReport {
    pub account: String,
    pub outcome: SeatOutcome,
}

/// The tally after every active seat has settled. Council never votes or
/// synthesizes an answer — this is a count, not a decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Convened {
    pub answered: usize,
    pub vendors: usize,
    /// One report per seat that was actually started — in the same order
    /// [`Roster::seats`] listed them.
    pub seats: Vec<SeatReport>,
}

impl Convened {
    /// At least two distinct vendors must have answered.
    fn is_quorate(&self) -> bool {
        self.vendors >= council::MINIMUM_COUNCILLORS
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// Fewer than two distinct vendors could be seated at all — a
    /// configuration/eligibility problem, not an execution failure.
    TooSmallRoster(String),
    /// `config.toml` itself could not be resolved, read, or parsed —
    /// prefixed "config error:" by [`to_failure`], same as
    /// `commands::dispatch`'s own `EngineError::ProjectResolution`.
    ProjectResolution(String),
    /// The project path does not resolve to a configured project, or that
    /// project has no `council` chain — no prefix added, the message is
    /// already complete on its own.
    NeedsConfig(String),
    /// Over the concurrency cap, or any other plan/convene refusal.
    Refused(String),
}

fn to_failure(error: EngineError) -> Failure {
    match error {
        EngineError::TooSmallRoster(message) | EngineError::NeedsConfig(message) => {
            Failure::AccountSelection(message)
        }
        EngineError::ProjectResolution(message) => Failure::Config(message),
        EngineError::Refused(message) => Failure::Operation(message),
    }
}

const DEFAULT_TIMEOUT_SECONDS: f64 = 900.0;

fn request_from(args: &ArgMatches) -> Request {
    Request {
        project_path: args.get_one::<String>("path").cloned().unwrap_or_default(),
        question: args
            .get_one::<String>("question")
            .cloned()
            .unwrap_or_default(),
        dry_run: args.get_flag("dry-run"),
        timeout_seconds: args
            .get_one::<String>("timeout")
            .and_then(|raw| raw.parse::<f64>().ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS),
    }
}

/// The project at `request.project_path` and its council candidate pool —
/// `[projects.<name>.chains.council]`, read whole, "a candidate pool, not
/// a chain" (`migrate.rs`'s own commentary), not a first-available walk —
/// or `None` for anything that keeps this from seating a real council:
/// the path does not resolve to a configured project, or that project has
/// no council pool, or a pool entry names an account that is not
/// configured. The no-`--yes` gate falls back to the generic refusal for
/// every one of those, rather than surfacing a config error of its own —
/// deciding "needs configuration" is `multiverse doctor`/the real
/// `--yes` path's job, not this preview's.
fn seat_council<'a>(config: &'a Config, request: &Request) -> Option<(PathBuf, Vec<&'a Account>)> {
    let project_path = std::fs::canonicalize(&request.project_path).ok()?;
    let project = config.projects.values().find(|p| {
        super::doctor::project_path(p)
            .and_then(|path| std::fs::canonicalize(path).ok())
            .as_deref()
            == Some(project_path.as_path())
    })?;
    let pool_names = project.chains.get(&Role::Council)?;
    if pool_names.is_empty() {
        return None;
    }
    let pool = pool_names
        .iter()
        .map(|name| config.accounts.get(name))
        .collect::<Option<Vec<_>>>()?;
    Some((project_path, pool))
}

/// `commands::mod`'s `authorization` gate calls this for `council` in
/// place of the generic "council requires --yes to execute" refusal. It
/// seats the real council and prints its roster first whenever `config`
/// and `request.project_path` resolve to one; anything short of that
/// falls back to the exact text the generic gate used to print, so a
/// bogus path or missing config still refuses the same way it always has.
pub fn refuse_without_yes(args: &ArgMatches) -> Result<(), Failure> {
    let request = request_from(args);
    let generic = || {
        Err(Failure::Operation(
            "council requires --yes to execute; use --dry-run to preview".to_string(),
        ))
    };

    let Ok((config_path, _source)) =
        super::doctor::resolve_config_path(args.get_one::<String>("config").map(String::as_str))
    else {
        return generic();
    };
    if !config_path.is_file() {
        return generic();
    }
    let Ok(config) = mv_config::load::load_config(&config_path) else {
        return generic();
    };
    let Some((project_path, pool)) = seat_council(&config, &request) else {
        return generic();
    };

    println!(
        "Council for {}: {:?}",
        project_path.display(),
        request.question
    );
    for account in &pool {
        println!("  {} ({})", account.name, account.kind);
    }

    Err(Failure::Operation(format!(
        "refusing to convene {} agents without --yes. This is the heaviest path in the system.",
        pool.len()
    )))
}

/// Run `council` against any [`Engine`]. `commands::run`'s `council` match
/// arm calls [`run_default`], which binds [`RealEngine`].
///
/// The seated roster (account/vendor/model/task_id) prints unconditionally,
/// before anything is convened — scout2-gaps.md #50, Python's
/// `cli.py:8826-8911`: a dry-run and a real `--yes` convene both show what
/// would run before either decides whether to actually run it. A real
/// convene then prints one line per seat afterward — `FAILED — …`, or its
/// settled status and `result.md` path.
pub fn run(args: &ArgMatches, engine: &dyn Engine, out: &mut dyn Write) -> Result<(), Failure> {
    let request = request_from(args);
    let dry_run = request.dry_run;
    let roster = engine.plan(&request).map_err(to_failure)?;
    for seat in &roster.seats {
        let _ = writeln!(
            out,
            "  {} ({}) model={} task={}",
            seat.account, seat.vendor, seat.model, seat.task_id
        );
    }
    if dry_run {
        let _ = writeln!(
            out,
            "Would ask {} councillors: {}.",
            roster.seats.len(),
            roster
                .seats
                .iter()
                .map(|seat| seat.account.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(());
    }
    let convened = engine.convene(&request, roster).map_err(to_failure)?;
    for seat in &convened.seats {
        match &seat.outcome {
            SeatOutcome::Answered {
                status,
                result_path,
            } => {
                let _ = writeln!(
                    out,
                    "  {}: status {status} {}",
                    seat.account,
                    result_path.display()
                );
            }
            SeatOutcome::Failed(reason) => {
                let _ = writeln!(out, "  FAILED — {}: {reason}", seat.account);
            }
        }
    }
    if convened.is_quorate() {
        let _ = writeln!(
            out,
            "{} councillors answered across {} vendors. Nothing was synthesized.",
            convened.answered, convened.vendors
        );
        Ok(())
    } else {
        Err(Failure::Operation(
            "That is not a council — read the results as individual opinions.".into(),
        ))
    }
}

// ---------------------------------------------------------------------
// RealEngine: config -> council chain -> seated roster -> convene
// ---------------------------------------------------------------------

/// `target`, made absolute against `cwd` and canonicalized — same rule
/// `commands::dispatch`'s own `resolve_user_path` uses, duplicated here
/// rather than reused: a council seat's `Request` has no `role`/`into`/
/// `scope`/`force` of `dispatch::Request`'s shape to share a resolver
/// signature with, and this file's write scope does not extend to
/// widening `dispatch.rs` beyond the one constructor noted on
/// [`RealDispatchStore::for_seat`].
fn resolve_user_path(raw: &str, cwd: &Path) -> Result<PathBuf, String> {
    let candidate = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    std::fs::canonicalize(&candidate)
        .map_err(|e| format!("{raw:?} does not resolve to a usable path: {e}"))
}

/// `config.project_for_path` (longest-configured-root-prefix match): every
/// `[projects.<name>]` whose own (canonicalized) `path` is `target` or an
/// ancestor of it is a candidate; the deepest one wins.
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

/// Read `config.toml` at the path `$MULTIVERSE_CONFIG`/`--config`/the
/// default would resolve to, tolerating retired `[roles.<old>]` chain
/// keys the same way `commands::dispatch`'s own `load_config` does
/// (migrated in memory only, never written back) — `council`'s own role
/// key was never renamed, but a config.toml using retired names for
/// *other* roles must still load.
fn load_config() -> Result<Config, EngineError> {
    let path = load::resolve_config_path_from_env(None).map_err(|e| {
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

/// Whether a Herdr `agent_status` counts as "open" for a seat cap —
/// Python's `_OPEN_AGENT_STATES` (`accounts.py:623-624`): only
/// `working`/`blocked` hold a slot. `unknown` is excluded on purpose
/// ("not evidence of anything, open or otherwise"), matching
/// `commands::dispatch`'s own `is_open_agent_status`.
fn is_open_agent_status(status: &str) -> bool {
    matches!(status, "working" | "blocked")
}

/// Every agent `name` Herdr's listing reports, with its raw
/// `agent_status` — Python's `status_by_name` (`accounts.py:748-752`).
fn agent_status_by_name(
    result: &mv_engine::mv_herdr::envelope::CommandResult,
) -> HashMap<String, String> {
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
/// reissued once its previous occupant is no longer live, so several
/// records sharing one reissued name collapse to the newest dispatch.
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

/// Fold one `agent list` reply into open-agent counts, attributed to the
/// account that actually dispatched each pane — Python's
/// `open_agent_counts` (`accounts.py:686-708`): "An agent's account is
/// not read off its own name ... but off the dispatch record that
/// started it". Keys are **account names**, the same key
/// [`council::recheck_seat_caps`] looks up (`council.py:508`
/// `counts.get(councillor.account, 0)`). An agent name not found in any
/// project's log contributes to no seat's count, rather than guessing
/// from a trailing name segment. Collision-suffix names
/// (`{slug}-{role}-{short}-{instance}`) therefore still count against
/// the recorded account, not against the instance number. A dispatch
/// whose `account=` is no longer a config key (the seat was renamed)
/// still counts, recovered from the agent name's trailing short via
/// [`super::dispatch::resolve_task_account`] — Python's
/// `account_by_short` (`accounts.py:784-804`). No post-start
/// grace-window credit (see this file's module doc).
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
        let Some(account) = super::dispatch::resolve_task_account(task, config) else {
            continue;
        };
        *counts.entry(account.name.clone()).or_insert(0) += 1;
    }
    counts
}

/// One live `agent list` call, folded into per-seat open counts keyed by
/// account name — council's own narrower copy of
/// `commands::dispatch`'s `live_open_counts` (scout2 gap #24), without
/// that sibling's post-start grace-window credit (see this file's module
/// doc for why), and keyed by account **name** rather than short so a
/// later [`council::recheck_seat_caps`] call can consume the same map
/// (critic-grok3 row 2 / row 5). `None` when Herdr could not be asked at
/// all, or answered with an error — the same fail-closed reading
/// [`seat_from_account`] already gives a capped seat either way.
fn live_open_counts(
    config: &Config,
    herdr: &mut impl runner::Herdr,
) -> Option<HashMap<String, u32>> {
    let result = herdr
        .call(&argv::agent_list(), Duration::from_secs(10))
        .ok()?;
    if !result.ok() {
        return None;
    }
    let statuses = agent_status_by_name(&result);
    let latest = latest_tasks_by_agent(config);
    Some(live_seat_counts(&statuses, &latest, config))
}

/// One chain entry's config, joined with the live seatstate reads
/// `mv_engine::seatstate` already exposes, and — for a *capped* seat
/// only — the live per-seat open-agent count folded from `live_counts`.
/// `live_counts` is computed once per [`RealEngine::resolve`] call,
/// outside this function, so a chain of several capped seats costs one
/// Herdr round trip rather than one per seat. `None` there (Herdr could
/// not be asked, or was never asked because nothing in this chain is
/// capped) reads as "unreadable" — `selector::availability` already
/// treats that as at-capacity for a capped seat, same as before this fix.
fn seat_from_account(
    account: &Account,
    config_dir: &Path,
    live_counts: &Option<HashMap<String, u32>>,
) -> selector::Seat {
    let cap = mv_engine::seatstate::get_cap(config_dir, &account.name);
    let open_count = cap.and_then(|_| {
        live_counts
            .as_ref()
            .map(|counts| counts.get(&account.name).copied().unwrap_or(0))
    });
    selector::Seat {
        name: account.name.clone(),
        vendor: account.vendor.clone(),
        pool: account.pool.clone(),
        kind: account.kind.clone(),
        available: account.available,
        unavailable_reason: account.unavailable_reason.clone(),
        unavailable_until: None,
        lent: mv_engine::seatstate::is_lent(config_dir, &account.name),
        // Same posture as `commands::dispatch`: chain validation does not
        // require `[roles.council]`, so a miss is [`model_for_account`]'s
        // `NeedsConfig`, not a panic.
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

/// Account `model`, else `[roles.council.models.<kind>]`. `[roles.council]`
/// is optional at load — `validate_chain_entry` only checks models when
/// that table exists — so a miss is a config error, never a panic.
fn model_for_account(
    account: &Account,
    role_settings: Option<&RoleSettings>,
) -> Result<String, EngineError> {
    if let Some(model) = &account.model {
        return Ok(model.clone());
    }
    role_settings
        .and_then(|settings| settings.models.get(&account.kind).cloned())
        .ok_or_else(|| {
            EngineError::NeedsConfig(format!(
                "{} has no model for kind {:?}",
                Role::Council.display_name(),
                account.kind
            ))
        })
}

/// `accounts.<name>.config_dir`/`.model`/`.kind` still unfilled
/// `<<PLACEHOLDER: ...>>` sentinels, named the way `commands::dispatch`'s
/// own dry-run preview already names an unresolved value
/// (`dispatch.rs:1658-1677`). Empty when every value this seat needs is
/// real.
fn unresolved_placeholders(account: &Account, model: &str) -> Vec<String> {
    let mut unresolved = Vec::new();
    if account.config_dir.is_placeholder() {
        unresolved.push(format!("accounts.{}.config_dir", account.name));
    }
    if mv_config::placeholder::is_placeholder(model) {
        unresolved.push(format!("accounts.{}.model", account.name));
    }
    if mv_config::placeholder::is_placeholder(&account.kind) {
        unresolved.push(format!("accounts.{}.kind", account.name));
    }
    unresolved
}

/// Every active (not skipped) seat's display roster entry —
/// account/vendor/model/task_id — shared by [`RealEngine::plan`] (so a
/// dry-run shows it) and [`RealEngine::convene`] (so a real run prints the
/// same roster before starting anything). Scout2-gaps.md #50's "Unfilled
/// placeholders → exit 3, nothing convened" lives here too: a seat whose
/// config still names a `<<PLACEHOLDER: ...>>` value refuses the whole
/// roster rather than convening the seats that happen to be ready.
fn roster_seats(
    resolved: &Resolved,
    seated: &council::Council,
) -> Result<Vec<RosterSeat>, EngineError> {
    let role_settings = resolved.config.roles.get(&Role::Council);
    let mut seats = Vec::new();
    let mut unresolved = Vec::new();
    for councillor in seated.councillors.iter().filter(|c| !c.skipped) {
        let account = &resolved.config.accounts[&councillor.seat.account];
        let model = model_for_account(account, role_settings)?;
        let task_id = task_id_for(&resolved.project_root, &seated.question, &account.short);
        unresolved.extend(unresolved_placeholders(account, &model));
        seats.push(RosterSeat {
            account: account.name.clone(),
            vendor: councillor.seat.vendor.clone(),
            model,
            task_id,
        });
    }
    if !unresolved.is_empty() {
        return Err(EngineError::NeedsConfig(format!(
            "council cannot convene — unresolved config values: {}",
            unresolved.join(", ")
        )));
    }
    Ok(seats)
}

fn to_selector_policy(policy: ConfigFallbackPolicy) -> selector::FallbackPolicy {
    match policy {
        ConfigFallbackPolicy::Degrade => selector::FallbackPolicy::Degrade,
        ConfigFallbackPolicy::DegradeOnce => selector::FallbackPolicy::DegradeOnce,
        ConfigFallbackPolicy::Queue => selector::FallbackPolicy::Queue,
        ConfigFallbackPolicy::Warn => selector::FallbackPolicy::Warn,
        ConfigFallbackPolicy::Shrink => selector::FallbackPolicy::Degrade,
    }
}

/// A stable `<ID>`-shaped token matching Python `dispatch.task_id_for`
/// (`dispatch.py:410-450`) with `role=council`. When `seat` is the
/// councillor's account short, the id is `{date}-council-{seat}-{digest}`
/// and the seat is folded into the hash (`seat_in_id=True`), so N
/// councillors never share a task directory. Empty `seat` keeps the
/// no-suffix shape the selector uses (Python's default `seat=""`).
fn task_id_for(project_root: &Path, question: &str, seat: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_root.as_os_str().as_encoded_bytes());
    hasher.update([0u8]);
    hasher.update(b"council");
    hasher.update([0u8]);
    hasher.update(question.as_bytes());
    if !seat.is_empty() {
        hasher.update([0u8]);
        hasher.update(seat.as_bytes());
    }
    let digest = hasher.finalize();
    let hex6 = digest[..3]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let today = time::OffsetDateTime::now_utc().date();
    let suffix = if seat.is_empty() {
        String::new()
    } else {
        format!("-{seat}")
    };
    format!(
        "{:04}{:02}{:02}-council{suffix}-{hex6}",
        today.year(),
        u8::from(today.month()),
        today.day()
    )
}

/// Everything [`RealEngine::plan`] and [`RealEngine::convene`] both need,
/// resolved once from a [`Request`] — config through to the council chain's
/// seats. Each call resolves afresh (never shares one `Resolved` across the
/// two), the same "recheck against now, not against then" reasoning
/// `commands::dispatch::RealEngine::dispatch` already documents for itself.
struct Resolved {
    config: Config,
    config_dir: PathBuf,
    project_name: String,
    project_root: PathBuf,
    seats: Vec<selector::Seat>,
    on_exhausted: ConfigFallbackPolicy,
}

/// Delegates every breaker read to the wrapped query and refuses every
/// half-open trial claim — see [`RealEngine::seat`]'s doc for why a
/// council must never actually claim one. `status` is untouched, so the
/// shared gauge-exhaustion fence `commands::dispatch::real_breaker`
/// resolved stays exactly as fenced for council as for an ordinary
/// dispatch; only the write side is cut off.
struct NonClaimingBreaker<'a>(&'a dyn selector::BreakerQuery);

impl selector::BreakerQuery for NonClaimingBreaker<'_> {
    fn status(&self, pool: &str) -> selector::BreakerStatus {
        self.0.status(pool)
    }

    fn claim_trial(&self, _pool: &str, _task_id: &str, _token: Option<&str>) -> Option<String> {
        None
    }
}

/// The real [`Engine`]: reads `config.toml`, resolves the project and its
/// `council` chain, and seats/convenes a roster through `mv_engine::
/// council`. See this file's module doc for the scope this does not reach.
pub struct RealEngine;

impl RealEngine {
    fn resolve(&self, request: &Request) -> Result<Resolved, EngineError> {
        let cwd = std::env::current_dir().map_err(|e| {
            EngineError::Refused(format!("could not read the current directory: {e}"))
        })?;
        let target =
            resolve_user_path(&request.project_path, &cwd).map_err(EngineError::Refused)?;

        let config = load_config()?;
        let config_dir = load::config_state_dir_from_env(&config.source_path);

        let Some((project_name, project_root)) = resolve_project(&config, &target) else {
            return Err(EngineError::NeedsConfig(no_project_message(
                &target, &config,
            )));
        };
        let project_name = project_name.to_string();

        let project = &config.projects[&project_name];
        let chain_names = project.chains.get(&Role::Council).cloned().ok_or_else(|| {
            EngineError::NeedsConfig(format!(
                "no chain configured for role \"Council\" in project {project_name:?}"
            ))
        })?;
        let chain_names = super::dispatch::reorder_chain_by_preference(
            &chain_names,
            &config.accounts,
            &config_dir,
        );
        // Only ask Herdr when something in this chain is actually capped —
        // the same gate `commands::dispatch`'s own `resolve` uses, so an
        // uncapped council never pays for a listing it has no use for.
        let live_counts = chain_names
            .iter()
            .any(|name| mv_engine::seatstate::get_cap(&config_dir, name).is_some())
            .then(|| live_open_counts(&config, &mut runner::ProcessHerdr))
            .flatten();
        let seats: Vec<selector::Seat> = chain_names
            .iter()
            .map(|name| seat_from_account(&config.accounts[name], &config_dir, &live_counts))
            .collect();

        let role_settings = config.roles.get(&Role::Council);
        let on_exhausted = role_settings
            .map(|r| r.on_exhausted)
            .unwrap_or(ConfigFallbackPolicy::Degrade);

        Ok(Resolved {
            config,
            config_dir,
            project_name,
            project_root,
            seats,
            on_exhausted,
        })
    }

    /// Seat a council over `resolved`'s chain — `mv_engine::council::plan`
    /// via a fresh [`council::ChainSelector`] bound to `resolved.seats`.
    ///
    /// The breaker is `commands::dispatch`'s own [`super::dispatch::
    /// real_breaker`] (card `20261105-fixer-9d070c`): a real FENCED/
    /// AVAILABLE decision, shared with an ordinary dispatch rather than
    /// this file keeping its own "any entry on file" stand-in — a council
    /// that seated a rate-limited vendor because its own breaker read was
    /// simpler than dispatch's would be exactly the wrong-direction bug
    /// this sharing rules out. Wrapped in [`NonClaimingBreaker`] before it
    /// reaches the selector (critic finding critic-safety.md#1): Python's
    /// own council seats through `AccountSelector(config)` with no
    /// `trial_task_id`, so `select` never calls `breaker.claim_trial` on
    /// its behalf (`accounts.py:1530-1541`) and its convene-time recheck
    /// instead rejects a still-half-open pool outright
    /// (`council.py:434-436`, "half-open pool requires a trial
    /// dispatch"). Nothing downstream of [`Self::seat`]/[`Self::convene`]
    /// here binds or releases a trial, so letting the real claim through
    /// would leave it claimed — and every other dispatch to that pool
    /// refused — for the full half-open trial timeout.
    fn seat(&self, resolved: &Resolved, question: &str) -> Result<council::Council, EngineError> {
        let real_breaker = super::dispatch::real_breaker(&resolved.config_dir);
        let breaker = NonClaimingBreaker(&real_breaker);
        let floors = selector::QuotaFloors {
            reviewer_min_quota_percent: None,
            fleet_min_quota_percent: Some(resolved.config.dispatch_min_quota_percent as f64),
        };
        let policy = to_selector_policy(resolved.on_exhausted);
        let task_id = task_id_for(&resolved.project_root, question, "");
        let mut chain_selector = council::ChainSelector::new(
            &resolved.seats,
            &resolved.project_name,
            Role::Critic.config_key(),
            Role::Nexus.config_key(),
            floors,
            policy,
            &task_id,
            &breaker,
            Instant::now(),
        );
        // Python `cli.py:8822-8823`: this exact call site's own refusal is
        // the only one of council's three `CouncilTooSmallError` sites
        // wrapped in "cannot seat a council: " — the recheck calls below
        // (seat caps, convene-time availability) have no such prefix of
        // their own.
        let mut seated = council::plan(&mut chain_selector, question.to_string())
            .map_err(|e| EngineError::TooSmallRoster(format!("cannot seat a council: {e}")))?;
        // Card `<this task>` / Python `accounts.py:1546-1549`: each
        // councillor's own `Selection`, not just its chosen `Seat`, is what
        // says whether seating it skipped an earlier chain entry first —
        // `ChainSelector` retains the full selection by account name
        // exactly so this is readable here, after seating, rather than
        // re-derived. `RealDispatchStore::for_seat` reads this to decide
        // `outcome=chain-primary` vs `chain-fallback` on the routing log.
        for councillor in &mut seated.councillors {
            if let Some(selection) = chain_selector.selection(&councillor.seat.account) {
                councillor.is_fallback = selection.is_fallback();
                councillor.reason = super::dispatch::dispatch_reason(
                    selection,
                    Role::Council.display_name(),
                    &resolved.project_name,
                );
            }
        }
        council::recheck_seat_availability(&mut seated, &mut chain_selector)
            .map_err(|e| EngineError::TooSmallRoster(e.to_string()))?;

        // scout2-gaps.md row #30 / Python `council.plan`, which calls
        // `card_from_selection` (`dispatch.py:1677-1684`) once per seated
        // councillor right after `seat()` returns: a project with a
        // backlog item a human answered and nobody closed refuses the
        // whole roster — seat() still succeeded, but no card is ever built
        // for it, so nothing is seated or convened. The check is
        // account-independent (purely a property of `project_root`), so
        // one check here is equivalent to Python's per-seat repeats of the
        // same answer.
        let unclosed = engine_dispatch::unclosed_answered_items(&resolved.project_root);
        if !unclosed.is_empty() {
            return Err(EngineError::NeedsConfig(
                engine_dispatch::answered_items_unclosed_message(&resolved.project_name, &unclosed),
            ));
        }

        Ok(seated)
    }
}

/// `project_slots_grace_counts`'s open-task reading, the same narrowed,
/// local-only scope `commands::dispatch`'s own `open_task_count` uses.
fn open_task_count(project_root: &Path) -> u32 {
    mv_state::tasks::sweep_tasks(project_root)
        .iter()
        .filter(|task| !task.recorded)
        .count() as u32
}

/// Fleet-wide open count plus each *other* project's own floor gap, the
/// same shape `commands::dispatch`'s own `fleet_open_counts` builds.
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

impl Engine for RealEngine {
    fn plan(&self, request: &Request) -> Result<Roster, EngineError> {
        let resolved = self.resolve(request)?;
        let seated = self.seat(&resolved, &request.question)?;
        Ok(Roster {
            seats: roster_seats(&resolved, &seated)?,
        })
    }

    /// Re-resolves and re-seats fresh (never trusts `plan`'s now-possibly-
    /// stale roster — the same reasoning `commands::dispatch::RealEngine::
    /// dispatch` already documents), rebuilds the same roster `plan` would
    /// (so a real run refuses on an unfilled placeholder exactly like its
    /// own dry-run preview would have), then runs `recheck_seat_caps`,
    /// `recheck_seat_availability`, and `capacity_or_raise` under the
    /// same fleet lock as the starts (Python `council.py:650-657`).
    fn convene(&self, request: &Request, _roster: Roster) -> Result<Convened, EngineError> {
        let resolved = self.resolve(request)?;
        let mut seated = self.seat(&resolved, &request.question)?;
        let roster = roster_seats(&resolved, &seated)?;

        let mut stores = Vec::new();
        let mut herdrs = Vec::new();
        for (councillor, roster_seat) in seated
            .councillors
            .iter()
            .filter(|c| !c.skipped)
            .zip(&roster)
        {
            let account = resolved.config.accounts[&councillor.seat.account].clone();
            let reason = councillor.reason.clone();
            stores.push(RealDispatchStore::for_seat(
                resolved.config.clone(),
                resolved.config_dir.clone(),
                resolved.project_name.clone(),
                resolved.project_root.clone(),
                Role::Council,
                account,
                roster_seat.model.clone(),
                roster_seat.task_id.clone(),
                request.question.clone(),
                reason,
                councillor.is_fallback,
            ));
            herdrs.push(runner::ProcessHerdr);
        }

        let current_pane = std::env::var("HERDR_PANE_ID").unwrap_or_default();
        let timing = runner::Timing::default();
        let timeout = Duration::from_secs_f64(request.timeout_seconds.max(0.0));

        let real_breaker = super::dispatch::real_breaker(&resolved.config_dir);
        let breaker = NonClaimingBreaker(&real_breaker);
        let floors = selector::QuotaFloors {
            reviewer_min_quota_percent: None,
            fleet_min_quota_percent: Some(resolved.config.dispatch_min_quota_percent as f64),
        };
        let policy = to_selector_policy(resolved.on_exhausted);
        let task_id = task_id_for(&resolved.project_root, &request.question, "");
        let config = &resolved.config;
        let config_dir = &resolved.config_dir;
        let project_name = &resolved.project_name;
        let project_root = &resolved.project_root;
        let mut chain_selector = council::ChainSelector::new(
            &resolved.seats,
            project_name,
            Role::Critic.config_key(),
            Role::Nexus.config_key(),
            floors,
            policy,
            &task_id,
            &breaker,
            Instant::now(),
        );

        council::convene_with_admission(
            &mut seated,
            stores,
            herdrs,
            &current_pane,
            &timing,
            timeout,
            |council| {
                let live_counts = council
                    .councillors
                    .iter()
                    .filter(|c| !c.skipped)
                    .any(|c| mv_engine::seatstate::get_cap(config_dir, &c.seat.account).is_some())
                    .then(|| live_open_counts(config, &mut runner::ProcessHerdr))
                    .flatten();
                council::recheck_seat_caps(council, config_dir, live_counts.as_ref())
                    .map_err(|e| EngineError::TooSmallRoster(e.to_string()))?;
                council::recheck_seat_availability(council, &mut chain_selector)
                    .map_err(|e| EngineError::TooSmallRoster(e.to_string()))?;
                let (fleet_open, other_floor_gaps) = fleet_open_counts(config, project_name);
                let project = &config.projects[project_name];
                let capacity = engine_dispatch::FleetCapacityInputs {
                    max_concurrent: config.max_concurrent,
                    fleet_open,
                    project_open: open_task_count(project_root),
                    project_min_slots: project.min_slots,
                    project_max_slots: project.max_slots,
                    other_floor_gaps,
                    batch_count: 1,
                };
                council::capacity_or_raise(council, capacity)
                    .map_err(|e| EngineError::Refused(e.to_string()))?;
                Ok(())
            },
        )?;

        let answered = seated.answered().count();
        let vendors = seated
            .answered()
            .map(|c| c.seat.vendor.as_str())
            .collect::<HashSet<_>>()
            .len();
        let roster_by_account: HashMap<&str, &RosterSeat> =
            roster.iter().map(|s| (s.account.as_str(), s)).collect();
        let seat_reports = seated
            .councillors
            .iter()
            .filter(|c| !c.skipped)
            .filter_map(|councillor| {
                roster_by_account
                    .get(councillor.seat.account.as_str())
                    .copied()
                    .map(|roster_seat| (councillor, roster_seat))
            })
            .map(|(councillor, roster_seat)| {
                let outcome = match &councillor.status {
                    Some(status) if councillor.error.is_empty() => {
                        // `RealDispatchStore::prepare` can rebind `task_id`
                        // on an on-disk collision — this path uses the
                        // *requested* id, which is only guaranteed to match
                        // what actually landed when no such collision
                        // occurred (see this task's result.md).
                        let result_path = resolved
                            .project_root
                            .join(".multiverse/tasks")
                            .join(&roster_seat.task_id)
                            .join("result.md");
                        SeatOutcome::Answered {
                            status: status.to_string(),
                            result_path,
                        }
                    }
                    _ => SeatOutcome::Failed(if councillor.error.is_empty() {
                        "MISSING ANSWER".to_string()
                    } else {
                        councillor.error.clone()
                    }),
                };
                SeatReport {
                    account: roster_seat.account.clone(),
                    outcome,
                }
            })
            .collect();
        Ok(Convened {
            answered,
            vendors,
            seats: seat_reports,
        })
    }
}

pub fn run_default(args: &ArgMatches) -> Result<(), Failure> {
    run(args, &RealEngine, &mut std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        plan: Result<Roster, EngineError>,
        convene: Result<Convened, EngineError>,
    }

    impl Engine for Fake {
        fn plan(&self, _request: &Request) -> Result<Roster, EngineError> {
            self.plan.clone()
        }

        fn convene(&self, _request: &Request, _roster: Roster) -> Result<Convened, EngineError> {
            self.convene.clone()
        }
    }

    fn roster_seat(account: &str, vendor: &str) -> RosterSeat {
        RosterSeat {
            account: account.into(),
            vendor: vendor.into(),
            model: format!("{account}-model"),
            task_id: format!("20261105-council-{account}"),
        }
    }

    fn roster() -> Roster {
        Roster {
            seats: vec![
                roster_seat("claude", "claude"),
                roster_seat("codex", "chatgpt"),
                roster_seat("grok", "grok"),
            ],
        }
    }

    fn roster_lines() -> String {
        [
            "  claude (claude) model=claude-model task=20261105-council-claude",
            "  codex (chatgpt) model=codex-model task=20261105-council-codex",
            "  grok (grok) model=grok-model task=20261105-council-grok",
        ]
        .iter()
        .map(|line| format!("{line}\n"))
        .collect()
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = crate::args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("council").unwrap().clone()
    }

    #[test]
    fn request_from_captures_question_and_timeout() {
        let args = matches(&["council", "proj", "what now", "--yes", "--timeout", "60"]);
        let request = request_from(&args);
        assert_eq!(request.project_path, "proj");
        assert_eq!(request.question, "what now");
        assert_eq!(request.timeout_seconds, 60.0);
        assert!(!request.dry_run);
    }

    #[test]
    fn request_from_defaults_the_timeout_to_900() {
        let args = matches(&["council", "proj", "question", "--yes"]);
        assert_eq!(request_from(&args).timeout_seconds, 900.0);
    }

    #[test]
    fn dry_run_prints_the_roster_and_never_convenes() {
        let fake = Fake {
            plan: Ok(roster()),
            convene: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["council", "proj", "question", "--dry-run"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "{}Would ask 3 councillors: claude, codex, grok.\n",
                roster_lines()
            )
        );
    }

    #[test]
    fn a_quorate_council_prints_the_tally_and_synthesizes_nothing() {
        let fake = Fake {
            plan: Ok(roster()),
            convene: Ok(Convened {
                answered: 3,
                vendors: 3,
                seats: vec![
                    SeatReport {
                        account: "claude".into(),
                        outcome: SeatOutcome::Answered {
                            status: "done".into(),
                            result_path: PathBuf::from(
                                "/proj/.multiverse/tasks/t-claude/result.md",
                            ),
                        },
                    },
                    SeatReport {
                        account: "codex".into(),
                        outcome: SeatOutcome::Answered {
                            status: "blocked".into(),
                            result_path: PathBuf::from("/proj/.multiverse/tasks/t-codex/result.md"),
                        },
                    },
                    SeatReport {
                        account: "grok".into(),
                        outcome: SeatOutcome::Failed("timed out".into()),
                    },
                ],
            }),
        };
        let args = matches(&["council", "proj", "question", "--yes"]);
        let mut out = Vec::new();
        run(&args, &fake, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "{}  claude: status done /proj/.multiverse/tasks/t-claude/result.md\n\
                 \x20 codex: status blocked /proj/.multiverse/tasks/t-codex/result.md\n\
                 \x20 FAILED — grok: timed out\n\
                 3 councillors answered across 3 vendors. Nothing was synthesized.\n",
                roster_lines()
            )
        );
    }

    #[test]
    fn below_quorum_refuses_and_exits_one() {
        let fake = Fake {
            plan: Ok(roster()),
            convene: Ok(Convened {
                answered: 1,
                vendors: 1,
                seats: vec![SeatReport {
                    account: "claude".into(),
                    outcome: SeatOutcome::Answered {
                        status: "done".into(),
                        result_path: PathBuf::from("/proj/.multiverse/tasks/t-claude/result.md"),
                    },
                }],
            }),
        };
        let args = matches(&["council", "proj", "question", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(
            error.to_string(),
            "That is not a council — read the results as individual opinions."
        );
        assert_eq!(error.code(), 1);
        // The roster and per-seat lines still print — only the final tally
        // line is replaced by the refusal.
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "{}  claude: status done /proj/.multiverse/tasks/t-claude/result.md\n",
                roster_lines()
            )
        );
    }

    #[test]
    fn a_too_small_roster_needs_configuration_not_a_refusal() {
        let fake = Fake {
            plan: Err(EngineError::TooSmallRoster(
                "only one vendor eligible".into(),
            )),
            convene: Err(EngineError::Refused("must not be called".into())),
        };
        let args = matches(&["council", "proj", "question", "--yes"]);
        let mut out = Vec::new();
        let error = run(&args, &fake, &mut out).unwrap_err();
        assert_eq!(error.code(), 3);
    }

    #[test]
    fn real_engine_refuses_a_project_path_that_does_not_resolve() {
        let args = matches(&["council", "/no/such/path", "question", "--dry-run"]);
        let error = run(&args, &RealEngine, &mut Vec::new()).unwrap_err();
        assert_eq!(error.code(), 1);
        assert!(error.to_string().contains("/no/such/path"), "{error}");
        assert!(!error.to_string().contains("not implemented"));
    }

    /// `RealEngine::seat` directly, bypassing `resolve`'s env-based
    /// `load_config` (which `commands::dispatch`'s own `RealEngine` tests
    /// also never exercise end to end — real config resolution is covered
    /// by `conformance/scenarios/council-refusal-without-yes.json`
    /// instead): two accounts sharing one vendor (`pool = "claude:*"`
    /// derives `vendor = "claude"` for both, `mv_config::load`'s own
    /// default) can seat at most one councillor, below
    /// `council::MINIMUM_COUNCILLORS`.
    #[test]
    fn real_engine_needs_two_distinct_vendors_to_seat() {
        let toml = "\
[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"claude:a\"\n\
[accounts.claude-b]\nkind = \"claude\"\nshort = \"cb\"\ncommand = \"wrap\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: b>>\"\npool = \"claude:b\"\n";
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["claude-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };

        let message = match RealEngine.seat(&resolved, "should we ship?").unwrap_err() {
            EngineError::TooSmallRoster(message) => message,
            other => panic!("expected TooSmallRoster, got {other:?}"),
        };
        assert!(message.contains("at least 2 distinct vendors"), "{message}");
    }

    /// Card `20261105-fixer-9d070c`: before this task, `seat`'s own
    /// `RealBreaker` only ever read `breaker-state.json` — a pool with *no*
    /// breaker entry at all read as `CLOSED` (available) no matter what its
    /// usage gauge said, because `mv_engine::breaker::trip_from_gauge`'s
    /// "a fresh exhausted gauge reading can open a pool with no prior trip"
    /// step only runs inside the shared `mv_engine::breaker::StoreBreakerQuery`
    /// this task wires `seat` up to. Two distinct-vendor accounts, one
    /// gauge-exhausted and never breaker-tripped: the old reading would
    /// have happily seated it, handing council work to a rate-limited seat;
    /// the shared breaker must fence it on the gauge alone and shrink below
    /// `council::MINIMUM_COUNCILLORS`.
    #[test]
    fn seat_fences_a_gauge_exhausted_pool_with_no_breaker_entry_at_all() {
        let toml = "\
[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"claude:a\"\n\
[accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: b>>\"\npool = \"codex:b\"\n";
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();

        let mut gauges = mv_state::gauge::GaugeMap::new();
        gauges.insert(
            "claude:a".to_string(),
            mv_state::gauge::GaugeEntry {
                percent_left: Some(0.0),
                observed_at: Some("2026-01-01T00:00:00Z".to_string()),
                ..Default::default()
            },
        );
        mv_state::gauge::save(config_dir.path(), &gauges).unwrap();

        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };

        let message = match RealEngine.seat(&resolved, "should we ship?").unwrap_err() {
            EngineError::TooSmallRoster(message) => message,
            other => panic!("expected TooSmallRoster, got {other:?}"),
        };
        assert!(message.contains("at least 2 distinct vendors"), "{message}");
    }

    /// Critic finding critic-safety.md#1: before this task, `seat`'s shared
    /// `real_breaker` would `claim_trial` for real on a half-open pool,
    /// mint `trial_task_id`/`trial_token` into `breaker-state.json`, and
    /// then never release it — council has no `--into` dispatch to bind or
    /// release the claim through. Two distinct-vendor accounts, one with a
    /// manually-tripped, now-half-open pool: the seat must be skipped (too
    /// small, since only one vendor is left) and `breaker-state.json` must
    /// come back with no trial claimed at all, matching Python's own
    /// council — which never passes a `trial_task_id` to its selector in
    /// the first place (`council.py` constructs `AccountSelector(config)`
    /// with none) — rather than racing a real dispatch for the slot.
    #[test]
    fn seat_never_claims_a_half_open_pools_trial() {
        let toml = "\
[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"claude:a\"\n\
[accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\nconfig_dir_var = \"CODEX_HOME\"\nconfig_dir = \"<<PLACEHOLDER: b>>\"\npool = \"codex:b\"\n";
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();

        let now = time::OffsetDateTime::now_utc();
        mv_engine::breaker::trip(
            config_dir.path(),
            "claude:a",
            now,
            mv_engine::breaker::TripRequest {
                vendor: "claude",
                kind: mv_engine::breaker::BreakerKind::Rolling,
                rule: "manual fence",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: true,
            },
        )
        .unwrap();
        // Force the manual fence's reset into the past so the pool reads
        // half-open under the real clock `seat` uses.
        mv_state::breaker_store::with_locked(config_dir.path(), |map| {
            map.get_mut("claude:a").unwrap().reset_at = Some(mv_core::time::format_seconds_utc(
                now - time::Duration::minutes(1),
            ));
        })
        .unwrap();

        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };

        let message = match RealEngine.seat(&resolved, "should we ship?").unwrap_err() {
            EngineError::TooSmallRoster(message) => message,
            other => panic!("expected TooSmallRoster, got {other:?}"),
        };
        assert!(message.contains("at least 2 distinct vendors"), "{message}");

        let map = mv_state::breaker_store::load(config_dir.path());
        let entry = map.get("claude:a").expect("entry still on file");
        assert!(
            entry.trial_task_id.is_none() && entry.trial_token.is_none(),
            "council must never claim a half-open trial: {entry:?}"
        );
    }

    /// Scout2 gap #24: before this fix, `seat_from_account` hard-coded
    /// `open_count: None` for every seat, which `selector::availability`
    /// reads as at-capacity for any *capped* seat — so a capped seat with
    /// real room was always skipped. Live counts are keyed by account
    /// **name** (Python `council.py:508`), so a map entry for `claude-a`
    /// must produce `open_count: Some(1)`, not `None`.
    #[test]
    fn seat_from_account_reads_a_capped_seats_live_open_count_instead_of_hard_coding_none() {
        let dir = tempfile::tempdir().unwrap();
        let toml = "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
            config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"<<PLACEHOLDER: a>>\"\npool = \"claude:a\"\n";
        let config = load::load_config_str(toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            config_dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "claude-a".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(2),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();

        let mut live_counts = HashMap::new();
        live_counts.insert("claude-a".to_string(), 1u32);
        let seat = seat_from_account(
            &config.accounts["claude-a"],
            config_dir.path(),
            &Some(live_counts),
        );
        assert_eq!(seat.cap, Some(2));
        assert_eq!(
            seat.open_count,
            Some(1),
            "a capped seat must read its live open count, not a hard-coded None"
        );

        // An uncapped seat never consults the live counts at all — matching
        // `commands::dispatch`'s own `seat_from_account`.
        let uncapped = seat_from_account(&config.accounts["claude-a"], dir.path(), &None);
        assert_eq!(uncapped.cap, None);
        assert_eq!(uncapped.open_count, None);
    }

    /// Critic-grok3 row 2: `live_open_counts` follows Python's
    /// `open_agent_counts` (`accounts.py:623, 686-708`) and
    /// `recheck_seat_caps` (`council.py:508`). Only `working`/`blocked`
    /// count as open; the account comes from the dispatch record, never
    /// the last `-` segment of the agent name; keys are account names.
    #[test]
    fn live_open_counts_attributes_by_dispatch_record_not_name_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-council-aaaaaa\tproject=p\trole=council\taccount=claude-a\tpool=p\toutcome=chain-primary\treason=\tagent=proj-coun-ca-2\tbranch=main\n\
             2020-01-01T00:00:01+00:00\tevent=dispatch\ttask=20250101-council-bbbbbb\tproject=p\trole=council\taccount=claude-a\tpool=p\toutcome=chain-primary\treason=\tagent=proj-coun-ca\tbranch=main\n\
             2020-01-01T00:00:02+00:00\tevent=dispatch\ttask=20250101-council-cccccc\tproject=p\trole=council\taccount=codex-b\tpool=p\toutcome=chain-primary\treason=\tagent=proj-coun-cb\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n\
             [accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"{}/ca\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/cb\"\npool = \"codex:b\"\n",
            project.display(),
            dir.path().display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();

        struct FakeList;
        impl runner::Herdr for FakeList {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Ok(mv_engine::mv_herdr::envelope::CommandResult {
                    argv: vec!["herdr".into()],
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(serde_json::json!({
                        "result": {
                            "agents": [
                                // Collision suffix: last segment is "2", not "ca".
                                {"name": "proj-coun-ca-2", "agent_status": "working"},
                                // Same account, unknown — must not count.
                                {"name": "proj-coun-ca", "agent_status": "unknown"},
                                // Name ends in a real short, but this lane never dispatched it.
                                {"name": "a-human-pane-ca", "agent_status": "working"},
                                {"name": "proj-coun-cb", "agent_status": "blocked"},
                                {"name": "proj-coun-cb-idle", "agent_status": "idle"},
                            ]
                        }
                    })),
                    protocol_error: None,
                })
            }
        }

        let counts =
            live_open_counts(&config, &mut FakeList).expect("a successful reply must be Some");
        assert_eq!(
            counts.get("claude-a").copied(),
            Some(1),
            "collision-suffix working pane counts against the dispatch record's account name"
        );
        assert_eq!(
            counts.get("codex-b").copied(),
            Some(1),
            "blocked pane counts against its dispatch record's account name"
        );
        assert_eq!(counts.get("ca"), None, "must not key by short code");
        assert_eq!(
            counts.get("2"),
            None,
            "must not bucket by the last name segment of a collision-suffix agent"
        );
        assert_eq!(counts.get("cb"), None, "must not key by short code");
    }

    /// Python `open_agent_counts` (`accounts.py:798-804`): when the
    /// dispatch record's `account=` is no longer a config key (the seat
    /// was renamed), recover it from the agent name's trailing short via
    /// `account_by_short`. Council keys by account **name**, so a live
    /// pane recorded under `old-seat` must still count against `new-seat`.
    #[test]
    fn live_open_counts_recovers_a_renamed_account_from_the_agent_name_short() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-council-aaaaaa\tproject=p\trole=builder\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=proj-buil-ca\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n\
             [accounts.new-seat]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"{}/ca\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/cb\"\npool = \"codex:b\"\n\
             [accounts.grok-c]\nkind = \"grok\"\nshort = \"gc\"\ncommand = \"wrap\"\n\
             config_dir_var = \"GROK_HOME\"\nconfig_dir = \"{}/gc\"\npool = \"grok:c\"\n",
            project.display(),
            dir.path().display(),
            dir.path().display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();

        struct FakeList;
        impl runner::Herdr for FakeList {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Ok(mv_engine::mv_herdr::envelope::CommandResult {
                    argv: vec!["herdr".into()],
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(serde_json::json!({
                        "result": {
                            "agents": [
                                {"name": "proj-buil-ca", "agent_status": "working"},
                            ]
                        }
                    })),
                    protocol_error: None,
                })
            }
        }

        let counts =
            live_open_counts(&config, &mut FakeList).expect("a successful reply must be Some");
        assert_eq!(
            counts.get("new-seat").copied(),
            Some(1),
            "a task recorded under a since-renamed account name must still count via the agent-name short"
        );
        assert_eq!(
            counts.get("old-seat"),
            None,
            "the historical account name is gone from config and is not a count key"
        );
        assert_eq!(counts.get("ca"), None, "must not key by short code");

        let config_dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            config_dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "new-seat".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(1),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();
        let mut seated = council::Council {
            question: "q".into(),
            councillors: vec![
                council::Councillor {
                    seat: council::Seat {
                        vendor: "claude".into(),
                        account: "new-seat".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
                council::Councillor {
                    seat: council::Seat {
                        vendor: "chatgpt".into(),
                        account: "codex-b".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
                council::Councillor {
                    seat: council::Seat {
                        vendor: "grok".into(),
                        account: "grok-c".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
            ],
        };
        council::recheck_seat_caps(&mut seated, config_dir.path(), Some(&counts)).unwrap();
        assert!(
            seated.councillors[0].skipped,
            "cap-1 new-seat with a live renamed-account pane must be skipped"
        );
        assert!(!seated.councillors[1].skipped);
        assert!(!seated.councillors[2].skipped);

        // Instance suffix (`-2`) is stripped before the short is read.
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-council-aaaaaa\tproject=p\trole=builder\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=proj-buil-ca-2\tbranch=main\n",
        )
        .unwrap();
        struct FakeSuffixed;
        impl runner::Herdr for FakeSuffixed {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Ok(mv_engine::mv_herdr::envelope::CommandResult {
                    argv: vec!["herdr".into()],
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(serde_json::json!({
                        "result": {
                            "agents": [
                                {"name": "proj-buil-ca-2", "agent_status": "working"},
                            ]
                        }
                    })),
                    protocol_error: None,
                })
            }
        }
        let counts = live_open_counts(&config, &mut FakeSuffixed).expect("listing ok");
        assert_eq!(
            counts.get("new-seat").copied(),
            Some(1),
            "the instance suffix is not the short; strip it first"
        );

        // A trailing token that is not a configured short still contributes
        // nothing — the fallback is a lookup, not a guess.
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-council-aaaaaa\tproject=p\trole=builder\taccount=old-seat\tpool=p\toutcome=chain-primary\treason=\tagent=proj-buil-zz\tbranch=main\n",
        )
        .unwrap();
        struct FakeUnknown;
        impl runner::Herdr for FakeUnknown {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Ok(mv_engine::mv_herdr::envelope::CommandResult {
                    argv: vec!["herdr".into()],
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(serde_json::json!({
                        "result": {
                            "agents": [
                                {"name": "proj-buil-zz", "agent_status": "working"},
                            ]
                        }
                    })),
                    protocol_error: None,
                })
            }
        }
        let counts = live_open_counts(&config, &mut FakeUnknown).expect("listing ok");
        assert_eq!(
            counts.get("new-seat"),
            None,
            "an unknown name-suffix must not be attributed to a live seat"
        );
    }

    #[test]
    fn live_open_counts_is_none_when_herdr_cannot_be_asked() {
        let dir = tempfile::tempdir().unwrap();
        let config = load::load_config_str("", &dir.path().join("config.toml")).unwrap();
        struct FakeFails;
        impl runner::Herdr for FakeFails {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Err(mv_engine::runner::RunnerError::Refused("down".into()))
            }
        }
        assert!(live_open_counts(&config, &mut FakeFails).is_none());
    }

    /// Critic-grok3 row 5: `recheck_seat_caps` looks up account **name**.
    /// Wiring it to CLI `live_open_counts` (now also account-name keyed)
    /// must skip a cap-1 seat whose collision-suffix pane is live.
    #[test]
    fn recheck_seat_caps_consumes_live_open_counts_account_name_keys() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        std::fs::create_dir_all(project.join(".multiverse/tasks")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-council-aaaaaa\tproject=p\trole=council\taccount=claude-a\tpool=p\toutcome=chain-primary\treason=\tagent=proj-coun-ca-2\tbranch=main\n",
        )
        .unwrap();
        let toml = format!(
            "[projects.p]\npath = \"{}\"\n\
             [accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\nconfig_dir = \"{}/ca\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/cb\"\npool = \"codex:b\"\n\
             [accounts.grok-c]\nkind = \"grok\"\nshort = \"gc\"\ncommand = \"wrap\"\n\
             config_dir_var = \"GROK_HOME\"\nconfig_dir = \"{}/gc\"\npool = \"grok:c\"\n",
            project.display(),
            dir.path().display(),
            dir.path().display(),
            dir.path().display(),
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        mv_state::seatstate_store::save(
            config_dir.path(),
            &mv_state::seatstate_store::SeatMap::from([(
                "claude-a".to_string(),
                mv_state::seatstate_store::SeatEntry {
                    cap: Some(1),
                    ..Default::default()
                },
            )]),
        )
        .unwrap();

        struct FakeList;
        impl runner::Herdr for FakeList {
            fn call(
                &mut self,
                _command: &mv_engine::mv_herdr::argv::HerdrCommand,
                _timeout: Duration,
            ) -> Result<mv_engine::mv_herdr::envelope::CommandResult, mv_engine::runner::RunnerError>
            {
                Ok(mv_engine::mv_herdr::envelope::CommandResult {
                    argv: vec!["herdr".into()],
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(serde_json::json!({
                        "result": {
                            "agents": [
                                {"name": "proj-coun-ca-2", "agent_status": "working"},
                            ]
                        }
                    })),
                    protocol_error: None,
                })
            }
        }

        let counts = live_open_counts(&config, &mut FakeList).expect("listing ok");
        let mut seated = council::Council {
            question: "q".into(),
            councillors: vec![
                council::Councillor {
                    seat: council::Seat {
                        vendor: "claude".into(),
                        account: "claude-a".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
                council::Councillor {
                    seat: council::Seat {
                        vendor: "chatgpt".into(),
                        account: "codex-b".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
                council::Councillor {
                    seat: council::Seat {
                        vendor: "grok".into(),
                        account: "grok-c".into(),
                    },
                    status: None,
                    error: String::new(),
                    skipped: false,
                    unanswered: false,
                    is_fallback: false,
                    reason: String::new(),
                },
            ],
        };
        council::recheck_seat_caps(&mut seated, config_dir.path(), Some(&counts)).unwrap();
        assert!(
            seated.councillors[0].skipped,
            "cap-1 claude-a with a live collision-suffix pane must be skipped"
        );
        assert!(!seated.councillors[1].skipped);
        assert!(!seated.councillors[2].skipped);
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
            model_for_account(&config.accounts["seat"], None).unwrap(),
            "from-account"
        );
    }

    #[test]
    fn model_for_account_without_either_source_is_needs_config() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.seat]\nkind = \"codex\"\nshort = \"se\"\ncommand = \"wrap\"\n\
             config_dir_var = \"CODEX_HOME\"\nconfig_dir = \"{}/codex\"\npool = \"codex:test\"\n",
            dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let error = model_for_account(&config.accounts["seat"], None).unwrap_err();
        match &error {
            EngineError::NeedsConfig(message) => {
                assert!(message.contains("no model for kind"), "{message}");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(to_failure(error).code(), 3);
    }

    /// Scout2-gaps.md #50: "Unfilled placeholders → exit 3, nothing
    /// convened." A seat whose `config_dir` is still a `<<PLACEHOLDER:
    /// ...>>` must refuse the whole roster — before `plan`'s dry-run shows
    /// it, and before `convene` would start anything.
    #[test]
    fn roster_seats_refuses_an_unfilled_placeholder_before_anything_convenes() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"<<PLACEHOLDER: a>>\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{}/codex\"\npool = \"codex:b\"\n",
            dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };
        let seated = RealEngine.seat(&resolved, "should we ship?").unwrap();

        let error = roster_seats(&resolved, &seated).unwrap_err();
        match error {
            EngineError::NeedsConfig(message) => {
                assert!(
                    message.contains("accounts.claude-a.config_dir"),
                    "{message}"
                );
            }
            other => panic!("expected NeedsConfig, got {other:?}"),
        }
    }

    #[test]
    fn roster_seats_is_fine_when_every_value_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"{dir}/claude\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{dir}/codex\"\npool = \"codex:b\"\n",
            dir = dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };
        let seated = RealEngine.seat(&resolved, "should we ship?").unwrap();

        let seats = roster_seats(&resolved, &seated).unwrap();
        assert_eq!(seats.len(), 2);
        assert!(seats.iter().all(|s| !s.task_id.is_empty()));
    }

    /// Python `dispatch.task_id_for` (`dispatch.py:410-450`) with
    /// `seat_in_id=True` mints `{date}-council-{seat}-{digest}` and folds
    /// the seat into the hash. Rust used to append the short *after* a
    /// shared digest (`{date}-council-{digest}-{seat}`), so two
    /// councillors shared a hash and history readers looking for a
    /// trailing 6-hex suffix would miss the id.
    #[test]
    fn roster_seats_mints_python_shaped_per_seat_task_ids() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"{dir}/claude\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{dir}/codex\"\npool = \"codex:b\"\n",
            dir = dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };
        let seated = RealEngine.seat(&resolved, "should we ship?").unwrap();
        let seats = roster_seats(&resolved, &seated).unwrap();
        assert_eq!(seats.len(), 2);

        let mut digests = HashSet::new();
        for seat in &seats {
            let short = resolved.config.accounts[&seat.account].short.as_str();
            let expected_digest =
                python_council_digest(&resolved.project_root, "should we ship?", short);
            let parts: Vec<&str> = seat.task_id.split('-').collect();
            assert_eq!(
                parts.as_slice(),
                [parts[0], "council", short, expected_digest.as_str()],
                "expected Python {{date}}-council-{{seat}}-{{digest}}, got {:?}",
                seat.task_id
            );
            assert_eq!(parts[0].len(), 8);
            assert!(parts[0].chars().all(|c| c.is_ascii_digit()));
            assert!(
                digests.insert(expected_digest),
                "each seat must hash its own short, got {:?}",
                seat.task_id
            );
        }
    }

    /// Python `dispatch.py:444-447`: SHA-256 of
    /// `path + NUL + "council" + NUL + question + NUL + seat`, first six
    /// hex chars. Independent of `task_id_for` so a format-only swap
    /// still fails when the digest omits the seat.
    fn python_council_digest(project_root: &Path, question: &str, seat: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(project_root.as_os_str().as_encoded_bytes());
        hasher.update([0u8]);
        hasher.update(b"council");
        hasher.update([0u8]);
        hasher.update(question.as_bytes());
        hasher.update([0u8]);
        hasher.update(seat.as_bytes());
        let digest = hasher.finalize();
        digest[..3]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    }

    /// scout2-gaps.md row #30's accompanying council divergence: Python's
    /// `Selection.outcome` is `chain-fallback` for every seat after the
    /// first, because each later seat's own chain walk restarts from the
    /// top with every already-seated vendor excluded, and that vendor's own
    /// (now-excluded) chain entry is itself a skip before the next usable
    /// one is reached. `RealDispatchStore::for_seat` used to hardcode
    /// `outcome=chain-primary` for every seat; this checks the roster's own
    /// `is_fallback` flag instead, which is what `convene` now reads.
    #[test]
    fn only_the_first_seated_councillor_is_chain_primary() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"{dir}/claude\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{dir}/codex\"\npool = \"codex:b\"\n\
             [accounts.grok-c]\nkind = \"grok\"\nshort = \"gc\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"GROK_HOME\"\n\
             config_dir = \"{dir}/grok\"\npool = \"grok:c\"\n",
            dir = dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
                seat_from_account(&config.accounts["grok-c"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };

        let seated = RealEngine.seat(&resolved, "should we ship?").unwrap();
        assert_eq!(seated.councillors.len(), 3);
        assert!(
            !seated.councillors[0].is_fallback,
            "{:?}",
            seated.councillors[0]
        );
        assert!(
            seated.councillors[1].is_fallback,
            "{:?}",
            seated.councillors[1]
        );
        assert!(
            seated.councillors[2].is_fallback,
            "{:?}",
            seated.councillors[2]
        );

        // The routing-log `reason=` text must match Python's own chain-walk
        // wording exactly, single-quoted skip reasons included — not the
        // placeholder "council seat for vendor X" this used to hardcode.
        assert_eq!(
            seated.councillors[0].reason,
            "first entry in Council chain for p"
        );
        assert_eq!(
            seated.councillors[1].reason,
            "entry 2 in Council chain for p; skipped claude-a (vendor 'claude' excluded)"
        );
        assert_eq!(
            seated.councillors[2].reason,
            "entry 3 in Council chain for p; skipped claude-a (vendor 'claude' excluded); codex-b (vendor 'codex' excluded)"
        );
    }

    /// scout2-gaps.md row #30 / Python `council.plan`, which calls
    /// `card_from_selection` once per seated councillor right after
    /// `seat()` returns: an answered-but-unclosed backlog item refuses the
    /// whole roster even though seating two distinct vendors would
    /// otherwise succeed — no partial council, same "nothing starts"
    /// posture `roster_seats_refuses_an_unfilled_placeholder...` already
    /// covers for an unfilled placeholder.
    #[test]
    fn an_answered_unclosed_backlog_item_refuses_the_whole_seated_roster() {
        let dir = tempfile::tempdir().unwrap();
        let toml = format!(
            "[accounts.claude-a]\nkind = \"claude\"\nshort = \"ca\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"{dir}/claude\"\npool = \"claude:a\"\n\
             [accounts.codex-b]\nkind = \"codex\"\nshort = \"cb\"\ncommand = \"wrap\"\n\
             model = \"m\"\nconfig_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{dir}/codex\"\npool = \"codex:b\"\n",
            dir = dir.path().display()
        );
        let config = load::load_config_str(&toml, &dir.path().join("config.toml")).unwrap();
        let config_dir = tempfile::tempdir().unwrap();
        let resolved = Resolved {
            seats: vec![
                seat_from_account(&config.accounts["claude-a"], config_dir.path(), &None),
                seat_from_account(&config.accounts["codex-b"], config_dir.path(), &None),
            ],
            config,
            config_dir: config_dir.path().to_path_buf(),
            project_name: "p".into(),
            project_root: dir.path().to_path_buf(),
            on_exhausted: ConfigFallbackPolicy::Degrade,
        };

        let item = mv_state::backlog::ask(
            &resolved.project_root,
            "ship it?",
            &["yes".to_string(), "no".to_string()],
            1,
            "p",
            "",
            None,
            None,
            "P2",
            None,
        )
        .unwrap();
        mv_state::backlog::append_annotation_if_absent(
            &resolved.project_root,
            &item.item,
            "human",
            "go ahead",
            None,
        )
        .unwrap();

        let error = RealEngine.seat(&resolved, "should we ship?").unwrap_err();
        match error {
            EngineError::NeedsConfig(message) => {
                assert!(message.contains(&item.item), "{message}");
                assert!(message.contains("answered and never closed"), "{message}");
            }
            other => panic!("expected NeedsConfig, got {other:?}"),
        }

        mv_state::backlog::done(&resolved.project_root, &item.item, "closed in review", None)
            .unwrap();
        assert_eq!(
            RealEngine
                .seat(&resolved, "should we ship?")
                .unwrap()
                .councillors
                .len(),
            2
        );
    }
}

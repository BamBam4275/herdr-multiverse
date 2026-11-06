//! `multiverse start [projects...] [--all] [--reseed] [--dashboard]`: open
//! a Nexus pane per selected project, reusing one already open, with an
//! interactive seat.
//!
//! Ported from Python's `cli.cmd_start`. The Herdr-facing half (workspace
//! create/close, launching the seat, naming it, and landing its seed
//! prompt) lives in `mv_engine::fleet_start`, which this file calls
//! through `mv_engine::runner::{Herdr, ProcessHerdr}` — the same seam
//! `mv-engine`'s own `runner`/`council` modules use, and the reason this
//! crate can now reach Herdr at all without depending on `mv-herdr`
//! directly (architecture.md routes Herdr access through `mv-engine`).
//!
//! `--reseed` on a project whose workspace is already open re-points the
//! live session when the seed is stale, and skips a current seed. Card
//! `20261102-b-a01bbc`'s `FleetState::orchestrator_agent` finds the named
//! agent's pane by identity, a blocked/dialog state is reported rather
//! than typed into, and a clear one is re-seeded through
//! `mv_engine::runner::deliver_prompt_to`'s own retry-with-probe landing
//! confirmation (card `20261102-b-38b78a`).
//!
//! Claude interactive seats use `mv_engine::trust::claude_orchestrator_launch`
//! for their pane script; summaries retain the bare configured command.
//!
//! The project-level `_project_lock` around check-then-create (with a
//! fresh `workspace list`/`pane list` taken under that lock) and the
//! `--dashboard` tab (`cli.cmd_start` / `_ensure_dashboard_tab`) are
//! ported. Deduping `wanted` still guards a single process naming the
//! same project twice in one invocation (task `20261103-fixer-5a1736`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ArgMatches;
use mv_config::load::{
    Account, Config, DashboardTab, FallbackPolicy as ConfigFallbackPolicy, Project, ReviewPolicy,
};
use mv_config::roles::Role;
use mv_engine::fleet_start::{
    find_orchestrator_agent, nexus_agent_name, nexus_legacy_agent_name, FleetState,
};
use mv_engine::mv_herdr::argv;
use mv_engine::nexus_seed::{self, DispatchableRole, SeedContext, SeedRole};
use mv_engine::runner::{Herdr, ProcessHerdr, Timing};
use mv_engine::selector;
use mv_engine::selector::LocalLivenessProbe as _;
use sha2::{Digest, Sha256};

use super::doctor::{project_path, resolve_config_path, state_dir_from_config};
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::{Failure, ERROR, NEEDS_CONFIG};

/// Print a config-load failure for `run`'s own `println`-then-`Reported`
/// pattern (shared with `add`/`check`/`new`/`stop`, but this file owns
/// only this call site — card `20261102-b-4a5e3c`). On the one refusal
/// that names a retired, unmigrated role key, name the fix instead of
/// the generic "run doctor" pointer: an operator on a Python-era config
/// needs `multiverse config migrate`, not a readiness check that cannot
/// get past the same refusal.
fn print_config_load_error(message: &str) {
    println!("config error: {message}");
    if is_retired_role_key_error(message) {
        println!("{MIGRATE_FIX_LINE}");
    } else {
        println!("Run `multiverse doctor` for a full readiness check.");
    }
}

/// Same fix, for [`start_one_by_path`]'s `Result<_, String>` boundary:
/// its caller (`new.rs`) prints the string itself, so the line has to
/// live inside the message rather than beside a `println!`.
fn config_load_error_message(message: String) -> String {
    if is_retired_role_key_error(&message) {
        format!("{message}\n{MIGRATE_FIX_LINE}")
    } else {
        message
    }
}

const HERDR_TIMEOUT: Duration = Duration::from_secs(30);

/// `accounts._routing_log_lock_path`: the same `routing.log.lock` every
/// other project-level transaction holds (`accounts._project_lock`).
fn project_lock_path(project_root: &Path) -> PathBuf {
    project_root.join(".multiverse").join("routing.log.lock")
}

/// `config.dashboard_for` (`config.py:1623-1627`): `--dashboard` is the
/// operator override; otherwise `[board].dashboard_tab` (bool, or the
/// default list `["multiverse"]`) decides per project.
fn dashboard_for(flag: bool, config: &Config, project_name: &str) -> bool {
    if flag {
        return true;
    }
    match &config.board.dashboard_tab {
        DashboardTab::Enabled(enabled) => *enabled,
        DashboardTab::Projects(names) => names.iter().any(|name| name == project_name),
    }
}

fn posix_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'/' | b'-' | b'_' | b':'))
    {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn with_extra_lines(mut message: String, lines: Vec<String>) -> String {
    for line in lines {
        message.push('\n');
        message.push_str(&line);
    }
    message
}

/// `cli._ensure_dashboard_tab` (`cli.py:5331-5377`): create-if-absent one
/// unfocused `dashboard` tab, then `pane run` the board watcher. The
/// per-workspace lock covers list/create only, matching Python — the
/// watcher launch runs after it is released.
fn ensure_dashboard_tab(
    herdr: &mut impl Herdr,
    config_path: &Path,
    project: &Project,
    path: &Path,
    workspace_id: &str,
) -> Vec<String> {
    let created = {
        let mut hasher = Sha256::new();
        hasher.update(workspace_id.as_bytes());
        let key: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let lock_path = path
            .join(".multiverse")
            .join(format!("dashboard-{key}.lock"));
        let _lock = match mv_core::lock::acquire_default(&lock_path, "dashboard") {
            Ok(guard) => guard,
            Err(error) => {
                return vec![format!("      dashboard tab unavailable: {error}")];
            }
        };
        let listed = match herdr.call(&argv::tab_list(Some(workspace_id)), HERDR_TIMEOUT) {
            Ok(result) if result.ok() => result,
            Ok(result) => {
                return vec![format!(
                    "      dashboard tab unavailable: {}",
                    result.message()
                )];
            }
            Err(error) => {
                return vec![format!("      dashboard tab unavailable: {error}")];
            }
        };
        let existing = listed
            .payload
            .as_ref()
            .and_then(|payload| payload.get("result"))
            .and_then(|result| result.get("tabs"))
            .and_then(|tabs| tabs.as_array())
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let Some(create) = argv::ensure_dashboard_tab(existing, workspace_id, path, None) else {
            return vec!["      dashboard tab already open".to_string()];
        };
        match herdr.call(&create, HERDR_TIMEOUT) {
            Ok(result) => result,
            Err(error) => {
                return vec![format!("      dashboard tab FAILED — {error}")];
            }
        }
    };
    if !created.ok() {
        return vec![format!(
            "      dashboard tab FAILED — {}",
            created.message()
        )];
    }
    let Some(pane) = created
        .payload
        .as_ref()
        .and_then(|payload| payload.get("result"))
        .and_then(|result| result.get("root_pane"))
        .and_then(|pane| pane.get("pane_id"))
        .and_then(|id| id.as_str())
        .map(str::to_owned)
    else {
        return vec!["      dashboard tab created, but its pane was not returned".to_string()];
    };
    let target = if project.name == "multiverse" {
        "--all".to_string()
    } else {
        posix_quote(&project.name)
    };
    let script = format!(
        "multiverse --config {} board {target} --watch",
        posix_quote(&config_path.display().to_string())
    );
    match herdr.call(&argv::pane_run(&pane, &script), HERDR_TIMEOUT) {
        Ok(result) if result.ok() => vec!["      dashboard tab running".to_string()],
        Ok(result) => vec![format!(
            "      dashboard tab created, but watch failed: {}",
            result.message()
        )],
        Err(error) => vec![format!(
            "      dashboard tab created, but watch failed: {error}"
        )],
    }
}

#[allow(clippy::too_many_arguments)]
fn maybe_dashboard(
    herdr: &mut impl Herdr,
    config: &Config,
    config_path: &Path,
    project: &Project,
    path: &Path,
    workspace_id: Option<&str>,
    dashboard: bool,
    message: String,
) -> String {
    let Some(workspace_id) = workspace_id.filter(|id| !id.is_empty()) else {
        return message;
    };
    if !dashboard_for(dashboard, config, &project.name) {
        return message;
    }
    with_extra_lines(
        message,
        ensure_dashboard_tab(herdr, config_path, project, path, workspace_id),
    )
}

/// Whether every path in `paths` resolves under a system temp directory.
/// Ported from Python's `herdr.projects_are_temporary`.
fn projects_are_temporary<'a>(paths: impl Iterator<Item = &'a Path>) -> bool {
    let resolved: Vec<_> = paths
        .filter_map(|path| std::fs::canonicalize(path).ok())
        .collect();
    if resolved.is_empty() {
        return false;
    }
    let roots: Vec<_> = [
        std::env::temp_dir(),
        Path::new("/tmp").to_path_buf(),
        Path::new("/private/tmp").to_path_buf(),
        Path::new("/var/folders").to_path_buf(),
        Path::new("/private/var/folders").to_path_buf(),
    ]
    .into_iter()
    .filter_map(|path| std::fs::canonicalize(&path).ok().or(Some(path)))
    .collect();
    resolved.iter().all(|path| {
        roots
            .iter()
            .any(|root| path == root || path.starts_with(root))
    })
}

/// Refuse to touch Herdr at all when every configured project is under a
/// temp directory, unless the operator opted in — Python's
/// `herdr.temporary_projects_refusal`.
fn temporary_projects_refusal<'a>(paths: impl Iterator<Item = &'a Path>) -> Option<String> {
    temporary_projects_refusal_with(
        paths,
        std::env::var("MULTIVERSE_ALLOW_TEMP_PROJECTS").as_deref() == Ok("1"),
    )
}

fn temporary_projects_refusal_with<'a>(
    paths: impl Iterator<Item = &'a Path>,
    allow_temp: bool,
) -> Option<String> {
    if allow_temp {
        return None;
    }
    if !projects_are_temporary(paths) {
        return None;
    }
    Some(
        "refusing to create workspaces or start agents: every configured project path is \
         under a temporary directory. Set MULTIVERSE_ALLOW_TEMP_PROJECTS=1 only for an \
         intentional test run."
            .to_string(),
    )
}

/// Seat short and project short for [`nexus_agent_name`]: Python's
/// `agent_name(..., seat=account.short, short=project.short)`. An
/// absent interactive seat is the empty string, matching a call with
/// `seat=""`.
fn naming_parts<'a>(project: &'a Project, seat: Option<&'a Account>) -> (&'a str, &'a str) {
    (
        seat.map(|account| account.short.as_str()).unwrap_or(""),
        project.short.as_deref().unwrap_or(""),
    )
}

/// The Nexus pane's sidebar title — Python's `orchestrator.pane_label`
/// (`orchestrator.py:91-117`), the same `agent · command · model · seat ·
/// role task_id` builder [`mv_engine::runner::pane_label`] uses for a
/// dispatched lane, with the project name standing in for a lane's task id
/// (a Nexus is not a Card). Best-effort, like Python's own: an unresolved
/// model reads as empty rather than refusing — this only decorates a title,
/// never gates the launch.
fn orchestrator_pane_label(
    config: &Config,
    project: &Project,
    account: &Account,
    agent_name: &str,
    launch_command: &str,
) -> String {
    let model = account.model.clone().unwrap_or_else(|| {
        config
            .roles
            .get(&Role::Nexus)
            .and_then(|settings| settings.models.get(&account.kind).cloned())
            .unwrap_or_default()
    });
    format!(
        "{agent_name} · {launch_command} · {model} · {} · {} {}",
        account.name,
        Role::Nexus.config_key(),
        project.name
    )
}

/// Python `DISPATCHABLE_ROLES` (`roles.py:170-179`): every role except
/// Nexus (the seed's own reader) and Council (its own subcommand).
fn seed_role(role: Role) -> Option<SeedRole> {
    match role {
        Role::Nexus | Role::Council => None,
        Role::Scout => Some(SeedRole::Scout),
        Role::Builder => Some(SeedRole::Builder),
        Role::Archivist => Some(SeedRole::Archivist),
        Role::Critic => Some(SeedRole::Critic),
        Role::Seer => Some(SeedRole::Seer),
        Role::Artisan => Some(SeedRole::Artisan),
        Role::Watcher => Some(SeedRole::Watcher),
    }
}

/// `load::default_on_exhausted` is private in `mv-config`; this copy
/// matches that table so a missing `[roles.X]` still walks with the
/// same policy a real dispatch would.
fn default_on_exhausted(role: Role) -> ConfigFallbackPolicy {
    match role {
        Role::Nexus => ConfigFallbackPolicy::Warn,
        Role::Seer | Role::Critic => ConfigFallbackPolicy::Queue,
        Role::Council => ConfigFallbackPolicy::Shrink,
        Role::Builder | Role::Artisan => ConfigFallbackPolicy::DegradeOnce,
        Role::Scout | Role::Archivist | Role::Watcher => ConfigFallbackPolicy::Degrade,
    }
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

/// Seed-time seat: config fields only. Live breaker/quota/cap/lend are
/// not consulted — a closed breaker and `lent = true` so the walk reports
/// what dispatch would see on a healthy fleet, not this moment's gauges.
fn seed_seat(account: &Account) -> selector::Seat {
    selector::Seat {
        name: account.name.clone(),
        vendor: account.vendor.clone(),
        pool: account.pool.clone(),
        kind: account.kind.clone(),
        available: account.available,
        unavailable_reason: account.unavailable_reason.clone(),
        unavailable_until: None,
        lent: true,
        has_model: true,
        roles_allowed: account
            .roles_allowed
            .as_ref()
            .map(|roles| roles.iter().map(|r| r.config_key().to_string()).collect()),
        max_brief_chars: account.max_brief_chars.map(|v| v as usize),
        has_command: !account.command.trim().is_empty(),
        cap: None,
        open_count: None,
        quota_primary_percent: None,
        quota_secondary_percent: None,
        local_alive: (account.vendor == "local")
            .then(|| selector::HttpLocalLivenessProbe.probe(&account.name, &account.health_url)),
    }
}

struct ClosedBreaker;

impl selector::BreakerQuery for ClosedBreaker {
    fn status(&self, _pool: &str) -> selector::BreakerStatus {
        selector::BreakerStatus::CLOSED
    }

    fn claim_trial(&self, _pool: &str, _task_id: &str, _token: Option<&str>) -> Option<String> {
        None
    }
}

/// Python `orchestrator.dispatchable_roles`: walk each configured chain
/// through [`selector::select`], the same selector a real dispatch uses.
fn dispatchable_roles(config: &Config, project: &Project) -> Vec<DispatchableRole> {
    let mut rows = Vec::new();
    let now = Instant::now();
    for role in Role::ALL {
        let Some(seed_role) = seed_role(role) else {
            continue;
        };
        let Some(chain) = project.chains.get(&role) else {
            continue;
        };
        let seats: Vec<selector::Seat> = chain
            .iter()
            .filter_map(|name| config.accounts.get(name).map(seed_seat))
            .collect();
        let on_exhausted = config
            .roles
            .get(&role)
            .map(|settings| settings.on_exhausted)
            .unwrap_or_else(|| default_on_exhausted(role));
        let reviewer_floor =
            (role == Role::Critic).then_some(config.review_policy.min_quota_percent as f64);
        let fleet_floor = (config.dispatch_min_quota_percent > 0)
            .then_some(config.dispatch_min_quota_percent as f64);
        let params = selector::SelectParams {
            role: role.config_key(),
            project: &project.name,
            reviewer_role: Role::Critic.config_key(),
            nexus_role: Role::Nexus.config_key(),
            exclude_vendors: &[],
            task_len_chars: 0,
            floors: selector::QuotaFloors {
                reviewer_min_quota_percent: reviewer_floor,
                fleet_min_quota_percent: fleet_floor,
            },
            policy: to_selector_policy(on_exhausted),
            task_id: "seed",
            trial_token: None,
        };
        let account = match selector::select(&seats, &params, &ClosedBreaker, now) {
            Ok(selection) => Ok(selection.seat.name),
            Err(error) => Err(error.to_string()),
        };
        rows.push(DispatchableRole {
            role: seed_role,
            account,
        });
    }
    rows
}

/// Known `[review_policy].require_cross_vendor_for` labels — the same six
/// `mv_config::load` warns on rather than refuses, matching Python's
/// `KNOWN_RISK_AREAS`. Sorted the way `render_review_policy` joins them
/// in the unknown-area warning.
const KNOWN_RISK_AREAS: &[&str] = &[
    "auth",
    "deletion",
    "migration",
    "release",
    "secrets",
    "security",
];

fn unknown_risk_areas(policy: &ReviewPolicy) -> Vec<String> {
    let mut unknown: Vec<String> = policy
        .require_cross_vendor_for
        .iter()
        .filter(|area| !KNOWN_RISK_AREAS.contains(&area.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown.dedup();
    unknown
}

/// Python `config.render_review_policy`, copied from the private
/// `format_policy` in `review_policy.rs` (that file is owned by another
/// lane). Seed insertion prefixes each line with `- `, matching
/// `orchestrator.py`'s seed walk.
fn format_policy(policy: &ReviewPolicy) -> String {
    let critic = Role::Critic.display_name();
    let critic_noun = Role::Critic.config_key();
    let nexus_noun = Role::Nexus.config_key();
    let areas = if policy.require_cross_vendor_for.is_empty() {
        "(none)".to_owned()
    } else {
        policy.require_cross_vendor_for.join(", ")
    };
    let mut lines = vec![
        format!(
            "Cross-vendor review required for: {areas}. Everything else may \
             merge on verify-by-running evidence (full node ids, revert \
             mutation, full suite) and is tagged for optional batch review."
        ),
        if policy.rereview == "diff" {
            format!(
                "Re-review after REVISE: {} — only the fix-round diff plus \
                 the original findings, not the whole change again.",
                policy.rereview
            )
        } else {
            format!(
                "Re-review after REVISE: {} — the whole change again, not \
                 just the fix-round diff.",
                policy.rereview
            )
        },
    ];
    if policy.batch_low_risk {
        lines.push(format!(
            "Low-risk batching: on — up to {} merged low-risk lane(s) \
             accumulate for one {critic} over several lanes.",
            policy.batch_max_lanes
        ));
    } else {
        lines.push("Low-risk batching: off — every lane reviews on its own.".to_owned());
    }
    lines.push(format!(
        "After {} REVISE round(s), stop asking for another review and \
         escalate to the {nexus_noun} instead.",
        policy.max_rereviews
    ));
    let unknown = unknown_risk_areas(policy);
    if !unknown.is_empty() {
        lines.push(format!(
            "warning: require_cross_vendor_for names risk area(s) not in \
             the known set ({}): {}. Accepted, not refused — a fleet may \
             invent its own labels — but check the spelling.",
            KNOWN_RISK_AREAS.join(", "),
            unknown.join(", ")
        ));
    }
    if policy.scope == "diff" {
        lines.push(format!(
            "Review scope: diff — a scoped brief built from the lane's \
             diff against its merge base, truncated at {} line(s); read \
             only what it touches.",
            policy.max_diff_lines
        ));
    } else {
        lines.push(
            "Review scope: repo — no diff is built; explore the repository \
             as needed."
                .to_owned(),
        );
    }
    let output_tail = if policy.output == "terse" {
        "verdict plus numbered findings, no prose."
    } else {
        "whatever the finding needs."
    };
    lines.push(format!(
        "{critic} effort: {}. Output: {} — {output_tail}",
        policy.effort, policy.output
    ));
    lines.push(format!(
        "Timeout: {} minute(s) — `multiverse sweep` flags a {critic_noun} \
         still running past this.",
        policy.timeout_minutes
    ));
    lines.push(format!(
        "Minimum quota to dispatch: {}% left on the seat's last-read \
         usage gauge — below that, fall through to the next seat in the \
         chain (an unknown gauge never blocks).",
        policy.min_quota_percent
    ));
    if policy.run_tests {
        lines.push(format!(
            "Run tests: yes — left to the {critic}'s own judgement."
        ));
    } else {
        lines.push(format!(
            "Run tests: no — the {nexus_noun} already ran the mutations and \
             the full suite; read and reason only, do not rerun the suite."
        ));
    }
    if policy.budget_per_window == 0 {
        lines.push(format!("{critic} dispatch budget: unlimited."));
    } else {
        lines.push(format!(
            "{critic} dispatch budget: {} per project per rolling 24h \
             window (0 = unlimited) — `dispatch` refuses past it; \
             `multiverse review-policy` shows used/limit per project.",
            policy.budget_per_window
        ));
    }
    lines.join("\n")
}

fn review_policy_summary(policy: &ReviewPolicy) -> String {
    format_policy(policy)
        .lines()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Write this project's Nexus seed, including the selector-resolved
/// "what you may dispatch here" table and the `[review_policy]` summary.
///
/// Also returns the handoff resolution ([`nexus_seed::resolve_handoff`])
/// this seed was built from, so the caller can tell the operator which
/// card (or file, or neither) `start` is pointing this project's Nexus
/// at — not just write that decision silently into a file they would
/// otherwise have to go read to find out (card `20261102-b-4a5e3c`).
fn write_project_seed(
    config: &Config,
    config_path: &Path,
    project: &Project,
    path: &Path,
) -> std::io::Result<(Option<PathBuf>, nexus_seed::Handoff)> {
    with_project_seed(config, config_path, project, path, |seed_path, context| {
        let backup = nexus_seed::write_seed(seed_path, context)?;
        Ok((backup, context.handoff.clone()))
    })
}

/// Build the same seed context for freshness checks and writes.
fn with_project_seed<T>(
    config: &Config,
    config_path: &Path,
    project: &Project,
    path: &Path,
    action: impl FnOnce(&Path, &SeedContext<'_>) -> std::io::Result<T>,
) -> std::io::Result<T> {
    let state_dir = state_dir_from_config(config_path);
    let directives_root = mv_engine::directives::directives_root(&state_dir);
    let handoff = nexus_seed::resolve_handoff(path);
    let role_prompt = mv_engine::prompts::role_prompt("Nexus")
        .ok()
        .map(|prompt| prompt.body());
    let roles = dispatchable_roles(config, project);
    let review_policy = review_policy_summary(&config.review_policy);
    let context = SeedContext {
        project: &project.name,
        repository: path,
        handoff,
        directives_root: &directives_root,
        roles: &roles,
        review_policy: &review_policy,
        max_concurrent: config.max_concurrent as usize,
        dispatch_min_quota_percent: config.dispatch_min_quota_percent.min(100) as u8,
        lend_interactive_to_workers: config.lend_interactive_to_workers,
        role_prompt: role_prompt.as_deref(),
    };
    let seed_path = path.join(".multiverse").join(nexus_seed::SEED_FILENAME);
    action(&seed_path, &context)
}

/// One line naming which card (or file, or neither) [`write_project_seed`]
/// resolved this project's handoff to — the same resolution the seed
/// itself carries, surfaced on the console so `start`'s output answers
/// "which card will the Nexus be pointed at" without reading the seed.
fn handoff_summary(handoff: &nexus_seed::Handoff) -> String {
    match handoff {
        nexus_seed::Handoff::Card { item, title } => {
            format!("handoff: backlog card {item} (\"{title}\")")
        }
        nexus_seed::Handoff::File(path) => format!("handoff: file {}", path.display()),
        nexus_seed::Handoff::None => "handoff: none recorded".to_string(),
    }
}

/// What became of one project in the per-project loop — shared between
/// `run`'s batch and [`start_one_by_path`] (`new`'s final step), so the
/// two call sites cannot drift on what "opened"/"seeded" means.
enum Outcome {
    Opened { message: String, seeded: bool },
    Idle { message: String },
    Failed { message: String },
}

/// Python `pathlib.Path.home()`: a nonempty `$HOME` is used as-is; an
/// empty `$HOME` is `/`; an unset `$HOME` falls back to the passwd
/// database (`std::env::home_dir`). `None` only when even that fails —
/// the one case Python raises `RuntimeError` and `cmd_start` cannot wrap.
fn resolve_python_home_from(home: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    match home {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        Some(_) => Some(PathBuf::from("/")),
        None => std::env::home_dir().filter(|path| !path.as_os_str().is_empty()),
    }
}

fn resolve_python_home() -> Option<PathBuf> {
    resolve_python_home_from(std::env::var_os("HOME").as_deref())
}

fn nexus_launch_script(
    account: &Account,
    command: &str,
    home: Option<&Path>,
    directives_root: &Path,
) -> Result<String, String> {
    if account.kind != "claude" {
        return Ok(command.to_string());
    }
    let owned;
    let home = match home {
        Some(path) if !path.as_os_str().is_empty() => path,
        Some(_) => Path::new("/"),
        None => {
            owned = resolve_python_home().ok_or_else(|| {
                "cannot resolve HOME; refusing to launch a Claude Nexus without deny rules"
                    .to_string()
            })?;
            owned.as_path()
        }
    };
    Ok(mv_engine::trust::claude_orchestrator_launch(
        command,
        Path::new(account.config_dir.as_raw()),
        home,
        directives_root,
    ))
}

#[allow(clippy::too_many_arguments)]
fn launch_project<H: Herdr>(
    herdr: &mut H,
    config: &Config,
    config_path: &Path,
    project: &Project,
    path: &Path,
    seat_account: Option<&Account>,
    fleet: &FleetState,
    reseed: bool,
    dashboard: bool,
    timing: &Timing,
) -> Outcome {
    let name = &project.name;
    if !path.exists() {
        return Outcome::Failed {
            message: format!(
                "  {name:<20} FAILED — path does not exist: {}",
                path.display()
            ),
        };
    }

    if fleet.is_open(name, path) {
        let workspace_id = fleet.workspace_id(name, path);
        let (seat, short) = naming_parts(project, seat_account);
        // Python `cli.cmd_start`: a workspace staying registered in Herdr's
        // listing says nothing about whether the orchestrator *inside* it is
        // still alive — only the agent roster can say that. Checked before
        // the `!reseed` shortcut below, so a dead orchestrator still gets a
        // seed refresh and re-point attempt even without `--reseed`, instead
        // of reading as healthy because the seed on disk happens to be
        // current.
        let orchestrator_dead = seat_account.is_some()
            && find_orchestrator_agent(fleet, name, path, seat, short).is_none();
        if !reseed && !orchestrator_dead {
            return Outcome::Idle {
                message: maybe_dashboard(
                    herdr,
                    config,
                    config_path,
                    project,
                    path,
                    workspace_id.as_deref(),
                    dashboard,
                    format!("  {name:<20} already open"),
                ),
            };
        }
        let was_stale = with_project_seed(config, config_path, project, path, |seed, context| {
            nexus_seed::seed_is_stale(seed, context)
        });
        if matches!(was_stale, Ok(Some(false))) && !orchestrator_dead {
            let found = find_orchestrator_agent(fleet, name, path, seat, short);
            // Python `cli.cmd_start:5586-5589`: `_label_orchestrator_pane`
            // runs here too, not only on the fresh-open path above — a
            // reseed that never re-points (the file was already current)
            // must still refresh the pane's title.
            if let (Some(account), Some((agent_name, live))) = (seat_account, &found) {
                let launch_command = if account.command.trim().is_empty() {
                    account.kind.as_str()
                } else {
                    account.command.as_str()
                };
                let _ = herdr.call(
                    &argv::pane_rename(
                        &live.pane_id,
                        &orchestrator_pane_label(
                            config,
                            project,
                            account,
                            agent_name,
                            launch_command,
                        ),
                    ),
                    Duration::from_secs(30),
                );
            }
            let message = match found {
                Some((agent_name, live)) if live.is_blocked() => format!(
                    "  {name:<20} file is current, but {agent_name} is blocked \
                     and may never have read it — clear its prompt, then \
                     `multiverse start {name} --reseed`"
                ),
                _ => format!("  {name:<20} already current — not re-pointed"),
            };
            return Outcome::Idle {
                message: maybe_dashboard(
                    herdr,
                    config,
                    config_path,
                    project,
                    path,
                    workspace_id.as_deref(),
                    dashboard,
                    message,
                ),
            };
        }
        let (backup, _handoff) = match write_project_seed(config, config_path, project, path) {
            Ok(result) => result,
            Err(error) => {
                return Outcome::Failed {
                    message: format!(
                        "  {name:<20} FAILED — cannot preserve or write seed: {error}"
                    ),
                }
            }
        };
        let backup_note = match backup {
            Some(backup) => format!(
                "  {name:<20} backed up edited seed to {}\n",
                backup.display()
            ),
            None => String::new(),
        };
        let seed = path.join(".multiverse").join(nexus_seed::SEED_FILENAME);
        let seed_display = seed.display();
        let found = find_orchestrator_agent(fleet, name, path, seat, short);
        // Python `cli.cmd_start:5643-5646`: `_label_orchestrator_pane` runs
        // as soon as the pane is found, before the blocked/dialog/send
        // checks below — a reseed re-labels the pane even when the
        // re-point itself does not land. Skipped only when the pane could
        // not be found at all, same as Python's `found is None` guard.
        if let (Some(account), Some((agent_name, live))) = (seat_account, &found) {
            let launch_command = if account.command.trim().is_empty() {
                account.kind.as_str()
            } else {
                account.command.as_str()
            };
            let _ = herdr.call(
                &argv::pane_rename(
                    &live.pane_id,
                    &orchestrator_pane_label(config, project, account, agent_name, launch_command),
                ),
                Duration::from_secs(30),
            );
        }
        let reseed_line = match found {
            None => format!(
                "  {name:<20} RESEEDED {seed_display} — but its root pane could not be \
                 found, so the session was NOT re-pointed; it will read the new file only \
                 when it next starts"
            ),
            Some((agent_name, live)) if live.is_blocked() => format!(
                "  {name:<20} RESEEDED {seed_display} — but {agent_name} is blocked and \
                 may never have read it — clear its prompt, then \
                 `multiverse start {name} --reseed`"
            ),
            Some((agent_name, live)) => {
                // Python `cli.cmd_start`: a separate probe read before the
                // send, so an unreadable pane and a pane parked on a dialog
                // or menu each get their own message instead of both
                // collapsing into `deliver_prompt_to`'s generic RPC-failure
                // error — same split `nudge_one`/`plan_nudge`
                // (`commands/activity.rs`) and `tick_outcome`
                // (`commands/sweep.rs`) already make before their own sends.
                let pane_text = match herdr.call(
                    &argv::agent_read(&agent_name, Some("visible"), None),
                    Duration::from_secs(30),
                ) {
                    Ok(result) if result.ok() && !result.stdout.trim().is_empty() => {
                        Some(result.stdout)
                    }
                    _ => None,
                };
                match pane_text {
                    None => format!(
                        "  {name:<20} RESEEDED {seed_display} — but {agent_name} could not \
                         be read, so the session was NOT re-pointed; clear or inspect it, \
                         then `multiverse start {name} --reseed`"
                    ),
                    Some(text) => {
                        let kind = seat_account
                            .map(|account| account.kind.as_str())
                            .unwrap_or("");
                        match mv_engine::runner::dialog_or_menu_reason(&text, kind) {
                            Some(reason) => format!(
                                "  {name:<20} RESEEDED {seed_display} — but {agent_name} is \
                                 parked on a {reason}, so the session was NOT re-pointed; \
                                 clear it, then `multiverse start {name} --reseed`"
                            ),
                            None => {
                                let pointer = nexus_seed::seed_prompt(name, &seed, true);
                                match mv_engine::runner::deliver_prompt_to(
                                    herdr,
                                    &agent_name,
                                    &live.pane_id,
                                    &pointer,
                                    kind,
                                    live.state_change_seq,
                                    timing,
                                ) {
                                    Ok(()) => format!(
                                        "  {name:<20} reseeded and re-pointed ({agent_name})"
                                    ),
                                    Err(error) => format!(
                                        "  {name:<20} RESEEDED {seed_display} — re-point \
                                         FAILED: {error}. The file is current; the session \
                                         is not."
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        };
        return Outcome::Idle {
            message: maybe_dashboard(
                herdr,
                config,
                config_path,
                project,
                path,
                workspace_id.as_deref(),
                dashboard,
                format!("{backup_note}{reseed_line}"),
            ),
        };
    }
    if reseed {
        return Outcome::Idle {
            message: format!("  {name:<20} not open — nothing to reseed"),
        };
    }

    // Card `20261022-b-c62a28` finding 1: two concurrent `start <project>`
    // runs can both pass the snapshot check above. `_project_lock` plus a
    // fresh listing taken under it lets the second run see the first's
    // workspace instead of creating a duplicate.
    let lock_path = project_lock_path(path);
    let _lock = match mv_core::lock::acquire_default(&lock_path, "project") {
        Ok(guard) => guard,
        Err(error) => {
            return Outcome::Failed {
                message: format!("  {name:<20} FAILED — {error}"),
            };
        }
    };
    let live = match FleetState::fetch(herdr, seat_account.is_some()) {
        Ok(state) => state,
        Err(message) => {
            return Outcome::Failed {
                message: format!("  {name:<20} FAILED — {message}"),
            };
        }
    };
    if live.is_open(name, path) {
        return Outcome::Idle {
            message: format!(
                "  {name:<20} already open — opened by a concurrent `start` while this one waited"
            ),
        };
    }

    // Python `cli.cmd_start`: the expected orchestrator name is a lease.
    // If the workspace listing missed it but the roster still holds it,
    // refuse rather than opening a duplicate Nexus. Checked against `live`
    // (fetched under the project lock just above), not `fleet` (the
    // pre-lock snapshot) — otherwise the lock does nothing for this guard.
    if let Some(account) = seat_account {
        let (seat, short) = naming_parts(project, Some(account));
        let current = nexus_agent_name(name, seat, short).ok();
        let legacy = nexus_legacy_agent_name(name, seat, short).ok();
        let existing = current
            .as_deref()
            .filter(|n| live.has_named_agent(n))
            .or_else(|| {
                legacy
                    .as_deref()
                    .filter(|n| live.has_named_agent(n) && current.as_deref() != Some(*n))
            });
        if let Some(existing) = existing {
            return Outcome::Failed {
                message: format!(
                    "  {name:<20} REFUSED — orchestrator {existing} already exists; \
                     workspace listing may be stale"
                ),
            };
        }
    }

    let created = match mv_engine::fleet_start::create_workspace(herdr, name, path) {
        Ok(created) => created,
        Err(message) => {
            return Outcome::Failed {
                message: format!("  {name:<20} FAILED — {message}"),
            }
        }
    };

    let (seed, handoff) = match write_project_seed(config, config_path, project, path) {
        Ok((_, handoff)) => (
            path.join(".multiverse").join(nexus_seed::SEED_FILENAME),
            handoff,
        ),
        Err(error) => {
            let mut message =
                format!("  {name:<20} FAILED — cannot preserve or write seed: {error}");
            if let Err(cleanup) =
                mv_engine::fleet_start::close_workspace(herdr, &created.workspace_id)
            {
                message.push_str(&format!(
                    "\n  {name:<20} STRANDED workspace {} — cleanup failed: {cleanup}",
                    created.workspace_id
                ));
            }
            return Outcome::Failed { message };
        }
    };

    let mut seat_fragment = String::new();
    let mut seeded = false;
    if let Some(account) = seat_account {
        let launch_command = if account.command.trim().is_empty() {
            account.kind.as_str()
        } else {
            account.command.as_str()
        };
        let home = resolve_python_home();
        let directives_root =
            mv_engine::directives::directives_root(&state_dir_from_config(config_path));
        let script =
            match nexus_launch_script(account, launch_command, home.as_deref(), &directives_root) {
                Ok(script) => script,
                Err(error) => {
                    return Outcome::Failed {
                        message: format!("  {name:<20} FAILED — {error}"),
                    };
                }
            };
        let (seat, short) = naming_parts(project, Some(account));
        let agent_name = match nexus_agent_name(name, seat, short) {
            Ok(name) => name,
            Err(error) => {
                return Outcome::Failed {
                    message: format!("  {name:<20} FAILED — {error}"),
                };
            }
        };
        // Python `cli.cmd_start:5847`: `_label_orchestrator_pane` runs right
        // before the launch script, so the pane carries its title from the
        // first moment anything is running in it. Best-effort, matching
        // Python's own `try`/`except Exception: return` — a rename failure
        // here must never block the launch that follows.
        let _ = herdr.call(
            &argv::pane_rename(
                &created.pane,
                &orchestrator_pane_label(config, project, account, &agent_name, launch_command),
            ),
            Duration::from_secs(30),
        );
        let pointer = nexus_seed::seed_prompt(name, &seed, false);
        let result = mv_engine::fleet_start::launch_script_and_seed(
            herdr,
            &created.pane,
            launch_command,
            &script,
            &agent_name,
            &pointer,
            timing,
        );
        if result.seeded {
            seat_fragment = format!("  {launch_command}  {agent_name}");
            seeded = true;
        } else {
            seat_fragment = format!("  {launch_command}  (unseeded: {})", result.note);
        }
    }

    Outcome::Opened {
        message: maybe_dashboard(
            herdr,
            config,
            config_path,
            project,
            path,
            Some(&created.workspace_id),
            dashboard,
            format!(
                "  {name:<20} {}  pane {}  {}{seat_fragment}\n  {name:<20} {}",
                created.workspace_id,
                created.pane,
                path.display(),
                handoff_summary(&handoff)
            ),
        ),
        seeded,
    }
}

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

    if config.projects.is_empty() {
        println!("No projects configured.");
        return Err(Failure::Reported(NEEDS_CONFIG));
    }

    let all_projects: Vec<&Project> = config.projects.values().collect();
    let configured_paths: Vec<&Path> = all_projects
        .iter()
        .filter_map(|p| project_path(p))
        .collect();
    if let Some(refusal) = temporary_projects_refusal(configured_paths.into_iter()) {
        println!("{refusal}");
        return Err(Failure::Reported(ERROR));
    }

    let all = args.get_flag("all");
    let reseed = args.get_flag("reseed");
    let dashboard = args.get_flag("dashboard");
    let requested: Vec<String> = args
        .get_many::<String>("projects")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();

    if all && !requested.is_empty() {
        eprintln!("cannot combine --all with named projects");
        return Err(Failure::Reported(ERROR));
    }
    if reseed && requested.is_empty() && !all {
        eprintln!("start --reseed requires project names or --all");
        return Err(Failure::Reported(ERROR));
    }

    let mut herdr = ProcessHerdr;

    if requested.is_empty() && !all {
        let fleet = FleetState::fetch(&mut herdr, false).map_err(|message| {
            eprintln!("{message}");
            Failure::Reported(ERROR)
        })?;
        println!("Configured projects:");
        let mut names: Vec<&String> = config.projects.keys().collect();
        names.sort();
        for name in names {
            let project = &config.projects[name];
            let status = match project_path(project) {
                Some(path) if fleet.is_open(name, path) => "open",
                _ => "not open",
            };
            println!("  {name:<20} {status}");
        }
        println!("name the projects to start, or use --all");
        return Err(Failure::Reported(ERROR));
    }

    let wanted: Vec<String> = if all {
        let mut names: Vec<String> = config.projects.keys().cloned().collect();
        names.sort();
        names
    } else {
        // Dedupe, preserving the first occurrence's position: `fleet` below is
        // a single snapshot taken once, before the per-project loop, and
        // never updated as the loop opens workspaces. A name repeated in
        // `requested` (a typo, a shell glob expanding to a repeat) would
        // otherwise see that same stale "not open" snapshot twice and open
        // two competing workspaces for one project.
        let mut seen = std::collections::HashSet::new();
        requested
            .into_iter()
            .filter(|name| seen.insert(name.clone()))
            .collect()
    };
    let unknown: Vec<&String> = wanted
        .iter()
        .filter(|name| !config.projects.contains_key(*name))
        .collect();
    if !unknown.is_empty() {
        let mut configured: Vec<&str> = all_projects.iter().map(|p| p.name.as_str()).collect();
        configured.sort_unstable();
        let names: Vec<&str> = unknown.iter().map(|name| name.as_str()).collect();
        eprintln!(
            "no such project: {}; configured: {} (see `multiverse projects`)",
            names.join(", "),
            configured.join(", ")
        );
        return Err(Failure::Reported(NEEDS_CONFIG));
    }

    let seat_account = config.accounts.values().find(|account| account.interactive);

    // `FleetState::fetch`'s `include_agents` matches Python's transcript,
    // which fetches the live roster whenever an interactive seat is
    // configured — the same roster `launch_project`'s `--reseed` path
    // now searches via `FleetState::orchestrator_agent` to find an
    // already-open project's live Nexus pane by identity.
    let fleet = FleetState::fetch(&mut herdr, seat_account.is_some()).map_err(|message| {
        eprintln!("{message}");
        Failure::Reported(ERROR)
    })?;

    let timing = Timing::default();
    let mut opened = 0usize;
    let mut failed = 0usize;
    let mut seeded = 0usize;

    for name in &wanted {
        let project = &config.projects[name];
        let Some(path) = project_path(project) else {
            println!("  {name:<20} FAILED — path is not configured");
            failed += 1;
            continue;
        };
        match launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            path,
            seat_account,
            &fleet,
            reseed,
            dashboard,
            &timing,
        ) {
            Outcome::Opened {
                message,
                seeded: was_seeded,
            } => {
                println!("{message}");
                opened += 1;
                if was_seeded {
                    seeded += 1;
                }
            }
            Outcome::Idle { message } => println!("{message}"),
            Outcome::Failed { message } => {
                println!("{message}");
                failed += 1;
            }
        }
    }

    println!("\nopened {opened} of {}", wanted.len());
    if seat_account.is_none() {
        println!(
            "No account is marked `interactive = true`, so nothing was started in those panes.\n\
             Mark the seat you talk to and `start` will bring it up with each project."
        );
    } else if opened > 0 {
        let orch = mv_config::roles::Role::Nexus.display_name();
        if seeded > 0 {
            println!(
                "{seeded} of {opened} panes were handed `.multiverse/{}`: their repository, the \
                 roles\nthey may dispatch, and the rule that no other repository exists. Those \
                 sessions are\nthis project's **{orch}** — the one you talk to. Each dispatches \
                 its own specialists;\nnothing else was started and nothing was spent.",
                nexus_seed::SEED_FILENAME
            );
        }
        if seeded < opened {
            println!(
                "\n{} pane(s) were opened but never told they are the\n**{orch}** — the reason \
                 is printed under each one above. Until a session reads its seed\nit does not \
                 know this project, and it will not dispatch anything.",
                opened - seeded
            );
        }
    }

    if failed > 0 {
        Err(Failure::Reported(ERROR))
    } else {
        Ok(())
    }
}

/// `new`'s final step: start the one project it just created/registered.
/// Shares [`launch_project`] with `run`'s batch loop rather than
/// re-deriving what "started" means.
pub(crate) fn start_one_by_path(name: &str, path: &Path, reseed: bool) -> Result<String, String> {
    let (config_path, _source) = resolve_config_path(None).map_err(|error| error.to_string())?;
    if !config_path.is_file() {
        return Err(format!("no config file found at {}", config_path.display()));
    }
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| config_load_error_message(error.to_string()))?;
    let Some(project) = config.projects.get(name) else {
        return Err(format!("{name} is not a configured project"));
    };
    let seat_account = config.accounts.values().find(|account| account.interactive);

    let mut herdr = ProcessHerdr;
    let fleet = FleetState::fetch(&mut herdr, seat_account.is_some())?;

    match launch_project(
        &mut herdr,
        &config,
        &config_path,
        project,
        path,
        seat_account,
        &fleet,
        reseed,
        false,
        &Timing::default(),
    ) {
        Outcome::Failed { message } => Err(message),
        Outcome::Opened { message, .. } | Outcome::Idle { message } => Ok(message),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    use mv_config::load::load_config_str;
    use mv_engine::mv_herdr::envelope::CommandResult;
    use mv_engine::runner::RunnerError;
    use serde_json::json;

    #[test]
    fn a_project_under_the_temp_root_is_temporary() {
        let dir = std::env::temp_dir().join(format!("mv-start-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(projects_are_temporary(std::iter::once(dir.as_path())));
        std::fs::remove_dir(&dir).ok();
    }

    #[test]
    fn an_ordinary_project_path_is_not_temporary() {
        assert!(!projects_are_temporary(std::iter::once(Path::new("/"))));
    }

    #[test]
    fn no_projects_is_not_temporary() {
        assert!(!projects_are_temporary(std::iter::empty()));
    }

    #[test]
    fn temp_refusal_is_skipped_when_the_operator_opts_in() {
        let refusal =
            temporary_projects_refusal_with(std::iter::once(std::env::temp_dir().as_path()), true);
        assert!(refusal.is_none());
    }

    #[test]
    fn naming_parts_feed_python_agent_name() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let config = load_config_str(
            &format!(
                "[accounts.nexus-seat]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"{}\"\n\
                 interactive = true\n\
                 command = \"claude-interactive\"\n\
                 [projects.sandbox]\npath = \"{}\"\n",
                dir.path().display(),
                sandbox.display()
            ),
            Path::new("config.toml"),
        )
        .unwrap();
        let project = &config.projects["sandbox"];
        let seat = config.accounts.values().find(|a| a.interactive);
        let (seat_short, project_short) = naming_parts(project, seat);
        assert_eq!(seat_short, "ne");
        assert_eq!(project_short, "");
        assert_eq!(
            nexus_agent_name(&project.name, seat_short, project_short).unwrap(),
            "sandbox-nexu-ne"
        );
        assert_eq!(
            nexus_legacy_agent_name(&project.name, seat_short, project_short).unwrap(),
            "sandbox-orch-ne"
        );
    }

    /// Python `cli.py:5842-5846` + `Path.home()`: a claude-kind interactive
    /// seat is always wrapped. Empty `HOME` resolves to `/`; unset `HOME`
    /// falls back to the passwd database. Neither path launches the bare
    /// command (the previous unit test explicitly accepted that hole).
    #[test]
    fn nexus_launch_script_never_launches_claude_bare_when_home_is_missing_or_empty() {
        let (_dir, config) = chained_fixture(true);
        let directives = Path::new("/fleet/directives");
        let claude = &config.accounts["nexus-seat"];
        let config_dir = Path::new(claude.config_dir.as_raw());

        let empty = nexus_launch_script(
            claude,
            "claude-interactive",
            Some(Path::new("")),
            directives,
        )
        .unwrap();
        assert_ne!(
            empty, "claude-interactive",
            "an empty HOME must not skip the deny wrapper"
        );
        assert_eq!(
            empty,
            mv_engine::trust::claude_orchestrator_launch(
                "claude-interactive",
                config_dir,
                Path::new("/"),
                directives,
            )
        );

        let unset = nexus_launch_script(claude, "claude-interactive", None, directives).unwrap();
        assert_ne!(
            unset, "claude-interactive",
            "an unset HOME must not skip the deny wrapper"
        );
        assert!(
            unset.contains("--disallowedTools"),
            "unset HOME must still carry Edit denies, got {unset:?}"
        );
    }

    #[test]
    fn nexus_launch_script_keeps_other_kinds_bare() {
        let (_dir, config) = chained_fixture(true);
        let directives = Path::new("/fleet/directives");
        let home = Path::new("/home/demo");
        assert_eq!(
            nexus_launch_script(
                &config.accounts["worker"],
                "codex --flag",
                Some(home),
                directives
            )
            .unwrap(),
            "codex --flag"
        );
    }

    #[test]
    fn resolve_python_home_matches_pathlib() {
        use std::ffi::OsStr;
        assert_eq!(
            resolve_python_home_from(Some(OsStr::new("/home/demo"))),
            Some(PathBuf::from("/home/demo"))
        );
        assert_eq!(
            resolve_python_home_from(Some(OsStr::new(""))),
            Some(PathBuf::from("/"))
        );
    }

    #[test]
    fn review_policy_summary_matches_python_prose_with_critic_display_name() {
        let policy = ReviewPolicy::default();
        let summary = review_policy_summary(&policy);
        assert!(summary.starts_with(
            "- Cross-vendor review required for: security, auth, migration, deletion, secrets, release."
        ));
        assert!(summary.contains(
            "- Re-review after REVISE: diff — only the fix-round diff plus the original findings, not the whole change again."
        ));
        assert!(summary.contains(
            "- Low-risk batching: on — up to 6 merged low-risk lane(s) accumulate for one Critic over several lanes."
        ));
        assert!(summary.contains(
            "- After 2 REVISE round(s), stop asking for another review and escalate to the nexus instead."
        ));
        assert!(summary.contains(
            "- Review scope: diff — a scoped brief built from the lane's diff against its merge base, truncated at 1500 line(s); read only what it touches."
        ));
        assert!(summary.contains(
            "- Critic effort: medium. Output: terse — verdict plus numbered findings, no prose."
        ));
        assert!(summary.contains(
            "- Timeout: 30 minute(s) — `multiverse sweep` flags a critic still running past this."
        ));
        assert!(summary.contains("- Minimum quota to dispatch: 20%"));
        assert!(summary.contains("- Run tests: no — the nexus already ran the mutations"));
        assert!(summary.contains("- Critic dispatch budget: 3 per project per rolling 24h window"));
        assert!(!summary.contains("Reviewer"));
        assert!(!summary.contains("reviewer"));
        assert!(!summary.contains("orchestrator"));
        assert!(!summary.contains("Orchestrator"));
    }

    fn chained_fixture(worker_available: bool) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let claude_home = dir.path().join("claude-home");
        let worker_home = dir.path().join("worker-home");
        std::fs::create_dir_all(&sandbox).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        std::fs::create_dir_all(&worker_home).unwrap();
        let available = if worker_available {
            ""
        } else {
            "available = false\nunavailable_reason = \"quota exhausted\"\n"
        };
        let toml = format!(
            "[accounts.nexus-seat]\n\
             kind = \"claude\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"{}\"\n\
             interactive = true\n\
             command = \"claude-interactive\"\n\
             [accounts.worker]\n\
             kind = \"codex\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"{}\"\n\
             command = \"codex\"\n\
             {available}\
             [projects.sandbox]\n\
             path = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"worker\"]\n\
             builder = [\"worker\"]\n\
             critic = [\"worker\"]\n\
             nexus = [\"nexus-seat\"]\n\
             council = [\"worker\"]\n",
            claude_home.display(),
            worker_home.display(),
            sandbox.display()
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        (dir, config)
    }

    #[test]
    fn dispatchable_roles_lists_selector_resolved_seats_and_omits_nexus_and_council() {
        let (_dir, config) = chained_fixture(true);
        let project = &config.projects["sandbox"];
        let rows = dispatchable_roles(&config, project);
        let by_role: Vec<(SeedRole, Result<&str, &str>)> = rows
            .iter()
            .map(|row| {
                (
                    row.role,
                    row.account
                        .as_ref()
                        .map(|name| name.as_str())
                        .map_err(|reason| reason.as_str()),
                )
            })
            .collect();
        assert_eq!(
            by_role,
            vec![
                (SeedRole::Scout, Ok("worker")),
                (SeedRole::Builder, Ok("worker")),
                (SeedRole::Critic, Ok("worker")),
            ]
        );
    }

    #[test]
    fn dispatchable_roles_records_the_selector_exhaustion_reason() {
        let (_dir, config) = chained_fixture(false);
        let project = &config.projects["sandbox"];
        let rows = dispatchable_roles(&config, project);
        assert_eq!(rows.len(), 3);
        for row in &rows {
            let reason = row.account.as_ref().expect_err("chain is exhausted");
            assert!(
                reason.contains("worker") && reason.contains("quota exhausted"),
                "{reason}"
            );
        }
    }

    #[test]
    fn seed_render_includes_the_dispatch_table_and_review_policy_bullets() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        std::fs::create_dir_all(sandbox.join(".multiverse")).unwrap();
        let (_hold, config) = chained_fixture(true);
        let project = &config.projects["sandbox"];
        let roles = dispatchable_roles(&config, project);
        let review_policy = review_policy_summary(&config.review_policy);
        let directives = dir.path().join("directives");
        let context = SeedContext {
            project: &project.name,
            repository: &sandbox,
            handoff: nexus_seed::Handoff::None,
            directives_root: &directives,
            roles: &roles,
            review_policy: &review_policy,
            max_concurrent: config.max_concurrent as usize,
            dispatch_min_quota_percent: config.dispatch_min_quota_percent.min(100) as u8,
            lend_interactive_to_workers: config.lend_interactive_to_workers,
            role_prompt: None,
        };
        let rendered = nexus_seed::render_seed(&context);
        assert!(rendered.contains("| Scout | worker |"), "{rendered}");
        assert!(rendered.contains("| Builder | worker |"), "{rendered}");
        assert!(rendered.contains("| Critic | worker |"), "{rendered}");
        assert!(rendered.contains("--role scout"), "{rendered}");
        assert!(!rendered.contains("| Nexus |"), "{rendered}");
        assert!(!rendered.contains("| Council |"), "{rendered}");
        assert!(
            rendered.contains("- Cross-vendor review required for:"),
            "{rendered}"
        );
        assert!(rendered.contains("- Critic effort: medium."), "{rendered}");
    }

    #[test]
    fn handoff_summary_names_the_card_file_or_absence() {
        let card = nexus_seed::Handoff::Card {
            item: "20261101-b-5fd767".into(),
            title: "Handoff 2026-11-01".into(),
        };
        assert_eq!(
            handoff_summary(&card),
            "handoff: backlog card 20261101-b-5fd767 (\"Handoff 2026-11-01\")"
        );
        let file = nexus_seed::Handoff::File(PathBuf::from(".multiverse/handoff.md"));
        assert_eq!(
            handoff_summary(&file),
            "handoff: file .multiverse/handoff.md"
        );
        assert_eq!(
            handoff_summary(&nexus_seed::Handoff::None),
            "handoff: none recorded"
        );
    }

    #[test]
    fn config_load_error_message_appends_the_migrate_fix_only_for_retired_role_keys() {
        let retired = "roles.fixer: \"fixer\" is a retired role key; run the config migrator \
             (`multiverse config migrate`) to rewrite it before loading this file"
            .to_string();
        let fixed = config_load_error_message(retired.clone());
        assert!(fixed.starts_with(&retired));
        assert!(fixed.contains(MIGRATE_FIX_LINE));

        let unrelated = "malformed TOML: oops".to_string();
        assert_eq!(config_load_error_message(unrelated.clone()), unrelated);
    }

    fn ok(payload: serde_json::Value) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 0,
            stdout: String::new(),
            stderr: String::new(),
            payload: Some(payload),
            protocol_error: None,
        }
    }

    fn failed(message: &str) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 1,
            stdout: String::new(),
            stderr: message.to_string(),
            payload: None,
            protocol_error: None,
        }
    }

    struct Scripted {
        replies: VecDeque<(&'static str, CommandResult)>,
        calls: Vec<String>,
    }

    impl Scripted {
        fn new(replies: Vec<(&'static str, CommandResult)>) -> Self {
            Self {
                replies: replies.into_iter().collect(),
                calls: Vec::new(),
            }
        }
    }

    impl Herdr for Scripted {
        fn call(
            &mut self,
            command: &argv::HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let action = command.argv[1..].join(" ");
            self.calls.push(action.clone());
            let (expected, reply) = self
                .replies
                .pop_front()
                .unwrap_or_else(|| panic!("unexpected call: {action}"));
            assert!(
                action.starts_with(expected),
                "expected a call starting with {expected:?}, got {action:?}"
            );
            Ok(reply)
        }
    }

    fn sandbox_fixture(name: &str) -> (tempfile::TempDir, PathBuf, Config, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join(name);
        std::fs::create_dir_all(sandbox.join(".multiverse")).unwrap();
        let toml = format!("[projects.sandbox]\npath = \"{}\"\n", sandbox.display());
        let config_path = sandbox.join("config.toml");
        let config = load_config_str(&toml, &config_path).unwrap();
        (dir, sandbox, config, config_path)
    }

    fn empty_fleet() -> FleetState {
        let mut herdr = Scripted::new(vec![
            ("workspace list", ok(json!({"result": {"workspaces": []}}))),
            ("pane list", ok(json!({"result": {"panes": []}}))),
        ]);
        FleetState::fetch(&mut herdr, false).unwrap()
    }

    fn open_fleet(project: &str, path: &Path) -> FleetState {
        let cwd = path.to_string_lossy().into_owned();
        let mut herdr = Scripted::new(vec![
            (
                "workspace list",
                ok(json!({"result": {"workspaces": [{"label": project, "workspace_id": "w1"}]}})),
            ),
            (
                "pane list",
                ok(json!({"result": {"panes": [{"workspace_id": "w1", "foreground_cwd": cwd}]}})),
            ),
        ]);
        FleetState::fetch(&mut herdr, false).unwrap()
    }

    fn outcome_message(outcome: Outcome) -> String {
        match outcome {
            Outcome::Opened { message, .. }
            | Outcome::Idle { message }
            | Outcome::Failed { message } => message,
        }
    }

    #[test]
    fn dashboard_for_is_true_when_the_flag_is_set_or_the_project_is_listed() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let config = load_config_str(
            &format!("[projects.sandbox]\npath = \"{}\"\n", sandbox.display()),
            Path::new("config.toml"),
        )
        .unwrap();
        assert!(dashboard_for(true, &config, "sandbox"));
        assert!(!dashboard_for(false, &config, "sandbox"));
        assert!(dashboard_for(false, &config, "multiverse"));

        let enabled = load_config_str(
            &format!(
                "[board]\ndashboard_tab = true\n[projects.sandbox]\npath = \"{}\"\n",
                sandbox.display()
            ),
            Path::new("config.toml"),
        )
        .unwrap();
        assert!(dashboard_for(false, &enabled, "sandbox"));
    }

    #[test]
    fn a_re_list_under_the_project_lock_reports_a_concurrent_start() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let cwd = sandbox.to_string_lossy().into_owned();
        let mut herdr = Scripted::new(vec![
            (
                "workspace list",
                ok(json!({"result": {"workspaces": [{"label": "sandbox", "workspace_id": "w1"}]}})),
            ),
            (
                "pane list",
                ok(json!({"result": {"panes": [{"workspace_id": "w1", "foreground_cwd": cwd}]}})),
            ),
        ]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            None,
            &empty_fleet(),
            false,
            true,
            &Timing::default(),
        ));
        assert!(
            message.contains("already open — opened by a concurrent `start` while this one waited"),
            "{message}"
        );
        assert!(!message.contains("dashboard"));
        assert_eq!(
            herdr.calls,
            vec!["workspace list".to_string(), "pane list".to_string()]
        );
    }

    #[test]
    fn ensure_dashboard_tab_reports_already_open_when_the_label_exists() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let mut herdr = Scripted::new(vec![(
            "tab list",
            ok(json!({"result": {"tabs": [{"label": "dashboard"}]}})),
        )]);
        let lines = ensure_dashboard_tab(&mut herdr, &config_path, project, &sandbox, "w1");
        assert_eq!(lines, vec!["      dashboard tab already open".to_string()]);
        assert_eq!(herdr.calls, vec!["tab list --workspace w1".to_string()]);
    }

    #[test]
    fn ensure_dashboard_tab_runs_the_board_watcher_after_create() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let mut herdr = Scripted::new(vec![
            ("tab list", ok(json!({"result": {"tabs": []}}))),
            (
                "tab create",
                ok(json!({"result": {"root_pane": {"pane_id": "w1:p2"}}})),
            ),
            ("pane run", ok(json!({"result": {}}))),
        ]);
        let lines = ensure_dashboard_tab(&mut herdr, &config_path, project, &sandbox, "w1");
        assert_eq!(lines, vec!["      dashboard tab running".to_string()]);
        assert_eq!(herdr.calls.len(), 3);
        assert!(herdr.calls[0].starts_with("tab list --workspace w1"));
        assert!(herdr.calls[1].starts_with("tab create --workspace w1"));
        assert!(
            herdr.calls[2].starts_with("pane run w1:p2 "),
            "{}",
            herdr.calls[2]
        );
        assert!(herdr.calls[2].contains("board sandbox --watch"));
    }

    #[test]
    fn ensure_dashboard_tab_reports_failed_when_create_is_refused() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let mut herdr = Scripted::new(vec![
            ("tab list", ok(json!({"result": {"tabs": []}}))),
            ("tab create", failed("tab create refused")),
        ]);
        let lines = ensure_dashboard_tab(&mut herdr, &config_path, project, &sandbox, "w1");
        assert_eq!(
            lines,
            vec!["      dashboard tab FAILED — tab create refused".to_string()]
        );
    }

    #[test]
    fn duplicate_nexus_guard_reads_the_listing_taken_under_the_project_lock() {
        // Critic finding critic-safety.md#8: the lease check must answer
        // from `live` (fetched under the project lock, with agents) — not
        // from `fleet`, the pre-lock snapshot `run()` took before the lock
        // was acquired and before a concurrent `start` could have landed
        // the very agent this guard exists to catch.
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let claude_home = dir.path().join("claude-home");
        std::fs::create_dir_all(sandbox.join(".multiverse")).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        let config_path = sandbox.join("config.toml");
        let config = load_config_str(
            &format!(
                "[accounts.nexus-seat]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"{}\"\n\
                 interactive = true\n\
                 command = \"claude-interactive\"\n\
                 [projects.sandbox]\npath = \"{}\"\n",
                claude_home.display(),
                sandbox.display()
            ),
            &config_path,
        )
        .unwrap();
        let project = &config.projects["sandbox"];
        let seat = config.accounts.values().find(|a| a.interactive);

        // Pre-lock snapshot: nothing open, no agents known yet — this is
        // what `run()` fetched before the concurrent `start` created the
        // duplicate agent and before this call acquired the lock.
        let stale_fleet = empty_fleet();

        // Under the lock: still no open workspace by label (the case this
        // guard exists for — "workspace listing may be stale"), but the
        // roster now carries the expected Nexus name.
        let mut herdr = Scripted::new(vec![
            ("workspace list", ok(json!({"result": {"workspaces": []}}))),
            ("pane list", ok(json!({"result": {"panes": []}}))),
            (
                "agent list",
                ok(json!({"result": {"agents": [{"name": "sandbox-nexu-ne"}]}})),
            ),
        ]);

        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            seat,
            &stale_fleet,
            false,
            false,
            &Timing::default(),
        ));
        assert!(
            message.contains("REFUSED — orchestrator sandbox-nexu-ne already exists"),
            "{message}"
        );
        assert_eq!(
            herdr.calls,
            vec![
                "workspace list".to_string(),
                "pane list".to_string(),
                "agent list".to_string(),
            ]
        );
    }

    #[test]
    fn already_open_without_dashboard_does_not_call_herdr() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let fleet = open_fleet("sandbox", &sandbox);
        let mut herdr = Scripted::new(vec![]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            None,
            &fleet,
            false,
            false,
            &Timing::default(),
        ));
        assert!(message.contains("already open"), "{message}");
        assert!(!message.contains("dashboard"));
        assert!(herdr.calls.is_empty());
    }

    #[test]
    fn already_open_with_dashboard_appends_the_tab_status() {
        let (_dir, sandbox, config, config_path) = sandbox_fixture("sandbox");
        let project = &config.projects["sandbox"];
        let fleet = open_fleet("sandbox", &sandbox);
        let mut herdr = Scripted::new(vec![(
            "tab list",
            ok(json!({"result": {"tabs": [{"label": "dashboard"}]}})),
        )]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            None,
            &fleet,
            false,
            true,
            &Timing::default(),
        ));
        assert!(message.contains("already open"), "{message}");
        assert!(message.contains("dashboard tab already open"), "{message}");
    }

    #[test]
    fn current_reseed_preserves_the_seed_and_does_not_repoint() {
        for status in ["idle", "blocked"] {
            let (dir, config) = chained_fixture(true);
            let sandbox = dir.path().join("sandbox");
            let config_path = sandbox.join("config.toml");
            let project = &config.projects["sandbox"];
            let seat = &config.accounts["nexus-seat"];
            write_project_seed(&config, &config_path, project, &sandbox).unwrap();
            let seed = sandbox.join(".multiverse/orchestrator.md");
            // The digest still matches today's config: preserve human edits
            // rather than rewriting or backing them up on a current reseed.
            let original = format!("{}\nHuman note.\n", std::fs::read_to_string(&seed).unwrap());
            std::fs::write(&seed, &original).unwrap();
            let modified = std::fs::metadata(&seed).unwrap().modified().unwrap();
            let mut listings = Scripted::new(vec![
                (
                    "workspace list",
                    ok(
                        json!({"result": {"workspaces": [{"label": "sandbox", "workspace_id": "w1"}]}}),
                    ),
                ),
                (
                    "pane list",
                    ok(
                        json!({"result": {"panes": [{"workspace_id": "w1", "pane_id": "w1:p1", "foreground_cwd": sandbox}]}}),
                    ),
                ),
                (
                    "agent list",
                    ok(
                        json!({"result": {"agents": [{"name": "sandbox-nexu-ne", "pane_id": "w1:p1", "agent_status": status, "state_change_seq": 1}]}}),
                    ),
                ),
            ]);
            let fleet = FleetState::fetch(&mut listings, true).unwrap();
            // Card `20261106-b-d1d7c4`: Python's `_label_orchestrator_pane`
            // still runs here even though the seed is current and nothing
            // is re-pointed — the pane title must not go stale just because
            // there was nothing to resend.
            let mut herdr = Scripted::new(vec![("pane rename", ok(json!({"result": {}})))]);
            let message = outcome_message(launch_project(
                &mut herdr,
                &config,
                &config_path,
                project,
                &sandbox,
                Some(seat),
                &fleet,
                true,
                false,
                &Timing::default(),
            ));
            let expected = if status == "blocked" {
                "file is current, but sandbox-nexu-ne is blocked and may never have read it"
            } else {
                "already current — not re-pointed"
            };
            assert!(message.contains(expected), "{message}");
            assert_eq!(std::fs::read_to_string(&seed).unwrap(), original);
            assert_eq!(
                std::fs::metadata(&seed).unwrap().modified().unwrap(),
                modified
            );
            assert_eq!(
                std::fs::read_dir(seed.parent().unwrap()).unwrap().count(),
                1
            );
            assert_eq!(
                herdr.calls,
                vec!["pane rename w1:p1 sandbox-nexu-ne · claude-interactive ·  · nexus-seat · nexus sandbox"]
            );
        }
    }

    #[test]
    fn already_open_without_reseed_still_refreshes_when_no_orchestrator_agent_is_found() {
        // Card `20261105-b-126914`: a workspace staying registered in
        // Herdr's listing says nothing about whether the Nexus *inside* it
        // is still alive. Without this guard, `launch_project` took the
        // `already open` shortcut unconditionally and never rewrote the
        // seed or attempted a re-point, even though no live Nexus agent
        // could be found for this project.
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let claude_home = dir.path().join("claude-home");
        std::fs::create_dir_all(sandbox.join(".multiverse")).unwrap();
        std::fs::create_dir_all(&claude_home).unwrap();
        let config_path = sandbox.join("config.toml");
        let config = load_config_str(
            &format!(
                "[accounts.nexus-seat]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"{}\"\n\
                 interactive = true\n\
                 command = \"claude-interactive\"\n\
                 [projects.sandbox]\npath = \"{}\"\n",
                claude_home.display(),
                sandbox.display()
            ),
            &config_path,
        )
        .unwrap();
        let project = &config.projects["sandbox"];
        let seat = config.accounts.values().find(|account| account.interactive);
        // Even a current file must not hide a missing Nexus session.
        write_project_seed(&config, &config_path, project, &sandbox).unwrap();
        // `open_fleet` fetches no agents, matching a workspace whose Nexus
        // pane is no longer in the roster.
        let fleet = open_fleet("sandbox", &sandbox);
        let mut herdr = Scripted::new(vec![]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            seat,
            &fleet,
            false,
            false,
            &Timing::default(),
        ));
        assert!(!message.contains("already open"), "{message}");
        assert!(
            message.contains("RESEEDED") && message.contains("its root pane could not be found"),
            "{message}"
        );
    }

    /// A `CommandResult` carrying pane/agent-read text rather than a JSON
    /// payload — what `agent_read`/`pane_read` (`command`, not
    /// `json_command`) actually return.
    fn readable(stdout: &str) -> CommandResult {
        CommandResult {
            argv: Vec::new(),
            returncode: 0,
            stdout: stdout.to_string(),
            stderr: String::new(),
            payload: None,
            protocol_error: None,
        }
    }

    /// A fleet with one live Nexus orchestrator agent, already open in
    /// workspace `w1` and rooted at `sandbox` — what `reseed_line`'s four
    /// branches (card `20261106-b-d77644`) each start from once the seed is
    /// found stale (or missing) and the orchestrator is not dead.
    fn stale_agent_fleet(sandbox: &Path, pane: &str, status: &str, seq: u64) -> FleetState {
        let mut listings = Scripted::new(vec![
            (
                "workspace list",
                ok(json!({"result": {"workspaces": [{"label": "sandbox", "workspace_id": "w1"}]}})),
            ),
            (
                "pane list",
                ok(
                    json!({"result": {"panes": [{"workspace_id": "w1", "pane_id": pane, "foreground_cwd": sandbox}]}}),
                ),
            ),
            (
                "agent list",
                ok(
                    json!({"result": {"agents": [{"name": "sandbox-nexu-ne", "pane_id": pane, "agent_status": status, "state_change_seq": seq}]}}),
                ),
            ),
        ]);
        FleetState::fetch(&mut listings, true).unwrap()
    }

    /// Python `cli.cmd_start`'s `status == "blocked"` branch (`cli.py:5648-
    /// 5657`) ported as `reseed_line`'s own blocked arm — distinct from the
    /// "already current" blocked message `current_reseed_preserves_the_seed_
    /// and_does_not_repoint` above covers, since this one fires after a
    /// stale (or missing) seed has already been rewritten.
    #[test]
    fn stale_reseed_reports_a_blocked_pane_without_reading_or_sending() {
        let (dir, config) = chained_fixture(true);
        let sandbox = dir.path().join("sandbox");
        let config_path = sandbox.join("config.toml");
        let project = &config.projects["sandbox"];
        let seat = &config.accounts["nexus-seat"];
        let fleet = stale_agent_fleet(&sandbox, "w1:p1", "blocked", 1);
        let mut herdr = Scripted::new(vec![("pane rename", ok(json!({"result": {}})))]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            Some(seat),
            &fleet,
            true,
            false,
            &Timing::default(),
        ));
        assert!(
            message.contains("RESEEDED")
                && message.contains("sandbox-nexu-ne is blocked and may never have read it")
                && message.contains("clear its prompt, then `multiverse start sandbox --reseed`"),
            "{message}"
        );
        // Card `20261106-b-d1d7c4`: the pane still gets re-labeled even
        // though a blocked pane can never be re-pointed.
        assert_eq!(herdr.calls.len(), 1, "{:?}", herdr.calls);
    }

    /// Python's broad `except Exception: probe = None` / `not probe.ok`
    /// branch (`cli.py:5665-5680`): any failed pane read — before a dialog
    /// is even checked — is reported as unreadable, not typed into.
    #[test]
    fn stale_reseed_reports_an_unreadable_pane_before_sending() {
        let (dir, config) = chained_fixture(true);
        let sandbox = dir.path().join("sandbox");
        let config_path = sandbox.join("config.toml");
        let project = &config.projects["sandbox"];
        let seat = &config.accounts["nexus-seat"];
        let fleet = stale_agent_fleet(&sandbox, "w1:p1", "idle", 1);
        let mut herdr = Scripted::new(vec![
            ("pane rename", ok(json!({"result": {}}))),
            ("agent read", failed("pane gone")),
        ]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            Some(seat),
            &fleet,
            true,
            false,
            &Timing::default(),
        ));
        assert!(
            message.contains("RESEEDED")
                && message.contains(
                    "sandbox-nexu-ne could not be read, so the session was NOT re-pointed"
                )
                && message
                    .contains("clear or inspect it, then `multiverse start sandbox --reseed`"),
            "{message}"
        );
        assert_eq!(herdr.calls.len(), 2, "{:?}", herdr.calls);
    }

    /// Python's `dialog_or_menu_reason(probe_text, ...)` branch
    /// (`cli.py:5682-5701`): a pane parked on a trust/approval dialog is
    /// reported, never typed into, and reads differently from an unreadable
    /// pane even though both precede any send.
    #[test]
    fn stale_reseed_reports_a_parked_dialog_before_sending() {
        let (dir, config) = chained_fixture(true);
        let sandbox = dir.path().join("sandbox");
        let config_path = sandbox.join("config.toml");
        let project = &config.projects["sandbox"];
        let seat = &config.accounts["nexus-seat"];
        let fleet = stale_agent_fleet(&sandbox, "w1:p1", "idle", 1);
        let mut herdr = Scripted::new(vec![
            ("pane rename", ok(json!({"result": {}}))),
            ("agent read", readable("Do you trust this folder?")),
        ]);
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            Some(seat),
            &fleet,
            true,
            false,
            &Timing::default(),
        ));
        assert!(
            message.contains("RESEEDED")
                && message.contains("sandbox-nexu-ne is parked on a dialog (trust dialog)")
                && message.contains("clear it, then `multiverse start sandbox --reseed`"),
            "{message}"
        );
        assert_eq!(herdr.calls.len(), 2, "{:?}", herdr.calls);
    }

    /// Python's `if not sent.ok` branch (`cli.py:5704-5709`): once the pane
    /// is confirmed readable and clear of a dialog, a failed send still gets
    /// its own message — one `deliver_prompt_to` cannot confuse with either
    /// of the two probe-stage messages above.
    #[test]
    fn stale_reseed_reports_rpc_failure_distinct_from_unreadable_or_dialog() {
        let (dir, config) = chained_fixture(true);
        let sandbox = dir.path().join("sandbox");
        let config_path = sandbox.join("config.toml");
        let project = &config.projects["sandbox"];
        let seat = &config.accounts["nexus-seat"];
        let fleet = stale_agent_fleet(&sandbox, "w1:p1", "idle", 1);
        // Matches the probed agent/pane but keeps `state_change_seq` at the
        // baseline, so `confirm_submitted` (now shared with `deliver_prompt`)
        // reads it as "not landed yet" and lets the outer retry run its
        // course — a mismatched name/pane here would instead read as "could
        // not confirm submission at all" and short-circuit before either
        // retry, masking the RPC failure this test exists to isolate.
        let not_landed = ok(json!({"result": {"agent": {
            "name": "sandbox-nexu-ne",
            "pane_id": "w1:p1",
            "agent_status": "idle",
            "state_change_seq": 1
        }}}));
        let mut herdr = Scripted::new(vec![
            ("pane rename", ok(json!({"result": {}}))),
            ("agent read", readable("working on it")),
            ("pane read", readable("working on it")),
            ("agent prompt", failed("herdr refused the prompt")),
            ("agent get", not_landed.clone()),
            ("pane read", readable("working on it")),
            ("agent prompt", failed("herdr refused the prompt")),
            ("agent get", not_landed),
        ]);
        let timing = Timing {
            readiness: Duration::ZERO,
            prompt: Duration::ZERO,
            poll: Duration::ZERO,
        };
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            Some(seat),
            &fleet,
            true,
            false,
            &timing,
        ));
        assert!(
            message.contains("RESEEDED")
                && message.contains("re-point FAILED: herdr refused the prompt")
                && message.contains("The file is current; the session is not."),
            "{message}"
        );
        assert!(!message.contains("could not be read"), "{message}");
        assert!(!message.contains("parked on a"), "{message}");
    }

    /// The non-failure arm: once probed clear, a landed send still reports
    /// success exactly as before this card's split introduced the extra
    /// probe read.
    #[test]
    fn stale_reseed_reports_success_when_the_prompt_lands() {
        let (dir, config) = chained_fixture(true);
        let sandbox = dir.path().join("sandbox");
        let config_path = sandbox.join("config.toml");
        let project = &config.projects["sandbox"];
        let seat = &config.accounts["nexus-seat"];
        let fleet = stale_agent_fleet(&sandbox, "w1:p1", "idle", 1);
        let mut herdr = Scripted::new(vec![
            ("pane rename", ok(json!({"result": {}}))),
            ("agent read", readable("ready")),
            ("pane read", readable("ready")),
            ("agent prompt", ok(json!({"result": {}}))),
            (
                "agent get",
                ok(json!({"result": {"agent": {
                    "name": "sandbox-nexu-ne",
                    "pane_id": "w1:p1",
                    "agent_status": "idle",
                    "state_change_seq": 2
                }}})),
            ),
            ("pane read", readable("ready")),
        ]);
        let timing = Timing {
            readiness: Duration::ZERO,
            prompt: Duration::ZERO,
            poll: Duration::ZERO,
        };
        let message = outcome_message(launch_project(
            &mut herdr,
            &config,
            &config_path,
            project,
            &sandbox,
            Some(seat),
            &fleet,
            true,
            false,
            &timing,
        ));
        assert!(
            message.contains("reseeded and re-pointed (sandbox-nexu-ne)"),
            "{message}"
        );
    }
}

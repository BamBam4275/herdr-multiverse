//! `multiverse check`: read-only readiness report (exit 0 ready, 3 needs
//! configuration — never 1; spec-cli.md §5: "`check` returns only
//! `EXIT_NEEDS_CONFIG` or `EXIT_OK` ... never `EXIT_ERROR`, consistent
//! with its read-only, 'report don't fail' contract").
//!
//! The non-verbose, zero-accounts shape below is pinned byte-for-byte by
//! `conformance/scenarios/check-missing-accounts.json`. Live per-project
//! `slots: N open` counts (card `20260929-b-21b5ee`) are pinned by
//! `conformance/scenarios/check-live-open-slots.json` when a routing log
//! still holds an unrecorded task; a config whose every task is already
//! recorded, or that has no log at all, never asks Herdr and prints 0.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::ArgMatches;
use mv_config::load::{Account, Config, FallbackPolicy as ConfigFallbackPolicy, Project};
use mv_config::roles::Role;
use mv_engine::breaker;
use mv_engine::mv_herdr::argv;
use mv_engine::nexus_seed::{self, DispatchableRole, SeedContext, SeedRole};
use mv_engine::runner::{Herdr, ProcessHerdr};
use mv_engine::selector;
use mv_state::tasks::sweep_tasks;

use super::doctor::{project_path, resolve_config_path};
use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::{Failure, NEEDS_CONFIG};

/// One live Herdr agent, narrowed to the two fields [`project_open_agents`]
/// needs: the renamed lease, and the cwd used when that lease is gone.
struct LiveAgentRef<'a> {
    name: Option<&'a str>,
    cwd: Option<&'a str>,
}

/// Whether any configured project's routing log still holds a task that
/// is not yet RECORDED — the same `needs_live` gate Python's `cmd_check`
/// uses before asking Herdr (`cli.py:10638-10669`). A recorded task
/// cannot still be the live agent behind an open count.
fn needs_live_open_counts(config: &Config) -> bool {
    config.projects.values().any(|project| {
        project_path(project)
            .is_some_and(|path| sweep_tasks(path).iter().any(|task| !task.recorded))
    })
}

/// Per-project counts of currently open dispatched agents, matching
/// `runner.project_open_agents` (`runner.py:1386-1452`): a named agent is
/// attributed by exact match against that project's routing-log `agent=`
/// field; an unnamed one (Herdr lease lost) falls back to cwd against
/// each project's `.multiverse/worktrees` tree, then the project root.
/// A name absent from every log is not attributed to any project.
fn project_open_agents(config: &Config, agents: &[LiveAgentRef<'_>]) -> BTreeMap<String, u32> {
    let mut counts: BTreeMap<String, u32> = config
        .projects
        .keys()
        .cloned()
        .map(|name| (name, 0u32))
        .collect();

    let mut logged_names: BTreeMap<String, String> = BTreeMap::new();
    let mut lane_roots: Vec<(String, std::path::PathBuf)> = Vec::new();
    let mut project_roots: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (name, project) in &config.projects {
        let Some(path) = project_path(project) else {
            continue;
        };
        for task in sweep_tasks(path) {
            if let Some(agent) = task.agent {
                logged_names.entry(agent).or_insert_with(|| name.clone());
            }
        }
        lane_roots.push((name.clone(), path.join(".multiverse").join("worktrees")));
        project_roots.push((name.clone(), path.to_path_buf()));
    }

    for agent in agents {
        if let Some(name) = agent.name.filter(|n| !n.is_empty()) {
            if let Some(project) = logged_names.get(name) {
                *counts.entry(project.clone()).or_insert(0) += 1;
            }
            continue;
        }
        let Some(cwd) = agent.cwd.map(Path::new) else {
            continue;
        };
        if let Some((project, _)) = lane_roots.iter().find(|(_, root)| cwd.starts_with(root)) {
            *counts.entry(project.clone()).or_insert(0) += 1;
            continue;
        }
        if let Some((project, _)) = project_roots.iter().find(|(_, root)| cwd == root) {
            *counts.entry(project.clone()).or_insert(0) += 1;
        }
    }
    counts
}

struct OpenSnapshot {
    counts: BTreeMap<String, u32>,
    by_short: BTreeMap<String, u32>,
    panes: BTreeSet<String>,
}

fn any_capped_seat(config: &Config, state_dir: &Path) -> bool {
    config
        .accounts
        .keys()
        .any(|name| mv_engine::seatstate::get_cap(state_dir, name).is_some())
}

fn fetch_live_open_counts(config: &Config) -> Result<OpenSnapshot, String> {
    let mut herdr = ProcessHerdr;
    let result = herdr
        .call(&argv::agent_list(), Duration::from_secs(10))
        .map_err(|error| error.to_string())?;
    if !result.ok() {
        return Err(result.message());
    }
    let empty = Vec::new();
    let agents = result
        .payload
        .as_ref()
        .and_then(|payload| payload["result"]["agents"].as_array())
        .unwrap_or(&empty);
    let refs: Vec<LiveAgentRef<'_>> = agents
        .iter()
        .map(|agent| LiveAgentRef {
            name: agent["name"].as_str().filter(|name| !name.is_empty()),
            cwd: agent["foreground_cwd"]
                .as_str()
                .or_else(|| agent["cwd"].as_str()),
        })
        .collect();
    let mut by_short = BTreeMap::new();
    if let Some(payload) = result.payload.as_ref() {
        for account in config.accounts.values() {
            if let Some(n) = mv_engine::seatstate::open_count_from_payload(payload, &account.short)
            {
                by_short.insert(account.short.clone(), n);
            }
        }
    }
    Ok(OpenSnapshot {
        counts: project_open_agents(config, &refs),
        by_short,
        panes: agents
            .iter()
            .filter_map(|agent| agent["pane_id"].as_str().map(str::to_owned))
            .collect(),
    })
}

/// Cheap, read-only filesystem advisories (`cli.py:10689-10695,10766-10772`).
fn project_advisories(name: &str, root: &Path) -> Vec<String> {
    let mut notes = Vec::new();
    if let Ok(log) = std::fs::read(root.join(".multiverse/routing.log")) {
        let lines = log.split(|byte| *byte == b'\n').count() - usize::from(log.ends_with(b"\n"));
        if lines > 10_000 || log.len() > 2 * 1024 * 1024 {
            notes.push(format!("note: {name}'s routing log is {} lines ({:.1} MB). Nothing is wrong and nothing trims it automatically — there is no compaction command, deliberately. Archive or truncate it by hand when you want it smaller; anything already recorded stays true.", super::accounts::with_commas(lines as u64), log.len() as f64 / 1024.0 / 1024.0));
        }
    }
    if root.join("Dockerfile").is_file() {
        let ignore = std::fs::read_to_string(root.join(".dockerignore")).unwrap_or_default();
        let mut excluded = false;
        for pattern in ignore
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let (negate, pattern) = pattern
                .strip_prefix('!')
                .map_or((false, pattern), |pattern| (true, pattern));
            if matches!(
                pattern.trim_matches('/'),
                ".multiverse"
                    | ".multiverse/**"
                    | "**/.multiverse"
                    | "**/.multiverse/**"
                    | "*"
                    | "**"
            ) {
                excluded = !negate;
            }
        }
        if !excluded {
            notes.push(format!("warning: {name} builds a docker image from its root without excluding .multiverse in .dockerignore — every lane's worktree (each with its own node_modules) ships as build context. add .multiverse to {}/.dockerignore.", root.display()));
        }
    }
    notes
}

/// Count the actual prune preview outcomes; never mutate lanes or refs.
fn prunable_count(root: &Path) -> Result<usize, String> {
    use mv_state::worktrees::{BranchOutcome, HerdrLaneObserver, PruneOutcome, Worktrees};
    let worktrees = Worktrees::new(root).map_err(|error| error.to_string())?;
    let mut observer = HerdrLaneObserver::new(root);
    let lanes = worktrees
        .prune(&mut observer, false, true)
        .map_err(|error| error.to_string())?;
    let branches = worktrees
        .prune_orphans(true)
        .map_err(|error| error.to_string())?;
    Ok(lanes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome,
                PruneOutcome::WouldRemove { .. } | PruneOutcome::WouldArchive { .. }
            )
        })
        .count()
        + branches
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    BranchOutcome::Reclaimed { .. } | BranchOutcome::Archived { .. }
                )
            })
            .count())
}

/// Bound the host probe so default `check` cannot hang behind pmset.
fn idle_sleep_warning() -> Option<String> {
    if !cfg!(target_os = "macos") && std::env::var_os("MULTIVERSE_PMSET_BIN").is_none() {
        return None;
    }
    let command = argv::HerdrCommand {
        argv: vec![
            mv_engine::hostsleep::pmset_binary(),
            "-g".into(),
            "custom".into(),
        ],
        purpose: "read idle-sleep settings".into(),
        stdin: None,
        expects_json: false,
    };
    let output = mv_engine::mv_herdr::run::run(&command, Some(Duration::from_secs(2))).ok()?;
    if !output.ok() {
        return None;
    }
    let mut ac = true;
    for line in output.stdout.lines() {
        let line = line.trim();
        if line.ends_with(':') {
            if line.starts_with("Battery Power") {
                ac = false;
            }
            if line.starts_with("AC Power") {
                ac = true;
            }
        }
        let mut words = line.split_whitespace();
        if ac && words.next() == Some("sleep") {
            let minutes = words.next()?.parse::<u32>().ok()?;
            if minutes > 0 {
                return Some(format!("warning: this Mac's AC-power idle-sleep is {minutes} minute(s); background work may pause when the host sleeps. Set AC-power sleep to 0 for an always-on fleet."));
            }
        }
    }
    None
}

/// `cli.py` `_CHECK_LINE_WIDTH`: clip leading problem lines to one 100-col row.
const CHECK_LINE_WIDTH: usize = 100;
/// `cli.py` `TMP_WARN_KB`: `$TMPDIR` + `/private/tmp` together, in KB.
const TMP_WARN_KB: u64 = 10 * 1024 * 1024;
/// Whole scratch scan budget (`cli.py` `TMP_SCRATCH_TIMEOUT_SECONDS`).
const TMP_SCRATCH_TIMEOUT: Duration = Duration::from_secs(2);
/// Shared top-level names before a `tmp.*` dir counts as a project copy.
const TMP_SIGNATURE_MIN_SHARED: usize = 5;

fn clip_check_line(line: &str) -> String {
    if line.chars().count() <= CHECK_LINE_WIDTH {
        return line.to_string();
    }
    let clipped: String = line.chars().take(CHECK_LINE_WIDTH - 1).collect();
    format!("{clipped}…")
}

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

/// Live fence status without persisting a half-open trial (`check` is read-only).
struct CheckBreaker<'a> {
    config_dir: &'a Path,
}

impl selector::BreakerQuery for CheckBreaker<'_> {
    fn status(&self, pool: &str) -> selector::BreakerStatus {
        let config_dir = self.config_dir;
        let query = breaker::StoreBreakerQuery {
            config_dir,
            clock: breaker::ClockBridge::now(),
            gauge_for_pool: move |pool: &str| {
                mv_state::gauge::load(config_dir)
                    .get(pool)
                    .and_then(breaker::gauge_reading_from_entry)
            },
            is_pane_live: |_pane: &str| true,
        };
        query.status(pool)
    }

    fn claim_trial(&self, _pool: &str, _task_id: &str, token: Option<&str>) -> Option<String> {
        Some(token.unwrap_or("check-readonly").to_string())
    }
}

/// Convert a still-future `unavailable_until` wall time into a monotonic
/// [`Instant`] the selector can compare against `now`. A past timestamp
/// yields `None` so check 2 does not keep fencing a seat that has already
/// returned.
fn unavailable_until_instant(raw: Option<&str>) -> Option<Instant> {
    let until = raw.and_then(breaker::parse_stored_timestamp)?;
    let now = mv_core::time::now_utc();
    if until <= now {
        return None;
    }
    let secs = u64::try_from((until - now).whole_seconds().max(0)).unwrap_or(0);
    Some(Instant::now() + Duration::from_secs(secs))
}

fn check_seat(
    account: &Account,
    config: &Config,
    role: Role,
    state_dir: &Path,
    live_open: Option<&BTreeMap<String, u32>>,
    gauges: &mv_state::gauge::GaugeMap,
    local_liveness_probe: &dyn selector::LocalLivenessProbe,
) -> selector::Seat {
    let has_model = config
        .roles
        .get(&role)
        .map(|settings| settings.models.contains_key(&account.kind))
        .unwrap_or(true);
    let cap = mv_engine::seatstate::get_cap(state_dir, &account.name);
    let open_count =
        cap.and_then(|_| live_open.map(|counts| counts.get(&account.short).copied().unwrap_or(0)));
    let gauge = gauges.get(&account.pool);
    selector::Seat {
        name: account.name.clone(),
        vendor: account.vendor.clone(),
        pool: account.pool.clone(),
        kind: account.kind.clone(),
        available: account.available,
        unavailable_reason: account.unavailable_reason.clone(),
        unavailable_until: unavailable_until_instant(account.unavailable_until.as_deref()),
        lent: mv_engine::seatstate::is_lent(state_dir, &account.name),
        has_model,
        roles_allowed: account
            .roles_allowed
            .as_ref()
            .map(|roles| roles.iter().map(|r| r.config_key().to_string()).collect()),
        max_brief_chars: account.max_brief_chars.map(|v| v as usize),
        has_command: !account.command.trim().is_empty(),
        cap,
        open_count,
        quota_primary_percent: gauge.and_then(|entry| entry.percent_left),
        quota_secondary_percent: gauge.and_then(|entry| entry.percent_left_secondary),
        local_alive: (account.vendor == "local")
            .then(|| local_liveness_probe.probe(&account.name, &account.health_url)),
    }
}

fn shadowed_global_chain(config_path: &Path, project: &str, role: Role) -> Option<Vec<String>> {
    let text = std::fs::read_to_string(config_path).ok()?;
    let doc = text.parse::<toml_edit::DocumentMut>().ok()?;
    let array = doc
        .get("projects")?
        .as_table()?
        .get(project)?
        .as_table()?
        .get("chains")?
        .as_table()?
        .get(role.config_key())?
        .as_array()?;
    let chain: Vec<String> = array
        .iter()
        .filter_map(|item| item.as_str().map(str::to_owned))
        .collect();
    (!chain.is_empty()).then_some(chain)
}

/// Non-primary and unresolved chain lines (`cli.py` `_check_chain_rows`).
fn check_chain_rows(
    config: &Config,
    config_path: &Path,
    state_dir: &Path,
    live_open: Option<&BTreeMap<String, u32>>,
    gauges: &mv_state::gauge::GaugeMap,
    local_liveness_probe: &dyn selector::LocalLivenessProbe,
) -> (Vec<String>, bool) {
    let breaker = CheckBreaker {
        config_dir: state_dir,
    };
    let now = Instant::now();
    let mut rows = Vec::new();
    let mut unresolved_chain = false;
    for project in config.projects.values() {
        let roster = project_path(project).and_then(|path| {
            mv_config::roster::load_roster(&path.join(".multiverse").join("roster.toml"))
                .ok()
                .flatten()
        });
        let mut roles: Vec<Role> = project.chains.keys().copied().collect();
        roles.sort_by_key(|role| role.config_key());
        for role in roles {
            let chain = &project.chains[&role];
            let seats: Vec<selector::Seat> = chain
                .iter()
                .filter_map(|name| {
                    config.accounts.get(name).map(|account| {
                        check_seat(
                            account,
                            config,
                            role,
                            state_dir,
                            live_open,
                            gauges,
                            local_liveness_probe,
                        )
                    })
                })
                .collect();
            let seats = selector::reorder_chain_by_preference(&seats, |name| {
                mv_engine::seatstate::is_preferred(state_dir, name)
            });
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
                task_id: "check",
                trial_token: None,
            };
            let resolved = match selector::select(&seats, &params, &breaker, now) {
                Ok(selection) => {
                    if !selection.is_fallback() {
                        continue;
                    }
                    format!("→ {} (chain-fallback)", selection.seat.name)
                }
                Err(_) => {
                    unresolved_chain = true;
                    "→ UNRESOLVED: ChainExhaustedError".to_string()
                }
            };
            let local = if mv_config::roster::is_locally_overridden(roster.as_ref(), role) {
                "  (local)"
            } else {
                ""
            };
            let line = format!(
                "  {}/{}: {}  {resolved}{local}",
                project.name,
                role.display_name(),
                chain.join(" → ")
            );
            rows.push(clip_check_line(&line));
            if mv_config::roster::is_locally_overridden(roster.as_ref(), role) {
                if let Some(global) = shadowed_global_chain(config_path, &project.name, role) {
                    rows.push(clip_check_line(&format!(
                        "      shadows global: {}",
                        global.join(" → ")
                    )));
                }
            }
        }
    }
    (rows, unresolved_chain)
}

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

/// Seed-time seat: config fields only, matching `start.rs` so staleness
/// compares against the same digest `start` wrote.
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
        local_alive: None,
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

fn seed_review_policy_summary(policy: &mv_config::load::ReviewPolicy) -> String {
    super::review_policy::format_policy(policy)
        .lines()
        .map(|line| format!("- {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn stale_seed_note(
    config: &Config,
    config_path: &Path,
    project: &Project,
    root: &Path,
) -> Option<String> {
    let seed_path =
        nexus_seed::existing_seed_path(root).unwrap_or_else(|| nexus_seed::seed_path(root));
    let state_dir = mv_config::load::config_state_dir_from_env(config_path);
    let directives_root = mv_engine::directives::directives_root(&state_dir);
    let handoff = nexus_seed::resolve_handoff(root);
    let role_prompt = mv_engine::prompts::role_prompt("Nexus")
        .ok()
        .map(|prompt| prompt.body());
    let roles = dispatchable_roles(config, project);
    let review_policy = seed_review_policy_summary(&config.review_policy);
    let context = SeedContext {
        project: &project.name,
        repository: root,
        handoff,
        directives_root: &directives_root,
        roles: &roles,
        review_policy: &review_policy,
        max_concurrent: config.max_concurrent as usize,
        dispatch_min_quota_percent: config.dispatch_min_quota_percent.min(100) as u8,
        lend_interactive_to_workers: config.lend_interactive_to_workers,
        role_prompt: role_prompt.as_deref(),
    };
    match nexus_seed::seed_is_stale(&seed_path, &context) {
        Ok(Some(true)) => Some(format!(
            "note: {}'s Nexus seed is stale — its dispatch \
             table or role prompt has changed since it was written. \
             `multiverse stop {}` then `multiverse start {}` \
             regenerates it; `start` alone will not, because it is \
             idempotent by label and skips a workspace already open. \
             Nothing does this automatically.",
            project.name, project.name, project.name
        )),
        Ok(_) => None,
        Err(error) => Some(format!(
            "warning: {}: cannot read Nexus seed {}: {error}",
            project.name,
            seed_path.display()
        )),
    }
}

fn tmp_top_level_names(path: &Path) -> BTreeSet<String> {
    std::fs::read_dir(path)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default()
}

fn tmp_star_dirs(location: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(location) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("tmp."))
        })
        .collect();
    dirs.sort();
    dirs
}

fn du_size_kb(path: &Path, timeout: Duration) -> Option<u64> {
    if !path.is_dir() {
        return Some(0);
    }
    let command = argv::HerdrCommand {
        argv: vec!["du".into(), "-sk".into(), path.display().to_string()],
        purpose: "size scratch".into(),
        stdin: None,
        expects_json: false,
    };
    let output = mv_engine::mv_herdr::run::run(&command, Some(timeout)).ok()?;
    let first = output.stdout.split_whitespace().next()?;
    Some(first.parse().unwrap_or(0))
}

fn du_size_kb_many(paths: &[PathBuf], timeout: Duration) -> Option<BTreeMap<PathBuf, u64>> {
    if paths.is_empty() {
        return Some(BTreeMap::new());
    }
    let mut argv = vec!["du".into(), "-sk".into()];
    argv.extend(paths.iter().map(|path| path.display().to_string()));
    let command = argv::HerdrCommand {
        argv,
        purpose: "size scratch copies".into(),
        stdin: None,
        expects_json: false,
    };
    let output = mv_engine::mv_herdr::run::run(&command, Some(timeout)).ok()?;
    let mut sizes = BTreeMap::new();
    for line in output.stdout.lines() {
        let mut fields = line.splitn(2, char::is_whitespace);
        let Some(kb) = fields.next().and_then(|value| value.parse().ok()) else {
            continue;
        };
        let Some(path) = fields.next().map(str::trim) else {
            continue;
        };
        sizes.insert(PathBuf::from(path), kb);
    }
    for path in paths {
        sizes.entry(path.clone()).or_insert(0);
    }
    Some(sizes)
}

/// `cli.py` `tmp_scratch_warning`: oversized `$TMPDIR`/`/private/tmp` copies.
fn tmp_scratch_warning_at(
    config: &Config,
    tmpdir: Option<&Path>,
    private_tmp: Option<&Path>,
    warn_kb: u64,
) -> Option<String> {
    let mut locations = Vec::new();
    for location in [tmpdir, private_tmp].into_iter().flatten() {
        if !locations.contains(&location) {
            locations.push(location);
        }
    }
    if locations.is_empty() {
        return None;
    }
    let where_text = locations
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(" and ");
    let deadline = Instant::now() + TMP_SCRATCH_TIMEOUT;
    let mut total_kb = 0u64;
    for location in &locations {
        if !location.is_dir() {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Some(format!(
                "{where_text} scratch size is unknown (`du` timed out) — check by hand."
            ));
        }
        match du_size_kb(location, remaining.min(TMP_SCRATCH_TIMEOUT)) {
            Some(size) => total_kb += size,
            None => {
                return Some(format!(
                    "{where_text} scratch size is unknown (`du` timed out) — check by hand."
                ));
            }
        }
    }
    if total_kb < warn_kb {
        return None;
    }

    let mut star_dirs = Vec::new();
    for location in &locations {
        star_dirs.extend(tmp_star_dirs(location));
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Some(format!(
            "{where_text} scratch size is unknown (`du` timed out) — check by hand."
        ));
    }
    let star_sizes = match du_size_kb_many(&star_dirs, remaining.min(TMP_SCRATCH_TIMEOUT)) {
        Some(sizes) => sizes,
        None => {
            return Some(format!(
                "{where_text} scratch size is unknown (`du` timed out) — check by hand."
            ));
        }
    };
    let star_size_kb: u64 = star_sizes.values().sum();
    let star_size_text = format!("{:.1} GB", star_size_kb as f64 / (1024.0 * 1024.0));
    let total_gb = total_kb as f64 / (1024.0 * 1024.0);

    let mut groups: BTreeMap<BTreeSet<String>, Vec<PathBuf>> = BTreeMap::new();
    for star_dir in &star_dirs {
        groups
            .entry(tmp_top_level_names(star_dir))
            .or_default()
            .push(star_dir.clone());
    }
    let mut biggest_signature = BTreeSet::new();
    let mut biggest_members: Vec<PathBuf> = Vec::new();
    let mut biggest_group_kb = -1i64;
    for (signature, members) in &groups {
        let group_kb: u64 = members
            .iter()
            .map(|member| star_sizes.get(member).copied().unwrap_or(0))
            .sum();
        if group_kb as i64 > biggest_group_kb {
            biggest_group_kb = group_kb as i64;
            biggest_signature = signature.clone();
            biggest_members = members.clone();
        }
    }

    let mut best_project = None;
    let mut best_overlap = TMP_SIGNATURE_MIN_SHARED.saturating_sub(1);
    for project in config.projects.values() {
        let Some(root) = project_path(project) else {
            continue;
        };
        let overlap = biggest_signature
            .intersection(&tmp_top_level_names(root))
            .count();
        if overlap > best_overlap {
            best_overlap = overlap;
            best_project = Some(project);
        }
    }
    let Some(best_project) = best_project else {
        return Some(format!(
            "{where_text} together hold {total_gb:.1} GB — {} \
             tmp.* dir(s) ({star_size_text}). No configured project's root \
             shares {TMP_SIGNATURE_MIN_SHARED}+ top-level entries with the \
             largest group, so there's no safe marker file for a targeted \
             cleanup command.",
            star_dirs.len()
        ));
    };
    let best_root = project_path(best_project)?;
    let project_names = tmp_top_level_names(best_root);
    let shared: Vec<String> = biggest_signature
        .intersection(&project_names)
        .cloned()
        .collect();
    let mut elsewhere = BTreeSet::new();
    for project in config.projects.values() {
        if project.name == best_project.name {
            continue;
        }
        if let Some(root) = project_path(project) {
            elsewhere.extend(tmp_top_level_names(root));
        }
    }
    let marker = shared
        .iter()
        .find(|name| !elsewhere.contains(*name) && best_root.join(name).is_file());
    let Some(marker) = marker else {
        return Some(format!(
            "{where_text} together hold {total_gb:.1} GB — {} \
             tmp.* dir(s) ({star_size_text}), mostly {} \
             copies ({} of them). No file at \
             {}'s root is unique to it among configured \
             projects, so there's no safe marker for a cleanup command; \
             clear them by hand.",
            star_dirs.len(),
            best_project.name,
            biggest_members.len(),
            best_project.name
        ));
    };
    let mut parent_counts: BTreeMap<PathBuf, usize> = BTreeMap::new();
    for member in &biggest_members {
        if let Some(parent) = member.parent() {
            *parent_counts.entry(parent.to_path_buf()).or_insert(0) += 1;
        }
    }
    let target_dir = parent_counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(path, _)| path)
        .unwrap_or_else(|| biggest_members[0].clone());
    let find_cmd = format!(
        "find {} -maxdepth 1 -name 'tmp.*' -mmin +60 -exec sh -c \
         'test -f \"$1/{marker}\"' _ {{}} \\; -exec rm -rf {{}} +",
        target_dir.display()
    );
    Some(format!(
        "{where_text} together hold {total_gb:.1} GB — {} tmp.* \
         dir(s) ({star_size_text}), mostly {} copies \
         ({} of them). Clear stale ones: {find_cmd}",
        star_dirs.len(),
        best_project.name,
        biggest_members.len()
    ))
}

fn tmp_scratch_warning(config: &Config) -> Option<String> {
    let tmpdir = std::env::var_os("TMPDIR").map(PathBuf::from);
    tmp_scratch_warning_at(
        config,
        tmpdir.as_deref(),
        Some(Path::new("/private/tmp")),
        TMP_WARN_KB,
    )
}

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

    let verbose = args.get_flag("verbose");
    let dispatch_only = args.get_flag("dispatch-only");

    // Resolved up front — cheap, a pure path computation — so the first
    // block can name it alongside the config path: otherwise a
    // Herdr-managed install has no `herdr plugin config-dir`-style lookup
    // for the fleet state directory at all (this task's `result.md`: the
    // cutover runbook's gap item 3).
    let state_dir = mv_config::load::config_state_dir_from_env(&config_path);

    let mut blocks: Vec<String> = Vec::new();
    blocks.push(format!(
        "config: {}\nstate directory: {}\naccounts: {}   projects: {}   roles: {}   max_concurrent: {}",
        config_path.display(),
        state_dir.display(),
        config.accounts.len(),
        config.projects.len(),
        config.roles.len(),
        config.max_concurrent,
    ));

    let mut unresolved = config.accounts.is_empty() || config.projects.is_empty();
    if config.accounts.is_empty() {
        blocks.push(
            "No accounts configured. Add at least one [accounts.<name>] table to the config."
                .to_string(),
        );
    }

    if config.projects.is_empty() {
        blocks.push(
            "No projects configured. Add at least one [projects.<name>] table to the config."
                .into(),
        );
        blocks.push("chains:\n  (no projects configured)".into());
    }
    let gauges = mv_state::gauge::load(&state_dir);
    // One `agent list` for seat caps, chain select, and per-project open
    // slots — Python's cmd_check uses AccountSelector's cached listing for
    // caps plus a second listing for project_open_agents when a log still
    // holds an unrecorded task. Sharing one listing never exceeds that.
    let needs_listing = needs_live_open_counts(&config) || any_capped_seat(&config, &state_dir);
    let mut live_panes = None;
    let (open_counts, live_by_short) = if needs_listing {
        match fetch_live_open_counts(&config) {
            Ok(snapshot) => {
                live_panes = Some(snapshot.panes);
                (Some(snapshot.counts), Some(snapshot.by_short))
            }
            Err(message) => {
                blocks.push(format!("ERROR: live Herdr state unavailable: {message}"));
                unresolved = true;
                (None, None)
            }
        }
    } else {
        (
            Some(
                config
                    .projects
                    .keys()
                    .cloned()
                    .map(|name| (name, 0u32))
                    .collect(),
            ),
            Some(BTreeMap::new()),
        )
    };
    for account in config.accounts.values() {
        let count = mv_engine::seatstate::get_cap(&state_dir, &account.name).and_then(|_| {
            live_by_short
                .as_ref()
                .map(|counts| counts.get(&account.short).copied().unwrap_or(0))
        });
        let state = super::accounts::account_state(
            &config,
            &state_dir,
            &gauges,
            account,
            count,
            &selector::HttpLocalLivenessProbe,
        );
        if state.starts_with("FENCED")
            || state.starts_with("DRAINING")
            || state.starts_with("HALF-OPEN")
        {
            blocks.push(format!("{}: {state}", account.name));
        }
    }
    let mut placeholders = Vec::new();
    for account in config.accounts.values() {
        if account.config_dir.is_placeholder() {
            placeholders.push(format!(
                "accounts.{}.config_dir = {}",
                account.name,
                account.config_dir.as_raw()
            ));
        }
        if let Some(model) = account
            .model
            .as_deref()
            .filter(|model| mv_config::placeholder::is_placeholder(model))
        {
            placeholders.push(format!("accounts.{}.model = {model}", account.name));
        }
        if account.kind == "claude" && account.command.trim().is_empty() {
            blocks.push(format!("note: account {} is a Claude seat without the required command wrapper. Set accounts.{}.command to the wrapper from docs/install.md before dispatching.", account.name, account.name));
        }
    }
    for (role, settings) in &config.roles {
        for (kind, model) in &settings.models {
            if mv_config::placeholder::is_placeholder(model) {
                placeholders.push(format!(
                    "roles.{}.models.{kind} = {model}",
                    role.config_key()
                ));
            }
        }
    }
    let (chain_rows, unresolved_chain) = check_chain_rows(
        &config,
        &config_path,
        &state_dir,
        live_by_short.as_ref(),
        &gauges,
        &selector::HttpLocalLivenessProbe,
    );
    if unresolved_chain {
        unresolved = true;
    }
    if !config.projects.is_empty() && !chain_rows.is_empty() {
        let mut block = vec!["chains:".to_string()];
        block.extend(chain_rows);
        blocks.push(block.join("\n"));
    }
    if !placeholders.is_empty() {
        unresolved = true;
        blocks.push(format!("unfilled placeholders ({}):\n  {}\n\nMultiverse will not guess these (spec §0). Dry-runs work; real dispatch is blocked until they are set.", placeholders.len(), placeholders.join("\n  ")));
    }

    let projects: Vec<&Project> = config.projects.values().collect();
    // Per-project open/floor/ceiling reuse the listing fetched above.

    let mut project_lines = vec!["projects:".to_string()];
    if projects.is_empty() {
        project_lines.push("  (none configured)".into());
    }
    for project in &projects {
        let short = match &project.short {
            Some(value) => format!("short={value}"),
            None => "short=(none \u{2014} derived slug)".to_string(),
        };
        let ceiling = project.max_slots.unwrap_or(config.max_concurrent);
        let open_display = match &open_counts {
            None => "?".to_string(),
            Some(counts) => counts.get(&project.name).copied().unwrap_or(0).to_string(),
        };
        project_lines.push(format!(
            "  {}: {short}  slots: {open_display} open, floor {}, ceiling {ceiling}",
            project.name, project.min_slots
        ));
        if project_path(project).is_none() {
            unresolved = true;
        }
    }
    blocks.push(project_lines.join("\n"));
    for project in &projects {
        let Some(root) = project_path(project) else {
            continue;
        };
        blocks.extend(project_advisories(&project.name, root));
        let tasks = sweep_tasks(root);
        let unrecorded = tasks
            .iter()
            .filter(|task| !task.recorded && task.file_status.is_some())
            .count();
        if unrecorded > 0 {
            blocks.push(format!("note: {} has {unrecorded} unrecorded outcome(s) — task(s) that finished after their dispatch gave up. `multiverse tasks {} --reconcile` records them, or run `multiverse sweep` to reconcile it.", project.name, project.name));
        }
        if let Some(panes) = &live_panes {
            let awaiting = tasks
                .iter()
                .filter(|task| {
                    !task.recorded
                        && task.file_status.is_none()
                        && task.pane.as_ref().is_some_and(|pane| !panes.contains(pane))
                })
                .count();
            if awaiting > 0 {
                blocks.push(format!("note: {} has {awaiting} task(s) awaiting with no live agent, still inside their {}m lease. `multiverse tasks {} --close-stranded` classifies one right now; left alone, `sweep` records it `failed` once the lease expires.", project.name, config.awaiting_lease_minutes, project.name));
            }
        }
    }

    if verbose {
        for project in &projects {
            let Some(root) = project_path(project) else {
                continue;
            };
            match prunable_count(root) {
                Ok(count) if count > 0 => blocks.push(format!("note: {} has {count} prunable lane(s)/branch(es). Preview with `multiverse worktrees {} --prune --dry-run`; `multiverse worktrees {} --prune` would reclaim, archive, or remove them — nothing does this automatically.", project.name, root.display(), root.display())),
                Err(error) => blocks.push(format!("advisory scan: {} worktree pile check unavailable: {error}", project.name)),
                _ => {},
            }
        }
        for project in &projects {
            let Some(root) = project_path(project) else {
                continue;
            };
            if let Some(note) = stale_seed_note(&config, &config_path, project, root) {
                blocks.push(note);
            }
        }
        if let Some(warning) = tmp_scratch_warning(&config) {
            blocks.push(format!("warning: {warning}"));
        }
        blocks.push(format!(
            "review policy:\n  {}",
            super::review_policy::format_policy(&config.review_policy).replace('\n', "\n  ")
        ));
    } else if !projects.is_empty() {
        blocks.push("advisory scan: worktree pile check skipped; rerun `multiverse check --verbose` for details.".to_string());
        blocks.push(
            "advisory scans skipped; use `multiverse check --verbose` for details.".to_string(),
        );
    }
    if placeholders.is_empty() {
        blocks.push("no unfilled placeholders.".to_string());
    }

    let inside_pane = std::env::var("HERDR_PANE_ID")
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    if !inside_pane {
        if dispatch_only {
            unresolved = true;
        }
        blocks.push(
            "warning: NOT inside a Herdr pane (HERDR_PANE_ID is unset).\n  \
             `dispatch` would fail here \u{2014} it splits the pane it runs in.\n  \
             `multiverse start` works from anywhere; run dispatches from a project's pane."
                .to_string(),
        );
    }

    if let Some(warning) = idle_sleep_warning() {
        blocks.push(warning);
    }

    println!("{}", blocks.join("\n\n"));

    if unresolved {
        Err(Failure::Reported(NEEDS_CONFIG))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mv_config::load::load_config_str;
    use std::fs;

    fn write_unrecorded(root: &Path, agent: &str) {
        let state = root.join(".multiverse");
        fs::create_dir_all(&state).unwrap();
        fs::write(
            state.join("routing.log"),
            format!(
                "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-builder-aaaaaa\t\
                 project=sandbox\trole=builder\taccount=seat\tpool=p\toutcome=chain-primary\t\
                 reason=\tagent={agent}\tbranch=main\n\
                 2020-01-01T00:05:00+00:00\tevent=started\ttask=20250101-builder-aaaaaa\t\
                 pane=w1:p1\n"
            ),
        )
        .unwrap();
    }

    fn two_project_config(sandbox: &Path, other: &Path) -> Config {
        let toml = format!(
            "[accounts.seat]\n\
             kind = \"codex\"\n\
             short = \"st\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:t\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\n\
             [projects.other]\npath = \"{}\"\n",
            sandbox.display(),
            other.display(),
        );
        load_config_str(&toml, Path::new("config.toml")).unwrap()
    }

    #[test]
    fn needs_live_is_false_when_no_task_log_exists() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let other = dir.path().join("other");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&other).unwrap();
        let config = two_project_config(&sandbox, &other);
        assert!(!needs_live_open_counts(&config));
    }

    #[test]
    fn needs_live_is_true_when_a_task_is_unrecorded() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let other = dir.path().join("other");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&other).unwrap();
        write_unrecorded(&sandbox, "sandbox-buil-st");
        let config = two_project_config(&sandbox, &other);
        assert!(needs_live_open_counts(&config));
    }

    #[test]
    fn project_open_agents_attributes_a_named_live_agent_from_the_routing_log() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let other = dir.path().join("other");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&other).unwrap();
        write_unrecorded(&sandbox, "sandbox-buil-st");
        let config = two_project_config(&sandbox, &other);
        let agents = [LiveAgentRef {
            name: Some("sandbox-buil-st"),
            cwd: None,
        }];
        let counts = project_open_agents(&config, &agents);
        assert_eq!(counts.get("sandbox").copied(), Some(1));
        assert_eq!(counts.get("other").copied(), Some(0));
    }

    #[test]
    fn project_open_agents_ignores_a_live_name_absent_from_every_log() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let other = dir.path().join("other");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&other).unwrap();
        write_unrecorded(&sandbox, "sandbox-buil-st");
        let config = two_project_config(&sandbox, &other);
        let agents = [LiveAgentRef {
            name: Some("hand-renamed"),
            cwd: Some(sandbox.to_str().unwrap()),
        }];
        let counts = project_open_agents(&config, &agents);
        assert_eq!(counts.get("sandbox").copied(), Some(0));
    }

    #[test]
    fn project_open_agents_attributes_an_unnamed_agent_by_project_root_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let other = dir.path().join("other");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&other).unwrap();
        let config = two_project_config(&sandbox, &other);
        let cwd = sandbox.to_str().unwrap().to_string();
        let agents = [LiveAgentRef {
            name: None,
            cwd: Some(&cwd),
        }];
        let counts = project_open_agents(&config, &agents);
        assert_eq!(counts.get("sandbox").copied(), Some(1));
        assert_eq!(counts.get("other").copied(), Some(0));
    }

    #[test]
    fn clip_check_line_keeps_short_lines_and_ellipsis_long_ones() {
        assert_eq!(clip_check_line("short"), "short");
        let long = "x".repeat(120);
        let clipped = clip_check_line(&long);
        assert_eq!(clipped.chars().count(), CHECK_LINE_WIDTH);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn check_chain_rows_marks_a_claude_seat_without_command_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"claude\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"claude:primary\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             claude = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let gauges = mv_state::gauge::GaugeMap::new();
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            None,
            &gauges,
            &selector::HttpLocalLivenessProbe,
        );
        assert!(unresolved);
        assert!(
            rows.iter()
                .any(|row| row.contains("UNRESOLVED: ChainExhaustedError")),
            "{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("sandbox/Scout:") && row.contains("primary")),
            "{rows:?}"
        );
    }

    #[test]
    fn check_chain_rows_marks_an_at_cap_seat_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"codex\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:primary\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             codex = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        mv_engine::seatstate::set_cap(dir.path(), "primary", Some(1)).unwrap();
        let mut live = BTreeMap::new();
        live.insert("pr".to_string(), 1);
        let gauges = mv_state::gauge::GaugeMap::new();
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            Some(&live),
            &gauges,
            &selector::HttpLocalLivenessProbe,
        );
        assert!(unresolved, "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("UNRESOLVED: ChainExhaustedError")),
            "{rows:?}"
        );
    }

    #[test]
    fn check_chain_rows_stays_quiet_when_a_capped_seat_is_under_cap() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"codex\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:primary\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             codex = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        mv_engine::seatstate::set_cap(dir.path(), "primary", Some(1)).unwrap();
        let mut live = BTreeMap::new();
        live.insert("pr".to_string(), 0);
        let gauges = mv_state::gauge::GaugeMap::new();
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            Some(&live),
            &gauges,
            &selector::HttpLocalLivenessProbe,
        );
        assert!(!unresolved, "{rows:?}");
        assert!(
            rows.is_empty(),
            "primary-quiet chain should omit the row: {rows:?}"
        );
    }

    #[test]
    fn check_chain_rows_marks_a_quota_drained_seat_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "dispatch_min_quota_percent = 20\n\
             [accounts.primary]\n\
             kind = \"codex\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:primary\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             codex = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let mut gauges = mv_state::gauge::GaugeMap::new();
        gauges
            .entry("codex:primary".to_string())
            .or_default()
            .percent_left = Some(5.0);
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            None,
            &gauges,
            &selector::HttpLocalLivenessProbe,
        );
        assert!(unresolved, "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("UNRESOLVED: ChainExhaustedError")),
            "{rows:?}"
        );
    }

    /// A scripted [`selector::LocalLivenessProbe`] — card `20261106-b-255666`
    /// tests never make a real GET, mirroring how `Scripted` stands in for
    /// Herdr elsewhere in this crate.
    struct FakeLocalProbe(Result<(), String>);

    impl selector::LocalLivenessProbe for FakeLocalProbe {
        fn probe(&self, _account_name: &str, _url: &str) -> Result<(), String> {
            self.0.clone()
        }
    }

    #[test]
    fn check_chain_rows_marks_a_dead_local_seat_unresolved() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"local\"\n\
             vendor = \"local\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"local:primary\"\n\
             health_url = \"http://127.0.0.1:1234/v1/models\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             local = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let gauges = mv_state::gauge::GaugeMap::new();
        let probe = FakeLocalProbe(Err(
            "local seat health check failed at http://127.0.0.1:1234/v1/models (timed out)"
                .to_string(),
        ));
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            None,
            &gauges,
            &probe,
        );
        assert!(unresolved, "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("UNRESOLVED: ChainExhaustedError")),
            "{rows:?}"
        );
    }

    #[test]
    fn check_chain_rows_falls_back_past_a_dead_local_seat_to_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"local\"\n\
             vendor = \"local\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"local:primary\"\n\
             health_url = \"http://127.0.0.1:1234/v1/models\"\n\n\
             [accounts.fallback]\n\
             kind = \"codex\"\n\
             short = \"fb\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:fallback\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             local = \"m\"\n\
             codex = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\", \"fallback\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let gauges = mv_state::gauge::GaugeMap::new();
        let probe = FakeLocalProbe(Err(
            "local seat health check failed at http://127.0.0.1:1234/v1/models (timed out)"
                .to_string(),
        ));
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            None,
            &gauges,
            &probe,
        );
        assert!(!unresolved, "{rows:?}");
        assert!(
            rows.iter()
                .any(|row| row.contains("→ fallback (chain-fallback)")),
            "{rows:?}"
        );
    }

    #[test]
    fn check_chain_rows_stays_quiet_when_the_local_seat_is_actually_alive() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        fs::create_dir_all(&sandbox).unwrap();
        let toml = format!(
            "[accounts.primary]\n\
             kind = \"local\"\n\
             vendor = \"local\"\n\
             short = \"pr\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"local:primary\"\n\
             health_url = \"http://127.0.0.1:1234/v1/models\"\n\n\
             [roles.scout]\n\
             [roles.scout.models]\n\
             local = \"m\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n\
             [projects.sandbox.chains]\n\
             scout = [\"primary\"]\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let gauges = mv_state::gauge::GaugeMap::new();
        let probe = FakeLocalProbe(Ok(()));
        let (rows, unresolved) = check_chain_rows(
            &config,
            Path::new("config.toml"),
            dir.path(),
            None,
            &gauges,
            &probe,
        );
        assert!(!unresolved, "{rows:?}");
        assert!(rows.is_empty(), "{rows:?}");
    }

    #[test]
    fn tmp_scratch_warning_names_a_project_copy_above_the_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let sandbox = dir.path().join("sandbox");
        let tmpdir = dir.path().join("tmpdir");
        let private_tmp = dir.path().join("private");
        fs::create_dir_all(&sandbox).unwrap();
        fs::create_dir_all(&tmpdir).unwrap();
        fs::create_dir_all(&private_tmp).unwrap();
        for name in ["CHARTER.md", "README.md", "Cargo.toml", "src", "docs"] {
            let path = sandbox.join(name);
            if name.contains('.') {
                fs::write(&path, "marker\n").unwrap();
            } else {
                fs::create_dir(&path).unwrap();
            }
        }
        let leak = tmpdir.join("tmp.AAAAAA");
        fs::create_dir(&leak).unwrap();
        for name in ["CHARTER.md", "README.md", "Cargo.toml", "src", "docs"] {
            let path = leak.join(name);
            if name.contains('.') {
                fs::write(&path, "copy\n").unwrap();
            } else {
                fs::create_dir(&path).unwrap();
            }
        }
        let toml = format!(
            "[accounts.seat]\n\
             kind = \"codex\"\n\
             short = \"st\"\n\
             config_dir_var = \"CODEX_HOME\"\n\
             config_dir = \"/tmp\"\n\
             pool = \"codex:t\"\n\n\
             [projects.sandbox]\npath = \"{}\"\n",
            sandbox.display(),
        );
        let config = load_config_str(&toml, Path::new("config.toml")).unwrap();
        let message = tmp_scratch_warning_at(&config, Some(&tmpdir), Some(&private_tmp), 0)
            .expect("threshold 0 fires");
        assert!(message.contains("sandbox copies (1 of them)"), "{message}");
        assert!(message.contains("CHARTER.md"), "{message}");
    }
}

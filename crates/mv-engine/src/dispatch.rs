//! Dispatch planning: which role runs next, and on which seat.
//!
//! Ported from `multiverse/dispatch.py`'s `DispatchPlanner.plan`
//! (`dispatch.py:1073-1305`), narrowed to exactly the decision this crate's
//! brief names: chain resolution through [`crate::selector`], per-project
//! floors and ceilings, the fleet cap, exclude-vendor resolution, the
//! cross-vendor rule for the reviewer role, `--force` under a fleet halt
//! (checked both before planning and again right before pane creation),
//! and queue/`--wait-for-seat`. Building the brief, predicting a worktree,
//! naming the agent, and writing the routing log are a different lane's
//! files (`brief.rs`, `runner.rs`) — this module only decides, it never
//! touches disk or Herdr.
//!
//! [`AccountLookup`] stands in for the fleet's account/vendor/kind config
//! (`mv-config::load::Account`/`Config`, landed) and [`RoutingLookup`] for
//! one project's routing log (`mv-state::log::parse`, landed); this
//! module still takes them as narrow traits rather than those concrete
//! types, so a caller wires either in — `mv-cli`'s dispatch command does,
//! with `ConfigAccounts` and `ProjectRouting`. [`ReviewBudget`] and
//! [`HaltQuery`] are the same seam for a project's rolling 24h
//! reviewer-dispatch count and the fleet halt file.

use std::time::Instant;

use crate::selector::{
    self, BreakerQuery, ChainExhaustedError, FallbackPolicy, QuotaFloors, Seat, Selection,
    WaitError,
};

/// Every distinct vendor/account-name/account-kind the fleet knows about,
/// narrowed to exactly what `--exclude-vendor` resolution and the
/// cross-vendor rule need (`accounts.py`'s `Account`/`Config`, ported to
/// `mv-config::load::Account`/`Config`).
pub trait AccountLookup {
    /// Every distinct configured vendor, for an `UnknownVendor` error listing.
    fn all_vendors(&self) -> Vec<String>;
    /// Every configured account name, for an `UnknownVendor` error listing.
    fn all_account_names(&self) -> Vec<String>;
    /// Every distinct configured account kind, for an `UnknownVendor` error listing.
    fn all_kinds(&self) -> Vec<String>;
    /// Whether `raw` is itself a configured vendor.
    fn vendor_exists(&self, raw: &str) -> bool;
    /// `raw` as an account name: that account's vendor.
    fn vendor_of_account(&self, raw: &str) -> Option<String>;
    /// `raw` as an account kind: every distinct vendor of that kind.
    fn vendors_of_kind(&self, raw: &str) -> Vec<String>;
}

/// `_resolve_exclude_vendor` (`dispatch.py:1356`): literal vendor, then
/// account name, then account kind — the first that matches wins.
pub fn resolve_exclude_vendor(
    raw: &str,
    accounts: &dyn AccountLookup,
) -> Result<Vec<String>, DispatchError> {
    if accounts.vendor_exists(raw) {
        return Ok(vec![raw.to_string()]);
    }
    if let Some(vendor) = accounts.vendor_of_account(raw) {
        return Ok(vec![vendor]);
    }
    let mut by_kind = accounts.vendors_of_kind(raw);
    if !by_kind.is_empty() {
        by_kind.sort();
        by_kind.dedup();
        return Ok(by_kind);
    }
    let mut vendors = accounts.all_vendors();
    vendors.sort();
    vendors.dedup();
    let mut names = accounts.all_account_names();
    names.sort();
    let mut kinds = accounts.all_kinds();
    kinds.sort();
    kinds.dedup();
    Err(DispatchError::UnknownVendor {
        raw: raw.to_string(),
        vendors,
        accounts: names,
        kinds,
    })
}

/// One project's own routing-log dispatch record, narrowed to the one
/// field the cross-vendor rule needs.
#[derive(Debug, Clone)]
pub struct LoggedDispatch {
    pub account: Option<String>,
}

/// A project's own routing log, read-only, narrowed to the one lookup the
/// cross-vendor rule needs (`tasks.read_log`, ported — `mv-state::log`
/// parses the file, and `mv-state::tasks`'s own `read_log` port joins it
/// per task). A reviewed task is always looked up in *its own* project's
/// log, never another project's.
pub trait RoutingLookup {
    fn dispatch_for_task(&self, project: &str, task_id: &str) -> Option<LoggedDispatch>;
}

/// `_author_vendor` (`dispatch.py:1393`): resolve the vendor that produced
/// `task_id`, by reading `project`'s own routing log for its recorded
/// account, then resolving that account's current vendor.
pub fn author_vendor(
    project: &str,
    task_id: &str,
    routing: &dyn RoutingLookup,
    accounts: &dyn AccountLookup,
) -> Result<String, DispatchError> {
    let logged = routing.dispatch_for_task(project, task_id).ok_or_else(|| {
        DispatchError::UnknownReviewedTask {
            task_id: task_id.to_string(),
            project: project.to_string(),
        }
    })?;
    let account = logged.account.filter(|a| !a.is_empty()).ok_or_else(|| {
        DispatchError::ReviewedTaskMissingAccount {
            task_id: task_id.to_string(),
        }
    })?;
    accounts
        .vendor_of_account(&account)
        .ok_or_else(|| DispatchError::ReviewedTaskAccountGone {
            task_id: task_id.to_string(),
            account,
        })
}

/// Every vendor excluded for this dispatch, with a human-readable note
/// per vendor explaining why. Mirrors `_author_exclusions`
/// (`dispatch.py:1307-1355`): the union of `--reviewing`'s resolved
/// author vendor(s) and every `--exclude-vendor` value's resolution, each
/// vendor noted once even if named by more than one source.
#[derive(Debug, Clone, Default)]
pub struct ExclusionTrail {
    pub vendors: Vec<String>,
    pub notes: Vec<String>,
}

/// `_author_exclusions` (`dispatch.py:1307-1355`).
pub fn author_exclusions(
    project: &str,
    reviewing: &[String],
    explicit_exclude_vendor: &[String],
    routing: &dyn RoutingLookup,
    accounts: &dyn AccountLookup,
) -> Result<ExclusionTrail, DispatchError> {
    let mut trail = ExclusionTrail::default();
    let mut seen = std::collections::HashSet::new();

    for task_id in reviewing {
        let vendor = author_vendor(project, task_id, routing, accounts)?;
        if seen.insert(vendor.clone()) {
            trail.notes.push(format!(
                "{vendor:?} — author of task {task_id:?} (--reviewing)"
            ));
            trail.vendors.push(vendor);
        }
    }
    for raw in explicit_exclude_vendor {
        for vendor in resolve_exclude_vendor(raw, accounts)? {
            if seen.insert(vendor.clone()) {
                let note = if &vendor == raw {
                    format!("{vendor:?} — --exclude-vendor")
                } else {
                    format!("{vendor:?} — --exclude-vendor {raw:?}")
                };
                trail.notes.push(note);
                trail.vendors.push(vendor);
            }
        }
    }
    Ok(trail)
}

/// The fleet's review policy, narrowed to the fields the reviewer-role
/// preconditions need (`config.review_policy`, landed as
/// `mv-config::load::Config::review_policy`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ReviewPolicy {
    /// `review_policy.batch_max_lanes` — the most lanes one `--reviewing`
    /// batch may name at once.
    pub batch_max_lanes: usize,
    /// `review_policy.min_quota_percent` — the reviewer's own quota
    /// floor, stricter than the fleet-wide one.
    pub min_quota_percent: Option<f64>,
    /// `review_policy.budget_per_window` — the most reviewer dispatches
    /// one project may make in a rolling 24h window. `0` means unlimited.
    pub budget_per_window: u32,
}

/// A project's rolling 24h reviewer-dispatch count, narrowed from
/// `accounts.reviewer_dispatch_count` (reads the routing log — landed,
/// see [`recent_reviewer_dispatch_count`] below — though `mv-cli`'s own
/// dispatch command does not wire it into this trait yet).
pub trait ReviewBudget {
    fn reviewer_dispatch_count(&self, project: &str, now: Instant) -> u32;
}

/// Critic-only preconditions, checked before anything else is built
/// (`dispatch.py`'s `plan`, around `:1205-1222`): the reviewer-vendor
/// gate, the lane-batch size limit, and the rolling 24h dispatch budget.
/// A no-op for every other role.
#[allow(clippy::too_many_arguments)]
pub fn check_reviewer_preconditions(
    role: &str,
    reviewer_role: &str,
    project: &str,
    reviewing: &[String],
    explicit_exclude_vendor: &[String],
    policy: &ReviewPolicy,
    budget: &dyn ReviewBudget,
    now: Instant,
) -> Result<(), DispatchError> {
    if role != reviewer_role {
        return Ok(());
    }
    if reviewing.is_empty() && explicit_exclude_vendor.is_empty() {
        return Err(DispatchError::ReviewerVendorUnknown);
    }
    if reviewing.len() > policy.batch_max_lanes {
        return Err(DispatchError::TooManyLanes {
            count: reviewing.len(),
            max: policy.batch_max_lanes,
        });
    }
    if policy.budget_per_window > 0 {
        let used = budget.reviewer_dispatch_count(project, now);
        if used >= policy.budget_per_window {
            return Err(DispatchError::ReviewBudgetExceeded {
                project: project.to_string(),
                used,
                budget: policy.budget_per_window,
            });
        }
    }
    Ok(())
}

/// A fleet halt record (`FleetHalt`, `config.py:86`). `since` is the
/// recorded wall-clock timestamp (RFC3339), not a monotonic `Instant` —
/// Python's `_fleet_halt_line` (`cli.py:3241-3246`) renders it verbatim in
/// the refusal message, so a `None` here (an on-disk marker with no
/// timestamp) must still produce a readable line, never a missing field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetHalt {
    pub reason: String,
    pub since: Option<String>,
}

/// `dispatch refused: {this}; use --force to override.` — mirrors
/// `multiverse/cli.py`'s `_fleet_halt_line` exactly (`fleet halted since
/// <at>: <reason>`), falling back to `fleet halted: <reason>` when no
/// wall-clock timestamp was recorded.
fn fleet_halt_line(reason: &str, since: Option<&str>) -> String {
    match since {
        Some(at) => format!("fleet halted since {at}: {reason}"),
        None => format!("fleet halted: {reason}"),
    }
}

/// The fleet halt file, narrowed to a single read. An `Err` is a read
/// failure (corrupt/unreadable store), not "no halt recorded" — that case
/// is `Ok(None)`.
pub trait HaltQuery {
    fn read(&self) -> Result<Option<FleetHalt>, String>;
}

/// Check the fleet halt. Called at two sites, with different `dry_run`
/// behavior (`cli.py:3277-3283`, `:3708-3723`):
///
/// - Before planning: `dry_run` exempts the check entirely — a dry run
///   never spends a real dispatch slot, so it never needs `--force`.
/// - Immediately before pane creation (see
///   [`check_halt_before_pane_creation`]): never exempted, because a dry
///   run never reaches that call site at all.
///
/// An unreadable halt store is itself a refusal at both sites; `--force`
/// overrides only a genuinely active halt, never a read failure.
pub fn check_halt(halt: &dyn HaltQuery, force: bool, dry_run: bool) -> Result<(), DispatchError> {
    if dry_run {
        return Ok(());
    }
    match halt.read() {
        Ok(None) => Ok(()),
        Ok(Some(h)) => {
            if force {
                Ok(())
            } else {
                Err(DispatchError::FleetHaltActive {
                    reason: h.reason,
                    since: h.since,
                })
            }
        }
        Err(detail) => Err(DispatchError::FleetHaltUnreadable { detail }),
    }
}

/// The second halt check site: called exactly once, after planning
/// finishes and immediately before the runner would create a pane, in
/// case the fleet was halted mid-plan. Never exempted by `dry_run` — a
/// dry run never reaches this call site.
pub fn check_halt_before_pane_creation(
    halt: &dyn HaltQuery,
    force: bool,
) -> Result<(), DispatchError> {
    check_halt(halt, force, false)
}

/// Fleet/project concurrency state a capacity check needs, already
/// resolved by the caller. The live-Herdr-listing and Herdr-lag grace
/// counting behind `open_here`/`fleet_open`/`other_floor_gaps`
/// (`runner.py`'s `_project_slots_grace_counts`) depends on a live Herdr
/// read and the routing log, both outside this crate's narrow scope —
/// this struct is the seam a caller fills in; `mv-cli`'s
/// `fleet_open_counts` does, for the real dispatch command.
#[derive(Debug, Clone)]
pub struct FleetCapacityInputs {
    /// `config.max_concurrent`.
    pub max_concurrent: u32,
    /// Effective fleet-wide running count (live agents plus
    /// Herdr-lag grace), *before* this batch.
    pub fleet_open: u32,
    /// This project's own open count, *before* this batch.
    pub project_open: u32,
    /// `project.min_slots`; `0` means no floor.
    pub project_min_slots: u32,
    /// `project.max_slots`; `None` falls back to `max_concurrent`
    /// (`project_ceiling`, `config.py:1650`).
    pub project_max_slots: Option<u32>,
    /// Every other project still short of its own floor, as
    /// `(name, gap)`, where `gap = max(0, min_slots - open_here)`.
    pub other_floor_gaps: Vec<(String, u32)>,
    /// How many seats this dispatch batch wants (usually `1`).
    pub batch_count: u32,
}

/// `concurrency_or_raise` + `project_slots_or_raise`
/// (`runner.py:1361-1368`, `:1483`, `:1679`): fleet cap, then project
/// ceiling, then (unless this batch stays within its own floor gap) the
/// cross-project floor-borrowing check.
pub fn check_fleet_capacity(inputs: &FleetCapacityInputs) -> Result<(), DispatchError> {
    if inputs.fleet_open + inputs.batch_count > inputs.max_concurrent {
        return Err(DispatchError::FleetCapExceeded {
            running: inputs.fleet_open,
            cap: inputs.max_concurrent,
        });
    }

    let ceiling = inputs.project_max_slots.unwrap_or(inputs.max_concurrent);
    if inputs.project_open + inputs.batch_count > ceiling {
        return Err(DispatchError::ProjectCeilingExceeded {
            open: inputs.project_open,
            ceiling,
        });
    }

    // A batch that entirely fills its own floor gap bypasses floor
    // borrowing — it is only catching this project up, not borrowing
    // from anyone else's headroom.
    let floor_gap = inputs.project_min_slots.saturating_sub(inputs.project_open);
    if inputs.batch_count <= floor_gap {
        return Ok(());
    }

    let free_after =
        inputs.max_concurrent as i64 - (inputs.fleet_open as i64 + inputs.batch_count as i64);
    let needed_elsewhere: u32 = inputs.other_floor_gaps.iter().map(|(_, gap)| *gap).sum();
    if free_after >= needed_elsewhere as i64 {
        return Ok(());
    }

    let mut limiting: Vec<(String, u32)> = inputs
        .other_floor_gaps
        .iter()
        .filter(|(_, gap)| *gap > 0)
        .cloned()
        .collect();
    limiting.sort_by_key(|(_, gap)| std::cmp::Reverse(*gap));
    Err(DispatchError::ProjectFloorBorrowRefused {
        free_after: free_after.max(0) as u32,
        max_concurrent: inputs.max_concurrent,
        needed_elsewhere,
        limiting,
    })
}

/// Everything [`plan_dispatch`] needs about one dispatch request that
/// does not come from a live store (concurrency, halt, routing log) —
/// those are supplied separately so this struct stays plain data.
#[derive(Debug, Clone)]
pub struct DispatchRequest<'a> {
    pub role: &'a str,
    pub reviewer_role: &'a str,
    pub nexus_role: &'a str,
    pub project: &'a str,
    pub task_id: &'a str,
    pub task_len_chars: usize,
    /// Task ids named by `--reviewing`.
    pub reviewing: &'a [String],
    /// Raw `--exclude-vendor` values, unresolved.
    pub explicit_exclude_vendor: &'a [String],
    pub chain: &'a [Seat],
    pub policy: FallbackPolicy,
    pub review_policy: ReviewPolicy,
    /// `config.dispatch_min_quota_percent`; `0`/`None` disables it.
    pub fleet_min_quota_percent: Option<f64>,
    /// `--wait-for-seat`.
    pub wait_for_capacity: bool,
    /// `None` waits indefinitely; `Some(1800.0)` is the CLI default.
    pub capacity_timeout: Option<f64>,
    pub force: bool,
    pub dry_run: bool,
}

/// Every typed refusal [`plan_dispatch`] and its helpers can raise.
/// Mirrors the distinct exception classes in `dispatch.py`/`accounts.py`
/// this module's brief covers; `mv-cli`'s exit-code boundary maps these
/// to `EXIT_NEEDS_CONFIG`/`EXIT_ERROR` (`commands::dispatch`'s own match
/// over this enum).
#[derive(Debug, Clone, thiserror::Error)]
pub enum DispatchError {
    #[error(
        "{raw:?} is not a known vendor, account, or account kind. \
         Vendors: {vendors:?}. Accounts: {accounts:?}. Kinds: {kinds:?}."
    )]
    UnknownVendor {
        raw: String,
        vendors: Vec<String>,
        accounts: Vec<String>,
        kinds: Vec<String>,
    },

    #[error("task {task_id:?} was never dispatched in project {project:?}'s routing log")]
    UnknownReviewedTask { task_id: String, project: String },

    #[error("task {task_id:?}'s routing-log entry has no recorded account")]
    ReviewedTaskMissingAccount { task_id: String },

    #[error("task {task_id:?}'s recorded account {account:?} is no longer in config")]
    ReviewedTaskAccountGone { task_id: String, account: String },

    #[error(
        "\nCritic dispatch needs --reviewing <task-id> or --exclude-vendor <vendor> — neither was given, so there is no author vendor to exclude and this dispatch would be free to land on the author's own vendor.\n\
         Name the lane under review with --reviewing, or exclude a vendor directly with --exclude-vendor.\n"
    )]
    ReviewerVendorUnknown,

    #[error("--reviewing named {count} lanes, over the configured batch_max_lanes of {max}")]
    TooManyLanes { count: usize, max: usize },

    #[error(
        "project {project:?} has already made {used}/{budget} Critic dispatches in the last 24h"
    )]
    ReviewBudgetExceeded {
        project: String,
        used: u32,
        budget: u32,
    },

    #[error("dispatch refused: {}; use --force to override.", fleet_halt_line(.reason, .since.as_deref()))]
    FleetHaltActive {
        reason: String,
        since: Option<String>,
    },

    #[error("dispatch refused: could not verify fleet halt ({detail})")]
    FleetHaltUnreadable { detail: String },

    #[error("{running} agent(s) already running and max_concurrent is {cap}; nothing was started")]
    FleetCapExceeded { running: u32, cap: u32 },

    #[error("project already has {open} agent(s) running and its ceiling is {ceiling}; nothing was started")]
    ProjectCeilingExceeded { open: u32, ceiling: u32 },

    #[error(
        "starting another agent here would leave only {free_after} free fleet slot(s) \
         (max_concurrent is {max_concurrent}), but other projects still need \
         {needed_elsewhere} slot(s) to reach their own floor: {limiting:?}"
    )]
    ProjectFloorBorrowRefused {
        free_after: u32,
        max_concurrent: u32,
        needed_elsewhere: u32,
        limiting: Vec<(String, u32)>,
    },

    #[error(transparent)]
    ChainExhausted(#[from] ChainExhaustedError),

    #[error(transparent)]
    Wait(#[from] WaitError),
}

/// `DispatchPlanner.plan` (`dispatch.py:1073-1305`), narrowed to this
/// crate's decision scope: halt, fleet/project capacity, reviewer
/// preconditions, exclude-vendor resolution, then seat selection. Checks
/// run in Python's own order — refuse before spending anything.
#[allow(clippy::too_many_arguments)]
pub fn plan_dispatch(
    req: &DispatchRequest,
    routing: &dyn RoutingLookup,
    accounts: &dyn AccountLookup,
    budget: &dyn ReviewBudget,
    capacity: &FleetCapacityInputs,
    halt: &dyn HaltQuery,
    breaker: &dyn BreakerQuery,
    now: Instant,
) -> Result<Selection, DispatchError> {
    check_halt(halt, req.force, req.dry_run)?;
    check_fleet_capacity(capacity)?;
    check_reviewer_preconditions(
        req.role,
        req.reviewer_role,
        req.project,
        req.reviewing,
        req.explicit_exclude_vendor,
        &req.review_policy,
        budget,
        now,
    )?;

    let trail = author_exclusions(
        req.project,
        req.reviewing,
        req.explicit_exclude_vendor,
        routing,
        accounts,
    )?;

    let floors = QuotaFloors {
        reviewer_min_quota_percent: if req.role == req.reviewer_role {
            req.review_policy.min_quota_percent
        } else {
            None
        },
        fleet_min_quota_percent: req.fleet_min_quota_percent,
    };
    let params = selector::SelectParams {
        role: req.role,
        project: req.project,
        reviewer_role: req.reviewer_role,
        nexus_role: req.nexus_role,
        exclude_vendors: &trail.vendors,
        task_len_chars: req.task_len_chars,
        floors,
        policy: req.policy,
        task_id: req.task_id,
        trial_token: None,
    };

    // A dry run never waits: it plans and renders only, so it always
    // takes the no-wait path regardless of `--wait-for-seat`
    // (`dispatch.py`'s `plan`: `False` is passed for dry-run).
    if req.wait_for_capacity && !req.dry_run {
        let chain = req.chain.to_vec();
        let opts = selector::WaitOptions {
            timeout: req.capacity_timeout,
            poll_interval: selector::DEFAULT_POLL_INTERVAL_SECS,
            sleeper: &mut selector::RealSleeper,
            on_wait: &mut |_msg: &str| {},
        };
        Ok(selector::select_or_wait(
            &params,
            breaker,
            move || chain.clone(),
            opts,
            Instant::now,
        )?)
    } else {
        Ok(selector::select(req.chain, &params, breaker, now)?)
    }
}

/// Historical routing records retain both the old and current role names.
/// Missing logs are empty; unreadable logs must not fabricate an unused budget.
pub fn recent_reviewer_dispatch_count(
    root: &std::path::Path,
    now: time::OffsetDateTime,
) -> std::io::Result<usize> {
    let path = mv_state::log::ProjectLog::new(root);
    let text = match std::fs::read_to_string(path.path()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let since = now - time::Duration::hours(24);
    Ok(text
        .lines()
        .filter_map(|line| mv_state::log::parse(line).ok())
        .filter(|record| record.event() == "dispatch")
        .filter(|record| {
            record
                .fields
                .get("role")
                .is_some_and(|role| matches!(role.as_str(), "reviewer" | "critic"))
        })
        .filter(|record| {
            mv_core::time::parse_flexible(&record.when)
                .is_ok_and(|when| when >= since && when <= now)
        })
        .count())
}

/// Backlog items in `project_root` a human answered but nobody closed yet —
/// Python's `dispatch._unclosed_answered_items` (`dispatch.py:394-407`):
/// `state == "asked"` with at least one annotation recorded `by == "human"`.
/// An item merely `asked`, with no human annotation yet, is excluded on
/// purpose (matching Python's own docstring) — it is waiting on the human's
/// own response time, not the project's, and blocking dispatch on it would
/// punish the project for that wait.
pub fn unclosed_answered_items(
    project_root: &std::path::Path,
) -> Vec<mv_state::backlog::BacklogItem> {
    mv_state::backlog::read_items(project_root, false)
        .into_iter()
        .filter(|item| item.state == "asked" && mv_state::backlog::has_human_annotation(item))
        .collect()
}

/// `AnsweredItemsUnclosedError`'s own message (`dispatch.py:360-369`),
/// verbatim: the project name, every unclosed item's id, and the fix. No
/// `--force`-style escape is ported either — Python's own docstring is
/// explicit that this gate is absolute, the honest fix being to close the
/// item, not to add a second way around doing so.
pub fn answered_items_unclosed_message(
    project_name: &str,
    items: &[mv_state::backlog::BacklogItem],
) -> String {
    let named = items
        .iter()
        .map(|item| format!("  {}", item.item))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "{project_name} has {} backlog item(s) that were answered and never closed:\n{named}\n\
         Act on the answer, then close each one with `backlog done <item> --evidence ...` — \
         dispatch refuses this project until every item above is closed.",
        items.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeAccounts {
        vendor_by_name: HashMap<String, String>,
        vendor_by_kind: HashMap<String, Vec<String>>,
        vendors: Vec<String>,
    }

    impl AccountLookup for FakeAccounts {
        fn all_vendors(&self) -> Vec<String> {
            self.vendors.clone()
        }
        fn all_account_names(&self) -> Vec<String> {
            self.vendor_by_name.keys().cloned().collect()
        }
        fn all_kinds(&self) -> Vec<String> {
            self.vendor_by_kind.keys().cloned().collect()
        }
        fn vendor_exists(&self, raw: &str) -> bool {
            self.vendors.iter().any(|v| v == raw)
        }
        fn vendor_of_account(&self, raw: &str) -> Option<String> {
            self.vendor_by_name.get(raw).cloned()
        }
        fn vendors_of_kind(&self, raw: &str) -> Vec<String> {
            self.vendor_by_kind.get(raw).cloned().unwrap_or_default()
        }
    }

    fn fake_accounts() -> FakeAccounts {
        FakeAccounts {
            vendor_by_name: HashMap::from([
                ("claude-personal".to_string(), "claude".to_string()),
                ("codex-work".to_string(), "chatgpt".to_string()),
            ]),
            vendor_by_kind: HashMap::from([(
                "codex".to_string(),
                vec!["chatgpt".to_string(), "chatgpt:work".to_string()],
            )]),
            vendors: vec![
                "claude".to_string(),
                "chatgpt".to_string(),
                "chatgpt:work".to_string(),
                "grok".to_string(),
            ],
        }
    }

    struct FakeRouting {
        entries: HashMap<(String, String), LoggedDispatch>,
    }

    impl RoutingLookup for FakeRouting {
        fn dispatch_for_task(&self, project: &str, task_id: &str) -> Option<LoggedDispatch> {
            self.entries
                .get(&(project.to_string(), task_id.to_string()))
                .cloned()
        }
    }

    // --- exclude-vendor resolution ---

    #[test]
    fn reviewer_budget_counts_recent_dispatches_with_old_and_new_role_names() {
        let dir = tempfile::tempdir().unwrap();
        let now = mv_core::time::parse_flexible("2026-11-02T12:00:00+00:00").unwrap();
        assert_eq!(recent_reviewer_dispatch_count(dir.path(), now).unwrap(), 0);
        std::fs::create_dir(dir.path().join(".multiverse")).unwrap();
        let log = mv_state::log::ProjectLog::new(dir.path());
        std::fs::write(
            log.path(),
            concat!(
                "2026-11-02T10:00:00+00:00\tevent=dispatch\trole=reviewer\n",
                "2026-11-02T11:00:00Z\tevent=dispatch\trole=critic\n",
                "2026-10-31T12:00:00+00:00\tevent=dispatch\trole=reviewer\n",
                "2026-11-03T12:00:00+00:00\tevent=dispatch\trole=critic\n",
                "2026-11-02T10:00:00+00:00\tevent=result\trole=critic\n",
                "2026-11-02T10:00:00+00:00\tevent=dispatch\trole=builder\n",
                "invalid\tevent=dispatch\trole=critic\n",
            ),
        )
        .unwrap();
        assert_eq!(recent_reviewer_dispatch_count(dir.path(), now).unwrap(), 2);
        std::fs::remove_file(log.path()).unwrap();
        std::fs::create_dir(log.path()).unwrap();
        assert!(recent_reviewer_dispatch_count(dir.path(), now).is_err());
    }

    #[test]
    fn exclude_vendor_literal_wins_over_a_shadowing_account() {
        let mut accounts = fake_accounts();
        accounts
            .vendor_by_name
            .insert("claude".to_string(), "grok".to_string());
        accounts.vendors.push("claude".to_string());
        let resolved = resolve_exclude_vendor("claude", &accounts).unwrap();
        assert_eq!(resolved, vec!["claude".to_string()]);
    }

    #[test]
    fn exclude_vendor_by_account_name_resolves_to_its_vendor() {
        let accounts = fake_accounts();
        let resolved = resolve_exclude_vendor("claude-personal", &accounts).unwrap();
        assert_eq!(resolved, vec!["claude".to_string()]);
    }

    #[test]
    fn exclude_vendor_codex_skips_both_codex_and_its_sibling_pool_in_a_reviewer_chain() {
        let accounts = fake_accounts();
        let mut resolved = resolve_exclude_vendor("codex", &accounts).unwrap();
        resolved.sort();
        assert_eq!(
            resolved,
            vec!["chatgpt".to_string(), "chatgpt:work".to_string()]
        );
    }

    #[test]
    fn exclude_vendor_unknown_value_is_refused() {
        let accounts = fake_accounts();
        let err = resolve_exclude_vendor("not-a-thing", &accounts).unwrap_err();
        assert!(matches!(err, DispatchError::UnknownVendor { .. }));
    }

    // --- cross-vendor / author resolution ---

    #[test]
    fn reviewing_an_unknown_task_id_refuses() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::new(),
        };
        let err =
            author_vendor("example", "20261101-builder-abcdef", &routing, &accounts).unwrap_err();
        assert!(matches!(err, DispatchError::UnknownReviewedTask { .. }));
    }

    #[test]
    fn reviewing_a_task_with_no_recorded_account_refuses() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::from([(
                ("example".to_string(), "t1".to_string()),
                LoggedDispatch { account: None },
            )]),
        };
        let err = author_vendor("example", "t1", &routing, &accounts).unwrap_err();
        assert!(matches!(
            err,
            DispatchError::ReviewedTaskMissingAccount { .. }
        ));
    }

    #[test]
    fn reviewing_a_task_whose_account_is_gone_from_config_refuses() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::from([(
                ("example".to_string(), "t1".to_string()),
                LoggedDispatch {
                    account: Some("deleted-account".to_string()),
                },
            )]),
        };
        let err = author_vendor("example", "t1", &routing, &accounts).unwrap_err();
        assert!(matches!(err, DispatchError::ReviewedTaskAccountGone { .. }));
    }

    #[test]
    fn reviewing_a_task_id_from_another_project_does_not_resolve() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::from([(
                ("other-project".to_string(), "t1".to_string()),
                LoggedDispatch {
                    account: Some("claude-personal".to_string()),
                },
            )]),
        };
        let err = author_vendor("example", "t1", &routing, &accounts).unwrap_err();
        assert!(matches!(err, DispatchError::UnknownReviewedTask { .. }));
    }

    #[test]
    fn reviewing_a_fixer_task_excludes_its_vendor_and_the_trail_says_why() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::from([(
                ("example".to_string(), "t1".to_string()),
                LoggedDispatch {
                    account: Some("claude-personal".to_string()),
                },
            )]),
        };
        let trail =
            author_exclusions("example", &["t1".to_string()], &[], &routing, &accounts).unwrap();
        assert_eq!(trail.vendors, vec!["claude".to_string()]);
        assert!(trail.notes[0].contains("author of task"));
        assert!(trail.notes[0].contains("t1"));
    }

    #[test]
    fn reviewing_and_exclude_vendor_compose_to_exclude_both() {
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::from([(
                ("example".to_string(), "t1".to_string()),
                LoggedDispatch {
                    account: Some("claude-personal".to_string()),
                },
            )]),
        };
        let trail = author_exclusions(
            "example",
            &["t1".to_string()],
            &["grok".to_string()],
            &routing,
            &accounts,
        )
        .unwrap();
        let mut vendors = trail.vendors.clone();
        vendors.sort();
        assert_eq!(vendors, vec!["claude".to_string(), "grok".to_string()]);
    }

    // --- reviewer preconditions ---

    struct FakeBudget(u32);
    impl ReviewBudget for FakeBudget {
        fn reviewer_dispatch_count(&self, _project: &str, _now: Instant) -> u32 {
            self.0
        }
    }

    #[test]
    fn reviewer_with_neither_reviewing_nor_exclude_vendor_is_refused() {
        let budget = FakeBudget(0);
        let err = check_reviewer_preconditions(
            "critic",
            "critic",
            "example",
            &[],
            &[],
            &ReviewPolicy::default(),
            &budget,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::ReviewerVendorUnknown));
    }

    #[test]
    fn non_reviewer_roles_skip_the_reviewer_gate_entirely() {
        let budget = FakeBudget(0);
        assert!(check_reviewer_preconditions(
            "builder",
            "critic",
            "example",
            &[],
            &[],
            &ReviewPolicy::default(),
            &budget,
            Instant::now(),
        )
        .is_ok());
    }

    #[test]
    fn too_many_lanes_named_at_once_is_refused() {
        let budget = FakeBudget(0);
        let policy = ReviewPolicy {
            batch_max_lanes: 1,
            ..Default::default()
        };
        let err = check_reviewer_preconditions(
            "critic",
            "critic",
            "example",
            &["t1".to_string(), "t2".to_string()],
            &[],
            &policy,
            &budget,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::TooManyLanes { count: 2, max: 1 }
        ));
    }

    #[test]
    fn review_budget_exceeded_refuses_another_reviewer_dispatch() {
        let budget = FakeBudget(3);
        let policy = ReviewPolicy {
            batch_max_lanes: 10,
            budget_per_window: 3,
            ..Default::default()
        };
        let err = check_reviewer_preconditions(
            "critic",
            "critic",
            "example",
            &["t1".to_string()],
            &[],
            &policy,
            &budget,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DispatchError::ReviewBudgetExceeded {
                used: 3,
                budget: 3,
                ..
            }
        ));
        let message = err.to_string();
        assert!(message.contains("Critic dispatches"), "{message:?}");
        assert!(!message.to_lowercase().contains("reviewer"), "{message:?}");
    }

    #[test]
    fn a_zero_budget_never_refuses() {
        let budget = FakeBudget(1000);
        let policy = ReviewPolicy {
            batch_max_lanes: 10,
            ..Default::default() // budget_per_window defaults to 0 = unlimited
        };
        let err = check_reviewer_preconditions(
            "critic",
            "critic",
            "example",
            &["t1".to_string()],
            &[],
            &policy,
            &budget,
            Instant::now(),
        );
        assert!(err.is_ok());
    }

    // --- fleet halt ---

    struct FakeHalt(Result<Option<FleetHalt>, String>);
    impl HaltQuery for FakeHalt {
        fn read(&self) -> Result<Option<FleetHalt>, String> {
            self.0.clone()
        }
    }

    #[test]
    fn real_dispatch_refuses_during_halt() {
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "investigating a stuck lane".to_string(),
            since: Some("2026-11-02T12:00:00+00:00".to_string()),
        })));
        let err = check_halt(&halt, false, false).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("dispatch refused"));
        assert!(message.contains("investigating a stuck lane"));
        assert!(message.contains("--force"));
        assert_eq!(
            message,
            "dispatch refused: fleet halted since 2026-11-02T12:00:00+00:00: \
             investigating a stuck lane; use --force to override."
        );
    }

    #[test]
    fn real_dispatch_refuses_during_halt_with_no_recorded_timestamp() {
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "investigating a stuck lane".to_string(),
            since: None,
        })));
        let err = check_halt(&halt, false, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "dispatch refused: fleet halted: investigating a stuck lane; \
             use --force to override."
        );
    }

    #[test]
    fn force_overrides_an_active_halt() {
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "halted".to_string(),
            since: None,
        })));
        assert!(check_halt(&halt, true, false).is_ok());
    }

    #[test]
    fn dry_run_is_exempt_from_the_halt_check() {
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "halted".to_string(),
            since: None,
        })));
        assert!(check_halt(&halt, false, true).is_ok());
    }

    #[test]
    fn an_unreadable_halt_store_is_itself_a_refusal() {
        let halt = FakeHalt(Err("permission denied".to_string()));
        let err = check_halt(&halt, true, false).unwrap_err();
        assert!(matches!(err, DispatchError::FleetHaltUnreadable { .. }));
    }

    #[test]
    fn dispatch_rechecks_halt_before_starting_a_pane_never_exempted_by_dry_run() {
        // The pre-pane-creation check has no dry_run exemption, unlike the
        // planning-time check — a dry run never reaches it, but if it did,
        // it must still refuse.
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "late halt".to_string(),
            since: None,
        })));
        let err = check_halt_before_pane_creation(&halt, false).unwrap_err();
        assert!(matches!(err, DispatchError::FleetHaltActive { .. }));
    }

    // --- fleet/project capacity ---

    fn capacity() -> FleetCapacityInputs {
        FleetCapacityInputs {
            max_concurrent: 5,
            fleet_open: 2,
            project_open: 1,
            project_min_slots: 0,
            project_max_slots: None,
            other_floor_gaps: vec![],
            batch_count: 1,
        }
    }

    #[test]
    fn concurrency_or_raise_refuses_at_the_fleet_cap() {
        let mut inputs = capacity();
        inputs.fleet_open = 5;
        let err = check_fleet_capacity(&inputs).unwrap_err();
        assert!(matches!(err, DispatchError::FleetCapExceeded { .. }));
    }

    #[test]
    fn project_slots_or_raise_refuses_at_its_own_ceiling() {
        let mut inputs = capacity();
        inputs.project_max_slots = Some(1);
        inputs.project_open = 1;
        let err = check_fleet_capacity(&inputs).unwrap_err();
        assert!(matches!(err, DispatchError::ProjectCeilingExceeded { .. }));
    }

    #[test]
    fn an_unconfigured_project_falls_back_to_the_fleet_cap_as_its_ceiling() {
        let mut inputs = capacity();
        inputs.project_max_slots = None;
        inputs.project_open = 4; // under max_concurrent (5), fine
        assert!(check_fleet_capacity(&inputs).is_ok());
        inputs.project_open = 5; // now at the fleet cap itself
        assert!(matches!(
            check_fleet_capacity(&inputs).unwrap_err(),
            DispatchError::ProjectCeilingExceeded { .. }
        ));
    }

    #[test]
    fn project_slots_or_raise_never_refuses_a_project_still_under_its_own_floor() {
        let mut inputs = capacity();
        inputs.project_min_slots = 3;
        inputs.project_open = 0;
        inputs.fleet_open = 4; // would leave 0 free after this batch
        inputs.other_floor_gaps = vec![("other".to_string(), 10)]; // huge unmet need elsewhere
                                                                   // This batch (count=1) stays within its own floor gap (3), so the
                                                                   // cross-project borrow check is bypassed entirely.
        assert!(check_fleet_capacity(&inputs).is_ok());
    }

    #[test]
    fn project_slots_or_raise_refuses_a_borrow_that_would_starve_another_floor() {
        let mut inputs = capacity();
        inputs.max_concurrent = 5;
        inputs.fleet_open = 4;
        inputs.project_open = 2; // already past its own floor (0), so this is a "borrow"
        inputs.project_min_slots = 0;
        inputs.other_floor_gaps = vec![("needy".to_string(), 2)];
        // free_after = 5 - (4+1) = 0, needed_elsewhere = 2 > 0 -> refused.
        let err = check_fleet_capacity(&inputs).unwrap_err();
        assert!(matches!(
            err,
            DispatchError::ProjectFloorBorrowRefused { .. }
        ));
    }

    #[test]
    fn project_slots_or_raise_checks_only_the_batch_overage_past_the_floor() {
        let mut inputs = capacity();
        inputs.max_concurrent = 5;
        inputs.fleet_open = 3;
        inputs.project_open = 0;
        inputs.project_min_slots = 1;
        inputs.batch_count = 2; // 1 catches the floor up, the 2nd is a real borrow
        inputs.other_floor_gaps = vec![("needy".to_string(), 1)];
        // floor_gap = 1, batch_count (2) > floor_gap, so the borrow check
        // runs: free_after = 5 - (3+2) = 0, needed_elsewhere = 1 -> refused.
        let err = check_fleet_capacity(&inputs).unwrap_err();
        assert!(matches!(
            err,
            DispatchError::ProjectFloorBorrowRefused { .. }
        ));
    }

    // --- end-to-end plan_dispatch ---

    struct AlwaysOpenBreaker;
    impl BreakerQuery for AlwaysOpenBreaker {
        fn status(&self, _pool: &str) -> selector::BreakerStatus {
            selector::BreakerStatus::CLOSED
        }
        fn claim_trial(&self, _pool: &str, _task_id: &str, _token: Option<&str>) -> Option<String> {
            None
        }
    }

    fn plain_seat(name: &str) -> Seat {
        Seat {
            name: name.to_string(),
            vendor: "claude".to_string(),
            pool: "claude:personal".to_string(),
            kind: "claude".to_string(),
            available: true,
            unavailable_reason: None,
            unavailable_until: None,
            lent: true,
            has_model: true,
            roles_allowed: None,
            max_brief_chars: None,
            has_command: true,
            cap: None,
            open_count: None,
            quota_primary_percent: None,
            quota_secondary_percent: None,
            local_alive: None,
        }
    }

    #[test]
    fn plan_dispatch_refuses_during_halt_before_touching_selection() {
        let chain = [plain_seat("a")];
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::new(),
        };
        let budget = FakeBudget(0);
        let halt = FakeHalt(Ok(Some(FleetHalt {
            reason: "halted".to_string(),
            since: None,
        })));
        let req = DispatchRequest {
            role: "builder",
            reviewer_role: "critic",
            nexus_role: "nexus",
            project: "example",
            task_id: "20261102-builder-abcdef",
            task_len_chars: 10,
            reviewing: &[],
            explicit_exclude_vendor: &[],
            chain: &chain,
            policy: FallbackPolicy::Degrade,
            review_policy: ReviewPolicy::default(),
            fleet_min_quota_percent: None,
            wait_for_capacity: false,
            capacity_timeout: None,
            force: false,
            dry_run: false,
        };
        let err = plan_dispatch(
            &req,
            &routing,
            &accounts,
            &budget,
            &capacity(),
            &halt,
            &AlwaysOpenBreaker,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::FleetHaltActive { .. }));
    }

    #[test]
    fn plan_dispatch_selects_a_seat_when_every_check_passes() {
        let chain = [plain_seat("a")];
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::new(),
        };
        let budget = FakeBudget(0);
        let halt = FakeHalt(Ok(None));
        let req = DispatchRequest {
            role: "builder",
            reviewer_role: "critic",
            nexus_role: "nexus",
            project: "example",
            task_id: "20261102-builder-abcdef",
            task_len_chars: 10,
            reviewing: &[],
            explicit_exclude_vendor: &[],
            chain: &chain,
            policy: FallbackPolicy::Degrade,
            review_policy: ReviewPolicy::default(),
            fleet_min_quota_percent: None,
            wait_for_capacity: false,
            capacity_timeout: None,
            force: false,
            dry_run: false,
        };
        let selection = plan_dispatch(
            &req,
            &routing,
            &accounts,
            &budget,
            &capacity(),
            &halt,
            &AlwaysOpenBreaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "a");
    }

    #[test]
    fn plan_dispatch_dry_run_never_waits_on_a_capped_seat() {
        let mut capped = plain_seat("capped");
        capped.cap = Some(1);
        capped.open_count = Some(1);
        let chain = [capped];
        let accounts = fake_accounts();
        let routing = FakeRouting {
            entries: HashMap::new(),
        };
        let budget = FakeBudget(0);
        let halt = FakeHalt(Ok(None));
        let req = DispatchRequest {
            role: "builder",
            reviewer_role: "critic",
            nexus_role: "nexus",
            project: "example",
            task_id: "20261102-builder-abcdef",
            task_len_chars: 10,
            reviewing: &[],
            explicit_exclude_vendor: &[],
            chain: &chain,
            policy: FallbackPolicy::Queue,
            review_policy: ReviewPolicy::default(),
            fleet_min_quota_percent: None,
            wait_for_capacity: true, // requested, but...
            capacity_timeout: Some(0.01),
            force: false,
            dry_run: true, // ...dry-run must still return immediately, never wait.
        };
        // A capped, fully-occupied seat with wait_for_capacity=true would
        // otherwise poll; dry_run must force the no-wait path so this
        // returns (with a refusal) immediately rather than sleeping.
        let err = plan_dispatch(
            &req,
            &routing,
            &accounts,
            &budget,
            &capacity(),
            &halt,
            &AlwaysOpenBreaker,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(err, DispatchError::ChainExhausted(_)));
    }
}

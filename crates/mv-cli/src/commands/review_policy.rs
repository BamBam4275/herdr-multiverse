//! `multiverse review-policy [--lane TASK_ID]`: show the effective review
//! policy, optionally resolved for one lane.
//!
//! `mv-config::load` landed after this file was first written (merged from
//! `main` mid-task) and exposes exactly the entry point its own doc comment
//! names for `mv-cli`: [`resolve_config_path_from_env`] + [`load_config`].
//! [`LiveConfig`] wires through to both for real.
//!
//! `--lane TASK_ID` (finding 5, critic-sweep-board.md, task
//! `20261102-fixer-40e851`; content-risk patterns and merged-lane recovery
//! added by task `20261102-fixer-d9d751`, card `20261102-b-4f1cb3`): now
//! resolves to a real risk classification rather than echoing the
//! fleet-wide policy unchanged. `[review_policy]` is still a single
//! fleet-wide section — there is no per-lane override to show — so
//! `--lane` instead diffs that lane's own branch
//! ([`mv_engine::lane_diff::lane_diff`]) against
//! `[review_policy.review_risk_paths]`'s path globs *and* content patterns
//! ([`mv_engine::risk_paths::risk_areas_touched`]) and prints just the one
//! classification line, the same shape Python's `cmd_review_policy` prints
//! for `--lane` (no policy/seat/budget dump underneath it). Which project
//! owns the lane is resolved from cwd (or the sole configured project) — this crate still has no cheaper lane-to-project
//! index — so a lane whose branch is still on disk, or whose merge/
//! `Task:` trailer is still reachable from `HEAD`, in exactly one project
//! resolves; one that resolves in no configured project reports plainly
//! rather than guessed at.
//!
//! Per spec-cli.md's exit-code table, `review-policy` returns only
//! `OK`/`NEEDS_CONFIG` — never `ERROR` — so the seam's error type is
//! restricted to [`Failure::Config`], and [`mv_config::error::ConfigError`]
//! already documents that exact mapping.
//!
//! The fleet-wide "critic seats:" line (card `20261102-b-c459b1`) now
//! builds on `accounts::account_state` — the same fenced/half-open/
//! draining/lent-off state `multiverse accounts` itself shows — instead of
//! the three-way fenced/capped/available stub this file carried before.
//! [`reviewer_seat_display`] layers Python's own `reviewer_seat_summary`
//! rule on top: a fenced seat shortens to `"{name} fenced"`, an
//! otherwise-usable seat below the Critic's *own*, usually stricter
//! quota floor (`[review_policy].min_quota_percent`, distinct from the
//! fleet-wide `dispatch_min_quota_percent` floor `account_state`'s own
//! DRAINING check uses) reads `"{name} below quota floor (...)"`, a capped
//! seat always shows `{open}/{cap}` regardless of which of those two
//! branches it would otherwise have hit, and anything else falls through
//! to the bare state text. Same residual gaps as `account_state` itself
//! (no at-capacity reading, no gauge-staleness/manual-reset gating) — see
//! this task's result.md.

use std::collections::HashSet;
use std::path::Path;

use clap::ArgMatches;
use mv_config::load::{load_config, resolve_config_path_from_env, Config, ReviewPolicy};
use mv_config::roles::Role;
use mv_engine::{seatstate, selector};
use mv_state::gauge::GaugeMap;

use crate::commands::accounts::account_state;
use crate::exit::Failure;

pub trait ReviewPolicySource {
    /// `lane` is `--lane TASK_ID`'s value, when given; `config_override` is
    /// `--config PATH`'s value. Only [`Failure::Config`] may be returned —
    /// `review-policy` never exits `ERROR`.
    fn resolve(
        &self,
        lane: Option<&str>,
        config_override: Option<&Path>,
    ) -> Result<String, Failure>;
}

struct LiveConfig;

impl ReviewPolicySource for LiveConfig {
    fn resolve(
        &self,
        lane: Option<&str>,
        config_override: Option<&Path>,
    ) -> Result<String, Failure> {
        let path = resolve_config_path_from_env(config_override)
            .map_err(|error| Failure::Config(format!("cannot resolve the config path: {error}")))?;
        let config = load_config(&path).map_err(|error| Failure::Config(error.to_string()))?;
        if let Some(lane) = lane {
            return format_lane_risk(&config, lane);
        }
        let state_dir = mv_config::load::config_state_dir_from_env(&path);
        format_report(&config, &state_dir, mv_core::time::now_utc())
    }
}

/// Resolve the lane only in cwd's configured project, falling back to the
/// sole configured project when cwd is outside it (`cli.py:10846-10864`).
fn format_lane_risk(config: &Config, lane: &str) -> Result<String, Failure> {
    let cwd = std::env::current_dir().map_err(|error| Failure::Config(error.to_string()))?;
    format_lane_risk_at(config, lane, &cwd)
}

fn format_lane_risk_at(config: &Config, lane: &str, cwd: &Path) -> Result<String, Failure> {
    let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let project = config.projects.values().filter_map(|project| {
        let root = std::fs::canonicalize(project.path.value()?).ok()?;
        cwd.starts_with(&root).then_some((project, root.components().count()))
    }).max_by_key(|(_, depth)| *depth).map(|(project, _)| project)
        .or_else(|| (config.projects.len() == 1).then(|| config.projects.values().next()).flatten())
        .ok_or_else(|| Failure::AccountSelection(
            "no project named, and cwd is outside every configured project: --lane needs one to diff against. Run this from inside a project.".into()
        ))?;
    let root = project
        .path
        .value()
        .ok_or_else(|| Failure::Config("project path is unresolved".into()))?;
    let diff =
        mv_engine::lane_diff::lane_diff(Path::new(root), lane, config.review_policy.max_diff_lines)
            .map_err(|error| Failure::AccountSelection(error.to_string()))?;
    let areas = mv_engine::risk_paths::risk_areas_touched(
        &config.review_policy.risk_paths,
        &diff.touched_files,
        &diff.body_text,
    );
    Ok(if areas.is_empty() {
        format!("{lane}: low (batch list only)")
    } else {
        format!("{lane}: {}", areas.join(", "))
    })
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

/// Python `config.render_review_policy`, with every role word spelled
/// current (card `20261103-b-6694b2` plus this lane's remaining
/// common-noun replacements). Title-case positions use
/// [`Role::display_name`] (`Critic`); lowercase common-noun positions
/// (`a critic still running`, `the nexus already ran`) use
/// [`Role::config_key`] so they match `translate_prose_roles` applied to
/// Python's live output.
pub(super) fn format_policy(policy: &ReviewPolicy) -> String {
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

/// Whichever window (or both) of a reviewer seat's last-read gauge sits
/// *strictly below* `floor_percent`, named the way `accounts.py`'s
/// `_quota_floor_detail` phrases it — the secondary-only case names only
/// the secondary number, never a healthy primary one. `None` when neither
/// window is known or below floor. Deliberately `<`, not `account_state`'s
/// own `<=`: Python keeps these two floors as separate comparisons
/// (`_quota_floor_gauge` vs. `is_draining`), and a `floor_percent` of `0`
/// self-disables here the same way it does there — no reading is ever
/// strictly below zero.
fn below_quota_floor_detail(gauges: &GaugeMap, pool: &str, floor_percent: f64) -> Option<String> {
    let entry = gauges.get(pool)?;
    let primary = entry.percent_left.filter(|p| *p < floor_percent);
    let secondary = entry.percent_left_secondary.filter(|p| *p < floor_percent);
    match (primary, secondary) {
        (None, None) => None,
        (None, Some(secondary)) => Some(format!("{secondary}% left")),
        (Some(primary), Some(secondary)) => Some(format!("{primary}% left · {secondary}% left")),
        (Some(primary), None) => Some(format!("{primary}% left")),
    }
}

/// One reviewer seat's display for the no-`--lane` report — Python's
/// `reviewer_seat_summary`, one chain entry at a time. Builds on
/// [`account_state`] (the same state `multiverse accounts` itself shows)
/// rather than recomputing fenced/draining/lent-off from scratch, so the
/// two surfaces can never disagree about what a seat's own state means;
/// see this file's module doc for the full precedence and the residual
/// gaps both inherit from `account_state`.
fn reviewer_seat_display(
    config: &Config,
    state_dir: &Path,
    gauges: &GaugeMap,
    name: &str,
) -> String {
    let Some(account) = config.accounts.get(name) else {
        return format!("{name} (unknown account)");
    };
    // Resolved once and threaded into `account_state`'s own cap check
    // (card `20261105-fixer-9d070c`) rather than calling
    // `seatstate::live_open_count` a second time below — one `agent list`
    // round trip per capped seat, not two.
    let cap = seatstate::get_cap(state_dir, name);
    let live_open_count = cap.and_then(|_| seatstate::live_open_count(&account.short));
    let state = account_state(
        config,
        state_dir,
        gauges,
        account,
        live_open_count,
        &selector::HttpLocalLivenessProbe,
    );
    if state.starts_with("FENCED") {
        return format!("{name} fenced");
    }
    if state == "available" || state.starts_with("DRAINING") {
        let floor = config.review_policy.min_quota_percent as f64;
        if let Some(detail) = below_quota_floor_detail(gauges, &account.pool, floor) {
            return format!(
                "{name} below quota floor ({detail} < {}%)",
                config.review_policy.min_quota_percent
            );
        }
    }
    if let Some(cap) = cap {
        return match live_open_count {
            Some(open) => format!("{name} {open}/{cap}"),
            None => format!("{name} ?/{cap}"),
        };
    }
    format!("{name} {state}")
}

/// `recent_reviewer_dispatch_count`'s own window/role filter
/// (`mv_engine::dispatch`, out of this task's write scope — see this
/// file's module doc), corrected for finding 5: a dispatch whose task
/// later recorded `state=failed-to-start` never actually occupied a
/// reviewer seat and must not count against the window's budget.
/// Reimplemented over the same routing log rather than editing
/// `dispatch.rs`.
fn recent_reviewer_dispatch_count(
    root: &Path,
    now: time::OffsetDateTime,
) -> std::io::Result<usize> {
    let path = mv_state::log::ProjectLog::new(root);
    let text = match std::fs::read_to_string(path.path()) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let since = now - time::Duration::hours(24);
    let mut failed_to_start: HashSet<String> = HashSet::new();
    let mut dispatches: Vec<(String, String, time::OffsetDateTime)> = Vec::new();
    for record in text
        .lines()
        .filter_map(|line| mv_state::log::parse(line).ok())
    {
        let Some(task) = record.fields.get("task").cloned() else {
            continue;
        };
        match record.event() {
            "dispatch" => {
                // A redispatch reusing this task id starts over: the
                // previous round's failed-to-start verdict is not this
                // round's.
                failed_to_start.remove(&task);
                if let (Some(role), Ok(when)) = (
                    record.fields.get("role").cloned(),
                    mv_core::time::parse_flexible(&record.when),
                ) {
                    dispatches.push((task, role, when));
                }
            }
            "result"
                if record.fields.get("state").map(String::as_str) == Some("failed-to-start") =>
            {
                failed_to_start.insert(task);
            }
            _ => {}
        }
    }
    Ok(dispatches
        .into_iter()
        .filter(|(task, role, when)| {
            matches!(role.as_str(), "reviewer" | "critic")
                && *when >= since
                && *when <= now
                && !failed_to_start.contains(task)
        })
        .count())
}

fn format_report(
    config: &Config,
    state_dir: &Path,
    now: time::OffsetDateTime,
) -> Result<String, Failure> {
    let critic_noun = Role::Critic.config_key();
    let mut text = format!(
        "config: {}\n\n{}\n\n{critic_noun} seats:\n",
        config.source_path.display(),
        format_policy(&config.review_policy)
    );
    let gauges = mv_state::gauge::load(state_dir);
    let mut projects: Vec<&mv_config::load::Project> = config.projects.values().collect();
    projects.sort_by(|a, b| a.name.cmp(&b.name));
    let mut configured = false;
    for project in &projects {
        if let Some(chain) = project
            .chains
            .get(&Role::Critic)
            .filter(|chain| !chain.is_empty())
        {
            configured = true;
            let seats: Vec<String> = chain
                .iter()
                .map(|name| reviewer_seat_display(config, state_dir, &gauges, name))
                .collect();
            text.push_str(&format!("  {}: {}\n", project.name, seats.join(", ")));
        }
    }
    if !configured {
        text.push_str(&format!(
            "  (no project configures a {critic_noun} chain)\n"
        ));
    }
    text.push_str(&format!("\n{critic_noun} dispatch budget:\n"));
    // Python `cmd_review_policy`: budget 0 is one fleet-wide line, not a
    // per-project used/unlimited row.
    if config.review_policy.budget_per_window == 0 {
        text.push_str("  unlimited (review_budget_per_window = 0)\n");
    } else {
        let budget = config.review_policy.budget_per_window;
        for project in &projects {
            let used = match project.path.value() {
                Some(root) => recent_reviewer_dispatch_count(Path::new(root), now)
                    .map_err(|error| {
                        Failure::Config(format!(
                            "cannot read {critic_noun} dispatch history: {error}"
                        ))
                    })?
                    .to_string(),
                None => "unknown".into(),
            };
            text.push_str(&format!(
                "  {}: {used}/{budget} in the last 24h\n",
                project.name
            ));
        }
    }
    Ok(text.trim_end().to_owned())
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    run_with(args, &LiveConfig)
}

fn run_with(args: &ArgMatches, source: &dyn ReviewPolicySource) -> Result<(), Failure> {
    let lane = args.get_one::<String>("lane").map(String::as_str);
    let config_override = args
        .get_one::<String>("config")
        .map(|s| Path::new(s.as_str()));
    let policy = source.resolve(lane, config_override)?;
    println!("{policy}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;
    use std::cell::RefCell;

    struct Fixed(&'static str);
    impl ReviewPolicySource for Fixed {
        fn resolve(&self, _lane: Option<&str>, _config: Option<&Path>) -> Result<String, Failure> {
            Ok(self.0.to_owned())
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("review-policy").unwrap().clone()
    }

    struct TempFile(std::path::PathBuf);
    impl TempFile {
        fn write(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mv-cli-review-policy-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn report_uses_policy_overrides_and_configured_reviewer_chain() {
        let root = tempfile::tempdir().unwrap();
        let content = format!("[projects.sandbox]\npath = {:?}\n[review_policy]\nrereview = \"full\"\nbatch_low_risk = false\nreview_budget_per_window = 7\n", root.path().to_str().unwrap());
        let mut config =
            mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();
        config
            .projects
            .get_mut("sandbox")
            .unwrap()
            .chains
            .insert(Role::Critic, vec!["review-seat".into()]);
        let report = format_report(&config, root.path(), mv_core::time::now_utc()).unwrap();
        assert!(report.contains("config: config.toml"));
        assert!(report.contains(
            "Re-review after REVISE: full — the whole change again, not just the fix-round diff."
        ));
        assert!(report.contains("Low-risk batching: off — every lane reviews on its own."));
        assert!(report.contains("sandbox: review-seat"));
        assert!(!report.contains("no project configures"));
        assert!(report.contains("sandbox: 0/7 in the last 24h"));
        assert!(!root.path().join(".multiverse").exists());
    }

    #[test]
    fn format_policy_matches_python_prose_with_critic_display_name() {
        let policy = ReviewPolicy::default();
        let text = format_policy(&policy);
        assert!(text.starts_with(
            "Cross-vendor review required for: security, auth, migration, deletion, secrets, release."
        ));
        assert!(text.contains(
            "Re-review after REVISE: diff — only the fix-round diff plus the original findings, not the whole change again."
        ));
        assert!(text.contains(
            "Low-risk batching: on — up to 6 merged low-risk lane(s) accumulate for one Critic over several lanes."
        ));
        assert!(text.contains("After 2 REVISE round(s), stop asking for another review and escalate to the nexus instead."));
        assert!(text.contains(
            "Critic effort: medium. Output: terse — verdict plus numbered findings, no prose."
        ));
        assert!(text.contains(
            "Timeout: 30 minute(s) — `multiverse sweep` flags a critic still running past this."
        ));
        assert!(text.contains("Minimum quota to dispatch: 20%"));
        assert!(text.contains("Run tests: no — the nexus already ran the mutations"));
        assert!(text.contains("Critic dispatch budget: 3 per project per rolling 24h window"));
        assert!(!text.contains("effective review policy:"));
        assert!(!text.contains("Reviewer"));
        assert!(!text.contains("reviewer"));
        assert!(!text.contains("orchestrator"));
        assert!(!text.contains("Orchestrator"));
    }

    #[test]
    fn format_report_uses_current_role_names_in_every_heading() {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n",
            root.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();
        let report = format_report(&config, root.path(), mv_core::time::now_utc()).unwrap();
        assert!(report.contains("critic seats:"));
        assert!(report.contains("(no project configures a critic chain)"));
        assert!(report.contains("critic dispatch budget:"));
        assert!(!report.contains("reviewer seats:"));
        assert!(!report.contains("reviewer chain"));
        assert!(!report.contains("reviewer dispatch budget:"));
        assert!(!report.contains("orchestrator"));
    }

    #[test]
    fn format_policy_warns_on_unknown_risk_areas() {
        let policy = ReviewPolicy {
            require_cross_vendor_for: vec!["security".into(), "vibes".into()],
            ..ReviewPolicy::default()
        };
        let text = format_policy(&policy);
        assert!(text.contains("warning: require_cross_vendor_for names risk area(s) not in the known set (auth, deletion, migration, release, secrets, security): vibes."));
    }

    #[test]
    fn format_report_budget_zero_is_one_unlimited_line() {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy]\nreview_budget_per_window = 0\n",
            root.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();
        let report = format_report(&config, root.path(), mv_core::time::now_utc()).unwrap();
        assert!(report.contains("unlimited (review_budget_per_window = 0)"));
        assert!(!report.contains("sandbox: 0/unlimited"));
    }

    #[test]
    fn a_missing_config_file_needs_config_not_a_bare_error() {
        // Deliberately never calls `resolve_config_path_from_env` with no
        // override: this machine may have a real
        // `~/.config/multiverse/config.toml` (the operator's live fleet
        // config), and this test must never read it. Always pass an
        // explicit, definitely-absent path instead.
        let missing = std::env::temp_dir().join(format!(
            "mv-cli-review-policy-test-missing-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let err = LiveConfig.resolve(None, Some(&missing)).unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn a_real_config_file_is_loaded_and_its_defaults_are_shown() {
        let file = TempFile::write("defaults", "");
        let m = matches(&["--config", file.0.to_str().unwrap(), "review-policy"]);
        let text = {
            let lane = m.get_one::<String>("lane").map(String::as_str);
            let config_override = m.get_one::<String>("config").map(|s| Path::new(s.as_str()));
            LiveConfig.resolve(lane, config_override).unwrap()
        };
        assert!(text.contains("Review scope: diff"));
        assert!(text.contains("Critic effort: medium"));
        assert!(text.contains("Minimum quota to dispatch: 20%"));
    }

    #[test]
    fn a_malformed_config_file_needs_config_not_a_bare_error() {
        let file = TempFile::write("malformed", "not = [valid");
        let config_override = Some(file.0.as_path());
        let err = LiveConfig.resolve(None, config_override).unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn lane_flag_reaches_the_source() {
        struct Capture(RefCell<Option<String>>);
        impl ReviewPolicySource for Capture {
            fn resolve(
                &self,
                lane: Option<&str>,
                _config: Option<&Path>,
            ) -> Result<String, Failure> {
                *self.0.borrow_mut() = lane.map(str::to_owned);
                Ok("policy".into())
            }
        }
        let capture = Capture(RefCell::new(None));
        run_with(&matches(&["review-policy", "--lane", "lane-1"]), &capture).unwrap();
        assert_eq!(capture.0.borrow().as_deref(), Some("lane-1"));
    }

    #[test]
    fn a_resolved_policy_prints_and_succeeds() {
        assert!(run_with(&matches(&["review-policy"]), &Fixed("essay text")).is_ok());
    }

    // -- finding 5: `--lane` risk classification -------------------------

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    /// A real repo with a `multiverse/<task>` branch carrying one commit
    /// that touches `path` beyond the initial commit — enough for
    /// [`mv_engine::lane_diff::lane_diff`] to diff against. Also records
    /// `refs/multiverse/base/<task>` at the cut point, the same ref a real
    /// `Worktrees::create` dispatch writes — needed so a merged-lane
    /// recovery test (branch reclaimed after merge) still has a base to
    /// diff the recovered tip against.
    fn repo_with_lane_branch(task: &str, path: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.invalid"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "initial"]);
        git(
            dir.path(),
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        git(dir.path(), &["branch", &format!("multiverse/{task}")]);
        git(
            dir.path(),
            &["checkout", "-q", &format!("multiverse/{task}")],
        );
        std::fs::create_dir_all(dir.path().join(Path::new(path).parent().unwrap())).unwrap();
        std::fs::write(dir.path().join(path), "changed\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        dir
    }

    #[test]
    fn lane_flag_classifies_risk_from_the_lane_diff_and_review_risk_paths() {
        let repo = repo_with_lane_branch("20261101-fixer-abcdef", "src/auth/login.rs");
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"src/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef").unwrap(),
            "20261101-fixer-abcdef: auth"
        );
    }

    #[test]
    fn lane_flag_reports_low_when_nothing_touched_matches_a_risk_area() {
        let repo = repo_with_lane_branch("20261101-fixer-abcdef", "docs/readme.md");
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"src/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef").unwrap(),
            "20261101-fixer-abcdef: low (batch list only)"
        );
    }

    #[test]
    fn lane_flag_reports_plainly_when_no_project_has_the_branch() {
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q", "-b", "main"]);
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef")
                .unwrap_err()
                .code(),
            3
        );
    }

    #[test]
    fn live_config_resolve_with_a_lane_prints_only_the_risk_line() {
        let repo = repo_with_lane_branch("20261101-fixer-abcdef", "src/auth/login.rs");
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"src/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config_path = repo.path().join("config.toml");
        std::fs::write(&config_path, content).unwrap();

        let report = LiveConfig
            .resolve(Some("20261101-fixer-abcdef"), Some(&config_path))
            .unwrap();
        assert_eq!(report, "20261101-fixer-abcdef: auth");
    }

    // -- card 20261102-b-4f1cb3: content-risk patterns and merged-lane
    //    recovery for `--lane` -----------------------------------------

    #[test]
    fn lane_flag_classifies_risk_from_a_content_pattern_with_no_path_hint() {
        // `src/login.rs` matches no configured `auth` glob at all; only
        // the added `def login` line gives it away.
        let repo = repo_with_lane_branch_content(
            "20261101-fixer-abcdef",
            "src/login.rs",
            "fn login_handler() {}\ndef login():\n    pass\n",
        );
        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"**/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef").unwrap(),
            "20261101-fixer-abcdef: auth"
        );
    }

    /// Same shape as `repo_with_lane_branch`, but the lane's one commit
    /// writes `contents` to `path` rather than a fixed placeholder, so a
    /// test can control exactly what line a content pattern must match.
    fn repo_with_lane_branch_content(task: &str, path: &str, contents: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "t@example.invalid"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "hi\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "initial"]);
        git(
            dir.path(),
            &[
                "update-ref",
                &format!("refs/multiverse/base/{task}"),
                "HEAD",
            ],
        );
        git(dir.path(), &["branch", &format!("multiverse/{task}")]);
        git(
            dir.path(),
            &["checkout", "-q", &format!("multiverse/{task}")],
        );
        std::fs::create_dir_all(dir.path().join(Path::new(path).parent().unwrap())).unwrap();
        std::fs::write(dir.path().join(path), contents).unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-q", "-m", "work"]);
        git(dir.path(), &["checkout", "-q", "main"]);
        dir
    }

    #[test]
    fn lane_flag_recovers_a_merged_lane_once_its_branch_is_reclaimed() {
        let task = "20261101-fixer-abcdef";
        let branch = format!("multiverse/{task}");
        let repo = repo_with_lane_branch("20261101-fixer-abcdef", "src/auth/login.rs");
        git(
            repo.path(),
            &[
                "merge",
                "--no-ff",
                "-m",
                &format!("Merge branch '{branch}'"),
                &branch,
            ],
        );
        // `--prune` reclaims a merged lane's branch with `git branch -d`;
        // the diff must still resolve against the merge commit's own
        // second parent once that happens.
        git(repo.path(), &["branch", "-D", &branch]);

        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"src/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef").unwrap(),
            "20261101-fixer-abcdef: auth"
        );
    }

    #[test]
    fn lane_flag_recovers_a_fast_forwarded_lane_from_its_task_trailer() {
        let task = "20261101-fixer-abcdef";
        let branch = format!("multiverse/{task}");
        let repo = repo_with_lane_branch("20261101-fixer-abcdef", "src/auth/login.rs");
        // Overwrite the lane's one commit with a `Task:` trailer so
        // `_merged_lane_tip`'s second recovery path has something to find
        // once the fast-forward leaves no merge commit at all.
        git(
            repo.path(),
            &["checkout", "-q", "-b", "amend-trailer", &branch],
        );
        git(
            repo.path(),
            &[
                "commit",
                "--amend",
                "-q",
                "-m",
                &format!("work\n\nTask: {task}\n"),
            ],
        );
        git(repo.path(), &["branch", "-f", &branch, "amend-trailer"]);
        git(repo.path(), &["checkout", "-q", "main"]);
        git(repo.path(), &["branch", "-D", "amend-trailer"]);
        git(repo.path(), &["merge", "--ff-only", &branch]);
        git(repo.path(), &["branch", "-D", &branch]);

        let content = format!(
            "[projects.sandbox]\npath = {:?}\n[review_policy.review_risk_paths]\nauth = [\"src/auth/**\"]\n",
            repo.path().to_str().unwrap()
        );
        let config = mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();

        assert_eq!(
            format_lane_risk(&config, "20261101-fixer-abcdef").unwrap(),
            "20261101-fixer-abcdef: auth"
        );
    }

    // -- card 20261102-b-c459b1: reviewer seats match `account_state` -------

    #[test]
    fn reviewer_seat_display_shows_fenced_once_the_pool_breaker_is_open() {
        let state_dir = tempfile::tempdir().unwrap();
        let content = r#"
[accounts.review-seat]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
pool = "claude:default"
"#;
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        let gauges = GaugeMap::new();

        assert_eq!(
            reviewer_seat_display(&config, state_dir.path(), &gauges, "review-seat"),
            "review-seat available"
        );

        mv_engine::breaker::manual_trip(state_dir.path(), "claude:default", "claude", None, None)
            .unwrap();
        assert_eq!(
            reviewer_seat_display(&config, state_dir.path(), &gauges, "review-seat"),
            "review-seat fenced"
        );
    }

    #[test]
    fn reviewer_seat_display_shows_open_count_over_cap_once_set() {
        let _guard = HerdrEnvGuard::unreachable();
        let state_dir = tempfile::tempdir().unwrap();
        let content = r#"
[accounts.review-seat]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
pool = "claude:default"
"#;
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        mv_engine::seatstate::set_cap(state_dir.path(), "review-seat", Some(2)).unwrap();

        assert_eq!(
            reviewer_seat_display(&config, state_dir.path(), &GaugeMap::new(), "review-seat"),
            "review-seat ?/2"
        );
    }

    #[test]
    fn reviewer_seat_display_reports_below_its_own_quota_floor_before_a_cap() {
        let state_dir = tempfile::tempdir().unwrap();
        let content = r#"
[accounts.review-seat]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
pool = "claude:default"

[review_policy]
review_min_quota_percent = 20
"#;
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        let mut gauges = GaugeMap::new();
        gauges.insert(
            "claude:default".to_owned(),
            mv_state::gauge::GaugeEntry {
                percent_left: Some(5.0),
                ..Default::default()
            },
        );

        assert_eq!(
            reviewer_seat_display(&config, state_dir.path(), &gauges, "review-seat"),
            "review-seat below quota floor (5% left < 20%)"
        );
    }

    #[test]
    fn reviewer_seat_display_shows_lent_off_rather_than_a_bare_unavailable() {
        let state_dir = tempfile::tempdir().unwrap();
        let content = r#"
[accounts.review-seat]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"
pool = "claude:default"
"#;
        let config = mv_config::load::load_config_str(content, Path::new("config.toml")).unwrap();
        mv_engine::seatstate::set_lend(state_dir.path(), "review-seat", false).unwrap();

        assert_eq!(
            reviewer_seat_display(&config, state_dir.path(), &GaugeMap::new(), "review-seat"),
            "review-seat lent off — not in chains right now (`accounts lend` to change it)"
        );
    }

    #[test]
    fn reviewer_dispatch_count_skips_a_task_that_failed_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let now = mv_core::time::now_utc();
        let stamp = |minutes_ago: i64| {
            mv_core::time::format_seconds_utc(now - time::Duration::minutes(minutes_ago))
        };
        std::fs::create_dir_all(dir.path().join(".multiverse")).unwrap();
        std::fs::write(
            dir.path().join(".multiverse/routing.log"),
            format!(
                "{a_dispatch}\tevent=dispatch\ttask=20260101-reviewer-aaaaaa\trole=reviewer\n\
                 {a_result}\tevent=result\ttask=20260101-reviewer-aaaaaa\tstatus=failed\tstate=failed-to-start\n\
                 {b_dispatch}\tevent=dispatch\ttask=20260101-reviewer-bbbbbb\trole=reviewer\n",
                a_dispatch = stamp(10),
                a_result = stamp(9),
                b_dispatch = stamp(5),
            ),
        )
        .unwrap();

        assert_eq!(recent_reviewer_dispatch_count(dir.path(), now).unwrap(), 1);
    }
}

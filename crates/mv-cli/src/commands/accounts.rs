//! `multiverse accounts`: configured seats, pool breakers, and passive usage.
//! Listing is read-only; trip/reset use the engine's locked breaker store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::load::{self, Account, Config};
use mv_engine::mv_herdr::envelope::AgentState;
use mv_engine::{breaker, limits, seatstate, selector};
use mv_state::gauge::{GaugeEntry, GaugeMap};
use mv_state::nudge_memory;
use mv_state::tasks::{sweep_tasks, SUPERSEDED_MERGED, SUPERSEDED_REDISPATCHED};

use crate::exit::Failure;

pub trait AccountsSource {
    fn list(&self) -> Result<Vec<String>, Failure>;
    fn trip(
        &self,
        pool: &str,
        kind: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<String>, Failure>;
    fn reset(&self, pool: &str) -> Result<Vec<String>, Failure>;
    fn limit(&self, seat: &str, cap: Option<u32>) -> Result<(), Failure>;
    fn usage(&self, detail: Option<&str>) -> Result<Vec<String>, Failure>;
    fn lend(&self, seat: &str, lend: bool) -> Result<(), Failure>;
    fn prefer(&self, seat: Option<&str>, clear: bool) -> Result<(), Failure>;
}

#[cfg(test)]
struct Unconfigured;

fn unconfigured() -> Failure {
    Failure::Config("no accounts configured; run `multiverse doctor`".to_owned())
}

#[cfg(test)]
impl AccountsSource for Unconfigured {
    fn list(&self) -> Result<Vec<String>, Failure> {
        Err(unconfigured())
    }
    fn trip(
        &self,
        _pool: &str,
        _kind: Option<&str>,
        _until: Option<&str>,
    ) -> Result<Vec<String>, Failure> {
        Err(unconfigured())
    }
    fn reset(&self, _pool: &str) -> Result<Vec<String>, Failure> {
        Err(unconfigured())
    }
    fn limit(&self, _seat: &str, _cap: Option<u32>) -> Result<(), Failure> {
        Err(unconfigured())
    }
    fn usage(&self, _detail: Option<&str>) -> Result<Vec<String>, Failure> {
        Err(unconfigured())
    }
    fn lend(&self, _seat: &str, _lend: bool) -> Result<(), Failure> {
        Err(unconfigured())
    }
    fn prefer(&self, _seat: Option<&str>, _clear: bool) -> Result<(), Failure> {
        Err(unconfigured())
    }
}

struct LiveConfig {
    config: Config,
    state_dir: PathBuf,
}

impl LiveConfig {
    fn load(args: &ArgMatches) -> Result<Self, Failure> {
        let path =
            load::resolve_config_path_from_env(args.get_one::<String>("config").map(Path::new))
                .map_err(|error| Failure::Config(error.to_string()))?;
        let config =
            load::load_config(&path).map_err(|error| Failure::Config(error.to_string()))?;
        if config.accounts.is_empty() {
            return Err(unconfigured());
        }
        Ok(Self {
            config,
            state_dir: load::config_state_dir_from_env(&path),
        })
    }

    fn pool_vendor(&self, pool: &str, resetting: bool) -> Result<&str, Failure> {
        self.config
            .accounts
            .values()
            .find(|account| account.pool == pool)
            .map(|account| account.vendor.as_str())
            .ok_or_else(|| {
                if let Some(account) = self.config.accounts.get(pool) {
                    let tail = if resetting { "; nothing was reset" } else { "" };
                    Failure::AccountSelection(format!(
                        "{pool:?} is an account, not a pool — it uses pool {:?}. Try that instead{tail}.", account.pool
                    ))
                } else {
                    let pools = self.config.accounts.values().map(|a| a.pool.as_str()).collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(", ");
                    Failure::AccountSelection(format!("no configured account uses pool {pool:?}; configured pools: {pools}"))
                }
            })
    }

    /// Resolves a `seat` argument by its config name or its `short` code,
    /// the same lookup `commands::seat`'s `show` uses.
    fn seat_name(&self, seat: &str) -> Result<&str, Failure> {
        self.config
            .accounts
            .get(seat)
            .or_else(|| {
                self.config
                    .accounts
                    .values()
                    .find(|account| account.short == seat)
            })
            .map(|account| account.name.as_str())
            .ok_or_else(|| Failure::Operation(format!("unknown seat {seat:?}")))
    }

    /// `accounts usage --detail ACCOUNT` (`cli.py:9316-9358`): lookup by
    /// configured name, grok-only tables, unknown name is needs-config.
    fn usage_detail(&self, name: &str) -> Result<Vec<String>, Failure> {
        let account = self.config.accounts.get(name).ok_or_else(|| {
            let known = if self.config.accounts.is_empty() {
                "(none configured)".to_owned()
            } else {
                let mut names: Vec<_> = self.config.accounts.keys().cloned().collect();
                names.sort();
                names.join(", ")
            };
            Failure::Config(format!(
                "unknown account {name:?}; configured accounts: {known}"
            ))
        })?;
        if account.vendor != "grok" {
            return Ok(vec![format!(
                "{} is a {:?} seat — only grok has a per-day/per-project detail source today \
                 (card 20260930-b-57dbff).",
                account.name, account.vendor
            )]);
        }
        let Some(home) = account.config_dir.value() else {
            return Ok(vec![format!(
                "{}: no grok sessions found in the last {}d.",
                account.name,
                mv_engine::grok_usage::WINDOW_DAYS
            )]);
        };
        let Some(detail) = mv_engine::grok_usage::detail(Path::new(home)) else {
            return Ok(vec![format!(
                "{}: no grok sessions found in the last {}d.",
                account.name,
                mv_engine::grok_usage::WINDOW_DAYS
            )]);
        };
        let mut lines = vec![
            format!(
                "{} — grok usage detail (ESTIMATE; grok keeps no real token counter, card 20260930-b-57dbff)",
                account.name
            ),
            String::new(),
            "By day (UTC):".to_owned(),
            format!("  {:<12} {:>8} {:>14}", "DATE", "SESSIONS", "EST TOKENS"),
        ];
        for day in &detail.by_day {
            lines.push(format!(
                "  {:<12} {:>8} {:>14}",
                day.day,
                day.sessions,
                with_commas(day.estimated_tokens)
            ));
        }
        lines.push(String::new());
        lines.push(format!(
            "By project (last {}d):",
            mv_engine::grok_usage::WINDOW_DAYS
        ));
        if detail.by_project.is_empty() {
            lines.push("  (no session named a /Code/<project>/ cwd)".to_owned());
        } else {
            lines.push(format!(
                "  {:<24} {:>8} {:>14}",
                "PROJECT", "SESSIONS", "EST TOKENS"
            ));
            for project in &detail.by_project {
                lines.push(format!(
                    "  {:<24} {:>8} {:>14}",
                    project.project,
                    project.sessions,
                    with_commas(project.estimated_tokens)
                ));
            }
        }
        Ok(lines)
    }
}

impl AccountsSource for LiveConfig {
    fn list(&self) -> Result<Vec<String>, Failure> {
        let mut lines = vec![format!(
            "{:<18} {:<16} {:<12} CONFIG DIR",
            "ACCOUNT", "POOL", "STATE"
        )];
        let mut accounts: Vec<_> = self.config.accounts.values().collect();
        accounts.sort_by(|a, b| a.name.cmp(&b.name));

        let mut by_vendor: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for account in &accounts {
            by_vendor
                .entry(account.vendor.as_str())
                .or_default()
                .push(account.name.as_str());
        }
        let gauges = mv_state::gauge::load(&self.state_dir);
        // Python's `cmd_accounts` plain listing builds one `AccountSelector`
        // for the whole render with `read_only=_read_only_mode(args)`, and
        // every breaker check it makes — this account's own pool and each
        // same-vendor sibling's — persists the gauge-reopen/gauge-exhausted
        // corrections through that one flag. There is no `--read-only` flag
        // on `accounts` itself (only on report commands like `tasks`), so
        // `MULTIVERSE_READ_ONLY` is the only gate, same as `usage` below.
        let read_only = std::env::var("MULTIVERSE_READ_ONLY").is_ok_and(|value| value == "1");

        for account in accounts {
            if !read_only {
                breaker::pool_status_persisting(&self.state_dir, &account.pool);
            }
            let cap = seatstate::get_cap(&self.state_dir, &account.name);
            let live_open_count = cap.and_then(|_| seatstate::live_open_count(&account.short));
            let state = account_state(
                &self.config,
                &self.state_dir,
                &gauges,
                account,
                live_open_count,
                &selector::HttpLocalLivenessProbe,
            );
            let marker = if account.config_dir.is_placeholder() {
                "  ← placeholder"
            } else {
                ""
            };
            lines.push(format!(
                "{:<18} {:<16} {:<12} {}={}{marker}",
                account.name,
                account.pool,
                state,
                account.config_dir_var,
                account.config_dir.as_raw()
            ));

            if let Some(entry) = gauges.get(&account.pool) {
                for detail in quota_lines(entry) {
                    lines.push(format!("{:<18} ⛁ quota: {detail}", ""));
                }
            }
            if let Some(cap) = cap {
                match live_open_count {
                    Some(open) => lines.push(format!("{:<18} ⛁ open: {open}/{cap}", "")),
                    None => lines.push(format!("{:<18} ⛁ open: ?/{cap} (Herdr unreachable)", "")),
                }
            }
            if seatstate::is_preferred(&self.state_dir, &account.name) {
                lines.push(format!(
                    "{:<18} ⛁ prefer: ahead of its same-kind siblings \
                     (`accounts prefer --clear` to drop this)",
                    ""
                ));
            }

            let mut siblings: Vec<&str> = by_vendor
                .get(account.vendor.as_str())
                .into_iter()
                .flatten()
                .copied()
                .filter(|name| {
                    *name != account.name && self.config.accounts[*name].pool != account.pool
                })
                .collect();
            siblings.sort_unstable();
            for sibling_name in siblings {
                let sibling = &self.config.accounts[sibling_name];
                if !read_only {
                    breaker::pool_status_persisting(&self.state_dir, &sibling.pool);
                }
                let sibling_cap = seatstate::get_cap(&self.state_dir, &sibling.name);
                let sibling_live_open_count =
                    sibling_cap.and_then(|_| seatstate::live_open_count(&sibling.short));
                let sibling_state = account_state(
                    &self.config,
                    &self.state_dir,
                    &gauges,
                    sibling,
                    sibling_live_open_count,
                    &selector::HttpLocalLivenessProbe,
                );
                lines.push(format!(
                    "{:<18} ↳ same vendor ({}): {sibling_name} on {} — {sibling_state}",
                    "", account.vendor, sibling.pool
                ));
            }
        }
        Ok(lines)
    }

    fn trip(
        &self,
        pool: &str,
        kind: Option<&str>,
        until: Option<&str>,
    ) -> Result<Vec<String>, Failure> {
        let status = breaker::manual_trip(
            &self.state_dir,
            pool,
            self.pool_vendor(pool, false)?,
            kind,
            until,
        )
        .map_err(|error| Failure::Operation(error.to_string()))?;
        Ok(trip_confirmation(
            pool,
            kind.unwrap_or("rolling"),
            until,
            &status,
        ))
    }

    fn reset(&self, pool: &str) -> Result<Vec<String>, Failure> {
        self.pool_vendor(pool, true)?;
        let status = breaker::pool_status(&self.state_dir, pool);
        // Card `20261103-b-09f3fc` / Python `cli.py` cmd_accounts reset:
        // snapshot stranded panes fail-open *before* the breaker closes.
        // An unreachable Herdr (or a forbidden real-binary fallback) means
        // nothing is snapshotted, matching `listing.ok` being false.
        let exempted = snapshot_stranded_panes(&self.config, pool);
        breaker::manual_reset(&self.state_dir, pool)
            .map_err(|error| Failure::Operation(error.to_string()))?;
        let mut lines = reset_confirmation(pool, &status);
        if !exempted.is_empty() {
            lines.push(format!(
                "reset {pool}: {} still-stranded pane(s) exempted from \
                 re-tripping until they show new text: {}.",
                exempted.len(),
                exempted.join(", "),
            ));
        }
        Ok(lines)
    }

    fn usage(&self, detail: Option<&str>) -> Result<Vec<String>, Failure> {
        if let Some(name) = detail {
            return self.usage_detail(name);
        }
        let mut lines = vec![format!("{:<18} USAGE", "ACCOUNT")];
        let mut accounts: Vec<_> = self.config.accounts.values().collect();
        accounts.sort_by(|a, b| a.name.cmp(&b.name));
        for account in accounts {
            let read_only = std::env::var("MULTIVERSE_READ_ONLY").is_ok_and(|value| value == "1");
            let gauge = if matches!(account.vendor.as_str(), "grok" | "claude") {
                mv_state::gauge::load(&self.state_dir).remove(&account.pool)
            } else if matches!(account.vendor.as_str(), "codex" | "chatgpt") {
                account.config_dir.value().and_then(|dir| {
                    let reading = if read_only {
                        limits::read_codex_usage(Path::new(dir))
                    } else {
                        limits::read_and_record_codex_usage(
                            Path::new(dir),
                            &self.state_dir,
                            &account.pool,
                        )
                    }?;
                    Some(GaugeEntry {
                        percent_left: Some(reading.primary_percent_left),
                        label: Some("primary window".into()),
                        ..Default::default()
                    })
                })
            } else {
                None
            };
            let grok_summary = (account.vendor == "grok")
                .then(|| account.config_dir.value())
                .flatten()
                .and_then(|home| mv_engine::grok_usage::summarize(Path::new(home)));
            let suffix = if account.vendor == "grok" {
                grok_breaker_suffix(&self.state_dir, &account.pool)
            } else {
                String::new()
            };

            let mut pieces = Vec::new();
            if let Some(gauge) = gauge.as_ref() {
                if account.vendor == "claude" {
                    if let Some(piece) = format_claude_usage_piece(gauge) {
                        pieces.push(piece);
                    }
                } else if let Some(left) = gauge.percent_left {
                    let mut piece = format!("{}% left", display_percent(left));
                    if let Some(label) = gauge.label.as_ref().filter(|label| !label.is_empty()) {
                        piece.push_str(&format!(" · {label}"));
                    }
                    pieces.push(piece);
                }
            }
            if let Some(summary) = grok_summary.as_ref() {
                pieces.push(format_grok_usage_summary(summary));
            }

            if pieces.is_empty() {
                lines.push(format!("{:<18} unknown{suffix}", account.name));
            } else {
                lines.push(format!(
                    "{:<18} {}{suffix}",
                    account.name,
                    pieces.join(" · ")
                ));
            }
            if let Some(summary) = grok_summary.as_ref() {
                if summary.last_24h.sessions > u64::from(account.usage_warn_sessions_per_day) {
                    lines.push(format!(
                        "{:<18} WARN: {} sessions in 24h exceeds usage_warn_sessions_per_day={}",
                        "", summary.last_24h.sessions, account.usage_warn_sessions_per_day
                    ));
                }
            }
        }
        Ok(lines)
    }

    fn limit(&self, seat: &str, cap: Option<u32>) -> Result<(), Failure> {
        let name = self.seat_name(seat)?;
        seatstate::set_cap(&self.state_dir, name, cap)
            .map_err(|error| Failure::Operation(error.to_string()))?;
        match cap {
            Some(cap) => println!(
                "{name}: capped at {cap} open agent{} fleet-wide.",
                if cap == 1 { "" } else { "s" }
            ),
            None => println!("{name}: uncapped."),
        }
        Ok(())
    }
    fn lend(&self, seat: &str, lend: bool) -> Result<(), Failure> {
        let name = self.seat_name(seat)?;
        seatstate::set_lend(&self.state_dir, name, lend)
            .map_err(|error| Failure::Operation(error.to_string()))?;
        if lend {
            println!("{name}: lent — back in its chains.");
        } else {
            println!("{name}: not lent — pulled from every chain until turned back on.");
        }
        Ok(())
    }
    /// `--clear` names no seat (`args::command`'s "prefer" subcommand
    /// makes `seat` `required_unless_present("clear")`), so clearing
    /// means every configured seat, not one this call was told about —
    /// unpreferring all of them is the only way to guarantee no seat
    /// reads as preferred afterward.
    fn prefer(&self, seat: Option<&str>, clear: bool) -> Result<(), Failure> {
        if clear {
            for account in self.config.accounts.values() {
                seatstate::set_prefer(&self.state_dir, &account.name, false)
                    .map_err(|error| Failure::Operation(error.to_string()))?;
            }
            println!("cleared every seat preference — chains follow config order again.");
            return Ok(());
        }
        let seat = seat.ok_or_else(|| {
            Failure::Operation("accounts prefer requires a seat or --clear".to_owned())
        })?;
        let name = self.seat_name(seat)?;
        let kind = &self.config.accounts[name].kind;
        for other in self.config.accounts.values() {
            if other.name != name && other.kind == *kind {
                seatstate::set_prefer(&self.state_dir, &other.name, false)
                    .map_err(|error| Failure::Operation(error.to_string()))?;
            }
        }
        seatstate::set_prefer(&self.state_dir, name, true)
            .map_err(|error| Failure::Operation(error.to_string()))?;
        println!("{name}: ahead of every same-kind sibling in its chains now.");
        Ok(())
    }
}

/// One seat's STATE column, mirroring `accounts.py`'s `_account_state` in
/// the same precedence order: a config-level `available = false` is
/// absolute and checked first, then a still-future `unavailable_until`,
/// then the `lend` toggle (a human's own switch, checked ahead of the
/// breaker so a seat pulled from chains reads as "lent off", never as a
/// stale-looking "FENCED"), then the pool breaker (half-open, then open),
/// then the open-agent cap (`AccountSelector._availability` check 5), then
/// — only once nothing above said otherwise — whether the pool's own
/// last-read usage gauge has drained to or past `[dispatch_min_quota_
/// percent]`.
///
/// Card `20261105-fixer-9d070c`: the cap check takes `live_open_count`
/// already resolved by the caller — via [`seatstate::live_open_count`],
/// the same reader the adjacent "⛁ open: N/cap" line
/// (`LiveConfig::list`)/`review_policy::reviewer_seat_display` already call
/// for their own display — rather than making its own second `agent list`
/// round trip: Python's own `AccountSelector._open_counts` caches this
/// fleet-wide per listing, and a second independent live call here would
/// not only double this surface's own Herdr traffic but also desync this
/// function's own call count from Python's for a conformance scenario
/// scripting a fixed `agent list` reply. `None` reads as "not capped" when
/// [`seatstate::get_cap`] also says so, and as "capped but Herdr
/// unreachable" when it does not — the same fail-closed posture
/// `commands::dispatch`'s own `seat_from_account`/`live_open_counts` take
/// for the exact same signal: unreadable reads as at-cap, never as
/// available.
///
/// Card `20261106-b-255666`: the local-liveness gap this doc used to admit
/// (Python's `_availability` check 6, `accounts.py:1114-1123`) is closed —
/// a `vendor = "local"` seat is probed via `local_liveness_probe`
/// (verbatim Python wording from `_probe_ollama_liveness`, no caching,
/// matching that function's own no-cache behavior) right where check 6
/// sits in `_availability`'s precedence: after the cap check, before the
/// DRAINING gauge read (which `_availability` never reaches for a seat
/// check 6 already failed).
///
/// Gauge-staleness and manual-reset-tombstone gating on the DRAINING check
/// (`limits.gauge_is_stale` / `_manual_reset_at`) are applied in
/// [`draining_detail`].
///
/// `pub(crate)`: also the base `review_policy.rs`'s `reviewer_seat_display`
/// builds on for its fleet-wide "reviewer seats:" line — the one other
/// surface that needs to agree with `multiverse accounts` about what
/// "fenced"/"draining"/"lent off" mean for the exact same seat.
#[allow(clippy::too_many_arguments)]
pub(crate) fn account_state(
    config: &Config,
    state_dir: &Path,
    gauges: &GaugeMap,
    account: &Account,
    live_open_count: Option<u32>,
    local_liveness_probe: &dyn selector::LocalLivenessProbe,
) -> String {
    if !account.available {
        return "unavailable".to_owned();
    }
    if let Some(until) = account
        .unavailable_until
        .as_deref()
        .and_then(breaker::parse_stored_timestamp)
    {
        if mv_core::time::now_utc() < until {
            return format!(
                "unavailable until {}",
                mv_core::time::format_seconds_utc(until)
            );
        }
    }
    if !seatstate::is_lent(state_dir, &account.name) {
        return "lent off — not in chains right now (`accounts lend` to change it)".to_owned();
    }
    let status = breaker::pool_status(state_dir, &account.pool);
    if status.half_open {
        return format!(
            "HALF-OPEN (trial: {})",
            status.trial_task_id.as_deref().unwrap_or("unclaimed")
        );
    }
    if status.open {
        let label = if status.manual {
            "FENCED (manual) — "
        } else {
            "FENCED — "
        };
        return format!("{label}{}", breaker::describe(&account.pool, &status));
    }
    if let Some(cap) = seatstate::get_cap(state_dir, &account.name) {
        match live_open_count {
            None => {
                return format!(
                    "at cap: open-agent count unreadable (Herdr unreachable), cap is {cap}"
                );
            }
            Some(open) if open >= cap => {
                return format!("at cap: {open}/{cap} open agents");
            }
            Some(_) => {}
        }
    }
    if account.vendor == "local" {
        if let Err(reason) = local_liveness_probe.probe(&account.name, &account.health_url) {
            return reason;
        }
    }
    if config.dispatch_min_quota_percent > 0 {
        if let Some(detail) = draining_detail(
            gauges.get(&account.pool),
            config.dispatch_min_quota_percent as f64,
            mv_core::time::now_utc(),
            manual_reset_at(state_dir, &account.pool),
        ) {
            return format!("DRAINING — {detail}");
        }
    }
    "available".to_owned()
}

/// `accounts.py::_manual_reset_at`: the latest manual-reset tombstone for
/// `pool`, if any. A persisted gauge observed before this reset is no
/// longer evidence of draining (card `20261022-b-632789`) — a manual reset
/// closes the breaker but does not erase the last gauge snapshot, so
/// without this a seat a human just reset could read DRAINING again off a
/// reading from before the reset.
fn manual_reset_at(state_dir: &Path, pool: &str) -> Option<time::OffsetDateTime> {
    let entry = mv_state::breaker_store::load(state_dir).get(pool)?.clone();
    if entry.close_reason.as_deref() != Some("manual reset") {
        return None;
    }
    entry
        .closed_at
        .as_deref()
        .and_then(breaker::parse_stored_timestamp)
}

/// `limits.gauge_reading_is_before_reset`: whether a gauge observed at
/// `observed_at` predates `reset_at`. Missing either timestamp cannot
/// establish the ordering and so preserves the existing fail-open
/// behavior (never treats an unknown ordering as "before the reset").
fn gauge_reading_is_before_reset(
    observed_at: Option<time::OffsetDateTime>,
    reset_at: Option<time::OffsetDateTime>,
) -> bool {
    matches!((observed_at, reset_at), (Some(observed_at), Some(reset_at)) if observed_at <= reset_at)
}

/// `limits.GAUGE_MAX_AGE`: how long a reset-less gauge reading stays
/// current on its observation timestamp alone.
const GAUGE_MAX_AGE: time::Duration = time::Duration::hours(6);

/// `limits.gauge_is_stale`: whether a gauge window has already reset
/// (`resets_at` in the past), or — when it hasn't, or is unknown — its
/// `observed_at` is older than the shorter of [`GAUGE_MAX_AGE`] and the
/// window's own `window_minutes`. A missing `observed_at` cannot establish
/// age and so never expires on its own.
///
/// `pub(crate)`: `board_data::QuotaReading::at_or_below` reuses this same
/// helper for the ACCOUNTS card's DRAINING check, so a stale window the
/// `accounts` listing would ignore cannot still read DRAINING on the board.
pub(crate) fn gauge_is_stale(
    resets_at: Option<time::OffsetDateTime>,
    observed_at: Option<time::OffsetDateTime>,
    window_minutes: Option<u32>,
    now: time::OffsetDateTime,
) -> bool {
    if resets_at.is_some_and(|reset_at| reset_at <= now) {
        return true;
    }
    let Some(observed_at) = observed_at else {
        return false;
    };
    let mut max_age = GAUGE_MAX_AGE;
    if let Some(minutes) = window_minutes.filter(|minutes| *minutes > 0) {
        max_age = max_age.min(time::Duration::minutes(minutes.into()));
    }
    now - observed_at >= max_age
}

fn display_percent(value: f64) -> String {
    if value.is_finite() && value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// `cli.py` `_format_claude_usage_piece`: invert "% left" to "% used" at
/// render time, plus an optional UTC reset clock and label.
fn format_claude_usage_piece(gauge: &GaugeEntry) -> Option<String> {
    let left = gauge.percent_left?;
    let mut piece = format!("{}% used", display_percent(100.0 - left));
    if let Some(resets) = gauge.resets_at.as_deref().and_then(format_resets_utc) {
        piece.push_str(&format!(" · resets {resets}"));
    }
    if let Some(label) = gauge.label.as_ref().filter(|label| !label.is_empty()) {
        piece.push_str(&format!(" · {label}"));
    }
    Some(piece)
}

fn format_resets_utc(raw: &str) -> Option<String> {
    let parsed =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339).ok()?;
    let utc = parsed.to_offset(time::UtcOffset::UTC);
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}Z",
        utc.year(),
        u8::from(utc.month()),
        utc.day(),
        utc.hour(),
        utc.minute()
    ))
}

/// `cli.py` `_format_grok_usage_summary`.
fn format_grok_usage_summary(summary: &mv_engine::grok_usage::UsageSummary) -> String {
    let day = &summary.last_24h;
    let session_word = if day.sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    let mut parts = vec![format!(
        "24h: {} {session_word}, ~{} tokens (estimate)",
        day.sessions,
        with_commas(day.estimated_tokens)
    )];
    if !day.top_projects.is_empty() {
        let top = day
            .top_projects
            .iter()
            .map(|(project, count)| format!("{project} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        parts.push(format!("top: {top}"));
    }
    let week = &summary.last_7d;
    let week_word = if week.sessions == 1 {
        "session"
    } else {
        "sessions"
    };
    parts.push(format!(
        "7d: {} {week_word}, ~{} tokens (estimate)",
        week.sessions,
        with_commas(week.estimated_tokens)
    ));
    parts.join(" · ")
}

/// `cli.py` `_grok_breaker_suffix`: `" · FENCED — ..."` on the usage line.
fn grok_breaker_suffix(state_dir: &Path, pool: &str) -> String {
    let status = breaker::pool_status(state_dir, pool);
    if !status.open {
        return String::new();
    }
    let label = if status.manual {
        "FENCED (manual)"
    } else {
        "FENCED"
    };
    let mut detail = match status.reset_at {
        Some(reset) if reset.year() == breaker::NO_AUTOMATIC_RESET_YEAR => {
            "breaker open, no automatic reset".to_owned()
        }
        Some(reset) => format!(
            "breaker open, resets {}",
            reset
                .format(&time::format_description::well_known::Rfc3339)
                .unwrap_or_else(|_| reset.to_string())
        ),
        None => "breaker open, reset time unknown".to_owned(),
    };
    if status.gauge_would_reopen {
        detail.push_str("; gauge shows open; manual fence held");
    }
    format!(" · {label} — {detail}")
}

pub(super) fn with_commas(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, ch) in digits.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Whichever window (or both) of `entry` sits at or below `floor_percent`,
/// formatted the same way `accounts.py`'s `draining_detail` names it — the
/// secondary-only case names only the secondary number, never the healthy
/// primary one. `None` when neither window is known or drained.
///
/// Card `20261105-fixer-9d070c`: a reading from before `manual_reset_at`
/// (the pool's own last manual-reset tombstone) is discarded outright —
/// `gauge_reading_is_before_reset` — and each window is then checked
/// against [`gauge_is_stale`] on its own reset clock before its percentage
/// is trusted, mirroring `accounts.py`'s `is_draining`: a primary window
/// stale from its own reset must not hide a still-live, still-critical
/// secondary one, and vice versa.
fn draining_detail(
    entry: Option<&GaugeEntry>,
    floor_percent: f64,
    now: time::OffsetDateTime,
    manual_reset_at: Option<time::OffsetDateTime>,
) -> Option<String> {
    let entry = entry?;
    let observed_at = entry
        .observed_at
        .as_deref()
        .and_then(breaker::parse_stored_timestamp);
    if gauge_reading_is_before_reset(observed_at, manual_reset_at) {
        return None;
    }
    let resets_at = entry
        .resets_at
        .as_deref()
        .and_then(breaker::parse_stored_timestamp);
    let resets_at_secondary = entry
        .resets_at_secondary
        .as_deref()
        .and_then(breaker::parse_stored_timestamp);
    let primary = (!gauge_is_stale(resets_at, observed_at, entry.window_minutes, now))
        .then_some(entry.percent_left)
        .flatten()
        .filter(|p| *p <= floor_percent);
    let secondary = (!gauge_is_stale(
        resets_at_secondary,
        observed_at,
        entry.window_minutes_secondary,
        now,
    ))
    .then_some(entry.percent_left_secondary)
    .flatten()
    .filter(|p| *p <= floor_percent);
    match (primary, secondary) {
        (None, None) => None,
        (Some(primary), None) => Some(format!("{primary}% left (floor {floor_percent}%)")),
        (None, Some(secondary)) => Some(format!("{secondary}% left (floor {floor_percent}%)")),
        (Some(primary), Some(secondary)) => Some(format!(
            "{primary}% left · {secondary}% left (floor {floor_percent}%)"
        )),
    }
}

/// Every active quota window on `entry`, rendered the way `accounts.py`'s
/// `_gauge_quota_lines` labels them — `5h`/`weekly` for the two
/// `window_minutes` values this fleet actually uses, else the gauge's own
/// `label`, else a bare `quota`/`secondary` fallback — one line per window.
/// No staleness marker: this is the raw last-read number, unlike
/// [`draining_detail`]'s own DRAINING comparison, which gates on
/// [`gauge_is_stale`] before trusting it as current evidence.
fn quota_lines(entry: &GaugeEntry) -> Vec<String> {
    fn window_label(minutes: Option<u32>, label: Option<&str>, secondary: bool) -> String {
        match minutes {
            Some(300) => "5h".to_owned(),
            Some(10080) => "weekly".to_owned(),
            _ => label
                .map(str::to_owned)
                .unwrap_or_else(|| if secondary { "secondary" } else { "quota" }.to_owned()),
        }
    }
    let mut lines = Vec::new();
    if let Some(percent) = entry.percent_left {
        let label = window_label(entry.window_minutes, entry.label.as_deref(), false);
        let mut detail = format!("{percent}% left");
        if let Some(gauge_label) = &entry.label {
            detail.push_str(&format!(" · {gauge_label}"));
        }
        lines.push(format!("{label}: {detail}"));
    }
    if let Some(percent) = entry.percent_left_secondary {
        let label = window_label(
            entry.window_minutes_secondary,
            entry.label_secondary.as_deref(),
            true,
        );
        let mut detail = format!("{percent}% left");
        if let Some(gauge_label) = &entry.label_secondary {
            detail.push_str(&format!(" · {gauge_label}"));
        }
        lines.push(format!("{label}: {detail}"));
    }
    lines
}

/// Python `cli.py` cmd_accounts trip (~9113-9141): confirmation lines a
/// human operator sees on stdout. `--until` only announces the override
/// when `--kind` is not rolling (the default), matching
/// `kind is not limits.LimitKind.ROLLING`.
fn trip_confirmation(
    pool: &str,
    kind: &str,
    until: Option<&str>,
    status: &breaker::BreakerStatus,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(until) = until {
        if kind != "rolling" {
            lines.push(format!(
                "--until {until} overrides --kind {kind}'s cooldown; the explicit time was used."
            ));
        }
    }
    let no_automatic_reset = status
        .reset_at
        .is_some_and(|reset| reset.year() == breaker::NO_AUTOMATIC_RESET_YEAR);
    if no_automatic_reset {
        lines.push(format!(
            "tripped {pool}: no automatic reset (kind={kind}) — a \
             human has to resolve this and run `accounts reset`"
        ));
    } else if let Some(reset_at) = status.reset_at {
        lines.push(format!(
            "tripped {pool}: resets {}",
            mv_core::time::format_seconds_utc(reset_at)
        ));
    }
    lines.push(format!(
        "{pool}: this is a MANUAL fence — a later gauge reading \
         showing quota back will not reopen it; only `multiverse \
         accounts reset {pool}` (or this trip's own --until \
         passing) lifts it."
    ));
    lines
}

/// Python `cli.py` cmd_accounts reset (~9191-9199): one line saying
/// whether a breaker actually closed.
fn reset_confirmation(pool: &str, status: &breaker::BreakerStatus) -> Vec<String> {
    if status.open {
        let reset_at = status
            .reset_at
            .map(mv_core::time::format_seconds_utc)
            .unwrap_or_else(|| "unknown".to_owned());
        vec![format!(
            "reset {pool}: closed an open breaker (was resetting at {reset_at})."
        )]
    } else {
        vec![format!("reset {pool}: nothing to close (no open breaker).")]
    }
}

/// Fail-open `herdr agent list` before a reset, then snapshot every pane
/// still parked on `pool` into stranded-trip memory (card
/// `20261103-b-ffa660` / Python `stranded_panes_for_pool` +
/// `record_stranded_banner_at_reset`). An unreachable Herdr is the same
/// no-op Python takes when `listing.ok` is false. Returns
/// `{project}:{pane}` labels, sorted, for the exemption line.
fn snapshot_stranded_panes(config: &Config, pool: &str) -> Vec<String> {
    let Ok(listing) = mv_engine::mv_herdr::run::run(&mv_engine::mv_herdr::argv::agent_list(), None)
    else {
        return Vec::new();
    };
    if !listing.ok() {
        return Vec::new();
    }
    let Some(agents) = listing
        .payload
        .as_ref()
        .and_then(|payload| payload.get("result"))
        .and_then(|result| result.get("agents"))
        .and_then(|agents| agents.as_array())
    else {
        return Vec::new();
    };

    let mut live_panes: Vec<String> = Vec::new();
    let mut pane_status: BTreeMap<String, AgentState> = BTreeMap::new();
    for agent in agents {
        let Some(pane_id) = agent.get("pane_id").and_then(|v| v.as_str()) else {
            continue;
        };
        live_panes.push(pane_id.to_owned());
        if let Some(status) = agent
            .get("agent_status")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<AgentState>().ok())
        {
            pane_status.insert(pane_id.to_owned(), status);
        }
    }

    let mut exempted: Vec<String> = Vec::new();
    for name in &config.project_order {
        let Some(project) = config.projects.get(name) else {
            continue;
        };
        let Some(root) = project.path.value() else {
            continue;
        };
        let root = Path::new(root);
        for task in sweep_tasks(root) {
            let Some(account_name) = task.account.as_deref() else {
                continue;
            };
            let Some(account) = config.accounts.get(account_name) else {
                continue;
            };
            if account.pool != pool {
                continue;
            }
            let Some(pane) = task.pane.as_deref() else {
                continue;
            };
            if task.agent.is_none() {
                continue;
            }
            if task.recorded {
                let Some(state) = task.outcome_state.as_deref() else {
                    continue;
                };
                if state != SUPERSEDED_REDISPATCHED && state != SUPERSEDED_MERGED {
                    continue;
                }
            }
            if pane_status.get(pane) == Some(&AgentState::Working) {
                continue;
            }
            let read = mv_engine::mv_herdr::run::run(
                &mv_engine::mv_herdr::argv::agent_read(pane, Some("visible"), None),
                None,
            );
            let text = match read {
                Ok(result) if result.ok() => result.stdout,
                _ => continue,
            };
            if text.is_empty() {
                continue;
            }
            nudge_memory::record_stranded_banner_at_reset(
                root,
                pane,
                &text,
                Some(live_panes.as_slice()),
            );
            exempted.push(format!("{}:{pane}", project.name));
        }
    }
    exempted.sort();
    exempted
}

/// `accounts lend <seat> <state>`'s `state` positional: validated here,
/// before the source is consulted, since it is pure CLI input shape, not
/// business logic.
fn parse_lend_state(raw: &str) -> Result<bool, Failure> {
    match raw {
        "on" => Ok(true),
        "off" => Ok(false),
        other => Err(Failure::Operation(format!(
            "lend state must be \"on\" or \"off\", not {other:?}"
        ))),
    }
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    run_with(args, &LiveConfig::load(args)?)
}

fn run_with(args: &ArgMatches, source: &dyn AccountsSource) -> Result<(), Failure> {
    let Some((name, sub)) = args.subcommand() else {
        for line in source.list()? {
            println!("{line}");
        }
        return Ok(());
    };
    match name {
        "trip" => {
            let pool = sub.get_one::<String>("pool").expect("pool is required");
            for line in source.trip(
                pool,
                sub.get_one::<String>("kind").map(String::as_str),
                sub.get_one::<String>("until").map(String::as_str),
            )? {
                println!("{line}");
            }
            Ok(())
        }
        "reset" => {
            let pool = sub.get_one::<String>("pool").expect("pool is required");
            for line in source.reset(pool)? {
                println!("{line}");
            }
            Ok(())
        }
        "limit" => {
            let seat = sub.get_one::<String>("seat").expect("seat is required");
            let cap = (!sub.get_flag("off")).then(|| {
                *sub.get_one::<u32>("cap")
                    .expect("cap is required unless --off")
            });
            source.limit(seat, cap)
        }
        "usage" => {
            for line in source.usage(sub.get_one::<String>("detail").map(String::as_str))? {
                println!("{line}");
            }
            Ok(())
        }
        "lend" => {
            let seat = sub.get_one::<String>("seat").expect("seat is required");
            let state = sub.get_one::<String>("state").expect("state is required");
            let lend = parse_lend_state(state)?;
            source.lend(seat, lend)
        }
        "prefer" => {
            let seat = sub.get_one::<String>("seat").map(String::as_str);
            source.prefer(seat, sub.get_flag("clear"))
        }
        _ => unreachable!("clap only accepts accounts' own subcommands"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;
    use std::cell::RefCell;

    type TripCall = (String, Option<String>, Option<String>);

    #[derive(Default)]
    struct Fake {
        tripped: RefCell<Option<TripCall>>,
        reset: RefCell<Option<String>>,
        limited: RefCell<Option<(String, Option<u32>)>>,
        lent: RefCell<Option<(String, bool)>>,
        preferred: RefCell<Option<(Option<String>, bool)>>,
    }

    impl AccountsSource for Fake {
        fn list(&self) -> Result<Vec<String>, Failure> {
            Ok(vec!["seat-a: AVAILABLE".into()])
        }
        fn trip(
            &self,
            pool: &str,
            kind: Option<&str>,
            until: Option<&str>,
        ) -> Result<Vec<String>, Failure> {
            *self.tripped.borrow_mut() = Some((
                pool.into(),
                kind.map(str::to_owned),
                until.map(str::to_owned),
            ));
            Ok(Vec::new())
        }
        fn reset(&self, pool: &str) -> Result<Vec<String>, Failure> {
            *self.reset.borrow_mut() = Some(pool.into());
            Ok(Vec::new())
        }
        fn limit(&self, seat: &str, cap: Option<u32>) -> Result<(), Failure> {
            *self.limited.borrow_mut() = Some((seat.into(), cap));
            Ok(())
        }
        fn usage(&self, _detail: Option<&str>) -> Result<Vec<String>, Failure> {
            Ok(vec!["seat-a: 42% used".into()])
        }
        fn lend(&self, seat: &str, lend: bool) -> Result<(), Failure> {
            *self.lent.borrow_mut() = Some((seat.into(), lend));
            Ok(())
        }
        fn prefer(&self, seat: Option<&str>, clear: bool) -> Result<(), Failure> {
            *self.preferred.borrow_mut() = Some((seat.map(str::to_owned), clear));
            Ok(())
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("accounts").unwrap().clone()
    }

    #[test]
    fn unconfigured_bare_accounts_needs_config() {
        let err = run_with(&matches(&["accounts"]), &Unconfigured).unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn bare_accounts_lists_seats() {
        assert!(run_with(&matches(&["accounts"]), &Fake::default()).is_ok());
    }

    #[test]
    fn trip_reaches_the_source_with_kind_and_until() {
        let fake = Fake::default();
        run_with(
            &matches(&[
                "accounts",
                "trip",
                "chatgpt",
                "--kind",
                "weekly",
                "--until",
                "2030-01-01T00:00:00Z",
            ]),
            &fake,
        )
        .unwrap();
        assert_eq!(
            fake.tripped.borrow().as_ref().unwrap(),
            &(
                "chatgpt".to_owned(),
                Some("weekly".to_owned()),
                Some("2030-01-01T00:00:00Z".to_owned())
            )
        );
    }

    #[test]
    fn reset_reaches_the_source() {
        let fake = Fake::default();
        run_with(&matches(&["accounts", "reset", "chatgpt"]), &fake).unwrap();
        assert_eq!(fake.reset.borrow().as_deref(), Some("chatgpt"));
    }

    #[test]
    fn limit_off_clears_the_cap() {
        let fake = Fake::default();
        run_with(&matches(&["accounts", "limit", "scout-1", "--off"]), &fake).unwrap();
        assert_eq!(
            fake.limited.borrow().as_ref().unwrap(),
            &("scout-1".to_owned(), None)
        );
    }

    #[test]
    fn limit_with_a_value_sets_the_cap() {
        let fake = Fake::default();
        run_with(&matches(&["accounts", "limit", "scout-1", "3"]), &fake).unwrap();
        assert_eq!(
            fake.limited.borrow().as_ref().unwrap(),
            &("scout-1".to_owned(), Some(3))
        );
    }

    #[test]
    fn lend_accepts_on_and_off_and_rejects_anything_else() {
        let fake = Fake::default();
        run_with(&matches(&["accounts", "lend", "scout-1", "on"]), &fake).unwrap();
        assert_eq!(
            fake.lent.borrow().as_ref().unwrap(),
            &("scout-1".to_owned(), true)
        );
        run_with(&matches(&["accounts", "lend", "scout-1", "off"]), &fake).unwrap();
        assert_eq!(
            fake.lent.borrow().as_ref().unwrap(),
            &("scout-1".to_owned(), false)
        );
        let err = run_with(&matches(&["accounts", "lend", "scout-1", "maybe"]), &fake).unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn prefer_clear_and_seat_both_reach_the_source() {
        let fake = Fake::default();
        run_with(&matches(&["accounts", "prefer", "scout-1"]), &fake).unwrap();
        assert_eq!(
            fake.preferred.borrow().as_ref().unwrap(),
            &(Some("scout-1".to_owned()), false)
        );
        run_with(&matches(&["accounts", "prefer", "--clear"]), &fake).unwrap();
        assert_eq!(fake.preferred.borrow().as_ref().unwrap(), &(None, true));
    }

    #[test]
    fn usage_prints_each_line() {
        assert!(run_with(&matches(&["accounts", "usage"]), &Fake::default()).is_ok());
        assert!(run_with(
            &matches(&["accounts", "usage", "--detail", "grok"]),
            &Fake::default()
        )
        .is_ok());
    }

    #[test]
    fn pool_arguments_refuse_account_names_with_a_pool_hint() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        for reset in [false, true] {
            let error = if reset {
                live.reset("codex-a")
            } else {
                live.trip("codex-a", None, None)
            }
            .unwrap_err();
            assert_eq!(error.code(), 3);
            assert!(error.to_string().contains("is an account, not a pool"));
            assert!(error.to_string().contains("codex:solo"));
        }
        assert!(!dir.path().join("breaker-state.json").exists());
    }

    #[test]
    fn usage_header_and_unknown_match_the_python_oracle() {
        // `conformance/diff_fuzz.py` seed 9000: Python prints
        // `ACCOUNT            USAGE` then `<name>              unknown`.
        let dir = tempfile::tempdir().unwrap();
        let live = two_seat_live_config(dir.path());
        let lines = live.usage(None).unwrap();
        assert_eq!(lines[0], format!("{:<18} USAGE", "ACCOUNT"));
        assert!(
            lines.iter().any(|line| line.ends_with(" unknown")),
            "missing-source seats print 'unknown', not 'usage unknown': {lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("usage unknown")),
            "{lines:?}"
        );
    }

    #[test]
    fn usage_reads_persisted_vendor_gauges_without_probing() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = two_seat_live_config(dir.path());
        live.config.accounts.get_mut("scout-2").unwrap().vendor = "grok".into();
        let pool = live.config.accounts["scout-2"].pool.clone();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                pool,
                GaugeEntry {
                    percent_left: Some(42.0),
                    label: Some("daily window".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        assert!(live
            .usage(None)
            .unwrap()
            .iter()
            .any(|line| line.contains("42% left · daily window")));
    }

    /// Row #41 remainder: Claude inverts the gauge to "% used" at render
    /// time (`cli.py` `_format_claude_usage_piece`).
    #[test]
    fn usage_formats_claude_as_percent_used() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = two_seat_live_config(dir.path());
        live.config.accounts.get_mut("scout-1").unwrap().vendor = "claude".into();
        let pool = live.config.accounts["scout-1"].pool.clone();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                pool,
                GaugeEntry {
                    percent_left: Some(42.0),
                    resets_at: Some("2026-11-05T12:00:00Z".into()),
                    label: Some("session".into()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let lines = live.usage(None).unwrap();
        assert!(
            lines
                .iter()
                .any(|line| line.contains("58% used") && line.contains("resets 2026-11-05T12:00Z")),
            "{lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("42% left")),
            "{lines:?}"
        );
    }

    /// Row #41 remainder: Grok session ESTIMATE plus WARN when the 24h
    /// count exceeds `usage_warn_sessions_per_day`.
    #[test]
    fn usage_appends_grok_session_estimate_and_warn() {
        let dir = tempfile::tempdir().unwrap();
        let grok_home = dir.path().join("grok");
        let session = mv_engine::grok_usage::session_dir(&grok_home, "/home/dev/Code/demo", "s1");
        std::fs::create_dir_all(&session).unwrap();
        let created = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        std::fs::write(
            session.join("summary.json"),
            format!(r#"{{"cwd":"/home/dev/Code/demo","created_at":"{created}"}}"#),
        )
        .unwrap();
        std::fs::write(
            session.join("events.jsonl"),
            "{\"type\":\"loop_started\"}\n",
        )
        .unwrap();
        std::fs::write(session.join("chat_history.jsonl"), "xxxxxxxx").unwrap();

        let mut live = two_seat_live_config(dir.path());
        let account = live.config.accounts.get_mut("scout-2").unwrap();
        account.vendor = "grok".into();
        account.config_dir = mv_config::placeholder::MaybePlaceholder::parse(
            grok_home.to_string_lossy().into_owned(),
        );
        account.usage_warn_sessions_per_day = 0;
        let lines = live.usage(None).unwrap();
        assert!(
            lines.iter().any(|line| {
                line.contains("24h: 1 session") && line.contains("tokens (estimate)")
            }),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(
                |line| line.contains("WARN:") && line.contains("usage_warn_sessions_per_day=0")
            ),
            "{lines:?}"
        );
    }

    /// Row #41 remainder: Grok open breaker suffix uses FENCED / FENCED
    /// (manual), not `breaker::describe`'s pool-prefixed listing text.
    #[test]
    fn usage_grok_fenced_suffix_names_manual_fence() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = two_seat_live_config(dir.path());
        live.config.accounts.get_mut("scout-2").unwrap().vendor = "grok".into();
        let pool = live.config.accounts["scout-2"].pool.clone();
        breaker::manual_trip(dir.path(), &pool, "grok", Some("budget"), None).unwrap();
        let lines = live.usage(None).unwrap();
        let line = lines
            .iter()
            .find(|line| line.starts_with("scout-2 "))
            .cloned()
            .unwrap();
        assert!(line.contains("FENCED (manual)"), "{line}");
        assert!(line.contains("breaker open, no automatic reset"), "{line}");
        assert!(!line.contains("breaker open for pool"), "{line}");
    }

    #[test]
    fn usage_detail_unknown_account_is_needs_config() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_seat_live_config(dir.path());
        let err = live.usage(Some("no-such-account")).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(err.to_string().contains("unknown account"));
    }

    #[test]
    fn usage_detail_refuses_a_non_grok_account() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_seat_live_config(dir.path());
        let lines = live.usage(Some("scout-1")).unwrap();
        assert!(lines.iter().any(|l| l.contains("only grok")), "{lines:?}");
    }

    fn two_seat_live_config(state_dir: &Path) -> LiveConfig {
        let content = r#"
[accounts.scout-1]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "<<PLACEHOLDER: path>>"

[accounts.scout-2]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "<<PLACEHOLDER: path>>"
short = "s2"
"#;
        let config =
            load::load_config_str(content, Path::new("config.toml")).expect("valid fixture");
        LiveConfig {
            config,
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// `LiveConfig::prefer` used to always return `unconfigured()`
    /// (card `20261102-b-2f8d8e`) even with seats configured; this
    /// exercises the real write path through `seatstate::set_prefer`.
    #[test]
    fn live_config_prefer_sets_and_clears_every_configured_seat() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_seat_live_config(dir.path());

        live.prefer(Some("scout-1"), false).unwrap();
        assert!(seatstate::is_preferred(dir.path(), "scout-1"));
        assert!(!seatstate::is_preferred(dir.path(), "scout-2"));

        live.prefer(Some("scout-2"), false).unwrap();
        assert!(seatstate::is_preferred(dir.path(), "scout-2"));
        // Setting one seat's preference does not disturb another's —
        // only `--clear` unprefers every seat at once.
        assert!(seatstate::is_preferred(dir.path(), "scout-1"));

        live.prefer(None, true).unwrap();
        assert!(!seatstate::is_preferred(dir.path(), "scout-1"));
        assert!(!seatstate::is_preferred(dir.path(), "scout-2"));
    }

    #[test]
    fn prefer_replaces_only_same_kind_siblings() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = two_codex_pools_live_config(dir.path());
        let mut claude = live.config.accounts["codex-a"].clone();
        claude.name = "claude".into();
        claude.kind = "claude".into();
        live.config.accounts.insert(claude.name.clone(), claude);
        live.prefer(Some("claude"), false).unwrap();
        live.prefer(Some("codex-a"), false).unwrap();
        live.prefer(Some("codex-b"), false).unwrap();
        assert!(!seatstate::is_preferred(dir.path(), "codex-a"));
        assert!(seatstate::is_preferred(dir.path(), "codex-b"));
        assert!(seatstate::is_preferred(dir.path(), "claude"));
    }

    #[test]
    fn live_config_prefer_without_a_seat_or_clear_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_seat_live_config(dir.path());
        assert!(live.prefer(None, false).is_err());
    }

    // -- card 20261102-b-70e8c1: open-count, quota, same-vendor, lend-state --

    fn two_codex_pools_live_config(state_dir: &Path) -> LiveConfig {
        let content = r#"
[accounts.codex-a]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "<<PLACEHOLDER: path>>"
pool = "codex:solo"
short = "ca"

[accounts.codex-b]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "<<PLACEHOLDER: path>>"
pool = "codex:other"
short = "cb"
"#;
        let config =
            load::load_config_str(content, Path::new("config.toml")).expect("valid fixture");
        LiveConfig {
            config,
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn same_vendor_siblings_on_a_different_pool_get_their_own_line() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let lines = live.list().unwrap();
        assert!(lines.iter().any(|l| l.starts_with("codex-a ")));
        assert!(lines
            .iter()
            .any(|l| l.contains("↳ same vendor (codex): codex-b on codex:other — available")));
        assert!(lines
            .iter()
            .any(|l| l.contains("↳ same vendor (codex): codex-a on codex:solo — available")));
    }

    /// Card `<this task>` (scout2-gaps.md row #27): the default `accounts`
    /// listing — not just a fresh pure read — must persist the same two
    /// gauge-driven breaker corrections Python's `breaker.read_status(...,
    /// read_only=False)` does, via `AccountSelector`'s listing-wide
    /// `breaker_view`. Before this, `list()` only ever called the pure
    /// `breaker::pool_status`, so a reopened/auto-tripped pool never left a
    /// trace on `breaker-state.json` for anything else that reads the store
    /// directly (`check`, a later listing after the gauge reading itself
    /// goes stale) to see.
    #[test]
    fn bare_accounts_listing_persists_a_gauge_reopened_automatic_fence() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let now = time::OffsetDateTime::now_utc();
        mv_state::breaker_store::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                mv_state::breaker_store::BreakerEntry {
                    tripped_at: Some(mv_core::time::format_seconds_utc(
                        now - time::Duration::hours(2),
                    )),
                    reset_at: Some(mv_core::time::format_seconds_utc(
                        now + time::Duration::hours(3),
                    )),
                    vendor: Some("codex".to_owned()),
                    kind: Some("rolling".to_owned()),
                    caller: Some("runner".to_owned()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                GaugeEntry {
                    vendor: Some("codex".to_owned()),
                    percent_left: Some(50.0),
                    observed_at: Some(mv_core::time::format_seconds_utc(
                        now - time::Duration::minutes(5),
                    )),
                    ..Default::default()
                },
            );
        })
        .unwrap();

        let lines = live.list().unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("codex-a ") && l.contains("available")),
            "a fresh positive gauge reading must reopen the fence for this listing: {lines:?}"
        );

        let stored = mv_state::breaker_store::load(dir.path());
        let entry = &stored["codex:solo"];
        assert_eq!(
            entry.open,
            Some(false),
            "the listing must persist the reopen, not just compute it for this one read"
        );
        assert_eq!(
            entry.close_reason.as_deref(),
            Some("gauge shows window open")
        );
    }

    /// Companion to the test above: `MULTIVERSE_READ_ONLY=1` is the one gate
    /// Python's `cmd_accounts` checks before its listing persists anything
    /// (`accounts` has no `--read-only` flag of its own) — the listing must
    /// still show the reopened state, but must leave the store untouched.
    #[test]
    fn read_only_accounts_listing_computes_but_does_not_persist_the_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let now = time::OffsetDateTime::now_utc();
        let original = mv_state::breaker_store::BreakerEntry {
            tripped_at: Some(mv_core::time::format_seconds_utc(
                now - time::Duration::hours(2),
            )),
            reset_at: Some(mv_core::time::format_seconds_utc(
                now + time::Duration::hours(3),
            )),
            vendor: Some("codex".to_owned()),
            kind: Some("rolling".to_owned()),
            caller: Some("runner".to_owned()),
            ..Default::default()
        };
        mv_state::breaker_store::with_locked(dir.path(), |map| {
            map.insert("codex:solo".to_owned(), original.clone());
        })
        .unwrap();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                GaugeEntry {
                    vendor: Some("codex".to_owned()),
                    percent_left: Some(50.0),
                    observed_at: Some(mv_core::time::format_seconds_utc(
                        now - time::Duration::minutes(5),
                    )),
                    ..Default::default()
                },
            );
        })
        .unwrap();

        // Serialized against every other `MULTIVERSE_READ_ONLY` reader in
        // this binary the same way `HerdrEnvGuard` serializes
        // `HERDR_BIN_PATH` — `cargo test`'s default threaded runner shares
        // one process, so an unguarded env var here would race them.
        let _held = crate::commands::worktrees::herdr_env::lock();
        std::env::set_var("MULTIVERSE_READ_ONLY", "1");
        let result = live.list();
        std::env::remove_var("MULTIVERSE_READ_ONLY");
        let lines = result.unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("codex-a ") && l.contains("available")),
            "read-only still computes the live-correct answer for this one read: {lines:?}"
        );

        assert_eq!(
            mv_state::breaker_store::load(dir.path())["codex:solo"],
            original,
            "read-only must leave the store exactly as it found it"
        );
    }

    #[test]
    fn a_recorded_gauge_reading_prints_its_own_quota_line() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                GaugeEntry {
                    percent_left: Some(42.0),
                    window_minutes: Some(300),
                    ..Default::default()
                },
            );
        })
        .unwrap();

        let lines = live.list().unwrap();
        assert!(lines
            .iter()
            .any(|l| l.trim_start() == "⛁ quota: 5h: 42% left"));
    }

    /// Card `20261105-fixer-9d070c`: before this task, `draining_detail`
    /// trusted a gauge's raw percentage with no staleness check at all — a
    /// reading from well before the pool's own window last reset could
    /// still read DRAINING here, long after the vendor itself has moved on
    /// to a fresh window. `limits.gauge_is_stale`'s port fixes this: a
    /// `resets_at` already in the past (relative to `now`) makes the
    /// reading too old to trust.
    #[test]
    fn draining_ignores_a_gauge_whose_own_window_has_already_reset() {
        let dir = tempfile::tempdir().unwrap();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                GaugeEntry {
                    percent_left: Some(1.0),
                    observed_at: Some("2020-01-01T00:00:00Z".to_owned()),
                    resets_at: Some("2020-01-01T05:00:00Z".to_owned()),
                    window_minutes: Some(300),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let gauges = mv_state::gauge::load(dir.path());
        let entry = gauges.get("codex:solo");

        // Before the gauge's own reset clock: still trusted, still DRAINING.
        let before_reset = time::macros::datetime!(2020-01-01 01:00:00 UTC);
        assert_eq!(
            draining_detail(entry, 10.0, before_reset, None),
            Some("1% left (floor 10%)".to_owned())
        );

        // Past the gauge's own reset clock: too old to trust.
        let after_reset = time::macros::datetime!(2020-01-01 06:00:00 UTC);
        assert_eq!(
            draining_detail(entry, 10.0, after_reset, None),
            None,
            "a reading from before the pool's own reset clock is too old to trust"
        );
    }

    /// Card `20261105-fixer-9d070c`: a gauge observed before a pool's own
    /// manual-reset tombstone is no longer evidence of draining — a human
    /// reset the seat, and `draining_detail` must not immediately report it
    /// DRAINING again off the stale pre-reset reading.
    #[test]
    fn draining_ignores_a_gauge_observed_before_a_manual_reset() {
        let dir = tempfile::tempdir().unwrap();
        mv_state::breaker_store::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                mv_state::breaker_store::BreakerEntry {
                    close_reason: Some("manual reset".to_owned()),
                    closed_at: Some("2020-01-02T00:00:00Z".to_owned()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        mv_state::gauge::with_locked(dir.path(), |map| {
            map.insert(
                "codex:solo".to_owned(),
                GaugeEntry {
                    percent_left: Some(1.0),
                    observed_at: Some("2020-01-01T00:00:00Z".to_owned()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let gauges = mv_state::gauge::load(dir.path());
        let reset_at = manual_reset_at(dir.path(), "codex:solo");
        assert!(reset_at.is_some());

        let now = time::macros::datetime!(2020-01-02 01:00:00 UTC);
        let detail = draining_detail(gauges.get("codex:solo"), 10.0, now, reset_at);
        assert_eq!(
            detail, None,
            "a reading from before the pool's own manual reset is not evidence of draining"
        );
    }

    #[test]
    fn a_capped_seat_shows_herdr_unreachable_when_no_fake_binary_answers() {
        let _guard = HerdrEnvGuard::unreachable();
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        seatstate::set_cap(dir.path(), "codex-a", Some(2)).unwrap();

        let lines = live.list().unwrap();
        assert!(lines
            .iter()
            .any(|l| l.trim_start() == "⛁ open: ?/2 (Herdr unreachable)"));
    }

    /// Card `20261105-fixer-9d070c`: before this task, `account_state` had
    /// no open-agent-cap reading at all — a capped seat whose live count
    /// was actually at or above its cap still read `"available"` in the
    /// STATE column, the exact `AccountSelector._availability` check 5
    /// this task ports. `live_open_count` is passed in already resolved
    /// (the same value the caller also uses for the adjacent "⛁ open:
    /// N/cap" line) rather than `account_state` reading Herdr itself — see
    /// its own doc comment for why a second independent read was rejected.
    #[test]
    fn account_state_reads_at_cap_from_a_live_open_count_at_or_above_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        seatstate::set_cap(dir.path(), "codex-a", Some(1)).unwrap();

        let gauges = mv_state::gauge::load(dir.path());
        let state = account_state(
            &live.config,
            dir.path(),
            &gauges,
            &live.config.accounts["codex-a"],
            Some(1),
            &selector::HttpLocalLivenessProbe,
        );
        assert_eq!(state, "at cap: 1/1 open agents");
    }

    /// The Herdr-unreachable twin of the test above: the STATE column
    /// itself (not just the supplementary "⛁ open:" line) must read
    /// at-cap, matching `AccountSelector._availability`'s own fail-closed
    /// posture for a capped seat Herdr cannot be asked about.
    #[test]
    fn account_state_reads_at_cap_when_herdr_is_unreachable() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        seatstate::set_cap(dir.path(), "codex-a", Some(2)).unwrap();

        let gauges = mv_state::gauge::load(dir.path());
        let state = account_state(
            &live.config,
            dir.path(),
            &gauges,
            &live.config.accounts["codex-a"],
            None,
            &selector::HttpLocalLivenessProbe,
        );
        assert_eq!(
            state,
            "at cap: open-agent count unreadable (Herdr unreachable), cap is 2"
        );
    }

    /// A scripted [`selector::LocalLivenessProbe`] — card `20261106-b-255666`
    /// tests never make a real GET.
    struct FakeLocalProbe(Result<(), String>);

    impl selector::LocalLivenessProbe for FakeLocalProbe {
        fn probe(&self, _account_name: &str, _url: &str) -> Result<(), String> {
            self.0.clone()
        }
    }

    fn local_seat_live_config(state_dir: &Path) -> LiveConfig {
        let content = r#"
[accounts.ollama-win]
kind = "local"
vendor = "local"
config_dir_var = "LOCAL_HOME"
config_dir = "<<PLACEHOLDER: path>>"
pool = "local:ollama-win"
short = "ow"
health_url = "http://100.100.24.245:1234/v1/models"
"#;
        let config =
            load::load_config_str(content, Path::new("config.toml")).expect("valid fixture");
        LiveConfig {
            config,
            state_dir: state_dir.to_path_buf(),
        }
    }

    /// Card `20261106-b-255666`: before this task `account_state` never
    /// probed a `vendor = "local"` seat at all (see its own doc comment,
    /// pre-fix) — a dead local seat still read `"available"`. The reason
    /// string is returned verbatim, matching `_account_state` returning
    /// `_availability`'s own `why_not` unchanged for a non-breaker,
    /// non-capacity `SkipCause.UNAVAILABLE`.
    #[test]
    fn account_state_reports_a_dead_local_seat_s_own_probe_reason() {
        let dir = tempfile::tempdir().unwrap();
        let live = local_seat_live_config(dir.path());
        let gauges = mv_state::gauge::load(dir.path());
        let probe = FakeLocalProbe(Err(
            "local seat health check failed at http://100.100.24.245:1234/v1/models (timed out)"
                .to_string(),
        ));
        let state = account_state(
            &live.config,
            dir.path(),
            &gauges,
            &live.config.accounts["ollama-win"],
            None,
            &probe,
        );
        assert_eq!(
            state,
            "local seat health check failed at http://100.100.24.245:1234/v1/models (timed out)"
        );
    }

    #[test]
    fn account_state_reads_available_for_a_live_local_seat() {
        let dir = tempfile::tempdir().unwrap();
        let live = local_seat_live_config(dir.path());
        let gauges = mv_state::gauge::load(dir.path());
        let probe = FakeLocalProbe(Ok(()));
        let state = account_state(
            &live.config,
            dir.path(),
            &gauges,
            &live.config.accounts["ollama-win"],
            None,
            &probe,
        );
        assert_eq!(state, "available");
    }

    /// Card `20261105-fixer-9d070c`: `LiveConfig::list` resolves a capped
    /// seat's live open count once and threads it into both `account_state`
    /// (the STATE column's own at-cap check) and the adjacent "⛁ open:
    /// N/cap" line — never twice. A capped seat appearing in the listing
    /// must cost exactly one `agent list` call, the same single round trip
    /// Python's own cached `AccountSelector._open_counts()` makes per
    /// listing; two would desync this surface's own Herdr traffic (and any
    /// conformance scenario scripting a fixed reply) from Python's.
    #[test]
    fn listing_a_capped_seat_calls_herdr_agent_list_exactly_once() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        std::fs::write(
            script.path(),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf '%s' '{{\"result\": {{\"agents\": [{{\"pane_id\": \"w1:p1\", \"agent_status\": \"working\", \"name\": \"mv-fixer-1-sc\"}}]}}}}'\n",
                marker.path().display()
            ),
        )
        .unwrap();
        chmod_script(script.path());
        let _guard = HerdrEnvGuard::set(script.path());

        let dir = tempfile::tempdir().unwrap();
        // Two different-vendor seats (`two_seat_live_config`), never each
        // other's "same vendor" sibling — the one capped seat's own live
        // count is resolved exactly once per listing.
        let live = two_seat_live_config(dir.path());
        seatstate::set_cap(dir.path(), "scout-1", Some(1)).unwrap();

        let lines = live.list().unwrap();
        assert!(lines
            .iter()
            .any(|l| l.starts_with("scout-1") && l.contains("at cap: 1/1")));
        assert!(lines.iter().any(|l| l.trim_start() == "⛁ open: 1/1"));

        let log = std::fs::read_to_string(marker.path()).unwrap();
        assert_eq!(
            log.lines().filter(|l| *l == "agent list").count(),
            1,
            "exactly one `agent list` call for one capped seat: {log:?}"
        );
    }

    #[test]
    fn a_seat_with_lend_off_reads_lent_off_not_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        seatstate::set_lend(dir.path(), "codex-a", false).unwrap();

        let lines = live.list().unwrap();
        assert!(lines.iter().any(|l| l.starts_with("codex-a")
            && l.contains("lent off — not in chains right now (`accounts lend` to change it)")));
    }

    // -- cards 20261103-b-cad0bc / 20261103-b-09f3fc: trip/reset stdout --

    #[test]
    fn listing_after_a_weekly_trip_uses_python_comma_resets_form() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        live.trip("codex:solo", Some("weekly"), None).unwrap();
        let lines = live.list().unwrap();
        let line = lines
            .iter()
            .find(|l| l.starts_with("codex-a "))
            .expect("codex-a row");
        assert!(
            line.contains("FENCED (manual) — breaker open for pool 'codex:solo', resets ")
                && line.contains("; kind=weekly, vendor=codex, tripped by cmd_accounts"),
            "{line}"
        );
        assert!(!line.contains("resets at "), "{line}");
    }

    #[test]
    fn trip_prints_weekly_reset_time_and_manual_fence_warning() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let lines = live.trip("codex:solo", Some("weekly"), None).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("tripped codex:solo: resets "),
            "{lines:?}"
        );
        assert!(
            lines[1].contains("MANUAL fence") && lines[1].contains("accounts reset codex:solo"),
            "{lines:?}"
        );
    }

    #[test]
    fn trip_budget_prints_no_automatic_reset() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let lines = live.trip("codex:solo", Some("budget"), None).unwrap();
        assert_eq!(
            lines[0],
            "tripped codex:solo: no automatic reset (kind=budget) — a \
             human has to resolve this and run `accounts reset`"
        );
        assert!(lines[1].contains("MANUAL fence"), "{lines:?}");
    }

    #[test]
    fn trip_until_with_weekly_announces_the_override() {
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let lines = live
            .trip("codex:solo", Some("weekly"), Some("2030-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(
            lines[0],
            "--until 2030-01-01T00:00:00Z overrides --kind weekly's cooldown; \
             the explicit time was used."
        );
        assert!(
            lines[1].starts_with("tripped codex:solo: resets 2030-01-01T00:00:00"),
            "{lines:?}"
        );
    }

    #[test]
    fn reset_reports_nothing_to_close_on_an_untripped_pool() {
        let _guard = HerdrEnvGuard::unreachable();
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        let lines = live.reset("codex:solo").unwrap();
        assert_eq!(
            lines,
            vec!["reset codex:solo: nothing to close (no open breaker).".to_owned()]
        );
    }

    #[test]
    fn reset_reports_closing_an_open_breaker() {
        let _guard = HerdrEnvGuard::unreachable();
        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        live.trip("codex:solo", Some("weekly"), None).unwrap();
        let lines = live.reset("codex:solo").unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with("reset codex:solo: closed an open breaker (was resetting at ")
                && lines[0].ends_with(")."),
            "{lines:?}"
        );
    }

    #[test]
    fn reset_calls_herdr_agent_list_fail_open() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        std::fs::write(
            script.path(),
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 99\n",
                marker.path().display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(script.path()).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(script.path(), perms).unwrap();
        let _guard = HerdrEnvGuard::set(script.path());

        let dir = tempfile::tempdir().unwrap();
        let live = two_codex_pools_live_config(dir.path());
        live.reset("codex:solo").unwrap();

        let log = std::fs::read_to_string(marker.path()).unwrap();
        assert!(
            log.contains("agent list"),
            "reset must fail-open through herdr agent list: {log:?}"
        );
    }

    fn chmod_script(path: &Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    fn fake_herdr_list_and_read(agent_status: &str, banner: &str) -> tempfile::NamedTempFile {
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        std::fs::write(
            script.path(),
            format!(
                "#!/bin/sh\n\
                 case \" $* \" in\n\
                   *\" agent list \"*)\n\
                     printf '%s\\n' '{{\"id\":\"cli:agent:list\",\"result\":{{\"type\":\"agent_list\",\"agents\":[{{\"name\":\"demo-fix-cl\",\"pane_id\":\"w1:p1\",\"agent_status\":\"{agent_status}\"}}]}}}}'\n\
                     exit 0\n\
                     ;;\n\
                   *\" agent read \"*)\n\
                     printf '%s' '{banner}'\n\
                     exit 0\n\
                     ;;\n\
                 esac\n\
                 exit 1\n"
            ),
        )
        .unwrap();
        chmod_script(script.path());
        script
    }

    fn project_with_parked_task(state_dir: &Path, project: &Path) -> LiveConfig {
        std::fs::create_dir_all(project.join(".multiverse")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            "2026-09-07T06:04:23+00:00\tevent=dispatch\ttask=t1\tproject=sandbox\t\
             role=fixer\taccount=codex-a\tpool=codex:solo\toutcome=chain-primary\t\
             reason=r\tagent=demo-fix-cl\n\
             2026-09-07T06:04:30+00:00\tevent=started\ttask=t1\tpane=w1:p1\n",
        )
        .unwrap();
        let content = format!(
            r#"
[accounts.codex-a]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "<<PLACEHOLDER: path>>"
pool = "codex:solo"

[projects.sandbox]
path = {:?}
"#,
            project.to_str().unwrap()
        );
        let config =
            load::load_config_str(&content, Path::new("config.toml")).expect("valid fixture");
        LiveConfig {
            config,
            state_dir: state_dir.to_path_buf(),
        }
    }

    #[test]
    fn reset_snapshots_a_parked_pane_and_prints_the_exemption_line() {
        let script = fake_herdr_list_and_read("idle", "Your workspace is out of credits");
        let _guard = HerdrEnvGuard::set(script.path());
        let state_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let live = project_with_parked_task(state_dir.path(), project.path());
        live.trip("codex:solo", Some("budget"), None).unwrap();

        let lines = live.reset("codex:solo").unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l
                    .starts_with("reset codex:solo: closed an open breaker (was resetting at ")),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().map(String::as_str),
            Some(
                "reset codex:solo: 1 still-stranded pane(s) exempted from \
                 re-tripping until they show new text: sandbox:w1:p1."
            )
        );

        let memory = std::fs::read_to_string(
            project
                .path()
                .join(".multiverse")
                .join(nudge_memory::STRANDED_TRIP_MEMORY_FILENAME),
        )
        .unwrap();
        assert!(memory.contains("w1:p1"), "{memory}");
        assert!(
            memory.contains(&nudge_memory::fingerprint(
                "Your workspace is out of credits"
            )),
            "{memory}"
        );
    }

    #[test]
    fn reset_does_not_snapshot_a_working_pane() {
        let script = fake_herdr_list_and_read("working", "still computing");
        let _guard = HerdrEnvGuard::set(script.path());
        let state_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let live = project_with_parked_task(state_dir.path(), project.path());

        let lines = live.reset("codex:solo").unwrap();
        assert!(
            lines.iter().all(|l| !l.contains("still-stranded pane")),
            "{lines:?}"
        );
        assert!(!project
            .path()
            .join(".multiverse")
            .join(nudge_memory::STRANDED_TRIP_MEMORY_FILENAME)
            .exists());
    }
}

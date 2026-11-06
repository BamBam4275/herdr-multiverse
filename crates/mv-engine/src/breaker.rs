//! FENCED/AVAILABLE seat breaker, backed by
//! [`mv_state::breaker_store`].
//!
//! Decision order per pool (`spec-seats-quotas.md`, "FENCED / AVAILABLE /
//! half-open state machine"):
//! 1. No store/entry/unparseable `reset_at` → closed.
//! 2. A manual fence past its own `reset_at` → open, half-open.
//! 3. Still within `reset_at`: a fresh gauge reading that would reopen the
//!    pool surfaces as `gauge_would_reopen` on a **manual** trip (a human's
//!    fence is never silently overridden) but actually closes an
//!    **automatic** one.
//! 4. Otherwise, a fresh exhausted gauge reading (`percent_left<=0`,
//!    strictly newer than the entry's last recorded change) can open the
//!    pool with no prior pane-banner trip at all.
//! 5. `open_pools` is just this logic re-run per known pool.
//!
//! [`read_status`] is a pure decision function — it takes an entry and a
//! gauge reading and returns a [`BreakerStatus`], with no store access of
//! its own. The mutating entry points ([`trip`], [`reset`], the trial
//! lifecycle functions) hold [`mv_state::breaker_store`]'s advisory lock
//! across their entire read-decide-write span via `with_locked`, which is
//! what makes them race-safe: there is no window where one writer's read
//! can go stale before its write, so there is no separate "detect a
//! concurrent writer and re-read" step to port — the lock already rules
//! that case out structurally.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Instant;

use time::OffsetDateTime;

use mv_core::redact::redact;
use mv_core::time::format_seconds_utc;
use mv_state::breaker_store::{self, BreakerEntry, BreakerMap};
use mv_state::gauge::GaugeEntry;

use crate::selector;

/// Cooldowns and escalation constants (`spec-seats-quotas.md`,
/// "FENCED / AVAILABLE" and "Half-open trials" sections).
pub const DEFAULT_COOLDOWN: time::Duration = time::Duration::hours(5);
pub const WEEKLY_COOLDOWN: time::Duration = time::Duration::days(7);
pub const TRIAL_TIMEOUT: time::Duration = time::Duration::minutes(30);
pub const ESCALATION_RESET_WINDOW: time::Duration = time::Duration::minutes(30);
pub const ESCALATION_STEPS_MINUTES: [i64; 3] = [5, 10, 15];

/// A year far enough out that a `BUDGET` trip never auto-resets — a
/// human-reset-only sentinel rather than a special-cased `Option`, so every
/// other comparison (`now >= reset_at`) stays uniform.
pub const NO_AUTOMATIC_RESET_YEAR: i32 = 9999;

/// `rule`s that climb the escalation ladder instead of using a flat
/// cooldown. Deliberately excludes `claude_rate_limit_error` (the vendor
/// already states its own "retry in Ns" — never override with a guess) and
/// non-throttle rules like `codex_usage_limit_reached` (escalating would
/// shorten a real multi-hour outage to minutes).
pub fn is_escalating_rule(rule: &str) -> bool {
    rule == "codex_retry_limit_exceeded"
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerKind {
    Weekly,
    Budget,
    Rolling,
}

impl BreakerKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Weekly => "weekly",
            Self::Budget => "budget",
            Self::Rolling => "rolling",
        }
    }
}

/// The flat (non-escalating) cooldown for a trip of this kind.
pub fn reset_for_kind(kind: BreakerKind, now: OffsetDateTime) -> OffsetDateTime {
    match kind {
        BreakerKind::Weekly => now + WEEKLY_COOLDOWN,
        BreakerKind::Budget => budget_sentinel(now),
        BreakerKind::Rolling => now + DEFAULT_COOLDOWN,
    }
}

fn budget_sentinel(now: OffsetDateTime) -> OffsetDateTime {
    now.replace_year(NO_AUTOMATIC_RESET_YEAR).unwrap_or(now)
}

/// The decision this module exists to make: is a pool open (fenced) or
/// closed (available) right now, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerStatus {
    pub open: bool,
    pub reset_at: Option<OffsetDateTime>,
    pub rule: Option<String>,
    pub kind: Option<String>,
    pub vendor: Option<String>,
    pub caller: Option<String>,
    pub evidence_path: Option<String>,
    pub manual: bool,
    /// A fresh gauge reading would reopen this pool, but it is a manual
    /// fence, so it stays open — surfaced for display, never acted on
    /// automatically.
    pub gauge_would_reopen: bool,
    pub half_open: bool,
    pub trial_task_id: Option<String>,
}

impl BreakerStatus {
    /// The universal fail-open/available answer.
    pub fn closed() -> Self {
        Self {
            open: false,
            reset_at: None,
            rule: None,
            kind: None,
            vendor: None,
            caller: None,
            evidence_path: None,
            manual: false,
            gauge_would_reopen: false,
            half_open: false,
            trial_task_id: None,
        }
    }
}

/// A fresh quota reading, already parsed out of [`GaugeEntry`]'s raw
/// strings — see [`gauge_reading_from_entry`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GaugeReading {
    pub percent_left: f64,
    pub observed_at: OffsetDateTime,
    pub resets_at: Option<OffsetDateTime>,
}

/// Parse a stored [`GaugeEntry`] into a [`GaugeReading`]; `None` if the
/// entry is missing the fields a reading needs (no percent, or no
/// observation time).
pub fn gauge_reading_from_entry(entry: &GaugeEntry) -> Option<GaugeReading> {
    Some(GaugeReading {
        percent_left: entry.percent_left?,
        observed_at: entry
            .observed_at
            .as_deref()
            .and_then(parse_stored_timestamp)?,
        resets_at: entry.resets_at.as_deref().and_then(parse_stored_timestamp),
    })
}

/// Parse a stored ISO-8601 timestamp, coercing a naive (offset-less) value
/// to UTC rather than leaving it ambiguous — the `f7bd09c` regression this
/// must not reintroduce: a naive timestamp compared directly against an
/// offset-aware "now" silently misorders reset/trip times.
pub fn parse_stored_timestamp(value: &str) -> Option<OffsetDateTime> {
    if let Ok(parsed) = OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339)
    {
        return Some(parsed);
    }
    let naive_format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]");
    let naive = time::PrimitiveDateTime::parse(value, &naive_format).ok()?;
    Some(naive.assume_utc())
}

fn open_status_from_entry(entry: &BreakerEntry, reset_at: OffsetDateTime) -> BreakerStatus {
    BreakerStatus {
        open: true,
        half_open: false,
        reset_at: Some(reset_at),
        rule: entry.rule.clone(),
        kind: entry.kind.clone(),
        vendor: entry.vendor.clone(),
        caller: entry.caller.clone(),
        evidence_path: entry.evidence_path.clone(),
        manual: entry.manual == Some(true),
        gauge_would_reopen: false,
        trial_task_id: None,
    }
}

/// Step 4: a pool can open purely from a fresh exhausted gauge reading —
/// `percent_left <= 0`, strictly newer than `last_change_at` — with no
/// prior pane-banner trip at all. Always `kind=Rolling`,
/// `caller="gauge exhausted"`, reset from the gauge's own `resets_at` or
/// [`DEFAULT_COOLDOWN`].
fn trip_from_gauge(
    gauge: Option<&GaugeReading>,
    last_change_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> Option<BreakerStatus> {
    let gauge = gauge?;
    if gauge.percent_left > 0.0 {
        return None;
    }
    if let Some(last_change_at) = last_change_at {
        if gauge.observed_at <= last_change_at {
            return None;
        }
    }
    let reset_at = gauge.resets_at.unwrap_or(now + DEFAULT_COOLDOWN);
    Some(BreakerStatus {
        open: true,
        half_open: false,
        reset_at: Some(reset_at),
        rule: None,
        kind: Some(BreakerKind::Rolling.as_str().to_string()),
        vendor: None,
        caller: Some("gauge exhausted".to_string()),
        evidence_path: None,
        manual: false,
        gauge_would_reopen: false,
        trial_task_id: None,
    })
}

/// The pure FENCED/AVAILABLE decision for one pool. See the module doc for
/// the five-step order this implements.
pub fn read_status(
    entry: Option<&BreakerEntry>,
    gauge: Option<&GaugeReading>,
    now: OffsetDateTime,
) -> BreakerStatus {
    let Some(entry) = entry else {
        return trip_from_gauge(gauge, None, now).unwrap_or_else(BreakerStatus::closed);
    };

    // Entries accumulate history fields (`closed_at` on a tombstone,
    // `tripped_at` on an active trip); whichever is most recent is
    // "_last_change_at" for judging whether a fresh gauge reading is
    // actually newer than the last state change.
    let last_change_at = entry
        .closed_at
        .as_deref()
        .or(entry.tripped_at.as_deref())
        .and_then(parse_stored_timestamp);

    if entry.open == Some(false) {
        // A closed tombstone: stays closed unless a fresh exhausted gauge
        // reading (newer than the close) re-trips it.
        return trip_from_gauge(gauge, last_change_at, now).unwrap_or_else(BreakerStatus::closed);
    }

    let Some(reset_at) = entry.reset_at.as_deref().and_then(parse_stored_timestamp) else {
        // Step 1: no store/entry/unparseable reset_at -> closed.
        return trip_from_gauge(gauge, last_change_at, now).unwrap_or_else(BreakerStatus::closed);
    };

    let manual = entry.manual == Some(true);
    let tripped_at = entry.tripped_at.as_deref().and_then(parse_stored_timestamp);

    if now >= reset_at {
        if manual {
            // Step 2: a manual fence past its own reset_at is half-open,
            // not closed — it waits for a trial, not the clock.
            let mut status = open_status_from_entry(entry, reset_at);
            status.half_open = true;
            return status;
        }
        // An automatic trip past its own reset_at closes.
        return trip_from_gauge(gauge, last_change_at, now).unwrap_or_else(BreakerStatus::closed);
    }

    if tripped_at.is_none() {
        // A legacy bare entry with no tripped_at stays open unconditionally
        // until reset_at.
        return open_status_from_entry(entry, reset_at);
    }

    let gauge_reopens = gauge.is_some_and(|g| {
        g.percent_left > 0.0 && last_change_at.is_none_or(|last| g.observed_at > last)
    });

    if gauge_reopens {
        if manual {
            // Step 3: a human's fence is never silently overridden — it
            // only surfaces that a fresh reading would reopen it.
            let mut status = open_status_from_entry(entry, reset_at);
            status.gauge_would_reopen = true;
            return status;
        }
        // An automatic trip actually closes.
        return BreakerStatus::closed();
    }

    open_status_from_entry(entry, reset_at)
}

/// Step 5: re-run [`read_status`] for every known pool — no separate
/// parsing path.
pub fn open_pools(
    map: &BreakerMap,
    gauge_for_pool: impl Fn(&str) -> Option<GaugeReading>,
    now: OffsetDateTime,
) -> Vec<&str> {
    map.keys()
        .map(String::as_str)
        .filter(|pool| read_status(map.get(*pool), gauge_for_pool(pool).as_ref(), now).open)
        .collect()
}

/// Fill in `trial_task_id` on a half-open status if an unexpired trial is
/// currently active. A claim stays active for [`TRIAL_TIMEOUT`] regardless
/// of pane liveness; past that, it is only still active if its pane shows
/// up live. `is_pane_live` is the caller's Herdr-backed liveness check,
/// injected so this module carries no Herdr dependency of its own — a
/// liveness-check failure must be reported as `true` by the caller (fails
/// open, never silently expires a trial on a flaky call).
pub fn with_active_trial(
    mut status: BreakerStatus,
    entry: &BreakerEntry,
    now: OffsetDateTime,
    is_pane_live: impl FnOnce(&str) -> bool,
) -> BreakerStatus {
    if !status.half_open {
        return status;
    }
    let Some(claimed_at) = entry
        .trial_claimed_at
        .as_deref()
        .and_then(parse_stored_timestamp)
    else {
        return status;
    };
    let within_timeout = now - claimed_at <= TRIAL_TIMEOUT;
    let pane_live = entry.trial_pane_id.as_deref().is_some_and(is_pane_live);
    if within_timeout || pane_live {
        status.trial_task_id = entry.trial_task_id.clone();
    }
    status
}

/// `_owns_trial`: requires `manual == true`, the pool currently open (not a
/// closed tombstone), and an exact `(task_id, token)` match.
pub fn owns_trial(entry: &BreakerEntry, task_id: &str, token: &str) -> bool {
    entry.manual == Some(true)
        && entry.open != Some(false)
        && entry.trial_task_id.as_deref() == Some(task_id)
        && entry.trial_token.as_deref() == Some(token)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ClaimRefused {
    #[error("pool is not half-open")]
    NotHalfOpen,
    #[error("an unexpired trial is already claimed by another caller")]
    AlreadyClaimed,
}

/// Step: `claim_trial` only succeeds when the pool is currently half-open;
/// an existing unexpired trial refuses any claimant except the exact same
/// `(task_id, token)`.
pub fn claim_trial(
    config_dir: &Path,
    pool: &str,
    now: OffsetDateTime,
    task_id: &str,
    token: &str,
    gauge: Option<&GaugeReading>,
) -> Result<Result<(), ClaimRefused>, breaker_store::StoreError> {
    breaker_store::with_locked(config_dir, |map| {
        let entry = map.entry(pool.to_string()).or_default();
        let status = read_status(Some(entry), gauge, now);
        if !status.half_open {
            return Err(ClaimRefused::NotHalfOpen);
        }
        if owns_trial(entry, task_id, token) {
            return Ok(());
        }
        let trial_active = entry
            .trial_claimed_at
            .as_deref()
            .and_then(parse_stored_timestamp)
            .is_some_and(|claimed| now - claimed <= TRIAL_TIMEOUT);
        if trial_active && entry.trial_task_id.is_some() {
            return Err(ClaimRefused::AlreadyClaimed);
        }
        entry.trial_task_id = Some(task_id.to_string());
        entry.trial_token = Some(token.to_string());
        entry.trial_claimed_at = Some(format_seconds_utc(now));
        entry.manual = Some(true);
        Ok(())
    })
}

/// Release ownership of a trial without closing the pool — only the exact
/// owning `(task_id, token)` can do this.
pub fn release_trial(
    config_dir: &Path,
    pool: &str,
    task_id: &str,
    token: &str,
) -> Result<bool, breaker_store::StoreError> {
    breaker_store::with_locked(config_dir, |map| {
        let Some(entry) = map.get_mut(pool) else {
            return false;
        };
        if !owns_trial(entry, task_id, token) {
            return false;
        }
        entry.trial_task_id = None;
        entry.trial_token = None;
        entry.trial_claimed_at = None;
        entry.trial_pane_id = None;
        entry.trial_artifact_root = None;
        entry.trial_project_root = None;
        entry.trial_harness = None;
        true
    })
}

/// Attach the pane id (and, once a prompt lands, the artifact/project
/// root and harness) to an owned trial.
#[allow(clippy::too_many_arguments)]
pub fn bind_trial(
    config_dir: &Path,
    pool: &str,
    task_id: &str,
    token: &str,
    pane_id: &str,
    artifact_root: Option<&str>,
    project_root: Option<&str>,
    harness: Option<&str>,
) -> Result<bool, breaker_store::StoreError> {
    breaker_store::with_locked(config_dir, |map| {
        let Some(entry) = map.get_mut(pool) else {
            return false;
        };
        if !owns_trial(entry, task_id, token) {
            return false;
        }
        entry.trial_pane_id = Some(pane_id.to_string());
        if let Some(root) = artifact_root {
            entry.trial_artifact_root = Some(root.to_string());
        }
        if let Some(root) = project_root {
            entry.trial_project_root = Some(root.to_string());
        }
        if let Some(harness) = harness {
            entry.trial_harness = Some(harness.to_string());
        }
        true
    })
}

/// Close a trial by closing the pool outright (the trial succeeded, or the
/// task genuinely failed) — only the exact owning `(task_id, token)` can do
/// this.
pub fn close_trial(
    config_dir: &Path,
    pool: &str,
    task_id: &str,
    token: &str,
    now: OffsetDateTime,
    reason: &str,
) -> Result<bool, breaker_store::StoreError> {
    breaker_store::with_locked(config_dir, |map| {
        let Some(entry) = map.get_mut(pool) else {
            return false;
        };
        if !owns_trial(entry, task_id, token) {
            return false;
        }
        entry.open = Some(false);
        entry.closed_at = Some(format_seconds_utc(now));
        entry.close_reason = Some(reason.to_string());
        true
    })
}

/// Escalation step for a trip of an escalating `rule`: a cluster of the
/// same rule on the same pool within [`ESCALATION_RESET_WINDOW`] climbs
/// 5→10→15 minutes and holds at 15 past the third trip; a gap over the
/// window restarts at 5. Returns `(cooldown_minutes, new_trip_count)`.
pub fn escalated_cooldown_minutes(
    prior_count: u32,
    last_tripped_at: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> (i64, u32) {
    let restarts = match last_tripped_at {
        Some(last) => now - last > ESCALATION_RESET_WINDOW,
        None => true,
    };
    let next_count = if restarts { 1 } else { prior_count + 1 };
    let step_index = (next_count as usize - 1).min(ESCALATION_STEPS_MINUTES.len() - 1);
    (ESCALATION_STEPS_MINUTES[step_index], next_count)
}

/// Save redacted pane text as project-local diagnostic evidence.
/// Matches Python's `.multiverse/evidence/<UTC timestamp>-<safe pool>.txt`
/// layout and plain UTF-8 content, without adding a header or newline.
/// Evidence is best-effort: an I/O failure must never prevent a trip or
/// stuck-lane escalation.
pub(crate) fn write_evidence(
    project_root: &Path,
    pool: &str,
    now: OffsetDateTime,
    text: &str,
) -> Option<PathBuf> {
    let write = || -> std::io::Result<PathBuf> {
        let directory = project_root.join(".multiverse/evidence");
        fs::create_dir_all(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        let safe_pool: String = pool
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe_pool = safe_pool.replace("..", "_");
        let safe_pool = if safe_pool.is_empty() {
            "_"
        } else {
            &safe_pool
        };
        let timestamp = format_seconds_utc(now)
            .replace("+00:00", "Z")
            .replace(['-', ':'], "");
        let path = directory.join(format!("{timestamp}-{safe_pool}.txt"));
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?;
        file.write_all(redact(text).as_bytes())?;
        Ok(path)
    };
    write().ok()
}

/// Everything a trip needs that isn't the pool name or `now`.
pub struct TripRequest<'a> {
    pub vendor: &'a str,
    pub kind: BreakerKind,
    pub rule: &'a str,
    pub caller: &'a str,
    pub evidence_path: Option<&'a str>,
    /// Both a nonempty pane and its project are required to save evidence.
    /// A successful write supersedes any caller-supplied `evidence_path`.
    pub pane_text: Option<&'a str>,
    pub project_root: Option<&'a Path>,
    pub manual: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum TripError {
    #[error("vendor \"local\" cannot be fenced behind a breaker — nothing to trip")]
    LocalVendor,
    #[error(transparent)]
    Store(#[from] breaker_store::StoreError),
}

/// Trip `pool`'s breaker. `vendor == "local"` is refused outright — a local
/// (e.g. Ollama) seat has nothing to fence behind a breaker.
pub fn trip(
    config_dir: &Path,
    pool: &str,
    now: OffsetDateTime,
    req: TripRequest<'_>,
) -> Result<BreakerStatus, TripError> {
    trip_with_reset(config_dir, pool, now, req, None)
}

fn trip_with_reset(
    config_dir: &Path,
    pool: &str,
    now: OffsetDateTime,
    req: TripRequest<'_>,
    reset_override: Option<OffsetDateTime>,
) -> Result<BreakerStatus, TripError> {
    if req.vendor == "local" {
        return Err(TripError::LocalVendor);
    }
    let evidence_path = req
        .project_root
        .zip(req.pane_text.filter(|text| !text.is_empty()))
        .and_then(|(root, text)| write_evidence(root, pool, now, text));
    let escalating = is_escalating_rule(req.rule);
    let status = breaker_store::with_locked(config_dir, |map| {
        let prior = map.get(pool);
        let (reset_at, trip_count, last_tripped_at) = if escalating {
            let prior_count = prior.and_then(|e| e.trip_count).unwrap_or(0);
            let prior_last = prior
                .and_then(|e| e.last_tripped_at.as_deref())
                .and_then(parse_stored_timestamp);
            let (minutes, next_count) = escalated_cooldown_minutes(prior_count, prior_last, now);
            (
                now + time::Duration::minutes(minutes),
                Some(next_count),
                Some(now),
            )
        } else {
            (reset_for_kind(req.kind, now), None, None)
        };

        let reset_at = reset_override.unwrap_or(reset_at);
        // Python's `breaker.trip` writes `rule` only when the caller named
        // one (`if rule is not None`). An empty string is the CLI's
        // "no classification" sentinel (`manual_trip`); omit it so the
        // store matches `{caller, kind, manual, reset_at, tripped_at, vendor}`
        // (card `20261103-b-cad0bc`).
        let rule = (!req.rule.is_empty()).then(|| req.rule.to_string());
        let entry = BreakerEntry {
            open: None,
            tripped_at: Some(format_seconds_utc(now)),
            reset_at: Some(format_seconds_utc(reset_at)),
            vendor: Some(req.vendor.to_string()),
            kind: Some(req.kind.as_str().to_string()),
            rule,
            caller: Some(req.caller.to_string()),
            manual: req.manual.then_some(true),
            evidence_path: evidence_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .or_else(|| req.evidence_path.map(str::to_string)),
            trip_count,
            last_tripped_at: last_tripped_at.map(format_seconds_utc),
            ..Default::default()
        };
        map.insert(pool.to_string(), entry);
        open_status_from_entry(map.get(pool).expect("just inserted"), reset_at)
    })?;
    Ok(status)
}

/// Close `pool`'s breaker, writing a tombstone (`open: false`, `closed_at`,
/// `close_reason`) rather than deleting the entry — a popped entry loses
/// the `_last_change_at` a stale gauge reading needs to be told it is
/// stale, which is the bug this tombstone shape exists to prevent
/// (`audit-dispatch-tasks-seats.md` finding 6).
pub fn reset(
    config_dir: &Path,
    pool: &str,
    now: OffsetDateTime,
    reason: &str,
) -> Result<(), breaker_store::StoreError> {
    breaker_store::with_locked(config_dir, |map| {
        // Python's `breaker.reset` replaces the whole entry with a three-field
        // tombstone (`open`/`closed_at`/`close_reason`) rather than mutating
        // the prior trip in place — leftover `kind`/`caller`/`manual` keys
        // would diverge the on-disk store from the oracle.
        map.insert(
            pool.to_string(),
            BreakerEntry {
                open: Some(false),
                closed_at: Some(format_seconds_utc(now)),
                close_reason: Some(reason.to_string()),
                ..Default::default()
            },
        );
    })?;
    Ok(())
}

/// Read current pool admission state without mutating any store.
pub fn pool_status(state_dir: &Path, pool: &str) -> BreakerStatus {
    let entries = breaker_store::load(state_dir);
    let gauges = mv_state::gauge::load(state_dir);
    let gauge = gauges.get(pool).and_then(gauge_reading_from_entry);
    read_status(entries.get(pool), gauge.as_ref(), OffsetDateTime::now_utc())
}

/// Same decision as [`pool_status`], but — like Python's
/// `breaker.read_status(..., read_only=False)` — persists the two
/// gauge-driven corrections `read_status` only computes: closing an
/// **automatic** fence a fresh gauge reading shows reopened (step 3 in the
/// module doc), and opening a pool straight from an exhausted gauge with no
/// prior trip at all (step 4). A **manual** fence is never corrected this
/// way — it only ever surfaces `gauge_would_reopen` for a human to act on,
/// same as [`read_status`] — and a legacy bare entry or one still short of
/// its own `reset_at` has nothing to persist either. Callers that must
/// leave the store untouched (a `--read-only`/`MULTIVERSE_READ_ONLY`
/// listing) use [`pool_status`] instead.
pub fn pool_status_persisting(state_dir: &Path, pool: &str) -> BreakerStatus {
    let now = OffsetDateTime::now_utc();
    let gauge_entry = mv_state::gauge::load(state_dir).remove(pool);
    let gauge = gauge_entry.as_ref().and_then(gauge_reading_from_entry);
    let entries = breaker_store::load(state_dir);
    let Some(entry) = entries.get(pool) else {
        return trip_from_gauge_persisting(
            state_dir,
            pool,
            None,
            gauge.as_ref(),
            gauge_entry.as_ref(),
            now,
        );
    };

    let last_change_at = entry
        .closed_at
        .as_deref()
        .or(entry.tripped_at.as_deref())
        .and_then(parse_stored_timestamp);

    if entry.open == Some(false) {
        return trip_from_gauge_persisting(
            state_dir,
            pool,
            Some(entry),
            gauge.as_ref(),
            gauge_entry.as_ref(),
            now,
        );
    }

    let Some(reset_at) = entry.reset_at.as_deref().and_then(parse_stored_timestamp) else {
        return trip_from_gauge_persisting(
            state_dir,
            pool,
            Some(entry),
            gauge.as_ref(),
            gauge_entry.as_ref(),
            now,
        );
    };

    let manual = entry.manual == Some(true);
    let tripped_at = entry.tripped_at.as_deref().and_then(parse_stored_timestamp);

    if now >= reset_at {
        if manual {
            let mut status = open_status_from_entry(entry, reset_at);
            status.half_open = true;
            return status;
        }
        return trip_from_gauge_persisting(
            state_dir,
            pool,
            Some(entry),
            gauge.as_ref(),
            gauge_entry.as_ref(),
            now,
        );
    }

    if tripped_at.is_none() {
        return open_status_from_entry(entry, reset_at);
    }

    let gauge_reopens = gauge.is_some_and(|g| {
        g.percent_left > 0.0 && last_change_at.is_none_or(|last| g.observed_at > last)
    });

    if !gauge_reopens {
        return open_status_from_entry(entry, reset_at);
    }

    if manual {
        let mut status = open_status_from_entry(entry, reset_at);
        status.gauge_would_reopen = true;
        return status;
    }

    close_from_gauge_reopen(
        state_dir,
        pool,
        entry,
        &gauge.expect("gauge_reopens is only true when gauge is Some"),
        now,
    )
}

/// [`pool_status_persisting`]'s step-3 write: an automatic fence a fresh
/// gauge reading shows reopened closes for real, mutating the existing
/// entry's `open`/`closed_at`/`close_reason` (and `reset_at`, if the gauge
/// names one) in place rather than replacing it with the bare three-field
/// tombstone `reset`/`manual_reset` write — that would lose the `vendor`/
/// `rule`/`kind`/`caller` history `_last_change_at` and `describe` still
/// need. Mirrors Python's in-place `current["open"] = False` mutation,
/// including its same race rule: `current != entry` means something else
/// (most likely a genuine `trip`) wrote this pool between the lock-free
/// snapshot and this lock, so the correction is dropped and the status is
/// re-decided from whatever that write actually left, never layered on top
/// of stale data.
fn close_from_gauge_reopen(
    state_dir: &Path,
    pool: &str,
    expected: &BreakerEntry,
    gauge: &GaugeReading,
    now: OffsetDateTime,
) -> BreakerStatus {
    let wrote = breaker_store::with_locked(state_dir, |map| {
        if map.get(pool) != Some(expected) {
            return false;
        }
        let current = map.get_mut(pool).expect("just matched Some(expected)");
        current.open = Some(false);
        current.closed_at = Some(format_seconds_utc(now));
        current.close_reason = Some("gauge shows window open".to_string());
        if let Some(resets_at) = gauge.resets_at {
            current.reset_at = Some(format_seconds_utc(resets_at));
        }
        true
    });
    if !matches!(wrote, Ok(true)) {
        let entries = breaker_store::load(state_dir);
        let gauges = mv_state::gauge::load(state_dir);
        let fresh_gauge = gauges.get(pool).and_then(gauge_reading_from_entry);
        return read_status(entries.get(pool), fresh_gauge.as_ref(), now);
    }
    BreakerStatus::closed()
}

/// [`pool_status_persisting`]'s step-4 write: a pool can open purely from a
/// fresh exhausted gauge reading, reached from every path that found every
/// other reading closed — no entry, a closed tombstone, or an automatic
/// entry past its own `reset_at`. `vendor` is read straight off the gauge's
/// own stored entry, since [`GaugeReading`] itself carries no vendor field.
/// Mirrors Python's `_trip_from_gauge`'s own read-decide-write, including
/// its same-race fallback.
fn trip_from_gauge_persisting(
    state_dir: &Path,
    pool: &str,
    expected: Option<&BreakerEntry>,
    gauge: Option<&GaugeReading>,
    gauge_entry: Option<&GaugeEntry>,
    now: OffsetDateTime,
) -> BreakerStatus {
    let last_change_at = expected.and_then(|entry| {
        entry
            .closed_at
            .as_deref()
            .or(entry.tripped_at.as_deref())
            .and_then(parse_stored_timestamp)
    });
    let Some(status) = trip_from_gauge(gauge, last_change_at, now) else {
        return BreakerStatus::closed();
    };
    let reset_at = status
        .reset_at
        .expect("trip_from_gauge always sets reset_at when it returns Some");
    let vendor = gauge_entry.and_then(|entry| entry.vendor.clone());
    let wrote = breaker_store::with_locked(state_dir, |map| {
        if map.get(pool) != expected {
            return false;
        }
        map.insert(
            pool.to_string(),
            BreakerEntry {
                tripped_at: Some(format_seconds_utc(now)),
                reset_at: Some(format_seconds_utc(reset_at)),
                vendor,
                kind: Some(BreakerKind::Rolling.as_str().to_string()),
                caller: Some("gauge exhausted".to_string()),
                ..Default::default()
            },
        );
        true
    });
    if !matches!(wrote, Ok(true)) {
        let entries = breaker_store::load(state_dir);
        let gauges = mv_state::gauge::load(state_dir);
        let fresh_gauge = gauges.get(pool).and_then(gauge_reading_from_entry);
        return read_status(entries.get(pool), fresh_gauge.as_ref(), now);
    }
    status
}

/// Human-readable reason used by the account listing. Mirrors Python's
/// `_availability` why-not plus `accounts._breaker_why_suffix`: a budget
/// trip uses a colon and "no automatic reset"; a timed trip uses a comma
/// and "resets {timestamp}" with no "at" (`accounts.py` ~1195-1212).
/// `rule`/`kind`/`vendor`/`caller` each add their own clause when present,
/// `gauge_would_reopen` appends one further clause once a fresh gauge
/// reading would have closed this breaker were it not a manual fence, and
/// a store entry carrying none of those (written before any of those
/// fields existed) reads as "reason not recorded" rather than a bare,
/// unexplained reset time.
pub fn describe(pool: &str, status: &BreakerStatus) -> String {
    let body = match status.reset_at {
        Some(reset) if reset.year() == NO_AUTOMATIC_RESET_YEAR => {
            format!(
                "breaker open for pool '{pool}': no automatic reset — a human has to resolve this and run `accounts reset`"
            )
        }
        Some(reset) => format!(
            "breaker open for pool '{pool}', resets {}",
            format_seconds_utc(reset)
        ),
        None => format!("breaker open for pool '{pool}': reset time unknown"),
    };
    let mut detail = Vec::new();
    // An empty `rule` (`manual_trip`'s own sentinel for "no classification
    // was made") reads as absent, same as Python's `rule=None` — never a
    // literal `rule=` clause with nothing after the `=`.
    if let Some(rule) = status.rule.as_deref().filter(|rule| !rule.is_empty()) {
        detail.push(format!("rule={rule}"));
    }
    if let Some(kind) = &status.kind {
        detail.push(format!("kind={kind}"));
    }
    if let Some(vendor) = &status.vendor {
        detail.push(format!("vendor={vendor}"));
    }
    if let Some(caller) = &status.caller {
        detail.push(format!("tripped by {caller}"));
    }
    if status.gauge_would_reopen {
        detail.push("gauge shows open; manual fence held".to_owned());
    }
    let suffix = if detail.is_empty() {
        "; reason not recorded".to_owned()
    } else {
        format!("; {}", detail.join(", "))
    };
    format!("{body}{suffix}")
}

#[derive(Debug, thiserror::Error)]
pub enum ManualTripError {
    #[error("unknown breaker kind {0:?}; expected rolling, weekly, or budget")]
    Kind(String),
    #[error("invalid breaker reset timestamp {0:?}")]
    Until(String),
    #[error(transparent)]
    Trip(#[from] TripError),
}

/// Operator trips retain the explicit reset override in the same atomic write.
pub fn manual_trip(
    state_dir: &Path,
    pool: &str,
    vendor: &str,
    kind: Option<&str>,
    until: Option<&str>,
) -> Result<BreakerStatus, ManualTripError> {
    let kind = match kind.unwrap_or("rolling") {
        "rolling" => BreakerKind::Rolling,
        "weekly" => BreakerKind::Weekly,
        "budget" => BreakerKind::Budget,
        other => return Err(ManualTripError::Kind(other.to_owned())),
    };
    let reset = until
        .map(|value| {
            parse_stored_timestamp(value).ok_or_else(|| ManualTripError::Until(value.to_owned()))
        })
        .transpose()?;
    Ok(trip_with_reset(
        state_dir,
        pool,
        OffsetDateTime::now_utc(),
        TripRequest {
            vendor,
            kind,
            // Card `20261102-b-430436`: an operator's `accounts trip` never
            // names a classification rule in Python either (`cli.py`'s own
            // `breaker.trip(...)` call passes no `rule=` at all, so it
            // defaults to `None`) — `manuality` is already carried by
            // `manual: true` below, independent of this field. Empty, not
            // a placeholder word like the `"manual"` this used to read:
            // `describe` treats an empty rule as "none recorded", exactly
            // like Python's `None` does, rather than fabricating a
            // classification that was never made.
            rule: "",
            caller: "cmd_accounts",
            evidence_path: None,
            pane_text: None,
            project_root: None,
            manual: true,
        },
        reset,
    )?)
}

pub fn manual_reset(state_dir: &Path, pool: &str) -> Result<(), breaker_store::StoreError> {
    // Python's `breaker.reset` hardcodes `close_reason: "manual reset"`,
    // independent of the CLI caller name recorded on a trip.
    reset(state_dir, pool, OffsetDateTime::now_utc(), "manual reset")
}

/// Correlates one wall-clock reading with one `Instant` reading, so this
/// module's wall-clock decisions ([`read_status`]'s `reset_at`) can be
/// expressed as the `Instant` [`selector::BreakerQuery`] requires without
/// this module depending on `Instant` for anything else. The same trick
/// `_returns_soon` relies on from the other direction (there, dispatch
/// already has an `Instant` "now" and needs no wall clock at all); here, the
/// breaker decides in wall-clock terms and the caller needs an `Instant`
/// back, so the bridge carries both readings of the same instant.
#[derive(Debug, Clone, Copy)]
pub struct ClockBridge {
    pub wall_now: OffsetDateTime,
    pub instant_now: Instant,
}

impl ClockBridge {
    /// A bridge anchored to the real clock: `instant_now` and `wall_now` are
    /// read back to back, so the two readings describe the same instant to
    /// within a function call's worth of drift.
    pub fn now() -> Self {
        Self {
            wall_now: OffsetDateTime::now_utc(),
            instant_now: Instant::now(),
        }
    }

    /// `wall`, expressed as an `Instant` relative to this bridge's anchor.
    /// Saturates at `instant_now` rather than panicking or wrapping if
    /// `wall` is far enough in the past that the equivalent `Instant` would
    /// underflow — this only ever feeds a reset time into `ResetAt::At`,
    /// where a saturated "already passed" instant is exactly as correct as
    /// the real one would have been.
    fn to_instant(self, wall: OffsetDateTime) -> Instant {
        let delta = wall - self.wall_now;
        if delta.is_negative() {
            self.instant_now
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(self.instant_now)
        } else {
            self.instant_now + delta.unsigned_abs()
        }
    }
}

/// Same owner-liveness judgment as [`with_active_trial`], but returning the
/// full `(task_id, token)` pair [`selector::TrialClaim`] needs rather than
/// just the task id — nothing before [`StoreBreakerQuery`] needed the token
/// back out of a status read, only the trial lifecycle functions that
/// already take it as an argument.
fn active_trial_claim(
    entry: &BreakerEntry,
    now: OffsetDateTime,
    is_pane_live: impl FnOnce(&str) -> bool,
) -> Option<selector::TrialClaim> {
    let claimed_at = entry
        .trial_claimed_at
        .as_deref()
        .and_then(parse_stored_timestamp)?;
    let task_id = entry.trial_task_id.clone()?;
    let token = entry.trial_token.clone()?;
    let within_timeout = now - claimed_at <= TRIAL_TIMEOUT;
    let pane_live = entry.trial_pane_id.as_deref().is_some_and(is_pane_live);
    (within_timeout || pane_live).then_some(selector::TrialClaim { task_id, token })
}

/// A fresh, unguessable trial-claim token — this module's equivalent of
/// Python's `uuid.uuid4().hex`, without taking on a UUID dependency for this
/// one call site. Not a cryptographic guarantee (`RandomState` is not a CSPRNG),
/// which is fine here: the token is a capability check against concurrent
/// dispatches racing the same half-open pool, not a secrecy boundary, and
/// `RandomState::new()`'s own per-instance random keys already make it
/// unguessable in practice. The atomic counter only guarantees two tokens
/// minted in the same process never collide even if two calls landed in the
/// same instant.
fn generate_trial_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = u64::from(std::process::id());
    let first = RandomState::new().hash_one((counter, pid));
    let second = RandomState::new().hash_one((pid, counter));
    format!("{first:016x}{second:016x}")
}

/// The real [`selector::BreakerQuery`], backed by `breaker-state.json` via
/// [`read_status`]/[`claim_trial`] and bridged onto `Instant` via
/// [`ClockBridge`]. `gauge_for_pool` and `is_pane_live` are injected rather
/// than read from a fixed location, the same way [`with_active_trial`]
/// takes its liveness check as a parameter — this module stays free of a
/// Herdr dependency of its own; a caller wires in a real gauge store read
/// and a real pane-liveness check.
pub struct StoreBreakerQuery<'a, G, L> {
    pub config_dir: &'a Path,
    pub clock: ClockBridge,
    pub gauge_for_pool: G,
    pub is_pane_live: L,
}

impl<G, L> selector::BreakerQuery for StoreBreakerQuery<'_, G, L>
where
    G: Fn(&str) -> Option<GaugeReading>,
    L: Fn(&str) -> bool,
{
    fn status(&self, pool: &str) -> selector::BreakerStatus {
        let map = breaker_store::load(self.config_dir);
        let gauge = (self.gauge_for_pool)(pool);
        let status = read_status(map.get(pool), gauge.as_ref(), self.clock.wall_now);
        if !status.open {
            return selector::BreakerStatus::CLOSED;
        }
        let (reset_at, reset_display) = match status.reset_at {
            Some(t) if t.year() == NO_AUTOMATIC_RESET_YEAR => {
                (selector::ResetAt::NoAutomaticReset, None)
            }
            Some(t) => (
                selector::ResetAt::At(self.clock.to_instant(t)),
                Some(format_seconds_utc(t)),
            ),
            None => (selector::ResetAt::None, None),
        };
        let trial_claimed_by = if status.half_open {
            map.get(pool).and_then(|entry| {
                active_trial_claim(entry, self.clock.wall_now, &self.is_pane_live)
            })
        } else {
            None
        };
        selector::BreakerStatus {
            open: true,
            half_open: status.half_open,
            reset_at,
            reset_display,
            trial_claimed_by,
        }
    }

    fn claim_trial(&self, pool: &str, task_id: &str, token: Option<&str>) -> Option<String> {
        let gauge = (self.gauge_for_pool)(pool);
        let token = token
            .map(str::to_string)
            .unwrap_or_else(generate_trial_token);
        match claim_trial(
            self.config_dir,
            pool,
            self.clock.wall_now,
            task_id,
            &token,
            gauge.as_ref(),
        ) {
            Ok(Ok(())) => Some(token),
            Ok(Err(_)) | Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn manual_budget_trip_and_reset_round_trip_from_a_missing_store() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!pool_status(dir.path(), "codex:test").open);
        manual_trip(dir.path(), "codex:test", "codex", Some("budget"), None).unwrap();
        let status = pool_status(dir.path(), "codex:test");
        assert_eq!(describe("codex:test", &status), "breaker open for pool 'codex:test': no automatic reset — a human has to resolve this and run `accounts reset`; kind=budget, vendor=codex, tripped by cmd_accounts");
        assert!(status.manual);
        manual_reset(dir.path(), "codex:test").unwrap();
        assert!(!pool_status(dir.path(), "codex:test").open);
        let store = breaker_store::load(dir.path());
        assert_eq!(store["codex:test"].open, Some(false));
        assert!(store["codex:test"].closed_at.is_some());
        assert_eq!(
            store["codex:test"].close_reason.as_deref(),
            Some("manual reset")
        );
        assert!(
            store["codex:test"].tripped_at.is_none()
                && store["codex:test"].kind.is_none()
                && store["codex:test"].caller.is_none()
                && store["codex:test"].manual.is_none(),
            "reset replaces the trip with a three-field tombstone, matching Python"
        );
    }

    /// Card `20261103-b-cad0bc`: Python's `accounts trip` never writes a
    /// `rule` key (`breaker.trip` is called with `rule` defaulting to
    /// `None`). An empty-string sentinel must not leak into the store.
    #[test]
    fn manual_trip_omits_empty_rule_from_the_store() {
        let dir = tempfile::tempdir().unwrap();
        manual_trip(dir.path(), "codex:test", "codex", Some("weekly"), None).unwrap();
        let text = std::fs::read_to_string(breaker_store::store_path(dir.path())).unwrap();
        assert!(
            !text.contains("\"rule\""),
            "manual trip must omit rule, not write an empty string: {text}"
        );
        let store = breaker_store::load(dir.path());
        assert_eq!(store["codex:test"].rule, None);
        assert_eq!(store["codex:test"].caller.as_deref(), Some("cmd_accounts"));
        assert_eq!(store["codex:test"].kind.as_deref(), Some("weekly"));
        assert_eq!(store["codex:test"].manual, Some(true));
    }

    /// `manual_trip` names no classification rule (`rule: ""`), matching
    /// Python's own `accounts trip`, which never passes `rule=` to
    /// `breaker.trip` either — `describe` must not turn that sentinel
    /// into a bare, meaningless `rule=` clause.
    #[test]
    fn describe_treats_an_empty_rule_as_absent_not_a_bare_clause() {
        let status = BreakerStatus {
            reset_at: Some(datetime!(2030-01-01 00:00:00 UTC)),
            rule: Some(String::new()),
            vendor: Some("codex".to_owned()),
            ..BreakerStatus::closed()
        };
        assert_eq!(
            describe("codex:test", &status),
            "breaker open for pool 'codex:test', resets 2030-01-01T00:00:00+00:00; vendor=codex"
        );
    }

    #[test]
    fn describe_names_a_store_entry_with_none_of_the_four_reason_fields_as_not_recorded() {
        let status = BreakerStatus {
            reset_at: Some(datetime!(2030-01-01 00:00:00 UTC)),
            ..BreakerStatus::closed()
        };
        assert_eq!(
            describe("codex:test", &status),
            "breaker open for pool 'codex:test', resets 2030-01-01T00:00:00+00:00; reason not recorded"
        );
    }

    #[test]
    fn describe_appends_the_gauge_would_reopen_clause_once() {
        let status = BreakerStatus {
            reset_at: Some(datetime!(2030-01-01 00:00:00 UTC)),
            vendor: Some("codex".to_owned()),
            gauge_would_reopen: true,
            ..BreakerStatus::closed()
        };
        assert_eq!(
            describe("codex:test", &status),
            "breaker open for pool 'codex:test', resets 2030-01-01T00:00:00+00:00; vendor=codex, gauge shows open; manual fence held"
        );
    }

    #[test]
    fn manual_trip_honors_until_and_rejects_invalid_input_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(manual_trip(dir.path(), "pool", "codex", Some("typo"), None).is_err());
        assert!(manual_trip(dir.path(), "pool", "codex", None, Some("bad time")).is_err());
        assert!(breaker_store::load(dir.path()).is_empty());
        let until = "2030-01-01T00:00:00Z";
        let status = manual_trip(dir.path(), "pool", "codex", Some("budget"), Some(until)).unwrap();
        assert_eq!(status.reset_at, parse_stored_timestamp(until));
        assert_eq!(pool_status(dir.path(), "pool").reset_at, status.reset_at);
        assert!(manual_trip(dir.path(), "local", "local", None, None).is_err());
    }

    /// Writes one `GaugeEntry` for `pool` straight to the gauge store, the
    /// same on-disk shape `pool_status_persisting` reads back via
    /// `mv_state::gauge::load`.
    fn save_gauge(
        state_dir: &Path,
        pool: &str,
        vendor: &str,
        percent_left: f64,
        observed_at: OffsetDateTime,
    ) {
        let mut map = mv_state::gauge::GaugeMap::new();
        map.insert(
            pool.to_string(),
            GaugeEntry {
                vendor: Some(vendor.to_string()),
                percent_left: Some(percent_left),
                observed_at: Some(format_seconds_utc(observed_at)),
                ..Default::default()
            },
        );
        mv_state::gauge::save(state_dir, &map).unwrap();
    }

    #[test]
    fn pool_status_persisting_closes_and_persists_an_automatic_gauge_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let now = OffsetDateTime::now_utc();
        let mut map = BreakerMap::new();
        map.insert(
            "codex:test".to_string(),
            BreakerEntry {
                tripped_at: Some(format_seconds_utc(now - time::Duration::hours(2))),
                reset_at: Some(format_seconds_utc(now + time::Duration::hours(3))),
                vendor: Some("codex".to_string()),
                kind: Some("rolling".to_string()),
                caller: Some("runner".to_string()),
                ..Default::default()
            },
        );
        breaker_store::save(dir.path(), &map).unwrap();
        save_gauge(
            dir.path(),
            "codex:test",
            "codex",
            50.0,
            now - time::Duration::minutes(5),
        );

        let status = pool_status_persisting(dir.path(), "codex:test");
        assert!(
            !status.open,
            "a fresh positive gauge reading must close an automatic fence"
        );

        let stored = breaker_store::load(dir.path());
        let entry = &stored["codex:test"];
        assert_eq!(entry.open, Some(false));
        assert_eq!(
            entry.close_reason.as_deref(),
            Some("gauge shows window open")
        );
        assert_eq!(
            entry.vendor.as_deref(),
            Some("codex"),
            "the correction mutates the entry in place; it must not lose prior history"
        );
        assert_eq!(entry.caller.as_deref(), Some("runner"));

        // Idempotent: a second read sees the already-closed tombstone and
        // makes no further change.
        assert!(!pool_status_persisting(dir.path(), "codex:test").open);
        assert_eq!(breaker_store::load(dir.path())["codex:test"], *entry);
    }

    #[test]
    fn pool_status_persisting_never_writes_over_a_manual_fence() {
        let dir = tempfile::tempdir().unwrap();
        let now = OffsetDateTime::now_utc();
        let mut map = BreakerMap::new();
        map.insert(
            "codex:test".to_string(),
            BreakerEntry {
                tripped_at: Some(format_seconds_utc(now - time::Duration::hours(2))),
                reset_at: Some(format_seconds_utc(now + time::Duration::hours(3))),
                vendor: Some("codex".to_string()),
                manual: Some(true),
                ..Default::default()
            },
        );
        breaker_store::save(dir.path(), &map).unwrap();
        save_gauge(
            dir.path(),
            "codex:test",
            "codex",
            50.0,
            now - time::Duration::minutes(5),
        );

        let status = pool_status_persisting(dir.path(), "codex:test");
        assert!(status.open, "a manual fence is never silently overridden");
        assert!(status.gauge_would_reopen);

        let stored = breaker_store::load(dir.path());
        assert_eq!(
            stored["codex:test"], map["codex:test"],
            "a manual entry must be left byte-for-byte untouched"
        );
    }

    #[test]
    fn pool_status_persisting_opens_and_persists_a_fresh_exhausted_gauge() {
        let dir = tempfile::tempdir().unwrap();
        let now = OffsetDateTime::now_utc();
        assert!(breaker_store::load(dir.path()).is_empty());
        save_gauge(dir.path(), "chatgpt:codex", "codex", 0.0, now);

        let status = pool_status_persisting(dir.path(), "chatgpt:codex");
        assert!(status.open);
        assert_eq!(status.caller.as_deref(), Some("gauge exhausted"));

        let stored = breaker_store::load(dir.path());
        let entry = &stored["chatgpt:codex"];
        assert!(entry.tripped_at.is_some());
        assert_eq!(entry.kind.as_deref(), Some("rolling"));
        assert_eq!(entry.caller.as_deref(), Some("gauge exhausted"));
        assert_eq!(
            entry.vendor.as_deref(),
            Some("codex"),
            "the persisted trip must carry the gauge's own vendor, not drop it \
             the way `GaugeReading` (with no vendor field of its own) would"
        );
    }

    fn gauge(percent_left: f64, observed_at: OffsetDateTime) -> GaugeReading {
        GaugeReading {
            percent_left,
            observed_at,
            resets_at: None,
        }
    }

    #[test]
    fn no_entry_is_closed() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(read_status(None, None, now), BreakerStatus::closed());
    }

    #[test]
    fn entry_with_unparseable_reset_at_is_closed() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            reset_at: Some("not a timestamp".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(!read_status(Some(&entry), None, now).open);
    }

    #[test]
    fn active_automatic_trip_within_window_is_open() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
            rule: Some("codex_usage_limit_reached".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let status = read_status(Some(&entry), None, now);
        assert!(status.open);
        assert!(!status.half_open);
        assert!(!status.manual);
    }

    #[test]
    fn manual_fence_past_reset_at_is_half_open_not_closed() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T06:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            manual: Some(true),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let status = read_status(Some(&entry), None, now);
        assert!(status.open);
        assert!(status.half_open);
        assert!(status.manual);
    }

    #[test]
    fn automatic_trip_past_reset_at_closes() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T06:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(!read_status(Some(&entry), None, now).open);
    }

    #[test]
    fn fresh_gauge_reopens_an_automatic_trip_early() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let g = gauge(50.0, datetime!(2026-11-02 11:00:00 UTC));
        assert!(!read_status(Some(&entry), Some(&g), now).open);
    }

    #[test]
    fn fresh_gauge_never_silently_closes_a_manual_fence() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
            manual: Some(true),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let g = gauge(50.0, datetime!(2026-11-02 11:00:00 UTC));
        let status = read_status(Some(&entry), Some(&g), now);
        assert!(status.open);
        assert!(!status.half_open);
        assert!(status.gauge_would_reopen);
    }

    #[test]
    fn a_stale_gauge_reading_does_not_reopen_anything() {
        let entry = BreakerEntry {
            tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
            reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        // Observed *before* the trip: stale, must not reopen.
        let g = gauge(50.0, datetime!(2026-11-02 09:00:00 UTC));
        assert!(read_status(Some(&entry), Some(&g), now).open);
    }

    #[test]
    fn exhausted_gauge_opens_a_pool_with_no_prior_trip() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let g = gauge(0.0, now);
        let status = read_status(None, Some(&g), now);
        assert!(status.open);
        assert_eq!(status.caller.as_deref(), Some("gauge exhausted"));
        assert_eq!(status.kind.as_deref(), Some("rolling"));
    }

    #[test]
    fn exhausted_gauge_does_not_retrip_right_after_a_manual_reset() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let entry = BreakerEntry {
            open: Some(false),
            closed_at: Some(format_seconds_utc(now)),
            close_reason: Some("manual reset".to_string()),
            ..Default::default()
        };
        // A stale exhausted reading observed *before* the reset: must not
        // retrip — this is the tombstone's whole purpose.
        let g = gauge(0.0, now - time::Duration::minutes(5));
        assert!(!read_status(Some(&entry), Some(&g), now).open);
    }

    #[test]
    fn a_fresh_exhausted_gauge_after_a_reset_can_still_retrip() {
        let closed_at = datetime!(2026-11-02 12:00:00 UTC);
        let entry = BreakerEntry {
            open: Some(false),
            closed_at: Some(format_seconds_utc(closed_at)),
            ..Default::default()
        };
        let later = closed_at + time::Duration::minutes(10);
        let g = gauge(0.0, later);
        assert!(read_status(Some(&entry), Some(&g), later).open);
    }

    #[test]
    fn legacy_bare_entry_with_no_tripped_at_stays_open_until_reset_at() {
        let entry = BreakerEntry {
            reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
            ..Default::default()
        };
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(read_status(Some(&entry), None, now).open);
    }

    #[test]
    fn naive_timestamps_are_coerced_to_utc_not_misordered() {
        let instant = parse_stored_timestamp("2026-11-02T12:00:00").unwrap();
        assert_eq!(instant, datetime!(2026-11-02 12:00:00 UTC));
    }

    #[test]
    fn budget_cooldown_is_the_far_future_sentinel() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let reset_at = reset_for_kind(BreakerKind::Budget, now);
        assert_eq!(reset_at.year(), NO_AUTOMATIC_RESET_YEAR);
    }

    #[test]
    fn weekly_cooldown_is_seven_days() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(
            reset_for_kind(BreakerKind::Weekly, now),
            now + WEEKLY_COOLDOWN
        );
    }

    #[test]
    fn escalation_climbs_five_ten_fifteen_and_holds() {
        let t0 = datetime!(2026-11-02 12:00:00 UTC);
        let (m1, c1) = escalated_cooldown_minutes(0, None, t0);
        assert_eq!((m1, c1), (5, 1));
        let t1 = t0 + time::Duration::minutes(1);
        let (m2, c2) = escalated_cooldown_minutes(c1, Some(t0), t1);
        assert_eq!((m2, c2), (10, 2));
        let t2 = t1 + time::Duration::minutes(1);
        let (m3, c3) = escalated_cooldown_minutes(c2, Some(t1), t2);
        assert_eq!((m3, c3), (15, 3));
        let t3 = t2 + time::Duration::minutes(1);
        let (m4, c4) = escalated_cooldown_minutes(c3, Some(t2), t3);
        assert_eq!((m4, c4), (15, 4));
    }

    #[test]
    fn escalation_restarts_after_a_gap_over_the_window() {
        let t0 = datetime!(2026-11-02 12:00:00 UTC);
        let later = t0 + ESCALATION_RESET_WINDOW + time::Duration::minutes(1);
        let (minutes, count) = escalated_cooldown_minutes(2, Some(t0), later);
        assert_eq!((minutes, count), (5, 1));
    }

    #[test]
    fn vendor_local_can_never_be_tripped() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let result = trip(
            dir.path(),
            "ollama",
            now,
            TripRequest {
                vendor: "local",
                kind: BreakerKind::Rolling,
                rule: "whatever",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: false,
            },
        );
        assert!(matches!(result, Err(TripError::LocalVendor)));
    }

    fn evidence_request<'a>(root: Option<&'a Path>, text: Option<&'a str>) -> TripRequest<'a> {
        TripRequest {
            vendor: "codex",
            kind: BreakerKind::Rolling,
            rule: "codex_usage_limit_reached",
            caller: "test",
            evidence_path: None,
            pane_text: text,
            project_root: root,
            manual: false,
        }
    }

    #[test]
    fn trip_saves_redacted_evidence_and_round_trips_its_path() {
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 06:03:04 -06:00);
        let pane = "Limit reached — café\nBearer fake-bearer-value\napi_key='fake-key-value'";
        let status = trip(
            config.path(),
            "chatgpt:codex",
            now,
            evidence_request(Some(project.path()), Some(pane)),
        )
        .unwrap();
        let path = project
            .path()
            .join(".multiverse/evidence/20261102T120304Z-chatgpt_codex.txt");
        assert_eq!(status.evidence_path.as_deref(), path.to_str());
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "Limit reached — café\nBearer …[redacted]\napi_key='…[redacted]'"
        );
        let entries = breaker_store::load(config.path());
        assert_eq!(entries["chatgpt:codex"].evidence_path, status.evidence_path);
        assert_eq!(
            read_status(entries.get("chatgpt:codex"), None, now).evidence_path,
            status.evidence_path
        );
        assert!(!config.path().join(".multiverse/evidence").exists());
    }

    #[test]
    fn evidence_sanitizes_pool_names_to_one_filename_component() {
        let project = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        for (pool, safe) in [
            ("chatgpt:codex", "chatgpt_codex"),
            ("../../x\\y\0é", "____x_y__"),
            ("....", "__"),
            ("", "_"),
            ("pool_name-1.2", "pool_name-1.2"),
        ] {
            let path = write_evidence(project.path(), pool, now, "pane").unwrap();
            assert_eq!(
                path.parent(),
                Some(project.path().join(".multiverse/evidence").as_path())
            );
            assert_eq!(
                path.file_name().unwrap().to_str().unwrap(),
                format!("20261102T120000Z-{safe}.txt")
            );
            assert_eq!(fs::read_to_string(path).unwrap(), "pane");
        }
    }

    #[test]
    fn evidence_is_private_and_repeated_filenames_are_truncated() {
        let project = tempfile::tempdir().unwrap();
        let directory = project.path().join(".multiverse/evidence");
        fs::create_dir_all(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let path = write_evidence(project.path(), "pool", now, "longer original pane").unwrap();
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            write_evidence(project.path(), "pool", now, "new"),
            Some(path.clone())
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "new");
    }

    #[test]
    fn evidence_write_failure_does_not_prevent_the_trip() {
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        fs::create_dir(project.path().join(".multiverse")).unwrap();
        fs::write(
            project.path().join(".multiverse/evidence"),
            "not a directory",
        )
        .unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let status = trip(
            config.path(),
            "pool",
            now,
            evidence_request(Some(project.path()), Some("pane")),
        )
        .unwrap();
        assert!(status.open);
        assert_eq!(status.reset_at, Some(now + DEFAULT_COOLDOWN));
        assert_eq!(status.evidence_path, None);
        assert!(read_status(breaker_store::load(config.path()).get("pool"), None, now).open);
    }

    #[test]
    fn evidence_requires_both_nonempty_text_and_a_project() {
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        for (root, text) in [
            (None, None),
            (None, Some("pane")),
            (Some(project.path()), None),
            (Some(project.path()), Some("")),
        ] {
            let status = trip(config.path(), "pool", now, evidence_request(root, text)).unwrap();
            assert!(status.open);
            assert_eq!(status.evidence_path, None);
            assert!(!project.path().join(".multiverse").exists());
        }
        let mut request = evidence_request(Some(project.path()), Some("pane"));
        request.vendor = "local";
        assert!(matches!(
            trip(config.path(), "local", now, request),
            Err(TripError::LocalVendor)
        ));
        assert!(!project.path().join(".multiverse").exists());
    }

    #[test]
    fn trip_then_read_status_round_trips_through_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            now,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "codex_usage_limit_reached",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: false,
            },
        )
        .unwrap();
        let map = breaker_store::load(dir.path());
        let status = read_status(map.get("chatgpt"), None, now);
        assert!(status.open);
        assert!(!status.manual);
    }

    #[test]
    fn reset_writes_a_tombstone_not_a_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            now,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "codex_usage_limit_reached",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: true,
            },
        )
        .unwrap();
        reset(dir.path(), "chatgpt", now, "manual reset").unwrap();
        let map = breaker_store::load(dir.path());
        let entry = map.get("chatgpt").expect("tombstone, not a deletion");
        assert_eq!(entry.open, Some(false));
        assert_eq!(entry.close_reason.as_deref(), Some("manual reset"));
        assert!(
            entry.rule.is_none() && entry.tripped_at.is_none() && entry.kind.is_none(),
            "tombstone must not keep the prior trip's diagnostic fields"
        );
    }

    #[test]
    fn claim_trial_only_succeeds_when_half_open() {
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            now,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "codex_usage_limit_reached",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: false,
            },
        )
        .unwrap();
        // Not manual, so never half-open: claim must refuse.
        let result = claim_trial(dir.path(), "chatgpt", now, "task-1", "token-1", None).unwrap();
        assert_eq!(result, Err(ClaimRefused::NotHalfOpen));
    }

    #[test]
    fn claim_trial_succeeds_once_half_open_and_refuses_a_second_claimant() {
        let dir = tempfile::tempdir().unwrap();
        let trip_time = datetime!(2026-11-02 06:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            trip_time,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "manual fence",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: true,
            },
        )
        .unwrap();
        // Force past reset_at by resetting the stored reset_at backward.
        breaker_store::with_locked(dir.path(), |map| {
            map.get_mut("chatgpt").unwrap().reset_at =
                Some(format_seconds_utc(trip_time + time::Duration::minutes(1)));
        })
        .unwrap();
        let now = trip_time + time::Duration::hours(1);

        let first = claim_trial(dir.path(), "chatgpt", now, "task-1", "token-1", None).unwrap();
        assert_eq!(first, Ok(()));

        let second = claim_trial(dir.path(), "chatgpt", now, "task-2", "token-2", None).unwrap();
        assert_eq!(second, Err(ClaimRefused::AlreadyClaimed));

        // The same (task_id, token) re-claiming is idempotent, not refused.
        let same = claim_trial(dir.path(), "chatgpt", now, "task-1", "token-1", None).unwrap();
        assert_eq!(same, Ok(()));
    }

    #[test]
    fn owns_trial_requires_exact_token_match() {
        let entry = BreakerEntry {
            manual: Some(true),
            trial_task_id: Some("task-1".to_string()),
            trial_token: Some("token-1".to_string()),
            ..Default::default()
        };
        assert!(owns_trial(&entry, "task-1", "token-1"));
        assert!(!owns_trial(&entry, "task-1", "wrong-token"));
        assert!(!owns_trial(&entry, "other-task", "token-1"));
    }

    #[test]
    fn release_trial_clears_fields_without_closing_the_pool() {
        let dir = tempfile::tempdir().unwrap();
        breaker_store::with_locked(dir.path(), |map| {
            map.insert(
                "chatgpt".to_string(),
                BreakerEntry {
                    manual: Some(true),
                    trial_task_id: Some("task-1".to_string()),
                    trial_token: Some("token-1".to_string()),
                    reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        let released = release_trial(dir.path(), "chatgpt", "task-1", "token-1").unwrap();
        assert!(released);
        let map = breaker_store::load(dir.path());
        let entry = &map["chatgpt"];
        assert_eq!(entry.trial_task_id, None);
        assert_ne!(entry.open, Some(false));
    }

    #[test]
    fn with_active_trial_fails_open_on_a_flaky_liveness_check() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let entry = BreakerEntry {
            manual: Some(true),
            trial_task_id: Some("task-1".to_string()),
            trial_pane_id: Some("pane-1".to_string()),
            trial_claimed_at: Some(format_seconds_utc(
                now - TRIAL_TIMEOUT - time::Duration::hours(1),
            )),
            ..Default::default()
        };
        let status = BreakerStatus {
            half_open: true,
            ..BreakerStatus::closed()
        };
        // Past the timeout, but the caller reports liveness as true (a
        // flaky Herdr call failing open), so the trial is still active.
        let filled = with_active_trial(status, &entry, now, |_pane| true);
        assert_eq!(filled.trial_task_id.as_deref(), Some("task-1"));
    }

    #[test]
    fn open_pools_lists_only_pools_currently_open() {
        let mut map = BreakerMap::new();
        map.insert(
            "chatgpt".to_string(),
            BreakerEntry {
                tripped_at: Some("2026-11-02T10:00:00+00:00".to_string()),
                reset_at: Some("2026-11-02T15:00:00+00:00".to_string()),
                ..Default::default()
            },
        );
        map.insert("claude".to_string(), BreakerEntry::default());
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let open = open_pools(&map, |_pool| None, now);
        assert_eq!(open, vec!["chatgpt"]);
    }

    #[test]
    fn clock_bridge_converts_a_wall_clock_offset_to_the_same_relative_instant() {
        let bridge = ClockBridge {
            wall_now: datetime!(2026-11-02 12:00:00 UTC),
            instant_now: Instant::now(),
        };
        let future = bridge.wall_now + time::Duration::hours(2);
        assert_eq!(
            bridge.to_instant(future),
            bridge.instant_now + std::time::Duration::from_secs(2 * 3600)
        );
    }

    #[test]
    fn clock_bridge_converts_a_past_wall_clock_offset_the_same_way() {
        let bridge = ClockBridge {
            wall_now: datetime!(2026-11-02 12:00:00 UTC),
            instant_now: Instant::now(),
        };
        let past = bridge.wall_now - time::Duration::minutes(30);
        assert_eq!(
            bridge.to_instant(past),
            bridge.instant_now - std::time::Duration::from_secs(30 * 60)
        );
    }

    #[test]
    fn store_breaker_query_reports_closed_for_an_absent_pool() {
        use super::selector::BreakerQuery;
        let dir = tempfile::tempdir().unwrap();
        let store = StoreBreakerQuery {
            config_dir: dir.path(),
            clock: ClockBridge::now(),
            gauge_for_pool: |_: &str| None,
            is_pane_live: |_: &str| false,
        };
        assert_eq!(store.status("nope"), selector::BreakerStatus::CLOSED);
    }

    #[test]
    fn store_breaker_query_maps_the_budget_sentinel_to_no_automatic_reset() {
        use super::selector::BreakerQuery;
        let dir = tempfile::tempdir().unwrap();
        manual_trip(dir.path(), "codex:test", "codex", Some("budget"), None).unwrap();
        let store = StoreBreakerQuery {
            config_dir: dir.path(),
            clock: ClockBridge::now(),
            gauge_for_pool: |_: &str| None,
            is_pane_live: |_: &str| false,
        };
        let status = store.status("codex:test");
        assert!(status.open);
        assert!(matches!(
            status.reset_at,
            selector::ResetAt::NoAutomaticReset
        ));
    }

    #[test]
    fn store_breaker_query_reports_half_open_with_no_active_trial_as_claimable() {
        use super::selector::BreakerQuery;
        let dir = tempfile::tempdir().unwrap();
        let trip_time = datetime!(2026-11-02 06:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            trip_time,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "manual fence",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: true,
            },
        )
        .unwrap();
        // Force past reset_at by resetting the stored reset_at backward.
        breaker_store::with_locked(dir.path(), |map| {
            map.get_mut("chatgpt").unwrap().reset_at =
                Some(format_seconds_utc(trip_time + time::Duration::minutes(1)));
        })
        .unwrap();
        let wall_now = trip_time + time::Duration::hours(1);
        let store = StoreBreakerQuery {
            config_dir: dir.path(),
            clock: ClockBridge {
                wall_now,
                instant_now: Instant::now(),
            },
            gauge_for_pool: |_: &str| None,
            is_pane_live: |_: &str| false,
        };
        let status = store.status("chatgpt");
        assert!(status.open);
        assert!(status.half_open);
        assert!(status.trial_claimed_by.is_none());
    }

    #[test]
    fn store_breaker_query_claim_trial_mints_a_token_and_refuses_a_second_claimant() {
        use super::selector::BreakerQuery;
        let dir = tempfile::tempdir().unwrap();
        let trip_time = datetime!(2026-11-02 06:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            trip_time,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "manual fence",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: true,
            },
        )
        .unwrap();
        breaker_store::with_locked(dir.path(), |map| {
            map.get_mut("chatgpt").unwrap().reset_at =
                Some(format_seconds_utc(trip_time + time::Duration::minutes(1)));
        })
        .unwrap();
        let wall_now = trip_time + time::Duration::hours(1);
        let store = StoreBreakerQuery {
            config_dir: dir.path(),
            clock: ClockBridge {
                wall_now,
                instant_now: Instant::now(),
            },
            gauge_for_pool: |_: &str| None,
            is_pane_live: |_: &str| false,
        };

        let token = store
            .claim_trial("chatgpt", "task-1", None)
            .expect("a half-open pool with no active trial must mint a token");
        assert!(!token.is_empty());

        // The same owner re-presenting its own token is idempotent.
        let same = store.claim_trial("chatgpt", "task-1", Some(&token));
        assert_eq!(same.as_deref(), Some(token.as_str()));

        // A different task, with no token, is refused — the trial is live.
        assert!(store.claim_trial("chatgpt", "task-2", None).is_none());

        let status = store.status("chatgpt");
        let claim = status.trial_claimed_by.expect("claim recorded on status");
        assert_eq!(claim.task_id, "task-1");
        assert_eq!(claim.token, token);
    }

    #[test]
    fn store_breaker_query_claim_trial_refuses_a_pool_that_is_not_half_open() {
        use super::selector::BreakerQuery;
        let dir = tempfile::tempdir().unwrap();
        let now = datetime!(2026-11-02 12:00:00 UTC);
        trip(
            dir.path(),
            "chatgpt",
            now,
            TripRequest {
                vendor: "codex",
                kind: BreakerKind::Rolling,
                rule: "codex_usage_limit_reached",
                caller: "test",
                evidence_path: None,
                pane_text: None,
                project_root: None,
                manual: false,
            },
        )
        .unwrap();
        let store = StoreBreakerQuery {
            config_dir: dir.path(),
            clock: ClockBridge {
                wall_now: now,
                instant_now: Instant::now(),
            },
            gauge_for_pool: |_: &str| None,
            is_pane_live: |_: &str| false,
        };
        assert!(store.claim_trial("chatgpt", "task-1", None).is_none());
    }
}

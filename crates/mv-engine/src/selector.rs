//! Account/seat selection for a role, given breaker and quota state.
//!
//! Ported from `multiverse/accounts.py`'s `AccountSelector`: `_availability`
//! (`accounts.py:1048-1237`), `candidates` (`:1282-1467`), `select`
//! (`:1467-1577`), and `select_or_wait` (`:1593-1714`).
//!
//! `mv-config`'s `Account`/`FallbackPolicy` types and the real breaker/
//! seatstate/quota stores (`mv-engine::{breaker,seatstate,limits}`,
//! `mv-state::{breaker_store,seatstate_store,gauge}`) have not landed yet —
//! this module is deliberately self-contained, the same way `mv-herdr`'s
//! `error.rs` is. It defines its own [`Seat`] shape and a narrow
//! [`BreakerQuery`] trait rather than importing types that do not exist in
//! this checkout. A caller assembles a `Seat` per chain entry from
//! `mv-config`'s account config plus a live breaker/seatstate/quota read
//! once those land; this module never reads a store itself.
//!
//! Role names are taken as plain `&str` (the wire name a caller already
//! has from `mv-config`) rather than a role enum, for the same reason:
//! `mv-config::roles` has not landed. The one role distinction this module
//! cares about — "is this seat's pool breaker check skipped for the
//! orchestrator" and "does the reviewer's own quota floor apply" — is
//! expressed by comparing against caller-supplied role names
//! (`nexus_role`, `reviewer_role`), not a hardcoded wire string.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// A chain entry's static/observed state, as far as selection needs to
/// know it. Stand-in for `mv-config::Account` joined with a live read of
/// `mv-engine::{breaker,seatstate,limits}` — a caller builds one of these
/// per chain entry once those modules land.
#[derive(Debug, Clone)]
pub struct Seat {
    /// Account/seat name, used in skip messages and capacity keys.
    pub name: String,
    /// Vendor family (`account.vendor`) — drives Critic exclusion.
    pub vendor: String,
    /// Quota pool (`account.pool`) — drives breaker/capacity keying.
    /// Independent of `vendor`: two seats can share a vendor while
    /// holding distinct pools, or share a pool across two account names
    /// (two codex-family account names on one `chatgpt` pool).
    pub pool: String,
    /// Agent kind (`"claude" | "codex" | "grok" | "local" | ...`).
    pub kind: String,
    /// `account.available` — a static operator-set flag.
    pub available: bool,
    /// Human reason shown when `available` is false
    /// (`account.unavailable_reason`).
    pub unavailable_reason: Option<String>,
    /// `account.unavailable_until`, already resolved relative to the
    /// caller's own clock.
    pub unavailable_until: Option<Instant>,
    /// `seatstate.is_lent(name)`.
    pub lent: bool,
    /// Whether the role being dispatched has a model configured for this
    /// seat's `kind` (`settings.serves(account.kind)`).
    pub has_model: bool,
    /// `account.roles_allowed`; `None` means every role is allowed.
    pub roles_allowed: Option<Vec<String>>,
    /// `account.max_brief_chars`; `None` means uncapped.
    pub max_brief_chars: Option<usize>,
    /// Whether a non-empty command wrapper is configured. Only consulted
    /// when `kind == "claude"`.
    pub has_command: bool,
    /// `seatstate.get_cap(name)`; `None` means uncapped.
    pub cap: Option<u32>,
    /// Live open-agent count for this seat, or `None` when Herdr is
    /// unreachable. Only consulted when `cap` is set.
    pub open_count: Option<u32>,
    /// Last-read primary usage-window percent left, or `None` if unknown.
    pub quota_primary_percent: Option<f64>,
    /// Last-read secondary (e.g. weekly) usage-window percent left, or
    /// `None` if unknown/not applicable.
    pub quota_secondary_percent: Option<f64>,
    /// Liveness probe result for a `vendor == "local"` seat; `None` for
    /// every other vendor (never consulted). `Ok(())` is a successful probe;
    /// `Err(reason)` carries [`LocalLivenessProbe`]'s own prose verbatim —
    /// `_probe_ollama_liveness`'s wording (`accounts.py:521-566`) — which
    /// becomes check 6's skip reason unchanged. A caller (`mv-cli`'s
    /// `check_seat`/`account_state`) calls the probe
    /// once per seat read and fills this field; this module itself never
    /// performs the GET (see the module doc).
    pub local_alive: Option<Result<(), String>>,
}

/// `_LOCAL_LIVENESS_TIMEOUT` (`accounts.py:518`): how long the HTTP
/// readiness probe for a `vendor = "local"` seat's `health_url` may wait,
/// per socket operation (connect, then each read), before deciding the
/// seat is unreachable. Mirrors Python's `urllib.request.urlopen(...,
/// timeout=...)`, which bounds each blocking socket call rather than the
/// request as a whole — a slow trickle can still take longer than this in
/// either implementation.
pub const LOCAL_LIVENESS_TIMEOUT_SECS: f64 = 3.0;

/// `_probe_ollama_liveness`'s injection seam (`accounts.py:521-566`,
/// wired into `AccountSelector.__init__`'s `liveness_probe` parameter):
/// whether a configured local seat's HTTP API is ready, and why not.
/// [`HttpLocalLivenessProbe`] is the real, network-touching implementation;
/// a caller's tests inject a fixed-answer fake instead, so no suite run
/// ever makes a real GET (card `20261106-b-255666`).
pub trait LocalLivenessProbe {
    /// `account_name` feeds the "no health URL configured for local
    /// account {name}" wording when `url` is blank; `url` is the
    /// account's configured `health_url` (LM Studio's `/v1/models` for
    /// the Windows seat).
    fn probe(&self, account_name: &str, url: &str) -> Result<(), String>;
}

/// The production [`LocalLivenessProbe`]: a real, bounded GET of the
/// account's `health_url`, built on `std::net` alone — this workspace adds
/// no HTTP client dependency, matching its "no new async runtime" posture
/// for subprocess execution (AGENTS.md) and keeping `cargo deny check`'s
/// vetted dependency set unchanged for a single local-only health check.
pub struct HttpLocalLivenessProbe;

impl LocalLivenessProbe for HttpLocalLivenessProbe {
    fn probe(&self, account_name: &str, url: &str) -> Result<(), String> {
        probe_local_http(
            account_name,
            url,
            Duration::from_secs_f64(LOCAL_LIVENESS_TIMEOUT_SECS),
        )
    }
}

/// `http://host[:port][/path]` split into connect target and request path.
/// Only plain `http` is supported — a local seat's health endpoint is a
/// loopback/LAN address, never one needing TLS, and this workspace has no
/// TLS dependency to spend on it.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, port.parse::<u16>().ok()?),
        None => (authority, 80),
    };
    if host.is_empty() {
        return None;
    }
    Some((host.to_string(), port, path.to_string()))
}

/// Reads an HTTP status code off a response's leading status line
/// (`"HTTP/1.1 200 OK"`); `None` if the buffer has no parseable one.
fn parse_status_code(response: &[u8]) -> Option<u16> {
    let text = String::from_utf8_lossy(response);
    let status_line = text.lines().next()?;
    let code = status_line.split_whitespace().nth(1)?;
    code.parse::<u16>().ok()
}

/// A read/write/connect error's detail, in `_probe_ollama_liveness`'s own
/// vocabulary: Python's `str(exc)` on a `socket.timeout`/`TimeoutError`
/// reads as exactly `"timed out"` (the wording the originating backlog
/// finding quotes), so a Rust I/O timeout is normalized to match rather
/// than printing `io::Error`'s own, differently-worded message.
fn describe_io_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "timed out".to_string(),
        _ => error.to_string(),
    }
}

/// `_probe_ollama_liveness` (`accounts.py:521-566`): a GET of `url`, not a
/// cached flag or a bare TCP connect. The full body is read (not just
/// headers) under the same bounded timeout, so a server that accepts a
/// connection and sends headers but never completes the response is
/// treated as unreachable exactly as Python's own `response.read()` would
/// be — matching that function's own documented intent, not just its
/// happy path.
fn probe_local_http(account_name: &str, url: &str, timeout: Duration) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Err(format!(
            "no health URL configured for local account {account_name}"
        ));
    }
    let Some((host, port, path)) = parse_http_url(url) else {
        return Err(format!(
            "local seat health check failed at {url} (unsupported or invalid URL)"
        ));
    };
    let addr = (host.as_str(), port)
        .to_socket_addrs()
        .ok()
        .and_then(|mut addrs| addrs.next());
    let Some(addr) = addr else {
        return Err(format!(
            "local seat health check failed at {url} (could not resolve host)"
        ));
    };
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(error) => {
            return Err(format!(
                "local seat health check failed at {url} ({})",
                describe_io_error(&error)
            ))
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    let request =
        format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: multiverse-health-probe/1\r\n\r\n");
    if let Err(error) = stream.write_all(request.as_bytes()) {
        return Err(format!(
            "local seat health check failed at {url} ({})",
            describe_io_error(&error)
        ));
    }
    let mut response = Vec::new();
    if let Err(error) = stream.read_to_end(&mut response) {
        return Err(format!(
            "local seat health check failed at {url} ({})",
            describe_io_error(&error)
        ));
    }
    match parse_status_code(&response) {
        Some(status) if (200..300).contains(&status) => Ok(()),
        Some(status) => Err(format!(
            "local seat health check at {url} returned HTTP {status}"
        )),
        None => Err(format!(
            "local seat health check failed at {url} (malformed HTTP response)"
        )),
    }
}

/// The fleet-wide and reviewer-specific quota floors a selection run
/// checks. Mirrors `config.dispatch_min_quota_percent` and
/// `config.review_policy.min_quota_percent`.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaFloors {
    /// Critic's own floor (`review_policy.min_quota_percent`); only
    /// consulted when the role being dispatched is the reviewer role.
    /// Below this floor (strictly) is [`Cause::LowQuota`].
    pub reviewer_min_quota_percent: Option<f64>,
    /// Fleet-wide floor (`dispatch_min_quota_percent`); consulted for
    /// every other role, or for the reviewer role when it has no floor of
    /// its own. At or below this floor is [`Cause::Draining`]. `0` or
    /// `None` disables it.
    pub fleet_min_quota_percent: Option<f64>,
}

fn quota_strictly_below(seat: &Seat, floor: f64) -> bool {
    seat.quota_primary_percent.is_some_and(|p| p < floor)
        || seat.quota_secondary_percent.is_some_and(|p| p < floor)
}

fn quota_at_or_below(seat: &Seat, floor: f64) -> bool {
    seat.quota_primary_percent.is_some_and(|p| p <= floor)
        || seat.quota_secondary_percent.is_some_and(|p| p <= floor)
}

/// `FallbackPolicy` (`config.py:543`), controlling how far [`select`] walks
/// past an unusable seat before giving up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackPolicy {
    /// Stop at the first `Unavailable`, `AtCapacity`, or `NoCommand` skip
    /// rather than silently swapping seats.
    Warn,
    /// Stop at the first `AtCapacity` skip, or an `Unavailable` seat with
    /// a near-term timed return — the stop condition [`select_or_wait`]
    /// polls on.
    Queue,
    /// Tolerate exactly one `Unavailable`/`AtCapacity` skip, then stop.
    DegradeOnce,
    /// Walk the whole chain regardless of skip reason (the default).
    Degrade,
}

/// Why one chain entry could not be used, without the per-seat detail —
/// used to classify and compare skip reasons (`SkipCause` in
/// `accounts.py`). Checked, in this order, by `evaluate_candidate`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cause {
    VendorExcluded,
    NoModel,
    RoleNotAllowed,
    BriefTooLong,
    NoCommand,
    Unavailable,
    AtCapacity,
    LowQuota,
    Draining,
}

/// Causes that reflect deliberate configuration rather than an outage —
/// `_CHAIN_EXHAUSTED_BY_DESIGN_CAUSES` (`accounts.py:343`). Notably,
/// `NoModel`/`NoCommand`/`Draining` are *not* in this set despite also
/// never being a real candidate.
const BY_DESIGN_CAUSES: [Cause; 3] = [
    Cause::VendorExcluded,
    Cause::RoleNotAllowed,
    Cause::BriefTooLong,
];

/// One chain entry that could not be used, carrying enough detail to
/// build a [`ChainExhaustedError`] message and to drive
/// [`FallbackPolicy`]'s stop conditions.
#[derive(Debug, Clone, PartialEq)]
pub struct Skip {
    pub seat: String,
    pub cause: Cause,
    pub reason: String,
    /// Whether `_returns_soon` judged this seat likely to become usable
    /// again within the fixed 1800s horizon. Always `false` for causes
    /// other than `Unavailable`; irrelevant to every policy but `Queue`.
    pub returns_soon: bool,
}

/// The fence state of one quota pool, as far as the selector needs to
/// know it — stand-in for `breaker.read_status`'s return narrowed to
/// exactly what `_availability`/`_returns_soon`/`select` consume.
#[derive(Debug, Clone, PartialEq)]
pub struct BreakerStatus {
    pub open: bool,
    pub half_open: bool,
    pub reset_at: ResetAt,
    /// Wall-clock `isoformat(timespec="seconds")` form of a timed
    /// [`ResetAt::At`], used only in skip-reason prose (`accounts.py`
    /// `_availability`: `"breaker open for pool {pool!r}, resets {iso}"`).
    /// `Instant` cannot recover that string, so [`crate::breaker::StoreBreakerQuery`]
    /// fills it when it still has the `OffsetDateTime`.
    pub reset_display: Option<String>,
    /// Set only when a trial is currently active; compared to the
    /// dispatch's own task id to judge whether the trial is still
    /// claimable.
    pub trial_claimed_by: Option<TrialClaim>,
}

impl BreakerStatus {
    /// The universal closed/available answer.
    pub const CLOSED: BreakerStatus = BreakerStatus {
        open: false,
        half_open: false,
        reset_at: ResetAt::None,
        reset_display: None,
        trial_claimed_by: None,
    };
}

/// When a pool's fence is open, when (if ever) it is expected to lift.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ResetAt {
    /// A concrete instant the fence is expected to lift.
    At(Instant),
    /// `breaker.NO_AUTOMATIC_RESET` — human-reset-only; rendered as prose,
    /// never a timestamp.
    NoAutomaticReset,
    /// No reset recorded at all (closed breaker, or a legacy bare entry).
    None,
}

/// Identifies whoever currently holds a half-open trial claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrialClaim {
    pub task_id: String,
    pub token: String,
}

/// Breaker consumption narrowed to exactly the two entry points the
/// selector calls: a read, and an attempt to claim a half-open trial.
/// No other breaker mechanics (trip/reset/escalation) are needed here.
pub trait BreakerQuery {
    /// Current fence state for `pool`.
    fn status(&self, pool: &str) -> BreakerStatus;

    /// Attempt to claim (or re-confirm) the half-open trial slot for
    /// `pool` on behalf of `task_id`. Returns the owning token on
    /// success, `None` if another dispatch already holds it.
    fn claim_trial(&self, pool: &str, task_id: &str, token: Option<&str>) -> Option<String>;
}

/// `select`/`select_or_wait`'s fixed horizon for "returns soon"
/// (`DEFAULT_SEAT_WAIT_TIMEOUT`, `accounts.py:934`) — independent of
/// whatever timeout a caller passes to [`select_or_wait`].
const RETURNS_SOON_HORIZON: Duration = Duration::from_secs(1800);

/// `_returns_soon` (`accounts.py:1237-1260`): whether `seat` is likely to
/// become usable again within [`RETURNS_SOON_HORIZON`] of `now`. Only
/// meaningful for a seat that is currently `Unavailable`; `Queue` uses
/// this to decide whether to keep waiting on it rather than giving up.
fn returns_soon(seat: &Seat, breaker: &dyn BreakerQuery, now: Instant) -> bool {
    if !seat.available || !seat.lent {
        return false;
    }
    let horizon = now + RETURNS_SOON_HORIZON;
    let mut return_at = seat.unavailable_until;
    if let Some(until) = return_at {
        if until > horizon {
            return false;
        }
    }
    // Only an *open* breaker can push the return time out further (or
    // rule it out); a closed breaker leaves `unavailable_until` as the
    // whole answer, matching `_returns_soon` only consulting
    // `self._breaker_view` inside its `if status.open:` branch.
    let status = breaker.status(&seat.pool);
    if status.open {
        let reset_at = match status.reset_at {
            ResetAt::At(t) => Some(t),
            ResetAt::NoAutomaticReset | ResetAt::None => None,
        };
        if status.half_open || reset_at.is_none_or(|t| t > horizon) {
            return false;
        }
        return_at = match (return_at, reset_at) {
            (Some(u), Some(r)) => Some(u.max(r)),
            (Some(u), None) => Some(u),
            (None, r) => r,
        };
    }
    return_at.is_some_and(|t| now < t && t <= horizon)
}

fn unavailable(
    seat: &Seat,
    reason: impl Into<String>,
    breaker: &dyn BreakerQuery,
    now: Instant,
) -> Skip {
    Skip {
        seat: seat.name.clone(),
        cause: Cause::Unavailable,
        reason: reason.into(),
        returns_soon: returns_soon(seat, breaker, now),
    }
}

fn at_capacity(seat: &Seat, reason: impl Into<String>) -> Skip {
    Skip {
        seat: seat.name.clone(),
        cause: Cause::AtCapacity,
        reason: reason.into(),
        returns_soon: false,
    }
}

/// `_availability` (`accounts.py:1048-1237`): six ordered,
/// short-circuiting checks. `role == nexus_role` skips only the breaker
/// check (4), and only while the pool is not half-open — a manual fence
/// past its own reset still blocks the orchestrator until a trial is
/// claimed.
#[allow(clippy::too_many_arguments)]
fn availability(
    seat: &Seat,
    role: &str,
    nexus_role: &str,
    breaker: &dyn BreakerQuery,
    task_id: &str,
    trial_token: Option<&str>,
    now: Instant,
) -> Result<(), Skip> {
    // 1. Static operator flag.
    if !seat.available {
        return Err(unavailable(
            seat,
            seat.unavailable_reason
                .clone()
                .unwrap_or_else(|| "marked unavailable in config".to_string()),
            breaker,
            now,
        ));
    }

    // 2. A still-future unavailable_until.
    if let Some(until) = seat.unavailable_until {
        if until > now {
            return Err(unavailable(
                seat,
                "unavailable until a future time",
                breaker,
                now,
            ));
        }
    }

    // 3. Lent off.
    if !seat.lent {
        return Err(unavailable(
            seat,
            "lent off — not in chains right now (`accounts lend` to change it)",
            breaker,
            now,
        ));
    }

    // 4. Pool breaker, bypassed only for the orchestrator while not half-open.
    let status = breaker.status(&seat.pool);
    let trial_candidate = status.half_open
        && status.trial_claimed_by.as_ref().is_none_or(|claim| {
            trial_token.is_some_and(|token| claim.task_id == task_id && claim.token == token)
        });
    let breaker_check_applies = role != nexus_role || status.half_open;
    if status.open && !trial_candidate && breaker_check_applies {
        let reason = if status.half_open {
            format!(
                "HALF-OPEN (trial: {})",
                status
                    .trial_claimed_by
                    .as_ref()
                    .map(|c| c.task_id.as_str())
                    .unwrap_or("unclaimed")
            )
        } else if matches!(status.reset_at, ResetAt::NoAutomaticReset) {
            format!("breaker open for pool '{}': no automatic reset — a human has to resolve this and run `accounts reset`", seat.pool)
        } else if let Some(iso) = &status.reset_display {
            format!("breaker open for pool '{}', resets {iso}", seat.pool)
        } else {
            format!("breaker open for pool '{}'", seat.pool)
        };
        return Err(unavailable(seat, reason, breaker, now));
    }

    // 5. Seat cap vs. live open-agent count.
    if let Some(cap) = seat.cap {
        match seat.open_count {
            None => {
                return Err(at_capacity(
                    seat,
                    format!(
                        "at cap: open-agent count unreadable (Herdr unreachable), cap is {cap}"
                    ),
                ))
            }
            Some(open) if open >= cap => {
                return Err(at_capacity(
                    seat,
                    format!("at cap: {open}/{cap} open agents"),
                ))
            }
            Some(_) => {}
        }
    }

    // 6. Local liveness, only for a local vendor.
    if seat.vendor == "local" {
        if let Some(Err(reason)) = &seat.local_alive {
            return Err(unavailable(seat, reason.clone(), breaker, now));
        }
    }

    Ok(())
}

/// `candidates`' per-entry filter (`accounts.py:1282-1467`), in its exact
/// order: vendor exclusion, no model, roles_allowed, max_brief_chars, no
/// command, availability, then (only once available) the reviewer's own
/// quota floor or else the fleet-wide one.
#[allow(clippy::too_many_arguments)]
fn evaluate_candidate(
    seat: &Seat,
    role: &str,
    reviewer_role: &str,
    nexus_role: &str,
    exclude_vendors: &[String],
    task_len_chars: usize,
    floors: QuotaFloors,
    breaker: &dyn BreakerQuery,
    task_id: &str,
    trial_token: Option<&str>,
    now: Instant,
) -> Result<(), Skip> {
    if exclude_vendors.iter().any(|v| v == &seat.vendor) {
        return Err(Skip {
            seat: seat.name.clone(),
            cause: Cause::VendorExcluded,
            // Python's `f"vendor {account.vendor!r} excluded"`: `!r` is a
            // single-quoted repr, not `{:?}`'s double-quoted Rust Debug
            // form — a difference `conformance/diff_fuzz.py` catches on
            // any fallback reason this clause ends up inside (card
            // `<this task>`, scout2-gaps.md row #30's accompanying council
            // divergence, surfaced through council's own skip reasons).
            reason: format!("vendor '{}' excluded", seat.vendor),
            returns_soon: false,
        });
    }
    if !seat.has_model {
        return Err(Skip {
            seat: seat.name.clone(),
            cause: Cause::NoModel,
            // Same `!r`-vs-`{:?}` quoting fix as the vendor-excluded skip
            // just above.
            reason: format!("{role} has no model for kind '{}'", seat.kind),
            returns_soon: false,
        });
    }
    if let Some(allowed) = &seat.roles_allowed {
        if !allowed.iter().any(|r| r == role) {
            return Err(Skip {
                seat: seat.name.clone(),
                cause: Cause::RoleNotAllowed,
                reason: format!("{role} is not in {}'s roles_allowed", seat.name),
                returns_soon: false,
            });
        }
    }
    if let Some(max_chars) = seat.max_brief_chars {
        if task_len_chars > max_chars {
            return Err(Skip {
                seat: seat.name.clone(),
                cause: Cause::BriefTooLong,
                reason: format!(
                    "brief is {task_len_chars} chars, over {}'s max_brief_chars ({max_chars})",
                    seat.name
                ),
                returns_soon: false,
            });
        }
    }
    if seat.kind == "claude" && !seat.has_command {
        return Err(Skip {
            seat: seat.name.clone(),
            cause: Cause::NoCommand,
            reason: format!(
                "no command wrapper configured; set accounts.{}.command to the Claude wrapper from docs/install.md",
                seat.name
            ),
            returns_soon: false,
        });
    }

    availability(seat, role, nexus_role, breaker, task_id, trial_token, now)?;

    if role == reviewer_role {
        if let Some(floor) = floors.reviewer_min_quota_percent {
            if quota_strictly_below(seat, floor) {
                return Err(Skip {
                    seat: seat.name.clone(),
                    cause: Cause::LowQuota,
                    reason: format!("below reviewer's quota floor of {floor}%"),
                    returns_soon: false,
                });
            }
            return Ok(());
        }
    }
    if let Some(floor) = floors.fleet_min_quota_percent.filter(|f| *f > 0.0) {
        if quota_at_or_below(seat, floor) {
            return Err(Skip {
                seat: seat.name.clone(),
                cause: Cause::Draining,
                reason: format!("draining: at or below the fleet quota floor of {floor}%"),
                returns_soon: false,
            });
        }
    }
    Ok(())
}

/// Whether hitting `skip` under `policy` should end the walk right here.
/// `degrade_once_spent` tracks whether `DegradeOnce` has already used its
/// one tolerated step; vendor/model/role/brief-length mismatches never
/// consume it (`accounts.py`'s `VENDOR_EXCLUDED`/`NO_MODEL` carve-out,
/// extended here to the rest of [`BY_DESIGN_CAUSES`] plus `NoModel`,
/// none of which are in either policy's trigger set).
fn should_stop(policy: FallbackPolicy, skip: &Skip, degrade_once_spent: &mut bool) -> bool {
    match policy {
        FallbackPolicy::Degrade => false,
        FallbackPolicy::Warn => matches!(
            skip.cause,
            Cause::Unavailable | Cause::AtCapacity | Cause::NoCommand
        ),
        FallbackPolicy::Queue => {
            skip.cause == Cause::AtCapacity
                || (skip.cause == Cause::Unavailable && skip.returns_soon)
        }
        FallbackPolicy::DegradeOnce => {
            if matches!(skip.cause, Cause::Unavailable | Cause::AtCapacity) {
                if *degrade_once_spent {
                    true
                } else {
                    *degrade_once_spent = true;
                    false
                }
            } else {
                false
            }
        }
    }
}

/// A successful selection: the chosen seat, every entry skipped to reach
/// it, and (when the chosen seat's pool was half-open) the trial token
/// claimed on its behalf.
#[derive(Debug, Clone)]
pub struct Selection {
    pub seat: Seat,
    pub skipped: Vec<Skip>,
    pub trial_token: Option<String>,
}

impl Selection {
    /// Whether this selection fell back past at least one skipped seat,
    /// as opposed to landing on the chain's first entry.
    pub fn is_fallback(&self) -> bool {
        !self.skipped.is_empty()
    }
}

/// Every chain entry was unusable. Mirrors `ChainExhaustedError`
/// (`accounts.py:348-487`). `policy` is `Some` only when an early
/// policy-driven stop ended the walk (`Warn`/`Queue`/a spent
/// `DegradeOnce` step); it is `None` when `Degrade` ran out the chain
/// "naturally".
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{message}")]
pub struct ChainExhaustedError {
    pub role: String,
    pub skipped: Vec<Skip>,
    pub exclude_vendors: Vec<String>,
    pub policy: Option<FallbackPolicy>,
    pub message: String,
}

fn causes_of(skipped: &[Skip]) -> HashSet<Cause> {
    skipped.iter().map(|s| s.cause).collect()
}

/// `s.account: s.reason` pairs, joined by `sep` — Python's own
/// `"; ".join(...)`/`"\n".join(...)` comprehensions (`accounts.py:389,
/// 414,432`), which share this exact pairing and differ only in
/// separator and whether each line is bulleted.
fn join_reasons(skipped: &[Skip], sep: &str) -> String {
    skipped
        .iter()
        .map(|s| format!("{}: {}", s.seat, s.reason))
        .collect::<Vec<_>>()
        .join(sep)
}

/// `  - account: reason` lines, one per skip — `accounts.py:439`'s own
/// `"\n".join(f"  - {s.account}: {s.reason}" for s in skipped)`.
fn format_reasons(skipped: &[Skip]) -> String {
    skipped
        .iter()
        .map(|s| format!("  - {}: {}", s.seat, s.reason))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `role`'s current display name, falling back to the raw wire string for
/// anything [`mv_config::roles::Role`] does not recognise (never expected
/// in practice — every caller passes one of the nine current keys — but
/// this message must not panic over a title).
fn display_role(role: &str) -> String {
    mv_config::roles::Role::from_config_key(role)
        .map(|r| r.display_name().to_string())
        .unwrap_or_else(|_| role.to_string())
}

/// `self.policy`'s wire word — Python's `FallbackPolicy(str, Enum)` reads
/// back as its own value (`"queue"`, `"warn"`, `"degrade_once"`) in an
/// f-string, never a Python-ism like `FallbackPolicy.QUEUE`.
fn policy_word(policy: FallbackPolicy) -> &'static str {
    match policy {
        FallbackPolicy::Warn => "warn",
        FallbackPolicy::Queue => "queue",
        FallbackPolicy::DegradeOnce => "degrade_once",
        FallbackPolicy::Degrade => "degrade",
    }
}

/// Byte-faithful port of `ChainExhaustedError.__init__`
/// (`accounts.py:367-485`), including its branch order, its early
/// returns, and every literal sentence — this is the text a human (or an
/// orchestrator parsing stderr) reads, and [`crate::council`]'s own
/// `CouncilTooSmallError` embeds this message verbatim (newline-flattened,
/// `council.py:269`) rather than re-deriving a summary of its own.
fn build_message(
    role: &str,
    project: &str,
    skipped: &[Skip],
    exclude_vendors: &[String],
    policy: Option<FallbackPolicy>,
) -> String {
    let role_display = display_role(role);
    let causes = causes_of(skipped);

    // `accounts.py:388-401`: an early policy-driven stop (`Warn`/`Queue`)
    // never reaches the generic "every account ... is unavailable" shape
    // below, and carries no TAIL sentence of its own.
    if matches!(
        policy,
        Some(FallbackPolicy::Queue) | Some(FallbackPolicy::Warn)
    ) {
        let policy = policy.expect("guarded by the outer match");
        let detail = join_reasons(skipped, "; ");
        let action = match policy {
            FallbackPolicy::Queue => {
                "Wait for the seat to become available; capacity waits can be retried with --wait-for-seat."
            }
            FallbackPolicy::Warn => "Warn and stop; automatic seat swaps and capacity waits are disabled.",
            FallbackPolicy::DegradeOnce | FallbackPolicy::Degrade => {
                unreachable!("guarded by the outer match")
            }
        };
        return format!(
            "the first eligible seat in the {role_display} chain for '{project}' is \
             unavailable ({detail}); fallback policy is {}, which does not permit a \
             seat swap. {action}",
            policy_word(policy)
        );
    }

    // `accounts.py:403-419`: a chain skipped purely for `AtCapacity` is a
    // retry, not a config problem — the fleet may be well below its own
    // cap while this one project's chain is briefly full.
    if skipped.iter().all(|s| s.cause == Cause::AtCapacity) && !skipped.is_empty() {
        return format!(
            "every seat in the {role_display} chain for '{project}' is at capacity \
             ({}); retry on a later tick.",
            join_reasons(skipped, "; ")
        );
    }

    // `accounts.py:421-438`: `AtCapacity` mixed only with by-design causes
    // (never a real candidate either way) keeps the same "retry" framing,
    // not the generic "something is unavailable" one below.
    if !skipped.is_empty()
        && causes.contains(&Cause::AtCapacity)
        && causes
            .iter()
            .all(|c| *c == Cause::AtCapacity || BY_DESIGN_CAUSES.contains(c))
    {
        return format!(
            "every seat in the {role_display} chain for '{project}' is at capacity \
             or excluded by design ({}); retry on a later tick.",
            join_reasons(skipped, "; ")
        );
    }

    // `accounts.py:439-443`: the generic shape every other exhaustion
    // falls through to.
    let mut message = format!(
        "every account in the {role_display} chain for '{project}' is unavailable:\n{}\n",
        format_reasons(skipped)
    );

    // `accounts.py:451-465`: a chain exhausted entirely by eligibility
    // causes (vendor exclusion, a role allow-list, a brief-length cap) is
    // working as designed, not a missing account.
    if !skipped.is_empty()
        && !causes.is_empty()
        && causes.iter().all(|c| BY_DESIGN_CAUSES.contains(c))
    {
        if causes == HashSet::from([Cause::VendorExcluded]) && !exclude_vendors.is_empty() {
            let vendors = exclude_vendors
                .iter()
                .map(|v| format!("'{v}'"))
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!(
                "\nEvery seat left in the chain shares the excluded vendor ({vendors}) \
                 — reviewing same-vendor is refused by design (§12.1), not a missing \
                 account.\n"
            ));
        } else {
            message.push_str(
                "\nEvery seat left in the chain was never a candidate for this \
                 dispatch — excluded by §12.1's vendor rule, a role allow-list, or a \
                 brief-length cap — refused by design, not a missing account.\n",
            );
        }
    }

    // `accounts.py:466-473`.
    if !skipped.is_empty() && skipped.iter().all(|s| s.cause == Cause::LowQuota) {
        message.push_str(
            "\nEvery seat left in the chain is below [review_policy].min_quota_percent \
             — not down, just scarce. Wait for a seat's quota to reset, or lower \
             min_quota_percent in config if spending one anyway is acceptable.\n",
        );
    }

    // `accounts.py:474-480`: the only `policy` that can still be `Some`
    // here is `DegradeOnce` — `Warn`/`Queue` already returned above, and
    // `Degrade` running the chain out naturally passes `None` (this
    // module's own [`select`] mirrors that).
    if let Some(policy) = policy {
        message.push_str(&format!(
            "\n{role_display}'s fallback policy is {}, which accepts only one step \
             down the chain before refusing. That step is already spent on the seats \
             above, so any seat further down the chain was never tried.\n",
            policy_word(policy)
        ));
    }

    // `accounts.py:481-484`.
    message.push_str(
        "Multiverse will not route to an account outside the chain (route by path, \
         never by quota).",
    );
    message
}

impl ChainExhaustedError {
    fn new(
        role: &str,
        project: &str,
        skipped: Vec<Skip>,
        exclude_vendors: &[String],
        policy: Option<FallbackPolicy>,
    ) -> Self {
        let message = build_message(role, project, &skipped, exclude_vendors, policy);
        ChainExhaustedError {
            role: role.to_string(),
            skipped,
            exclude_vendors: exclude_vendors.to_vec(),
            policy,
            message,
        }
    }
}

/// Everything [`select`]/[`select_or_wait`] need about one dispatch's
/// chain walk *except* the chain itself, bundled so neither function
/// takes an unreadable argument list. The chain is passed separately
/// (to [`select`] directly, to [`select_or_wait`] through a closure) so a
/// caller can re-read live seat state — cap, open count, quota, breaker
/// inputs — on every poll, the way `_open_counts()`'s cache is cleared
/// and Herdr is re-read on every `select_or_wait` iteration
/// (`accounts.py:1593-1714`) rather than deciding against one stale
/// snapshot for the whole wait.
#[derive(Debug, Clone)]
pub struct SelectParams<'a> {
    pub role: &'a str,
    pub reviewer_role: &'a str,
    pub nexus_role: &'a str,
    pub exclude_vendors: &'a [String],
    pub task_len_chars: usize,
    pub floors: QuotaFloors,
    pub policy: FallbackPolicy,
    pub task_id: &'a str,
    pub trial_token: Option<&'a str>,
    /// The dispatching project's name, quoted into
    /// [`ChainExhaustedError`]'s message the way Python's own
    /// `project.name!r}` is (`accounts.py:397,417,435,441`) — this
    /// module is otherwise store/project-agnostic, but the refusal text
    /// a human reads needs to say which project's chain ran out.
    pub project: &'a str,
}

/// Reorders `chain` by seat preference before a [`select`] or
/// [`select_or_wait`] walk — the port of `accounts.py`'s
/// `AccountSelector._reorder_chain_by_preference` (card
/// `20261102-b-423e4e`: today neither walker reorders at all, so
/// `accounts prefer` configuration has no effect on seat choice).
///
/// A stable partition, not a full sort: every seat `preferred` answers
/// `true` for is moved ahead of every seat it does not, each group
/// keeping its original relative order. `lend` plays no part here —
/// a lent-off seat is not specially demoted by this function, because
/// it does not need to be: `availability`'s own check already skips
/// a `seat.lent == false` entry wherever it ends up in the chain, so a
/// preferred-but-lent-off seat still loses to the next usable one.
/// "lend off beats prefer" is therefore a property of availability,
/// not of reordering.
///
/// This module stays deliberately store-agnostic (see the module
/// doc) — `preferred` is a plain closure rather than a live
/// `mv_engine::seatstate` read, so a caller supplies
/// `|name| seatstate::is_preferred(state_dir, name)` (or an
/// equivalent) once it has a seatstate store to read, and calls this
/// before [`select`]/[`select_or_wait`] rather than this module
/// reading one itself.
pub fn reorder_chain_by_preference(chain: &[Seat], preferred: impl Fn(&str) -> bool) -> Vec<Seat> {
    let mut front = Vec::with_capacity(chain.len());
    let mut back = Vec::with_capacity(chain.len());
    for seat in chain {
        if preferred(&seat.name) {
            front.push(seat.clone());
        } else {
            back.push(seat.clone());
        }
    }
    front.extend(back);
    front
}

/// `select` (`accounts.py:1467-1577`): walk the chain, claiming a
/// half-open trial on the first otherwise-usable seat and failing that
/// one candidate over (not the whole call) if another dispatch already
/// holds it.
pub fn select(
    chain: &[Seat],
    params: &SelectParams,
    breaker: &dyn BreakerQuery,
    now: Instant,
) -> Result<Selection, ChainExhaustedError> {
    let mut skipped = Vec::new();
    let mut degrade_once_spent = false;

    for seat in chain {
        let candidate = evaluate_candidate(
            seat,
            params.role,
            params.reviewer_role,
            params.nexus_role,
            params.exclude_vendors,
            params.task_len_chars,
            params.floors,
            breaker,
            params.task_id,
            params.trial_token,
            now,
        );
        let skip = match candidate {
            Ok(()) => {
                let status = breaker.status(&seat.pool);
                if !status.half_open {
                    return Ok(Selection {
                        seat: seat.clone(),
                        skipped,
                        trial_token: None,
                    });
                }
                match breaker.claim_trial(&seat.pool, params.task_id, params.trial_token) {
                    Some(token) => {
                        return Ok(Selection {
                            seat: seat.clone(),
                            skipped,
                            trial_token: Some(token),
                        })
                    }
                    None => Skip {
                        seat: seat.name.clone(),
                        cause: Cause::Unavailable,
                        reason: "half-open trial claimed by another dispatch".to_string(),
                        returns_soon: false,
                    },
                }
            }
            Err(skip) => skip,
        };

        let stop = should_stop(params.policy, &skip, &mut degrade_once_spent);
        skipped.push(skip);
        if stop {
            return Err(ChainExhaustedError::new(
                params.role,
                params.project,
                skipped,
                params.exclude_vendors,
                Some(params.policy),
            ));
        }
    }

    Err(ChainExhaustedError::new(
        params.role,
        params.project,
        skipped,
        params.exclude_vendors,
        None,
    ))
}

/// Something able to pause the calling thread, injected so
/// [`select_or_wait`] is testable without a real 1800-second wait.
pub trait Sleeper {
    fn sleep(&mut self, duration: Duration);
}

/// The production [`Sleeper`]: a real, blocking `std::thread::sleep`.
pub struct RealSleeper;

impl Sleeper for RealSleeper {
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// `DEFAULT_SEAT_WAIT_TIMEOUT` (`accounts.py:934`).
pub const DEFAULT_SEAT_WAIT_TIMEOUT_SECS: f64 = 1800.0;
/// `select_or_wait`'s default `poll_interval`.
pub const DEFAULT_POLL_INTERVAL_SECS: f64 = 30.0;

/// Why [`select_or_wait`] gave up.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum WaitError {
    /// `timeout` was `NaN`, infinite, or negative.
    #[error("timeout must be None or a finite, non-negative number of seconds, got {0:?}")]
    InvalidTimeout(f64),
    /// Either the chain ran out for a reason waiting cannot fix, or the
    /// timeout elapsed while every remaining skip was still at capacity.
    #[error(transparent)]
    ChainExhausted(#[from] ChainExhaustedError),
}

/// The entries [`select_or_wait`] is willing to keep waiting on: for
/// `Queue`, only the final skip (its whole point is "wait for the one
/// seat it would have chosen"); for every other policy, every skip from
/// this attempt.
fn wait_entries(err: &ChainExhaustedError) -> &[Skip] {
    if err.policy == Some(FallbackPolicy::Queue) {
        let len = err.skipped.len();
        &err.skipped[len.saturating_sub(1)..]
    } else {
        &err.skipped[..]
    }
}

fn should_keep_waiting(err: &ChainExhaustedError) -> bool {
    if err.policy == Some(FallbackPolicy::Warn) {
        return false;
    }
    let entries = wait_entries(err);
    !entries.is_empty() && entries.iter().all(|s| s.cause == Cause::AtCapacity)
}

/// Knobs for [`select_or_wait`] beyond the chain-walk request itself.
pub struct WaitOptions<'a> {
    /// `None` waits indefinitely; `Some(seconds)` must be finite and
    /// non-negative.
    pub timeout: Option<f64>,
    pub poll_interval: f64,
    pub sleeper: &'a mut dyn Sleeper,
    /// Called with a human-readable wait message before each sleep.
    pub on_wait: &'a mut dyn FnMut(&str),
}

impl<'a> WaitOptions<'a> {
    pub fn new(sleeper: &'a mut dyn Sleeper, on_wait: &'a mut dyn FnMut(&str)) -> Self {
        WaitOptions {
            timeout: Some(DEFAULT_SEAT_WAIT_TIMEOUT_SECS),
            poll_interval: DEFAULT_POLL_INTERVAL_SECS,
            sleeper,
            on_wait,
        }
    }
}

fn wait_message(err: &ChainExhaustedError, poll_interval: f64, waited: f64) -> String {
    let subject = if err.policy == Some(FallbackPolicy::Queue) {
        "the selected seat"
    } else {
        "every seat"
    };
    let entries = wait_entries(err)
        .iter()
        .map(|s| format!("{}: {}", s.seat, s.reason))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "waiting on {subject} ({entries}); polled every {poll_interval}s, waited {waited}s so far"
    )
}

/// `select_or_wait` (`accounts.py:1593-1714`): poll [`select`] until it
/// succeeds, the timeout elapses, or a skip is hit that waiting cannot
/// fix. `chain` is called fresh on every attempt (including the first) so
/// a caller can hand back newly-read live state each poll — mirroring
/// Python re-reading Herdr every iteration rather than trusting a stale
/// snapshot. `now` is likewise re-read every attempt so a fake clock can
/// advance it in lockstep with a fake [`Sleeper`].
pub fn select_or_wait(
    params: &SelectParams,
    breaker: &dyn BreakerQuery,
    mut chain: impl FnMut() -> Vec<Seat>,
    opts: WaitOptions,
    mut now: impl FnMut() -> Instant,
) -> Result<Selection, WaitError> {
    if let Some(timeout) = opts.timeout {
        if !timeout.is_finite() || timeout < 0.0 {
            return Err(WaitError::InvalidTimeout(timeout));
        }
    }

    let mut waited = 0.0_f64;
    loop {
        let current_chain = chain();
        match select(&current_chain, params, breaker, now()) {
            Ok(selection) => return Ok(selection),
            Err(err) => {
                if !should_keep_waiting(&err) {
                    return Err(WaitError::ChainExhausted(err));
                }
                if let Some(timeout) = opts.timeout {
                    if waited >= timeout {
                        return Err(WaitError::ChainExhausted(err));
                    }
                }
                (opts.on_wait)(&wait_message(&err, opts.poll_interval, waited));
                let this_sleep = match opts.timeout {
                    None => opts.poll_interval,
                    Some(timeout) => opts.poll_interval.min(timeout - waited),
                }
                .max(0.0);
                opts.sleeper.sleep(Duration::from_secs_f64(this_sleep));
                waited += this_sleep;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(name: &str, pool: &str) -> Seat {
        Seat {
            name: name.to_string(),
            vendor: pool.split(':').next().unwrap_or(pool).to_string(),
            pool: pool.to_string(),
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

    struct FakeBreaker {
        statuses: std::collections::HashMap<String, BreakerStatus>,
        claims: std::cell::RefCell<std::collections::HashMap<String, (String, String)>>,
    }

    impl FakeBreaker {
        fn new() -> Self {
            FakeBreaker {
                statuses: std::collections::HashMap::new(),
                claims: std::cell::RefCell::new(std::collections::HashMap::new()),
            }
        }

        fn with(mut self, pool: &str, status: BreakerStatus) -> Self {
            self.statuses.insert(pool.to_string(), status);
            self
        }
    }

    impl BreakerQuery for FakeBreaker {
        fn status(&self, pool: &str) -> BreakerStatus {
            self.statuses
                .get(pool)
                .cloned()
                .unwrap_or(BreakerStatus::CLOSED)
        }

        fn claim_trial(&self, pool: &str, task_id: &str, token: Option<&str>) -> Option<String> {
            let mut claims = self.claims.borrow_mut();
            match claims.get(pool) {
                Some((holder_task, holder_token)) => {
                    if holder_task == task_id && Some(holder_token.as_str()) == token {
                        Some(holder_token.clone())
                    } else {
                        None
                    }
                }
                None => {
                    let new_token = format!("token-{task_id}");
                    claims.insert(pool.to_string(), (task_id.to_string(), new_token.clone()));
                    Some(new_token)
                }
            }
        }
    }

    const ROLE: &str = "builder";
    const REVIEWER: &str = "critic";
    const NEXUS: &str = "nexus";

    fn params(exclude: &[String], policy: FallbackPolicy) -> SelectParams<'_> {
        SelectParams {
            role: ROLE,
            project: "sandbox",
            reviewer_role: REVIEWER,
            nexus_role: NEXUS,
            exclude_vendors: exclude,
            task_len_chars: 100,
            floors: QuotaFloors::default(),
            policy,
            task_id: "20261102-builder-abcdef",
            trial_token: None,
        }
    }

    // --- availability ordering ---

    #[test]
    fn available_false_beats_an_open_and_an_expired_breaker() {
        let mut s = seat("a", "pool");
        s.available = false;
        s.unavailable_reason = Some("disabled by operator".to_string());
        let breaker = FakeBreaker::new().with(
            "pool",
            BreakerStatus {
                open: true,
                half_open: false,
                reset_at: ResetAt::None,
                reset_display: None,
                trial_claimed_by: None,
            },
        );
        let err = availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).unwrap_err();
        assert_eq!(err.cause, Cause::Unavailable);
        assert_eq!(err.reason, "disabled by operator");
    }

    #[test]
    fn orchestrator_role_only_skips_the_breaker_check_when_not_half_open() {
        let s = seat("a", "pool");
        let breaker = FakeBreaker::new().with(
            "pool",
            BreakerStatus {
                open: true,
                half_open: false,
                reset_at: ResetAt::None,
                reset_display: None,
                trial_claimed_by: None,
            },
        );
        assert!(availability(&s, NEXUS, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
        assert!(availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_err());
    }

    #[test]
    fn orchestrator_still_blocked_by_a_half_open_breaker_without_a_trial() {
        let s = seat("a", "pool");
        let breaker = FakeBreaker::new().with(
            "pool",
            BreakerStatus {
                open: true,
                half_open: true,
                reset_at: ResetAt::None,
                reset_display: None,
                trial_claimed_by: Some(TrialClaim {
                    task_id: "other-task".to_string(),
                    token: "other-token".to_string(),
                }),
            },
        );
        let err = availability(&s, NEXUS, NEXUS, &breaker, "t1", None, Instant::now()).unwrap_err();
        assert_eq!(err.reason, "HALF-OPEN (trial: other-task)");
    }

    #[test]
    fn tripping_one_pool_leaves_a_sibling_pool_of_the_same_vendor_usable() {
        let codex = seat("codex", "chatgpt");
        let codex_work = seat("codex-work", "chatgpt:work");
        let breaker = FakeBreaker::new().with(
            "chatgpt",
            BreakerStatus {
                open: true,
                half_open: false,
                reset_at: ResetAt::None,
                reset_display: None,
                trial_claimed_by: None,
            },
        );
        assert!(availability(&codex, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_err());
        assert!(availability(
            &codex_work,
            ROLE,
            NEXUS,
            &breaker,
            "t1",
            None,
            Instant::now()
        )
        .is_ok());
    }

    #[test]
    fn timed_breaker_skip_reason_includes_the_reset_timestamp() {
        // Python `_availability` (`accounts.py:1205-1212`): a timed trip
        // renders `breaker open for pool {pool!r}, resets {iso}`. Found
        // missing the `, resets …` clause via `conformance/diff_fuzz.py`
        // seed 9000.
        let s = seat("a", "pool");
        let breaker = FakeBreaker::new().with(
            "pool",
            BreakerStatus {
                open: true,
                half_open: false,
                reset_at: ResetAt::At(Instant::now() + Duration::from_secs(3600)),
                reset_display: Some("2030-01-01T00:00:00+00:00".into()),
                trial_claimed_by: None,
            },
        );
        let err = availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).unwrap_err();
        assert_eq!(
            err.reason,
            "breaker open for pool 'pool', resets 2030-01-01T00:00:00+00:00"
        );
    }

    // --- capacity ---

    #[test]
    fn availability_below_cap_is_available() {
        let mut s = seat("a", "pool");
        s.cap = Some(2);
        s.open_count = Some(1);
        let breaker = FakeBreaker::new();
        assert!(availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
    }

    #[test]
    fn availability_at_cap_reads_at_capacity() {
        let mut s = seat("a", "pool");
        s.cap = Some(2);
        s.open_count = Some(2);
        let breaker = FakeBreaker::new();
        let err = availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).unwrap_err();
        assert_eq!(err.cause, Cause::AtCapacity);
    }

    #[test]
    fn unreadable_count_is_at_capacity_only_for_a_capped_seat() {
        let mut capped = seat("capped", "pool");
        capped.cap = Some(1);
        capped.open_count = None;
        let mut uncapped = seat("uncapped", "pool2");
        uncapped.cap = None;
        uncapped.open_count = None;
        let breaker = FakeBreaker::new();
        assert_eq!(
            availability(&capped, ROLE, NEXUS, &breaker, "t1", None, Instant::now())
                .unwrap_err()
                .cause,
            Cause::AtCapacity
        );
        assert!(availability(&uncapped, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
    }

    // --- local liveness (card `20261106-b-255666`) ---

    #[test]
    fn failed_local_liveness_probe_is_unavailable_with_the_probe_s_own_reason() {
        let mut s = seat("local-seat", "pool");
        s.vendor = "local".to_string();
        s.local_alive = Some(Err(
            "local seat health check failed at http://127.0.0.1:1234/v1/models (timed out)"
                .to_string(),
        ));
        let breaker = FakeBreaker::new();
        let err = availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).unwrap_err();
        assert_eq!(err.cause, Cause::Unavailable);
        assert_eq!(
            err.reason,
            "local seat health check failed at http://127.0.0.1:1234/v1/models (timed out)"
        );
    }

    #[test]
    fn successful_local_liveness_probe_is_available() {
        let mut s = seat("local-seat", "pool");
        s.vendor = "local".to_string();
        s.local_alive = Some(Ok(()));
        let breaker = FakeBreaker::new();
        assert!(availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
    }

    #[test]
    fn an_unprobed_local_seat_is_not_blocked_by_check_6() {
        // `local_alive: None` means "not probed by this caller" (e.g.
        // `seed_seat`'s digest-only build) — check 6 must not fail-closed
        // on a seat it was never asked to evaluate.
        let mut s = seat("local-seat", "pool");
        s.vendor = "local".to_string();
        let breaker = FakeBreaker::new();
        assert!(availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
    }

    #[test]
    fn a_failed_probe_on_a_non_local_vendor_is_never_consulted() {
        let mut s = seat("cloud-seat", "pool");
        s.local_alive = Some(Err("would fail the chain if read".to_string()));
        let breaker = FakeBreaker::new();
        assert!(availability(&s, ROLE, NEXUS, &breaker, "t1", None, Instant::now()).is_ok());
    }

    #[test]
    fn parse_http_url_splits_host_port_and_path() {
        assert_eq!(
            parse_http_url("http://127.0.0.1:1234/v1/models"),
            Some(("127.0.0.1".to_string(), 1234, "/v1/models".to_string()))
        );
        assert_eq!(
            parse_http_url("http://localhost"),
            Some(("localhost".to_string(), 80, "/".to_string()))
        );
        assert_eq!(parse_http_url("https://example.com"), None);
        assert_eq!(parse_http_url("http://:1234/"), None);
    }

    #[test]
    fn parse_status_code_reads_the_leading_status_line() {
        assert_eq!(parse_status_code(b"HTTP/1.1 200 OK\r\n\r\n{}"), Some(200));
        assert_eq!(
            parse_status_code(b"HTTP/1.1 404 Not Found\r\n\r\n"),
            Some(404)
        );
        assert_eq!(parse_status_code(b"not an http response"), None);
    }

    /// Spins a one-shot loopback listener that writes `response` to the
    /// first connection then closes — a hermetic stand-in for the LM
    /// Studio/Ollama endpoint `probe_local_http` targets, touching only
    /// `127.0.0.1` and never the real fleet (AGENTS.md).
    fn serve_once(response: &'static [u8]) -> std::net::SocketAddr {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response);
                let _ = stream.flush();
            }
        });
        addr
    }

    #[test]
    fn probe_local_http_reports_ok_for_a_2xx_response() {
        let addr = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        let url = format!("http://{addr}/v1/models");
        let result = probe_local_http("local", &url, Duration::from_secs(2));
        assert_eq!(result, Ok(()));
    }

    #[test]
    fn probe_local_http_reports_the_status_for_a_non_2xx_response() {
        let addr = serve_once(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
        let url = format!("http://{addr}/v1/models");
        let result = probe_local_http("local", &url, Duration::from_secs(2));
        assert_eq!(
            result,
            Err(format!(
                "local seat health check at {url} returned HTTP 503"
            ))
        );
    }

    #[test]
    fn probe_local_http_reports_connection_refused_against_a_closed_port() {
        // Bind, read the ephemeral port, then drop the listener so nothing
        // is answering it — still entirely within loopback.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let url = format!("http://{addr}/v1/models");
        let result = probe_local_http("local", &url, Duration::from_secs(2));
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .starts_with(&format!("local seat health check failed at {url} (")));
    }

    #[test]
    fn probe_local_http_reports_no_health_url_configured_for_a_blank_url() {
        let result = probe_local_http("ollama-win", "   ", Duration::from_secs(2));
        assert_eq!(
            result,
            Err("no health URL configured for local account ollama-win".to_string())
        );
    }

    #[test]
    fn http_local_liveness_probe_wires_the_account_name_and_url_through() {
        let result = HttpLocalLivenessProbe.probe("ollama-win", "");
        assert_eq!(
            result,
            Err("no health URL configured for local account ollama-win".to_string())
        );
    }

    // --- reorder_chain_by_preference ---

    #[test]
    fn prefer_puts_the_preferred_seat_ahead_in_select() {
        let a = seat("a", "pool-a");
        let b = seat("b", "pool-b");
        let chain = [a, b];
        let reordered = reorder_chain_by_preference(&chain, |name| name == "b");
        let names: Vec<&str> = reordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "a"]);

        let breaker = FakeBreaker::new();
        let exclude = vec![];
        let selection = select(
            &reordered,
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "b");
    }

    #[test]
    fn lend_off_beats_prefer() {
        let a = seat("a", "pool-a");
        let mut b = seat("b", "pool-b");
        b.lent = false;
        let chain = [a, b];
        // "b" is preferred and so reordered ahead of "a" — but its lend
        // being off still makes it unavailable, so `select` falls
        // through to "a" regardless of chain position.
        let reordered = reorder_chain_by_preference(&chain, |name| name == "b");
        assert_eq!(reordered[0].name, "b");

        let breaker = FakeBreaker::new();
        let exclude = vec![];
        let selection = select(
            &reordered,
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "a");
    }

    #[test]
    fn prefer_reversal_flips_the_selection_back() {
        let a = seat("a", "pool-a");
        let b = seat("b", "pool-b");
        let chain = [a, b];

        let b_preferred = reorder_chain_by_preference(&chain, |name| name == "b");
        assert_eq!(b_preferred[0].name, "b");

        let a_preferred = reorder_chain_by_preference(&chain, |name| name == "a");
        assert_eq!(a_preferred[0].name, "a");
    }

    #[test]
    fn reorder_is_a_stable_partition_not_a_sort() {
        let chain = [
            seat("a", "pool-a"),
            seat("b", "pool-b"),
            seat("c", "pool-c"),
            seat("d", "pool-d"),
        ];
        let reordered = reorder_chain_by_preference(&chain, |name| name == "b" || name == "d");
        let names: Vec<&str> = reordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "d", "a", "c"]);
    }

    #[test]
    fn reorder_with_no_preference_leaves_the_chain_unchanged() {
        let chain = [seat("a", "pool-a"), seat("b", "pool-b")];
        let reordered = reorder_chain_by_preference(&chain, |_| false);
        let names: Vec<&str> = reordered.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    // --- FallbackPolicy stop conditions ---

    #[test]
    fn degrade_walks_past_both_and_reaches_the_third_seat() {
        let mut a = seat("a", "pool-a");
        a.available = false;
        let mut b = seat("b", "pool-b");
        b.cap = Some(1);
        b.open_count = Some(1);
        let c = seat("c", "pool-c");
        let chain = [a, b, c];
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let selection = select(
            &chain,
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "c");
        assert_eq!(selection.skipped.len(), 2);
    }

    #[test]
    fn degrade_once_takes_one_step_then_refuses_the_next() {
        let mut a = seat("a", "pool-a");
        a.available = false;
        let b = seat("b", "pool-b");
        let c = seat("c", "pool-c");
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let chain_one_down = [a.clone(), b.clone(), c.clone()];
        let selection = select(
            &chain_one_down,
            &params(&exclude, FallbackPolicy::DegradeOnce),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "b");

        let mut b_unavailable = b.clone();
        b_unavailable.available = false;
        let chain_two_down = [a, b_unavailable, c];
        let err = select(
            &chain_two_down,
            &params(&exclude, FallbackPolicy::DegradeOnce),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.skipped.len(), 2);
        assert_eq!(err.policy, Some(FallbackPolicy::DegradeOnce));
    }

    #[test]
    fn vendor_exclusion_and_no_model_never_spend_the_degrade_once_step() {
        let mut excluded = seat("excluded", "pool-a");
        excluded.vendor = "codex".to_string();
        let mut no_model = seat("no-model", "pool-b");
        no_model.has_model = false;
        let mut unavailable_seat = seat("unavailable", "pool-c");
        unavailable_seat.available = false;
        let target = seat("target", "pool-d");
        let chain = [excluded, no_model, unavailable_seat, target];
        let exclude = vec!["codex".to_string()];
        let breaker = FakeBreaker::new();
        let selection = select(
            &chain,
            &params(&exclude, FallbackPolicy::DegradeOnce),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        // Only one *countable* skip (the UNAVAILABLE seat) preceded the
        // selection, so DegradeOnce's single step was not yet spent.
        assert_eq!(selection.seat.name, "target");
        assert_eq!(selection.skipped.len(), 3);
    }

    #[test]
    fn no_command_stops_warn_but_not_degrade_once() {
        let mut no_command = seat("no-command", "pool-a");
        no_command.has_command = false;
        let fallback = seat("fallback", "pool-b");
        let chain = [no_command, fallback];
        let exclude = vec![];
        let breaker = FakeBreaker::new();

        let err = select(
            &chain,
            &params(&exclude, FallbackPolicy::Warn),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.skipped.len(), 1);
        assert_eq!(err.skipped[0].cause, Cause::NoCommand);

        let selection = select(
            &chain,
            &params(&exclude, FallbackPolicy::DegradeOnce),
            &breaker,
            Instant::now(),
        )
        .unwrap();
        assert_eq!(selection.seat.name, "fallback");
    }

    #[test]
    fn warn_refuses_capacity_without_waiting_or_swapping() {
        let mut capped = seat("capped", "pool-a");
        capped.cap = Some(1);
        capped.open_count = Some(1);
        let fallback = seat("fallback", "pool-b");
        let chain = [capped, fallback];
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let err = select(
            &chain,
            &params(&exclude, FallbackPolicy::Warn),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.skipped.len(), 1);
        assert_eq!(err.policy, Some(FallbackPolicy::Warn));
    }

    #[test]
    fn queue_waits_for_primary_capacity_instead_of_using_a_free_fallback() {
        let mut capped = seat("capped", "pool-a");
        capped.cap = Some(1);
        capped.open_count = Some(1);
        let fallback = seat("fallback", "pool-b");
        let chain = [capped, fallback];
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let err = select(
            &chain,
            &params(&exclude, FallbackPolicy::Queue),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.skipped.len(), 1);
        assert_eq!(err.skipped[0].cause, Cause::AtCapacity);
    }

    #[test]
    fn chain_exhausted_purely_at_capacity_is_a_single_line_retry_refusal() {
        let mut a = seat("a", "pool-a");
        a.cap = Some(1);
        a.open_count = Some(1);
        let mut b = seat("b", "pool-b");
        b.cap = Some(1);
        b.open_count = Some(1);
        let chain = [a, b];
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let err = select(
            &chain,
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert!(err.message.contains(
            "is at capacity (a: at cap: 1/1 open agents; b: at cap: 1/1 open agents); \
             retry on a later tick."
        ));
    }

    // --- quota floors ---

    #[test]
    fn reviewer_low_quota_is_strict_fleet_draining_is_at_or_below() {
        let mut low = seat("low", "pool-a");
        low.quota_primary_percent = Some(10.0);
        let floors = QuotaFloors {
            reviewer_min_quota_percent: Some(10.0),
            fleet_min_quota_percent: None,
        };
        // Strictly-below only: exactly at the reviewer floor still passes.
        let breaker = FakeBreaker::new();
        assert!(evaluate_candidate(
            &low,
            REVIEWER,
            REVIEWER,
            NEXUS,
            &[],
            10,
            floors,
            &breaker,
            "t1",
            None,
            Instant::now()
        )
        .is_ok());

        let mut draining = seat("draining", "pool-b");
        draining.quota_primary_percent = Some(5.0);
        let fleet_floor = QuotaFloors {
            reviewer_min_quota_percent: None,
            fleet_min_quota_percent: Some(5.0),
        };
        let err = evaluate_candidate(
            &draining,
            ROLE,
            REVIEWER,
            NEXUS,
            &[],
            10,
            fleet_floor,
            &breaker,
            "t1",
            None,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.cause, Cause::Draining);
    }

    // --- returns_soon ---

    #[test]
    fn returns_soon_waits_on_unavailable_until_with_a_closed_breaker() {
        let mut s = seat("a", "pool");
        let now = Instant::now();
        s.unavailable_until = Some(now + Duration::from_secs(60));
        // No entry in FakeBreaker's map resolves to `BreakerStatus::CLOSED`
        // (`open: false`), the common case for a seat that was merely lent
        // out for a while, not fenced by the breaker at all.
        let breaker = FakeBreaker::new();
        assert!(returns_soon(&s, &breaker, now));
    }

    #[test]
    fn returns_soon_false_past_the_horizon_even_with_a_closed_breaker() {
        let mut s = seat("a", "pool");
        let now = Instant::now();
        s.unavailable_until = Some(now + RETURNS_SOON_HORIZON + Duration::from_secs(1));
        let breaker = FakeBreaker::new();
        assert!(!returns_soon(&s, &breaker, now));
    }

    #[test]
    fn returns_soon_false_without_a_timed_return_even_with_a_closed_breaker() {
        let s = seat("a", "pool");
        let now = Instant::now();
        let breaker = FakeBreaker::new();
        assert!(!returns_soon(&s, &breaker, now));
    }

    #[test]
    fn returns_soon_still_consults_the_breaker_when_open() {
        let mut s = seat("a", "pool");
        let now = Instant::now();
        s.unavailable_until = Some(now + Duration::from_secs(60));
        let breaker = FakeBreaker::new().with(
            "pool",
            BreakerStatus {
                open: true,
                half_open: false,
                reset_at: ResetAt::NoAutomaticReset,
                reset_display: None,
                trial_claimed_by: None,
            },
        );
        // An open breaker with no automatic reset overrides a nearby timed
        // `unavailable_until` — matching `_returns_soon`'s `if status.open:`
        // branch, which is only reached (and only then applies) when the
        // breaker is actually open.
        assert!(!returns_soon(&s, &breaker, now));
    }

    #[test]
    fn queue_stops_at_a_timed_unavailable_seat_with_closed_breaker_instead_of_falling_back() {
        let mut timed_out = seat("a", "pool-a");
        timed_out.unavailable_until = Some(Instant::now() + Duration::from_secs(60));
        let fallback = seat("fallback", "pool-b");
        let chain = [timed_out, fallback];
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let err = select(
            &chain,
            &params(&exclude, FallbackPolicy::Queue),
            &breaker,
            Instant::now(),
        )
        .unwrap_err();
        assert_eq!(err.policy, Some(FallbackPolicy::Queue));
        assert_eq!(err.skipped.len(), 1);
        assert_eq!(err.skipped[0].seat, "a");
        assert_eq!(err.skipped[0].cause, Cause::Unavailable);
        assert!(err.skipped[0].returns_soon);
    }

    // --- select_or_wait ---

    struct FakeSleeper {
        calls: Vec<Duration>,
    }

    impl Sleeper for FakeSleeper {
        fn sleep(&mut self, duration: Duration) {
            self.calls.push(duration);
        }
    }

    #[test]
    fn select_or_wait_rejects_non_finite_or_negative_timeout() {
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut on_wait = |_: &str| {};
        for bad in [f64::NAN, f64::INFINITY, -1.0] {
            let opts = WaitOptions {
                timeout: Some(bad),
                poll_interval: 1.0,
                sleeper: &mut sleeper,
                on_wait: &mut on_wait,
            };
            let err = select_or_wait(
                &params(&exclude, FallbackPolicy::Queue),
                &breaker,
                || vec![seat("a", "pool")],
                opts,
                Instant::now,
            )
            .unwrap_err();
            assert!(matches!(err, WaitError::InvalidTimeout(_)));
        }
    }

    #[test]
    fn select_or_wait_gives_up_after_its_timeout() {
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut on_wait = |_: &str| {};
        let start = Instant::now();
        let elapsed = std::cell::Cell::new(Duration::from_secs(0));
        let opts = WaitOptions {
            timeout: Some(5.0),
            poll_interval: 2.0,
            sleeper: &mut sleeper,
            on_wait: &mut on_wait,
        };
        let chain = || {
            let mut capped = seat("capped", "pool");
            capped.cap = Some(1);
            capped.open_count = Some(1); // never frees
            vec![capped]
        };
        // Drive the fake clock forward exactly as far as each queued sleep
        // asked for, rather than it actually elapsing.
        let result = select_or_wait(
            &params(&exclude, FallbackPolicy::Queue),
            &breaker,
            chain,
            opts,
            || {
                let t = start + elapsed.get();
                elapsed.set(elapsed.get() + Duration::from_secs_f64(2.0));
                t
            },
        );
        assert!(matches!(result, Err(WaitError::ChainExhausted(_))));
        assert!(!sleeper.calls.is_empty());
    }

    #[test]
    fn select_or_wait_raises_immediately_for_a_non_capacity_exhaustion() {
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut on_wait = |_: &str| {};
        let opts = WaitOptions {
            timeout: Some(100.0),
            poll_interval: 1.0,
            sleeper: &mut sleeper,
            on_wait: &mut on_wait,
        };
        let chain = || {
            let mut unavailable_seat = seat("a", "pool");
            unavailable_seat.available = false;
            vec![unavailable_seat]
        };
        let err = select_or_wait(
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            chain,
            opts,
            Instant::now,
        )
        .unwrap_err();
        assert!(matches!(err, WaitError::ChainExhausted(_)));
        assert!(sleeper.calls.is_empty());
    }

    #[test]
    fn select_or_wait_raises_immediately_when_only_some_skips_are_at_capacity() {
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut on_wait = |_: &str| {};
        let opts = WaitOptions {
            timeout: Some(100.0),
            poll_interval: 1.0,
            sleeper: &mut sleeper,
            on_wait: &mut on_wait,
        };
        let chain = || {
            let mut unavailable_seat = seat("a", "pool-a");
            unavailable_seat.available = false;
            let mut capped = seat("b", "pool-b");
            capped.cap = Some(1);
            capped.open_count = Some(1);
            vec![unavailable_seat, capped]
        };
        // DEGRADE walks both and the final skip set is {UNAVAILABLE,
        // AT_CAPACITY} — not every entry is AT_CAPACITY, so there is
        // nothing to wait on.
        let err = select_or_wait(
            &params(&exclude, FallbackPolicy::Degrade),
            &breaker,
            chain,
            opts,
            Instant::now,
        )
        .unwrap_err();
        assert!(matches!(err, WaitError::ChainExhausted(_)));
        assert!(sleeper.calls.is_empty());
    }

    #[test]
    fn select_or_wait_polls_until_capacity_frees_then_succeeds() {
        let open_count = std::cell::Cell::new(Some(1_u32));
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut polls = 0;
        let mut on_wait = |_: &str| {
            polls += 1;
            open_count.set(Some(0)); // the seat frees up after the first poll
        };
        let opts = WaitOptions {
            timeout: Some(100.0),
            poll_interval: 1.0,
            sleeper: &mut sleeper,
            on_wait: &mut on_wait,
        };
        let chain = || {
            let mut capped = seat("capped", "pool");
            capped.cap = Some(1);
            capped.open_count = open_count.get();
            vec![capped]
        };
        let result = select_or_wait(
            &params(&exclude, FallbackPolicy::Queue),
            &breaker,
            chain,
            opts,
            Instant::now,
        );
        let selection = result.expect("capacity freed on the second poll");
        assert_eq!(selection.seat.name, "capped");
        assert_eq!(polls, 1);
        assert_eq!(sleeper.calls.len(), 1);
    }

    #[test]
    fn select_or_wait_calls_on_wait_with_a_clear_message() {
        let exclude = vec![];
        let breaker = FakeBreaker::new();
        let mut sleeper = FakeSleeper { calls: vec![] };
        let mut messages = Vec::new();
        let open_count = std::cell::Cell::new(Some(1_u32));
        let mut on_wait = |msg: &str| {
            messages.push(msg.to_string());
            open_count.set(Some(0));
        };
        let opts = WaitOptions {
            timeout: Some(10.0),
            poll_interval: 1.0,
            sleeper: &mut sleeper,
            on_wait: &mut on_wait,
        };
        let chain = || {
            let mut capped = seat("capped", "pool");
            capped.cap = Some(1);
            capped.open_count = open_count.get();
            vec![capped]
        };
        select_or_wait(
            &params(&exclude, FallbackPolicy::Queue),
            &breaker,
            chain,
            opts,
            Instant::now,
        )
        .expect("capacity frees on the second poll");
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("capped"));
    }
}

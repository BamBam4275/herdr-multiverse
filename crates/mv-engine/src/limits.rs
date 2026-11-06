//! Pool model: codex and its sibling pool share the chatgpt pool; claude
//! and its plugin variant share the claude pool.
//!
//! `pool` is an operator-configured string on each account — that field
//! itself lives in `mv-config`. Most of this module still takes plain
//! `&str`/`&Path` values rather than a concrete `Account`, to stay usable
//! from a caller that only has those fragments; [`sweep_tick_codex_probe`]
//! is the one function below that does take `&Account` directly, the same
//! way `crate::sweep_nudges` already does, because its own gating
//! (`launches_by_command`, `vendor`/`kind`) has no meaning apart from one.
//! Two accounts sharing a `pool` value share one breaker/gauge entry;
//! `mv_state::breaker_store` and `mv_state::gauge` already key by pool, so
//! "the pool model" at this layer is mostly: which pool does a piece of
//! *evidence* belong to.
//!
//! The one place that needs active disambiguation is Codex: codex and its
//! sibling pool read the same `~/.codex` sessions directory, so a session
//! log's pool is decided by its own first line's `session_meta.payload.
//! model_provider` (`spec-seats-quotas.md`, "Pool model").

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mv_config::load::Account;
use regex::Regex;
use serde_json::Value;
use time::OffsetDateTime;

use crate::blocking_prompt::{
    codex_dialog_re, codex_exhausted_re, grok_dialog_re, grok_exhausted_re, live_banner_region,
};
use crate::breaker::BreakerKind;

// --- Vendor exhaustion verdicts (`classify`) --------------------------------
//
// Port of Python's `limits.classify`/`Verdict`/`LimitKind` (frozen reference,
// `multiverse/limits.py`; task `20261103-fixer-08598b`, card
// `20261102-b-9350c1`).
//
// Reuses [`crate::breaker::BreakerKind`] for [`Verdict::kind`] rather than a
// second three-way enum: `BreakerKind::{Rolling, Weekly, Budget}` is already
// exactly Python's `LimitKind`, and `breaker::reset_for_kind`/
// `breaker::is_escalating_rule` already own the cooldown math a parallel type
// would otherwise force a caller to re-derive. No `Budget` classifier exists
// here, same as Python: nothing below ever constructs one automatically — it
// remains a kind a human sets by hand (`breaker::ManualTripError`, `accounts
// trip --kind budget` in the frozen Python CLI).
//
// **Not wired to [`crate::breaker::trip`] by this port.** `breaker::trip`'s
// only public, non-manual entry point always derives its own reset time (the
// escalation ladder for `codex_retry_limit_exceeded`, or `reset_for_kind`
// otherwise) and has no parameter for a caller-supplied override, so
// `Verdict::reset_at` — claude's parsed "Retry in Ns.", codex's own
// five-minute 429 floor — has nowhere to plug in yet. A caller that wants
// `breaker::trip` to honour an explicit `reset_at` needs that gap closed on
// the `breaker` side first, which is outside this task's `limits`/
// `blocking_prompt` write scope — see this task's `result.md`.

/// What a pane's text proved, and what a caller should do about it. Mirrors
/// Python's `Verdict`. `kind` says which clock (if any) governs the reset —
/// see [`BreakerKind`] — and is required precisely so a caller can never
/// construct a verdict without picking one. `reset_at` is `None` when the
/// vendor's own text gave no reset time, which is the normal case for every
/// vendor here — the caller derives one from `kind` instead
/// (`breaker::reset_for_kind` / `breaker::is_escalating_rule`).
/// `interactive` is `true` when the pane is parked on an unanswered dialog
/// (codex's Yes/No "Request increase?", grok's numbered options) rather than
/// a plain banner — the caller must trip the breaker either way, but must
/// leave an interactive pane open and its dialog unanswered.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub vendor: String,
    pub rule: String,
    pub kind: BreakerKind,
    pub reset_at: Option<OffsetDateTime>,
    pub interactive: bool,
}

/// Python's `_CODEX_ROLLING_RE`: anchored on the CLI's own retry-exhaustion
/// wording, not bare "429" — the out-of-credits banner never states a status
/// code, and bare "429" risks matching prose that merely discusses rate
/// limits.
fn codex_rolling_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"exceeded retry limit, last status: 429 Too Many Requests")
            .expect("static regex compiles")
    })
}

/// Mike's own floor, verbatim: "if we get 429 on codex we should wait a min
/// of 5 min before retrying." Python's `_CODEX_RETRY_COOLDOWN`. A floor for a
/// single, isolated 429 only — not the escalating backoff
/// `breaker::is_escalating_rule`/`breaker::escalated_cooldown_minutes`
/// already implement for repeated trips of this same rule.
pub const CODEX_RETRY_COOLDOWN: time::Duration = time::Duration::minutes(5);

/// Python's `_classify_codex`: codex's two captured exhaustion shapes, or
/// `None`. Both search only [`live_banner_region`], reusing
/// [`crate::blocking_prompt`]'s own anchors so the two modules can never
/// disagree about where a live dialog sits or what counts as "exhausted".
fn classify_codex(text: &str, now: Option<OffsetDateTime>) -> Option<Verdict> {
    let region = live_banner_region(text);
    if codex_exhausted_re().is_match(&region) {
        return Some(Verdict {
            vendor: "codex".to_owned(),
            rule: "codex_usage_limit_reached".to_owned(),
            kind: BreakerKind::Rolling,
            reset_at: None,
            interactive: codex_dialog_re().is_match(&region),
        });
    }
    if codex_rolling_re().is_match(&region) {
        return Some(Verdict {
            vendor: "codex".to_owned(),
            rule: "codex_retry_limit_exceeded".to_owned(),
            kind: BreakerKind::Rolling,
            reset_at: now.map(|now| now + CODEX_RETRY_COOLDOWN),
            interactive: false,
        });
    }
    None
}

/// Python's `_classify_grok`: anchored on the past-tense "You hit your
/// weekly limit." — not the "Weekly limit" phrase the healthy gauge also
/// prints, so a still-serving seat is never tripped.
fn classify_grok(text: &str) -> Option<Verdict> {
    let region = live_banner_region(text);
    if !grok_exhausted_re().is_match(&region) {
        return None;
    }
    Some(Verdict {
        vendor: "grok".to_owned(),
        rule: "grok_weekly_limit_reached".to_owned(),
        kind: BreakerKind::Weekly,
        reset_at: None,
        interactive: grok_dialog_re().is_match(&region),
    })
}

/// Python's `_CLAUDE_ROLLING_RE`: the vendor's own error type, not the word
/// "exhausted" — that word also describes codex's own out-of-credits banner,
/// which clears on a clock rather than naming real credit exhaustion.
fn claude_rolling_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#""type"\s*:\s*"rate_limit_error""#).expect("static regex compiles")
    })
}

/// Python's `_CLAUDE_RETRY_RE`: the vendor's own stated cooldown, "Retry in
/// 60s." — captured nowhere else in this directory (codex and grok state no
/// interval at all).
fn claude_retry_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"Retry in (\d+)s\.").expect("static regex compiles"))
}

/// Python's `_classify_claude`: claude's `rate_limit_error` rolling 429, or
/// `None`. Searches only [`live_banner_region`], like codex and grok. Parses
/// its LAST "Retry in Ns." into an explicit `reset_at` when `now` is given —
/// the last match, not the first, because a pasted fixture or earlier
/// scrollback can carry its own banner ahead of the one the pane shows now
/// (same reasoning as the grok gauge reader). No dialog is attached to this
/// banner — it is not something anyone has to answer.
fn classify_claude(text: &str, now: Option<OffsetDateTime>) -> Option<Verdict> {
    let region = live_banner_region(text);
    if !claude_rolling_re().is_match(&region) {
        return None;
    }
    let reset_at = now.and_then(|now| {
        claude_retry_re()
            .captures_iter(&region)
            .last()
            .and_then(|caps| caps.get(1))
            .and_then(|m| m.as_str().parse::<i64>().ok())
            .map(|secs| now + time::Duration::seconds(secs))
    });
    Some(Verdict {
        vendor: "claude".to_owned(),
        rule: "claude_rate_limit_error".to_owned(),
        kind: BreakerKind::Rolling,
        reset_at,
        interactive: false,
    })
}

/// Classify one pane's text for vendor exhaustion, or return no verdict.
/// Python's `classify`. `kind` is the Herdr agent kind (`"claude"`,
/// `"codex"`, `"grok"`). `now` is never read as a clock by this function —
/// it is whatever reading the caller already had, passed through only to
/// turn a vendor's own stated cooldown into an explicit `reset_at` (claude's
/// "Retry in Ns.", codex's own five-minute floor); every other rule ignores
/// it, including codex's out-of-credits rule, which states no cooldown or
/// floor worth preferring over `kind`'s own derived default.
///
/// Fail-open, same contract as [`crate::blocking_prompt::classify_prompt`]:
/// empty/whitespace-only text, a `kind` with no classifier registered, and
/// text that classifier does not recognise all return `None`. There is no
/// other return path.
pub fn classify(text: &str, kind: &str, now: Option<OffsetDateTime>) -> Option<Verdict> {
    if text.trim().is_empty() {
        return None;
    }
    match kind {
        "claude" => classify_claude(text, now),
        "codex" => classify_codex(text, now),
        "grok" => classify_grok(text),
        _ => None,
    }
}

/// Which of the two Codex-family pools a session log belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodexPool {
    /// `model_provider == "openai"` — the bare `codex` pool.
    Base,
    /// Anything else — the `:`-suffixed sibling pool.
    Secondary,
}

/// Classify a Codex session log by its own `model_provider` field, read
/// from the log's first (`session_meta`) line.
pub fn codex_log_pool(model_provider: &str) -> CodexPool {
    if model_provider == "openai" {
        CodexPool::Base
    } else {
        CodexPool::Secondary
    }
}

/// A percent-of-window-left reading, already range-checked into `[0, 100]`.
pub type PercentLeft = f64;

/// Convert a vendor's `used_percent` (0-100) into percent-left, rejecting
/// anything outside that range as *unknown* rather than clamping it — a
/// parsing bug should surface as "no gauge," never a confidently wrong
/// number (mirrors `test_out_of_range_pane_percentage_is_unknown`).
pub fn percent_left_from_used(used_percent: f64) -> Option<PercentLeft> {
    if !(0.0..=100.0).contains(&used_percent) {
        return None;
    }
    Some(100.0 - used_percent)
}

/// Read `<window>.used_percent` out of a Codex `rate_limits` JSON payload
/// and convert it to percent-left. A JSON boolean is rejected by
/// `Value::as_f64`, which only matches `Value::Number` — the Rust-side
/// guard for Python's `isinstance(x, bool)` footgun
/// (`test_boolean_primary_percentage_is_not_a_gauge`), automatic here
/// because `serde_json::Value` keeps `Bool` and `Number` as distinct
/// variants rather than Python's `bool`-is-`int` hierarchy.
pub fn codex_rate_limit_percent_left(rate_limits: &Value, window: &str) -> Option<PercentLeft> {
    let used = rate_limits.get(window)?.get("used_percent")?.as_f64()?;
    percent_left_from_used(used)
}

fn grok_gauge_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)weekly limit left:\s*(\d+(?:\.\d+)?)\s*%").expect("static regex compiles")
    })
}

/// Grok's live pane-banner reading: "Weekly limit left: N%" — the only
/// non-estimate Grok signal (`crate::grok_usage` carries the separate,
/// explicitly-an-estimate byte count). Out-of-range matches are `None`,
/// same posture as [`percent_left_from_used`].
///
/// **Confidence note:** the exact banner wording was not independently
/// verified against a captured real pane in this task (no
/// `tests/fixtures/rate-limit/` equivalent exists in this checkout — see
/// this task's `result.md`); this regex is this module's best-effort
/// reading of the behavioral description, not a ported literal pattern.
pub fn parse_grok_gauge_banner(text: &str) -> Option<PercentLeft> {
    let caught = grok_gauge_re().captures(text)?;
    let percent: f64 = caught.get(1)?.as_str().parse().ok()?;
    if (0.0..=100.0).contains(&percent) {
        Some(percent)
    } else {
        None
    }
}

/// A seat is `DRAINING` when *either* window — primary or
/// weekly/secondary — is below the floor, checked against both as of the
/// `ee6db27` regression fix (an earlier version only checked primary).
pub fn meets_quota_floor(
    primary_percent_left: PercentLeft,
    secondary_percent_left: Option<PercentLeft>,
    floor_percent: f64,
) -> bool {
    if primary_percent_left < floor_percent {
        return false;
    }
    if let Some(secondary) = secondary_percent_left {
        if secondary < floor_percent {
            return false;
        }
    }
    true
}

/// Stamp a gauge entry with a recovery-probe timestamp — "never triggered
/// by invoking a vendor CLI" (the Python original's own docstring), just a
/// marker of when a passive re-check last happened.
pub fn record_probe(entry: &mut mv_state::gauge::GaugeEntry, now_stamp: impl Into<String>) {
    entry.last_probe_at = Some(now_stamp.into());
}

pub fn last_probe_at(entry: &mv_state::gauge::GaugeEntry) -> Option<&str> {
    entry.last_probe_at.as_deref()
}

/// Quota windows from the newest usable Codex token-count record.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexUsage {
    pub primary_percent_left: f64,
    pub secondary_percent_left: Option<f64>,
    pub resets_at: Option<String>,
    pub resets_at_secondary: Option<String>,
}

/// Supports both early token_count records and current event_msg payloads.
pub fn parse_codex_usage(record: &Value) -> Option<CodexUsage> {
    let payload = record.get("payload")?;
    if record.get("type").and_then(Value::as_str) != Some("token_count")
        && payload.get("type").and_then(Value::as_str) != Some("token_count")
    {
        return None;
    }
    let limits = payload.get("rate_limits")?;
    let reset = |window: &str| {
        let seconds = limits.get(window)?.get("resets_at")?.as_i64()?;
        let instant = time::OffsetDateTime::from_unix_timestamp(seconds).ok()?;
        Some(mv_core::time::format_seconds_utc(instant))
    };
    Some(CodexUsage {
        primary_percent_left: codex_rate_limit_percent_left(limits, "primary")?,
        secondary_percent_left: codex_rate_limit_percent_left(limits, "secondary"),
        resets_at: reset("primary"),
        resets_at_secondary: reset("secondary"),
    })
}

/// Scan only this seat's sessions tree, ignoring symlinks and malformed lines.
/// Record observation time from the rollout's mtime so a repeated read cannot
/// make an old gauge appear newly observed. A store failure does not hide a
/// valid usage reading from the operator.
pub fn read_and_record_codex_usage(
    codex_home: &std::path::Path,
    state_dir: &std::path::Path,
    pool: &str,
) -> Option<CodexUsage> {
    let (usage, observed_at) = latest_codex_usage(codex_home)?;
    let _ = mv_state::gauge::with_locked(state_dir, |map| {
        let entry = map.entry(pool.to_owned()).or_default();
        // Never replace newer persisted evidence with an older transcript.
        if entry
            .observed_at
            .as_deref()
            .and_then(crate::breaker::parse_stored_timestamp)
            > observed_at
                .as_deref()
                .and_then(crate::breaker::parse_stored_timestamp)
        {
            return;
        }
        entry.vendor = Some("codex".into());
        entry.percent_left = Some(usage.primary_percent_left);
        entry.percent_left_secondary = usage.secondary_percent_left;
        entry.label = Some("primary window".into());
        entry.label_secondary = usage
            .secondary_percent_left
            .map(|_| "secondary window".into());
        entry.resets_at = usage.resets_at.clone();
        entry.resets_at_secondary = usage.resets_at_secondary.clone();
        entry.observed_at = observed_at;
    });
    Some(usage)
}

/// Read Codex's passive session-log gauge without writing fleet state.
pub fn read_codex_usage(codex_home: &std::path::Path) -> Option<CodexUsage> {
    latest_codex_usage(codex_home).map(|(usage, _)| usage)
}

fn latest_codex_usage(codex_home: &std::path::Path) -> Option<(CodexUsage, Option<String>)> {
    use std::io::BufRead;
    fn collect(dir: &std::path::Path, depth: usize, paths: &mut Vec<std::path::PathBuf>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                collect(&path, depth - 1, paths);
            } else if kind.is_file() && path.extension().is_some_and(|ext| ext == "jsonl") {
                paths.push(path);
            }
        }
    }
    let mut paths = Vec::new();
    collect(&codex_home.join("sessions"), 5, &mut paths);
    paths.sort_by_key(|path| {
        (
            path.metadata().and_then(|m| m.modified()).ok(),
            path.clone(),
        )
    });
    let mut latest = None;
    for path in paths.into_iter().rev() {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let mut reading = None;
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if let Some(usage) = parse_codex_usage(&record) {
                reading = Some(usage);
            }
        }
        if let Some(reading) = reading {
            let observed = path
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .map(time::OffsetDateTime::from)
                .map(mv_core::time::format_seconds_utc);
            latest = Some((reading, observed));
            break;
        }
    }
    latest
}

/// `cli.py`'s `CODEX_RECOVERY_PROBE_INTERVAL`: a stale rate-limit window is
/// not evidence the next one is healthy, but spending one probe per sweep
/// while a seat stays drained would waste quota on every pass — this is
/// the dedupe window between probes for the same pool.
pub const CODEX_RECOVERY_PROBE_INTERVAL_MINUTES: i64 = 30;

/// `cli.py`'s `CODEX_RECOVERY_PROBE_TIMEOUT_SECONDS`: the probe subprocess
/// itself is killed rather than left to hang a sweep pass.
const CODEX_RECOVERY_PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// `cli.py`'s `CODEX_RECOVERY_PROBE_ARGV`, minus the `codex` binary itself
/// (that is the injectable `binary` argument below) — a deliberately tiny,
/// non-project completion that lets the session log refresh without
/// dispatching a task into a lane.
const CODEX_RECOVERY_PROBE_ARGS: [&str; 5] = [
    "exec",
    "--skip-git-repo-check",
    "-c",
    "model_reasoning_effort=low",
    "Reply with exactly: PONG",
];

/// Run `binary` with [`CODEX_RECOVERY_PROBE_ARGS`] in a throwaway temp
/// directory, `config_dir_var` set to `config_dir` in its environment, no
/// stdin, and no shell. Best-effort like Python's own
/// `subprocess.run(..., check=False)`: a spawn failure, a nonzero exit, or
/// a hung process past [`CODEX_RECOVERY_PROBE_TIMEOUT`] (killed, then
/// waited on so it cannot linger as a zombie) are all silently swallowed —
/// the caller's own `read_and_record_codex_usage` call afterward is what
/// actually tells it whether the probe moved anything.
fn run_codex_recovery_probe(binary: &Path, config_dir_var: &str, config_dir: &str) {
    let cwd = std::env::temp_dir().join(format!(
        "multiverse-codex-probe-{}-{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    ));
    if std::fs::create_dir_all(&cwd).is_err() {
        return;
    }
    let spawned = Command::new(binary)
        .args(CODEX_RECOVERY_PROBE_ARGS)
        .current_dir(&cwd)
        .env(config_dir_var, config_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    if let Ok(mut child) = spawned {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {
                    if start.elapsed() >= CODEX_RECOVERY_PROBE_TIMEOUT {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
    }
    let _ = std::fs::remove_dir_all(&cwd);
}

/// Run one bounded Codex probe for a draining or stale low gauge, mirroring
/// `cli.py`'s `_sweep_tick_codex_probe`. `binary` is an injectable seam for
/// tests; [`sweep_tick_codex_probe`] passes `codex` for a real run.
///
/// Returns whether a probe actually ran — `false` covers every skip case:
/// not a bare Codex seat (`vendor != "chatgpt" || kind != "codex"`), a seat
/// that launches through a pane shell function rather than execing its own
/// command directly (`!account.command.trim().is_empty()` — this probe
/// execs a bare argv with no shell and no pane, so it has no way to invoke
/// that seat's own shell function, and running the bare `codex` binary
/// under the probed seat's own `config_dir` instead would silently probe a
/// *different* profile while still stamping this seat's own dedupe clock),
/// quota recovery disabled (`dispatch_min_quota_percent == 0`), an
/// unresolved placeholder `config_dir`, no persisted gauge reading for this
/// pool yet, a reading already above the floor, or a probe already
/// recorded within [`CODEX_RECOVERY_PROBE_INTERVAL_MINUTES`].
pub fn sweep_tick_codex_probe_with(
    binary: &Path,
    account: &Account,
    dispatch_min_quota_percent: u32,
    state_dir: &Path,
    now: OffsetDateTime,
) -> bool {
    if account.vendor != "chatgpt" || account.kind != "codex" {
        return false;
    }
    if !account.command.trim().is_empty() {
        return false;
    }
    if dispatch_min_quota_percent == 0 {
        return false;
    }
    let Some(config_dir) = account.config_dir.value() else {
        return false;
    };
    let gauges = mv_state::gauge::load(state_dir);
    let entry = gauges.get(&account.pool);
    let Some(percent_left) = entry.and_then(|entry| entry.percent_left) else {
        return false;
    };
    if percent_left > f64::from(dispatch_min_quota_percent) {
        return false;
    }
    if let Some(previous) = entry
        .and_then(|entry| entry.last_probe_at.as_deref())
        .and_then(crate::breaker::parse_stored_timestamp)
    {
        if now - previous < time::Duration::minutes(CODEX_RECOVERY_PROBE_INTERVAL_MINUTES) {
            return false;
        }
    }
    let now_stamp = mv_core::time::format_seconds_utc(now);
    let _ = mv_state::gauge::with_locked(state_dir, |map| {
        record_probe(map.entry(account.pool.clone()).or_default(), now_stamp);
    });
    run_codex_recovery_probe(binary, &account.config_dir_var, config_dir);
    read_and_record_codex_usage(Path::new(config_dir), state_dir, &account.pool);
    true
}

/// Resolve the token used to invoke the recovery probe's own `codex`
/// binary: `$MULTIVERSE_CODEX_BIN` when set and non-blank, else the bare
/// `codex` token. Mirrors [`crate::hostsleep::pmset_binary`]'s own override
/// shape — the same one-off-host-binary test seam, for the same reason: a
/// conformance scenario (or this crate's own `commands::sweep` tests) can
/// substitute a scripted fake for the real CLI without going through PATH.
pub fn codex_binary() -> String {
    resolve_codex_binary(std::env::var("MULTIVERSE_CODEX_BIN").ok())
}

fn resolve_codex_binary(raw: Option<String>) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value,
        _ => "codex".to_string(),
    }
}

/// [`sweep_tick_codex_probe_with`] against [`codex_binary`].
pub fn sweep_tick_codex_probe(
    account: &Account,
    dispatch_min_quota_percent: u32,
    state_dir: &Path,
    now: OffsetDateTime,
) -> bool {
    sweep_tick_codex_probe_with(
        Path::new(&codex_binary()),
        account,
        dispatch_min_quota_percent,
        state_dir,
        now,
    )
}

// --- Live-pane gauge reader (card `20261103-b-bb7230`, leg 2 of
// commands::sweep's own "not ported" list) -----------------------------
//
// Port of Python's `limits.Gauge`/`_read_grok_gauge`/`_read_claude_gauge`/
// `has_gauge_reader`/`read_gauge`/`record_gauge` (frozen reference,
// `multiverse/limits.py`), plus `cli._sweep_tick_gauge`
// (`cli.py:11502-11541`). Narrowed to the fields grok's and claude's own
// readers ever set — `window_minutes`/`secondary_*` are codex-only and
// already live on [`CodexUsage`] instead, ported separately.

/// A live, informational quota reading straight off a pane's own status
/// text — categorically not a [`Verdict`]: a gauge, however low, means the
/// seat is still serving, never a reason to call [`crate::breaker::trip`].
/// Mirrors Python's `limits.Gauge`, narrowed as this module header
/// describes.
#[derive(Debug, Clone, PartialEq)]
pub struct Gauge {
    pub vendor: String,
    pub percent_left: PercentLeft,
    pub label: Option<String>,
    pub resets_at: Option<OffsetDateTime>,
}

/// grok's live pane banner, wrapped as a [`Gauge`]: reuses
/// [`parse_grok_gauge_banner`] for the percentage (see that function's own
/// confidence note — this reader inherits the same not-independently-
/// verified-against-a-captured-pane caveat), with no label or reset clock,
/// the same as Python's own `_read_grok_gauge` leaves `resets_at` unset.
fn read_grok_gauge(text: &str) -> Option<Gauge> {
    Some(Gauge {
        vendor: "grok".to_owned(),
        percent_left: parse_grok_gauge_banner(text)?,
        label: None,
        resets_at: None,
    })
}

/// Python's `_CLAUDE_GAUGE_RE`: "You've used N% of your weekly/session
/// limit · resets <date> (<tz>)" — the reset clause is optional, both in
/// the banner itself and in this pattern.
fn claude_gauge_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"You've used (-?\d+)% of your (weekly|session) limit(?:\s*\x{b7}\s*resets\s+([^(]+?)\s*\(([^)]+)\))?",
        )
        .expect("static regex compiles")
    })
}

/// Python's `_CLAUDE_TZ_OFFSETS`: the only two timezone abbreviations
/// [`parse_claude_reset`] resolves a reset clock against — an abbreviation
/// not listed here still yields a [`Gauge`], just with `resets_at: None`.
fn claude_tz_offset(abbrev: &str) -> Option<time::UtcOffset> {
    match abbrev.trim() {
        "UTC" => Some(time::UtcOffset::UTC),
        "IST" => time::UtcOffset::from_hms(5, 30, 0).ok(),
        _ => None,
    }
}

/// Python's `_CLAUDE_RESET_DATE_FORMAT` ("%b %d, %Y, %I:%M %p" — "Oct 19,
/// 2026, 5:30 PM"), hand-matched rather than pulled in via a strptime-style
/// dependency this crate does not otherwise take.
fn claude_reset_date_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(\w{3})\s+(\d{1,2}),\s+(\d{4}),\s+(\d{1,2}):(\d{2})\s*(AM|PM)$")
            .expect("static regex compiles")
    })
}

fn claude_reset_month(abbrev: &str) -> Option<time::Month> {
    use time::Month::*;
    Some(match abbrev.to_ascii_lowercase().as_str() {
        "jan" => January,
        "feb" => February,
        "mar" => March,
        "apr" => April,
        "may" => May,
        "jun" => June,
        "jul" => July,
        "aug" => August,
        "sep" => September,
        "oct" => October,
        "nov" => November,
        "dec" => December,
        _ => return None,
    })
}

/// Python's `_parse_claude_reset`: the pane's "resets <date> (<tz>)" clock
/// as UTC, or `None` — an abbreviation not in [`claude_tz_offset`], or a
/// date that does not match [`claude_reset_date_re`], yields `None` rather
/// than a guessed offset; the gauge itself still stands either way.
fn parse_claude_reset(date_text: &str, tz_abbrev: &str) -> Option<OffsetDateTime> {
    let offset = claude_tz_offset(tz_abbrev)?;
    let caught = claude_reset_date_re().captures(date_text.trim())?;
    let month = claude_reset_month(&caught[1])?;
    let day: u8 = caught[2].parse().ok()?;
    let year: i32 = caught[3].parse().ok()?;
    let mut hour: u8 = caught[4].parse().ok()?;
    let minute: u8 = caught[5].parse().ok()?;
    if hour > 12 {
        return None;
    }
    let is_pm = caught[6].eq_ignore_ascii_case("PM");
    hour %= 12;
    if is_pm {
        hour += 12;
    }
    let date = time::Date::from_calendar_date(year, month, day).ok()?;
    let time_of_day = time::Time::from_hms(hour, minute, 0).ok()?;
    Some(
        date.with_time(time_of_day)
            .assume_offset(offset)
            .to_offset(time::UtcOffset::UTC),
    )
}

/// Python's `_read_claude_gauge`: claude/claude-alt's own "You've used N% of
/// your weekly/session limit" footer, or `None`. Last match, not the
/// first — same reasoning as [`read_grok_gauge`]'s own banner reader:
/// scrollback can carry an earlier quoted banner ahead of what the pane
/// shows now. `percent_left` is `100 - used`; an unparseable reset date
/// leaves `resets_at` `None` rather than dropping the reading.
fn read_claude_gauge(text: &str) -> Option<Gauge> {
    let caught = claude_gauge_re().captures_iter(text).last()?;
    let used: f64 = caught.get(1)?.as_str().parse().ok()?;
    if !(0.0..=100.0).contains(&used) {
        return None;
    }
    let window_kind = caught.get(2)?.as_str();
    let resets_at = match (caught.get(3), caught.get(4)) {
        (Some(date), Some(tz)) => parse_claude_reset(date.as_str(), tz.as_str()),
        _ => None,
    };
    Some(Gauge {
        vendor: "claude".to_owned(),
        percent_left: 100.0 - used,
        label: Some(format!("{window_kind} limit")),
        resets_at,
    })
}

/// Python's `has_gauge_reader`: whether `kind` has a registered gauge
/// reader. Lets a caller skip an extra pane read entirely for a harness
/// with nothing to look for, rather than reading the pane and discarding
/// the result.
pub fn has_gauge_reader(kind: &str) -> bool {
    matches!(kind, "grok" | "claude")
}

/// Python's `read_gauge`: read one pane's live quota gauge, or return none.
/// Same fail-open contract as [`classify`]: empty/whitespace-only text, an
/// unrecognised `kind`, and text that reader does not recognise all return
/// `None`. Pure, like `classify` — this only ever reads `text`; it never
/// trips anything and never touches disk (see [`record_gauge`] for the
/// deliberately separate persisted side).
pub fn read_gauge(text: &str, kind: &str) -> Option<Gauge> {
    if text.trim().is_empty() {
        return None;
    }
    match kind {
        "grok" => read_grok_gauge(text),
        "claude" => read_claude_gauge(text),
        _ => None,
    }
}

/// Python's `cli._sweep_tick_gauge`, narrowed to the one Herdr call it
/// actually makes (`herdr.agent_read(name, source="visible")`) — finding
/// the live orchestrator pane by identity is the caller's own job (
/// [`crate::fleet_start::FleetState::orchestrator_agent`], the same lookup
/// `commands::sweep::tick_outcome` already makes), passed in here as
/// `agent_name` only once a caller has confirmed that pane exists. Reads
/// regardless of Herdr's own reported status — a seat is just as worth a
/// reading while its orchestrator is busy as while it is idle — and never
/// types into or sends anything. Fails open to `None` for every
/// "nothing to show" case: no gauge reader for `kind` (checked first, the
/// same no-cost-for-the-common-case guard [`has_gauge_reader`] exists for),
/// a failed Herdr call, or an empty or gauge-less pane read.
pub fn sweep_tick_gauge(
    herdr: &mut impl crate::runner::Herdr,
    agent_name: &str,
    kind: &str,
) -> Option<Gauge> {
    if !has_gauge_reader(kind) {
        return None;
    }
    let command = mv_herdr::argv::agent_read(agent_name, Some("visible"), None);
    let result = herdr.call(&command, Duration::from_secs(30)).ok()?;
    if !result.ok() || result.stdout.trim().is_empty() {
        return None;
    }
    read_gauge(&result.stdout, kind)
}

/// Python's `limits.record_gauge`: persist `gauge` as `pool`'s most recent
/// reading, read-modify-write under the gauge store's own lock
/// ([`mv_state::gauge::with_locked`]) so a concurrent recovery probe's own
/// `last_probe_at` stamp is never lost. One narrow carry-forward, same as
/// Python: an *exhausted* reading (`percent_left <= 0`) that states no
/// `resets_at` of its own inherits the previous reading's, if there was
/// one — discarding a still-real window clock onto a credits-depleted
/// marker that simply has no way to state it would otherwise lose
/// information [`crate::breaker`]'s own trip-from-gauge logic needs. A
/// *healthy* reading with no `resets_at` of its own is never touched by
/// this — carrying a stale clock onto an unrelated positive reading would
/// misreport it, not merely omit it. Best-effort, like [`record_probe`]'s
/// own caller: a lock/write failure here is silently swallowed rather than
/// hiding a valid reading from the operator.
pub fn record_gauge(state_dir: &Path, pool: &str, gauge: &Gauge, observed_at: OffsetDateTime) {
    let _ = mv_state::gauge::with_locked(state_dir, |map| {
        let carried_resets_at = if gauge.resets_at.is_none() && gauge.percent_left <= 0.0 {
            map.get(pool).and_then(|entry| entry.resets_at.clone())
        } else {
            None
        };
        let entry = map.entry(pool.to_string()).or_default();
        entry.vendor = Some(gauge.vendor.clone());
        entry.percent_left = Some(gauge.percent_left);
        entry.label = gauge.label.clone();
        entry.resets_at = gauge
            .resets_at
            .map(mv_core::time::format_seconds_utc)
            .or(carried_resets_at);
        entry.observed_at = Some(mv_core::time::format_seconds_utc(observed_at));
    });
}

#[cfg(test)]
mod classify_tests {
    use super::*;
    use time::macros::datetime;

    const CODEX_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-out-of-credits.txt");
    const CODEX_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-rolling-429.txt");
    const CODEX_MODEL_SWITCH: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt");
    const CLAUDE_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/claude-rolling-429.txt");
    const GROK_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-reached.txt");
    const GROK_LIVE_PANE: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-live-pane.txt");
    const GROK_GAUGE: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt");
    const GROK_SANDBOX: &str =
        include_str!("../../../tests/fixtures/sandbox-dialog/grok-reviewer-tool-approval.txt");
    const CODEX_SANDBOX: &str =
        include_str!("../../../tests/fixtures/sandbox-dialog/codex-reviewer-loopback-approval.txt");
    const CODEX_UPDATE_PROMPT: &str =
        include_str!("../../../tests/fixtures/stall-cause/codex-cli-update-prompt.txt");
    const README: &str = include_str!("../../../tests/fixtures/rate-limit/README.md");

    //: Real prose that *discusses* usage limits without being the banner
    //: itself (Python's own `PROSE_ABOUT_RATE_LIMITS` fixture) — the most
    //: important guard in this module: naive keyword scanning hit exactly
    //: this shape live, tripping seats merely *discussing* rate limits.
    const PROSE_ABOUT_RATE_LIMITS: &str = "\
### The seat works, and the upgrade bought limits rather than models

`grok models` reported exactly one model before the upgrade and the same one after,
with a freshly minted post-upgrade token. The plan raises usage limits; it does not
widen the model list. Worth stating plainly, because the human spent money partly
expecting otherwise.
";

    const ORDINARY_AGENT_OUTPUT: &str = "\
## Task 1 — Verify a command against the real binary · DONE

**The CLI is installed.** No blocked-task procedure was needed.

Captured `--version`/`--help` output into `docs/verification/` (committed, so a
later session can diff against a new release rather than re-deriving).
";

    #[test]
    fn the_captured_codex_fixture_classifies_as_exhausted_with_no_reset_time() {
        assert_eq!(
            classify(CODEX_EXHAUSTED, "codex", None),
            Some(Verdict {
                vendor: "codex".to_owned(),
                rule: "codex_usage_limit_reached".to_owned(),
                kind: BreakerKind::Rolling,
                reset_at: None,
                interactive: true,
            })
        );
    }

    #[test]
    fn ordinary_agent_output_does_not_classify() {
        assert_eq!(classify(ORDINARY_AGENT_OUTPUT, "codex", None), None);
    }

    #[test]
    fn prose_discussing_rate_limits_does_not_classify() {
        assert_eq!(classify(PROSE_ABOUT_RATE_LIMITS, "codex", None), None);
        assert_eq!(classify(PROSE_ABOUT_RATE_LIMITS, "claude", None), None);
        assert_eq!(classify(PROSE_ABOUT_RATE_LIMITS, "grok", None), None);
    }

    #[test]
    fn a_banner_quoted_or_documented_far_from_the_pane_end_does_not_classify() {
        // This directory's own README quotes the exact live banner text
        // verbatim, for documentation — a plain substring search cannot
        // tell that from a vendor CLI printing it right now; `classify`
        // anchors on *where* the banner sits, not only its wording.
        assert_eq!(classify(README, "codex", None), None);
        assert_eq!(classify(README, "grok", None), None);
    }

    #[test]
    fn unknown_vendor_returns_no_verdict() {
        assert_eq!(classify(CODEX_EXHAUSTED, "claude", None), None);
        assert_eq!(classify(CODEX_EXHAUSTED, "grok", None), None);
        assert_eq!(classify(CODEX_EXHAUSTED, "brand-new-vendor", None), None);
    }

    #[test]
    fn empty_and_whitespace_input_return_no_verdict() {
        assert_eq!(classify("", "codex", None), None);
        assert_eq!(classify("   \n\t  ", "codex", None), None);
    }

    #[test]
    fn classify_is_a_pure_function() {
        assert_eq!(
            classify(CODEX_EXHAUSTED, "codex", None),
            classify(CODEX_EXHAUSTED, "codex", None)
        );
    }

    #[test]
    fn the_captured_grok_fixture_classifies_as_a_weekly_exhausted_dialog() {
        assert_eq!(
            classify(GROK_EXHAUSTED, "grok", None),
            Some(Verdict {
                vendor: "grok".to_owned(),
                rule: "grok_weekly_limit_reached".to_owned(),
                kind: BreakerKind::Weekly,
                reset_at: None,
                interactive: true,
            })
        );
    }

    #[test]
    fn the_live_grok_pane_with_herdr_chrome_classifies_as_exhausted() {
        // Herdr's navigation and pane-footer lines trail the real
        // exhaustion dialog, so the live window must retain the sentence
        // itself as well as the trailing non-blank lines.
        let verdict = classify(GROK_LIVE_PANE, "grok", None).unwrap();
        assert_eq!(verdict.vendor, "grok");
        assert_eq!(verdict.rule, "grok_weekly_limit_reached");
        assert_eq!(verdict.kind, BreakerKind::Weekly);
    }

    #[test]
    fn grok_gauge_fixture_is_not_classified_as_exhausted() {
        // The critical trap: both the dead-banner fixture and the healthy
        // gauge fixture contain the phrase "Weekly limit" — matching on
        // that phrase alone would falsely trip a working seat.
        assert_eq!(classify(GROK_GAUGE, "grok", None), None);
    }

    #[test]
    fn codex_verdict_is_interactive_because_its_dialog_is_attached() {
        let verdict = classify(CODEX_EXHAUSTED, "codex", None).unwrap();
        assert!(verdict.interactive);
    }

    #[test]
    fn a_plain_banner_with_no_dialog_is_not_interactive() {
        let plain_banner = "Usage limit reached\n\
            Request a limit increase from your owner to continue using codex.\n";
        let verdict = classify(plain_banner, "codex", None).unwrap();
        assert!(!verdict.interactive);
    }

    #[test]
    fn the_model_switch_fixture_does_not_classify_as_any_exhaustion_kind() {
        // The sharp trap this guards: the dialog opens with "Approaching
        // rate limits", the same words a real codex exhaustion banner
        // would use. It must classify as no exhaustion kind at all, for
        // every vendor label, not just codex.
        assert_eq!(classify(CODEX_MODEL_SWITCH, "codex", None), None);
        assert_eq!(classify(CODEX_MODEL_SWITCH, "grok", None), None);
        assert_eq!(classify(CODEX_MODEL_SWITCH, "claude", None), None);
    }

    #[test]
    fn the_claude_fixture_classifies_as_a_rolling_rate_limit() {
        // `"pool \"product\""` in the captured text is Anthropic's own
        // internal account pool, not a Multiverse one; `Verdict` carries no
        // pool field at all, so there is nothing here that could parse it
        // as one.
        let verdict = classify(CLAUDE_ROLLING, "claude", None).unwrap();
        assert_eq!(
            verdict,
            Verdict {
                vendor: "claude".to_owned(),
                rule: "claude_rate_limit_error".to_owned(),
                kind: BreakerKind::Rolling,
                reset_at: None,
                interactive: false,
            }
        );
        assert_ne!(verdict.kind, BreakerKind::Budget);
    }

    #[test]
    fn claude_reset_at_is_parsed_from_the_vendors_own_cooldown_when_now_is_given() {
        let now = datetime!(2026-09-11 06:09:56 UTC);
        let verdict = classify(CLAUDE_ROLLING, "claude", Some(now)).unwrap();
        assert_eq!(verdict.reset_at, Some(now + time::Duration::seconds(60)));
        assert_eq!(verdict.kind, BreakerKind::Rolling);
    }

    #[test]
    fn claude_reset_at_falls_back_to_none_without_a_supplied_now() {
        let verdict = classify(CLAUDE_ROLLING, "claude", None).unwrap();
        assert_eq!(verdict.reset_at, None);
    }

    #[test]
    fn claude_reset_at_uses_the_last_retry_banner_not_the_first() {
        // Card `20260927-b-7bf160`'s own regression, ported: an earlier
        // banner still sitting in scrollback must not decide `reset_at`
        // instead of the pane's current one.
        let stale_banner = CLAUDE_ROLLING.replace("Retry in 60s.", "Retry in 5s.");
        let text = format!("{stale_banner}\n{CLAUDE_ROLLING}");
        let now = datetime!(2026-09-11 06:09:56 UTC);
        let verdict = classify(&text, "claude", Some(now)).unwrap();
        assert_eq!(verdict.reset_at, Some(now + time::Duration::seconds(60)));
    }

    #[test]
    fn claude_fixture_is_never_read_for_the_wrong_vendor() {
        assert_eq!(classify(CLAUDE_ROLLING, "codex", None), None);
        assert_eq!(classify(CLAUDE_ROLLING, "grok", None), None);
    }

    #[test]
    fn the_out_of_credits_fixture_is_rolling_not_budget() {
        let verdict = classify(CODEX_EXHAUSTED, "codex", None).unwrap();
        assert_eq!(verdict.kind, BreakerKind::Rolling);
        assert_ne!(verdict.kind, BreakerKind::Budget);
        assert_eq!(verdict.reset_at, None);
    }

    #[test]
    fn the_codex_rolling_fixture_classifies_as_a_rolling_rate_limit() {
        assert_eq!(
            classify(CODEX_ROLLING, "codex", None),
            Some(Verdict {
                vendor: "codex".to_owned(),
                rule: "codex_retry_limit_exceeded".to_owned(),
                kind: BreakerKind::Rolling,
                reset_at: None,
                interactive: false,
            })
        );
    }

    #[test]
    fn codex_rolling_reset_at_is_the_five_minute_floor_when_now_is_supplied() {
        let now = datetime!(2026-09-15 12:00:00 UTC);
        let verdict = classify(CODEX_ROLLING, "codex", Some(now)).unwrap();
        assert_eq!(verdict.reset_at, Some(now + time::Duration::minutes(5)));
        assert_eq!(verdict.kind, BreakerKind::Rolling);
    }

    #[test]
    fn codex_out_of_credits_reset_at_stays_none_even_when_now_is_supplied() {
        // The `DO NOT TOUCH` rule: giving `classify_codex` a `now` for the
        // 429 rule must not make the exhausted-allowance rule start using
        // it too — it has no floor and no parsed cooldown.
        let now = datetime!(2026-09-15 12:00:00 UTC);
        let verdict = classify(CODEX_EXHAUSTED, "codex", Some(now)).unwrap();
        assert_eq!(verdict.reset_at, None);
    }

    #[test]
    fn the_codex_out_of_credits_fixture_is_not_read_through_the_429_rule() {
        let verdict = classify(CODEX_EXHAUSTED, "codex", None).unwrap();
        assert_eq!(verdict.rule, "codex_usage_limit_reached");
        assert_ne!(verdict.rule, "codex_retry_limit_exceeded");
        assert!(verdict.interactive);
    }

    #[test]
    fn codex_rolling_fixture_is_never_read_for_the_wrong_vendor() {
        assert_eq!(classify(CODEX_ROLLING, "claude", None), None);
        assert_eq!(classify(CODEX_ROLLING, "grok", None), None);
    }

    #[test]
    fn neither_sandbox_fixture_is_classified_as_exhaustion() {
        // A permission prompt is not a seat running out — if `classify`
        // ever matched these, a blocked reviewer would trip a breaker and
        // take the pool down for every project sharing it.
        for text in [GROK_SANDBOX, CODEX_SANDBOX] {
            for vendor in ["grok", "codex", "claude"] {
                assert_eq!(classify(text, vendor, None), None, "{vendor}");
            }
        }
    }

    #[test]
    fn the_codex_update_prompt_is_not_mistaken_for_exhaustion() {
        for vendor in ["codex", "grok", "claude"] {
            assert_eq!(
                classify(CODEX_UPDATE_PROMPT, vendor, None),
                None,
                "{vendor}"
            );
        }
    }

    #[test]
    fn an_invented_near_miss_sandbox_dialog_is_not_classified() {
        let near_miss = "Would you like to make this change?\n\n\
            Destination: /home/agent/project/main.go\n\n\
            › 1. Yes, proceed (y)\n  \
            2. Yes, and don't ask again for these files (f)\n  \
            3. No, and tell Codex what to do differently (esc)\n\n\
            Press enter to confirm or esc to cancel\n";
        for vendor in ["codex", "grok", "claude", "chatgpt"] {
            assert_eq!(classify(near_miss, vendor, None), None);
        }
    }

    #[test]
    fn claude_classify_is_a_pure_function() {
        assert_eq!(
            classify(CLAUDE_ROLLING, "claude", None),
            classify(CLAUDE_ROLLING, "claude", None)
        );
        let now = datetime!(2026-09-11 06:09:56 UTC);
        assert_eq!(
            classify(CLAUDE_ROLLING, "claude", Some(now)),
            classify(CLAUDE_ROLLING, "claude", Some(now))
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn codex_rollout_records_support_both_shapes_and_reject_bad_percentages() {
        let direct = json!({"type": "token_count", "payload": {"rate_limits": {
            "primary": {"used_percent": 40}, "secondary": {"used_percent": 25}
        }}});
        let parsed = parse_codex_usage(&direct).unwrap();
        assert_eq!(parsed.primary_percent_left, 60.0);
        assert_eq!(parsed.secondary_percent_left, Some(75.0));
        let current = json!({"type": "event_msg", "payload": {"type": "token_count",
            "rate_limits": {"primary": {"used_percent": 40, "resets_at": 0}}}});
        assert_eq!(
            parse_codex_usage(&current).unwrap().resets_at.as_deref(),
            Some("1970-01-01T00:00:00+00:00")
        );
        for bad in [json!(true), json!(-1), json!(101), json!("40")] {
            let mut record = direct.clone();
            record["payload"]["rate_limits"]["primary"]["used_percent"] = bad;
            assert!(parse_codex_usage(&record).is_none());
        }
    }

    #[test]
    fn codex_scan_records_last_valid_reading_and_does_not_freshen_it() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let sessions = dir.path().join("sessions/2026/09/27");
        std::fs::create_dir_all(&sessions).unwrap();
        std::fs::write(sessions.join("rollout.jsonl"), concat!(
            "garbage\n",
            "{\"type\":\"token_count\",\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":10}}}}\n",
            "{\"type\":\"event_msg\",\"payload\":{\"type\":\"token_count\",\"rate_limits\":{\"primary\":{\"used_percent\":40}}}}\n",
            "{\"type\":\"token_count\",\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":true}}}}\n",
        )).unwrap();
        assert_eq!(
            read_and_record_codex_usage(dir.path(), state.path(), "test")
                .unwrap()
                .primary_percent_left,
            60.0
        );
        let before = mv_state::gauge::load(state.path());
        assert_eq!(before["test"].percent_left, Some(60.0));
        read_and_record_codex_usage(dir.path(), state.path(), "test").unwrap();
        assert_eq!(before, mv_state::gauge::load(state.path()));
        assert!(read_and_record_codex_usage(state.path(), state.path(), "missing").is_none());
    }

    #[test]
    fn codex_pool_splits_on_model_provider() {
        assert_eq!(codex_log_pool("openai"), CodexPool::Base);
        assert_eq!(codex_log_pool("azure"), CodexPool::Secondary);
        assert_eq!(codex_log_pool(""), CodexPool::Secondary);
    }

    #[test]
    fn percent_left_inverts_used_percent() {
        assert_eq!(percent_left_from_used(30.0), Some(70.0));
        assert_eq!(percent_left_from_used(0.0), Some(100.0));
        assert_eq!(percent_left_from_used(100.0), Some(0.0));
    }

    #[test]
    fn out_of_range_percentage_is_unknown() {
        assert_eq!(percent_left_from_used(-1.0), None);
        assert_eq!(percent_left_from_used(100.5), None);
    }

    #[test]
    fn boolean_used_percent_is_not_a_gauge() {
        let rate_limits = json!({"primary": {"used_percent": true}});
        assert_eq!(codex_rate_limit_percent_left(&rate_limits, "primary"), None);
    }

    #[test]
    fn numeric_used_percent_reads_through() {
        let rate_limits = json!({"primary": {"used_percent": 25.0}});
        assert_eq!(
            codex_rate_limit_percent_left(&rate_limits, "primary"),
            Some(75.0)
        );
    }

    #[test]
    fn missing_window_is_none_not_a_panic() {
        let rate_limits = json!({"primary": {"used_percent": 1.0}});
        assert_eq!(
            codex_rate_limit_percent_left(&rate_limits, "secondary"),
            None
        );
    }

    #[test]
    fn grok_banner_parses_a_percentage() {
        assert_eq!(
            parse_grok_gauge_banner("status: Weekly limit left: 42%"),
            Some(42.0)
        );
        assert_eq!(parse_grok_gauge_banner("no banner here"), None);
    }

    #[test]
    fn grok_banner_out_of_range_is_unknown() {
        assert_eq!(parse_grok_gauge_banner("Weekly limit left: 150%"), None);
    }

    // --- has_gauge_reader/read_gauge (card `20261103-b-bb7230`) ------------

    #[test]
    fn grok_has_a_gauge_reader_and_claude_and_codex_do_not() {
        assert!(has_gauge_reader("grok"));
        assert!(has_gauge_reader("claude"));
        assert!(!has_gauge_reader("codex"));
        assert!(!has_gauge_reader("chatgpt"));
    }

    #[test]
    fn read_gauge_dispatches_grok_to_its_own_reader() {
        let gauge = read_gauge("Weekly limit left: 3% · Grok 4.6 (high)", "grok").unwrap();
        assert_eq!(gauge.vendor, "grok");
        assert_eq!(gauge.percent_left, 3.0);
    }

    #[test]
    fn read_gauge_is_fail_open_for_empty_text_and_unlisted_kinds() {
        assert_eq!(read_gauge("", "grok"), None);
        assert_eq!(read_gauge("   ", "claude"), None);
        assert_eq!(read_gauge("Weekly limit left: 3%", "codex"), None);
    }

    // --- read_claude_gauge, ported from `tests/fixtures/rate-limit/
    // claude-weekly-usage.txt`/`claude-session-usage.txt` in the frozen
    // Python reference -------------------------------------------------

    #[test]
    fn claude_weekly_usage_banner_parses_percent_left_and_reset_clock() {
        let gauge = read_claude_gauge(
            "You've used 91% of your weekly limit · resets Oct 19, 2026, 5:30 PM (IST)",
        )
        .unwrap();
        assert_eq!(gauge.vendor, "claude");
        assert_eq!(gauge.percent_left, 9.0);
        assert_eq!(gauge.label.as_deref(), Some("weekly limit"));
        // Oct 19, 2026, 17:30 IST (+05:30) == Oct 19, 2026, 12:00 UTC.
        assert_eq!(
            gauge.resets_at,
            Some(time::macros::datetime!(2026-10-19 12:00:00 UTC))
        );
    }

    #[test]
    fn claude_session_usage_banner_parses_the_am_reset_clock() {
        let gauge = read_claude_gauge(
            "You've used 42% of your session limit · resets Oct 16, 2026, 2:15 AM (IST)",
        )
        .unwrap();
        assert_eq!(gauge.percent_left, 58.0);
        assert_eq!(gauge.label.as_deref(), Some("session limit"));
        // Oct 16, 2026, 02:15 IST (+05:30) == Oct 15, 2026, 20:45 UTC.
        assert_eq!(
            gauge.resets_at,
            Some(time::macros::datetime!(2026-10-15 20:45:00 UTC))
        );
    }

    #[test]
    fn claude_banner_with_no_reset_clause_still_yields_a_gauge() {
        let gauge = read_claude_gauge("You've used 10% of your weekly limit").unwrap();
        assert_eq!(gauge.percent_left, 90.0);
        assert_eq!(gauge.resets_at, None);
    }

    #[test]
    fn claude_banner_with_an_unlisted_timezone_still_yields_a_gauge_with_no_reset_clock() {
        let gauge = read_claude_gauge(
            "You've used 10% of your weekly limit · resets Oct 19, 2026, 5:30 PM (PST)",
        )
        .unwrap();
        assert_eq!(gauge.percent_left, 90.0);
        assert_eq!(gauge.resets_at, None);
    }

    #[test]
    fn claude_banner_takes_the_last_match_not_the_first() {
        let text = "You've used 91% of your weekly limit\n\
                     ...scrollback...\n\
                     You've used 10% of your weekly limit";
        let gauge = read_claude_gauge(text).unwrap();
        assert_eq!(gauge.percent_left, 90.0, "must read the latest banner");
    }

    #[test]
    fn claude_banner_out_of_range_is_unknown() {
        assert_eq!(
            read_claude_gauge("You've used 150% of your weekly limit"),
            None
        );
        assert_eq!(read_claude_gauge("no banner here"), None);
    }

    // --- record_gauge (card `20261103-b-bb7230`) ---------------------------

    #[test]
    fn record_gauge_persists_vendor_percent_and_label() {
        let dir = tempfile::tempdir().unwrap();
        let now = time::macros::datetime!(2026-11-02 20:00:00 UTC);
        let gauge = Gauge {
            vendor: "claude".to_string(),
            percent_left: 9.0,
            label: Some("weekly limit".to_string()),
            resets_at: Some(time::macros::datetime!(2026-10-19 12:00:00 UTC)),
        };
        record_gauge(dir.path(), "claude", &gauge, now);
        let stored = mv_state::gauge::load(dir.path());
        let entry = &stored["claude"];
        assert_eq!(entry.vendor.as_deref(), Some("claude"));
        assert_eq!(entry.percent_left, Some(9.0));
        assert_eq!(entry.label.as_deref(), Some("weekly limit"));
        assert!(entry.observed_at.is_some());
    }

    #[test]
    fn record_gauge_carries_forward_the_previous_reset_clock_for_an_exhausted_reading_only() {
        let dir = tempfile::tempdir().unwrap();
        let now = time::macros::datetime!(2026-11-02 20:00:00 UTC);
        let healthy = Gauge {
            vendor: "claude".to_string(),
            percent_left: 50.0,
            label: None,
            resets_at: Some(time::macros::datetime!(2026-10-19 12:00:00 UTC)),
        };
        record_gauge(dir.path(), "claude", &healthy, now);

        let exhausted = Gauge {
            vendor: "claude".to_string(),
            percent_left: 0.0,
            label: None,
            resets_at: None,
        };
        record_gauge(dir.path(), "claude", &exhausted, now);
        let stored = mv_state::gauge::load(dir.path());
        assert_eq!(
            stored["claude"].resets_at.as_deref(),
            Some("2026-10-19T12:00:00+00:00"),
            "an exhausted reading with no reset clock of its own must inherit the previous one"
        );

        let healthy_again = Gauge {
            vendor: "claude".to_string(),
            percent_left: 50.0,
            label: None,
            resets_at: None,
        };
        record_gauge(dir.path(), "claude", &healthy_again, now);
        assert_eq!(
            mv_state::gauge::load(dir.path())["claude"].resets_at,
            None,
            "a healthy reading with no reset clock of its own must not inherit a stale one"
        );
    }

    #[test]
    fn record_gauge_preserves_a_concurrently_recorded_probe_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let now = time::macros::datetime!(2026-11-02 20:00:00 UTC);
        let _ = mv_state::gauge::with_locked(dir.path(), |map| {
            record_probe(map.entry("claude".to_string()).or_default(), "stamped");
        });
        let gauge = Gauge {
            vendor: "claude".to_string(),
            percent_left: 50.0,
            label: None,
            resets_at: None,
        };
        record_gauge(dir.path(), "claude", &gauge, now);
        assert_eq!(
            last_probe_at(&mv_state::gauge::load(dir.path())["claude"]),
            Some("stamped")
        );
    }

    #[cfg(unix)]
    mod sweep_tick_gauge_tests {
        use super::*;
        use crate::runner::{Herdr, RunnerError};
        use mv_herdr::envelope::CommandResult;

        /// A scripted [`Herdr`] standing in for a real `agent read` call —
        /// `sweep_tick_gauge` must call it exactly the way Python's own
        /// `herdr.agent_read(name, source="visible")` does and nothing else.
        struct FakeHerdr {
            stdout: String,
            ok: bool,
            calls: Vec<String>,
        }

        impl Herdr for FakeHerdr {
            fn call(
                &mut self,
                command: &mv_herdr::argv::HerdrCommand,
                _timeout: std::time::Duration,
            ) -> Result<CommandResult, RunnerError> {
                self.calls.push(command.purpose.clone());
                Ok(CommandResult {
                    argv: vec![],
                    returncode: if self.ok { 0 } else { 1 },
                    stdout: self.stdout.clone(),
                    stderr: String::new(),
                    payload: None,
                    protocol_error: None,
                })
            }
        }

        #[test]
        fn a_readable_pane_with_a_known_gauge_shape_is_read_and_parsed() {
            let mut herdr = FakeHerdr {
                stdout: "Weekly limit left: 3% · Grok 4.6 (high)".to_string(),
                ok: true,
                calls: Vec::new(),
            };
            let gauge = sweep_tick_gauge(&mut herdr, "sandbox-nexus", "grok").unwrap();
            assert_eq!(gauge.percent_left, 3.0);
            assert_eq!(herdr.calls.len(), 1, "exactly one Herdr call, no retries");
        }

        #[test]
        fn a_kind_with_no_gauge_reader_is_never_read() {
            let mut herdr = FakeHerdr {
                stdout: "anything".to_string(),
                ok: true,
                calls: Vec::new(),
            };
            assert_eq!(sweep_tick_gauge(&mut herdr, "sandbox-nexus", "codex"), None);
            assert!(
                herdr.calls.is_empty(),
                "has_gauge_reader must be checked before any Herdr call"
            );
        }

        #[test]
        fn a_failed_or_empty_read_yields_no_gauge() {
            let mut refused = FakeHerdr {
                stdout: String::new(),
                ok: false,
                calls: Vec::new(),
            };
            assert_eq!(
                sweep_tick_gauge(&mut refused, "sandbox-nexus", "grok"),
                None
            );

            let mut empty = FakeHerdr {
                stdout: "   ".to_string(),
                ok: true,
                calls: Vec::new(),
            };
            assert_eq!(sweep_tick_gauge(&mut empty, "sandbox-nexus", "grok"), None);
        }

        #[test]
        fn a_readable_pane_with_no_gauge_line_yields_none() {
            let mut herdr = FakeHerdr {
                stdout: "still working on it".to_string(),
                ok: true,
                calls: Vec::new(),
            };
            assert_eq!(sweep_tick_gauge(&mut herdr, "sandbox-nexus", "grok"), None);
        }
    }

    #[test]
    fn quota_floor_checks_both_windows() {
        assert!(meets_quota_floor(50.0, Some(50.0), 10.0));
        assert!(!meets_quota_floor(5.0, Some(50.0), 10.0));
        assert!(!meets_quota_floor(50.0, Some(5.0), 10.0));
        assert!(meets_quota_floor(50.0, None, 10.0));
    }

    #[test]
    fn probe_timestamp_round_trips_on_a_gauge_entry() {
        let mut entry = mv_state::gauge::GaugeEntry::default();
        assert_eq!(last_probe_at(&entry), None);
        record_probe(&mut entry, "2026-11-02T12:00:00+00:00");
        assert_eq!(last_probe_at(&entry), Some("2026-11-02T12:00:00+00:00"));
    }

    // --- sweep_tick_codex_probe_with (leg 3 of commands::sweep's own
    // "not ported" list) --------------------------------------------------

    #[test]
    fn codex_binary_honors_the_env_override() {
        assert_eq!(
            resolve_codex_binary(Some("/path/to/fake-codex".to_string())),
            "/path/to/fake-codex"
        );
        assert_eq!(resolve_codex_binary(Some(String::new())), "codex");
        assert_eq!(resolve_codex_binary(Some("   ".to_string())), "codex");
        assert_eq!(resolve_codex_binary(None), "codex");
    }

    #[cfg(unix)]
    mod codex_probe_tests {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn chatgpt_account_config(config_dir: &Path, extra: &str) -> mv_config::load::Config {
            let content = format!(
                "[accounts.codex-main]\n\
                 kind = \"codex\"\n\
                 config_dir_var = \"CODEX_HOME\"\n\
                 config_dir = \"{}\"\n\
                 pool = \"chatgpt\"\n\
                 {extra}\n",
                config_dir.display()
            );
            mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap()
        }

        /// A fake `codex` that marks itself invoked by appending one line to
        /// `<config_dir>/probe-calls`, so a test can tell a probe actually
        /// ran (not just that the function returned `true`) and count how
        /// many times — the interval dedupe must stop a second call cold.
        fn write_fake_codex(dir: &Path) -> std::path::PathBuf {
            std::fs::create_dir_all(dir).unwrap();
            let script = dir.join("fake-codex.sh");
            std::fs::write(
                &script,
                "#!/bin/sh\necho called >> \"$CODEX_HOME/probe-calls\"\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            script
        }

        fn probe_calls(config_dir: &Path) -> usize {
            std::fs::read_to_string(config_dir.join("probe-calls"))
                .map(|text| text.lines().count())
                .unwrap_or(0)
        }

        /// Seed `state_dir`'s gauge store with one `chatgpt` reading, and
        /// `config_dir`'s own Codex sessions tree with a fresh token_count
        /// record the probe's own `read_and_record_codex_usage` call picks
        /// up afterward — standing in for the real CLI having just written
        /// a new rollout log.
        fn seed_low_gauge_and_fresh_session(config_dir: &Path, state_dir: &Path) {
            let mut map = mv_state::gauge::GaugeMap::new();
            map.insert(
                "chatgpt".to_string(),
                mv_state::gauge::GaugeEntry {
                    vendor: Some("codex".to_string()),
                    percent_left: Some(3.0),
                    ..Default::default()
                },
            );
            mv_state::gauge::save(state_dir, &map).unwrap();

            let sessions = config_dir.join("sessions/2026/09/27");
            std::fs::create_dir_all(&sessions).unwrap();
            std::fs::write(
                sessions.join("rollout.jsonl"),
                "{\"type\":\"token_count\",\"payload\":{\"rate_limits\":{\"primary\":{\"used_percent\":20}}}}\n",
            )
            .unwrap();
        }

        #[test]
        fn a_drained_pool_is_probed_and_its_gauge_refreshed() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            seed_low_gauge_and_fresh_session(&config_dir, &state_dir);
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "");
            let account = &config.accounts["codex-main"];

            let probed = sweep_tick_codex_probe_with(
                &binary,
                account,
                5,
                &state_dir,
                mv_core::time::now_utc(),
            );

            assert!(probed);
            assert_eq!(
                probe_calls(&config_dir),
                1,
                "the fake codex must run exactly once"
            );
            let gauges = mv_state::gauge::load(&state_dir);
            assert_eq!(
                gauges["chatgpt"].percent_left,
                Some(80.0),
                "read_and_record_codex_usage must refresh the gauge from the fresh session log"
            );
            assert!(
                gauges["chatgpt"].last_probe_at.is_some(),
                "record_probe must stamp this pool's dedupe clock"
            );
        }

        #[test]
        fn a_second_probe_inside_the_interval_is_a_no_op() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            seed_low_gauge_and_fresh_session(&config_dir, &state_dir);
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "");
            let account = &config.accounts["codex-main"];
            let now = mv_core::time::now_utc();

            assert!(sweep_tick_codex_probe_with(
                &binary, account, 5, &state_dir, now
            ));
            assert!(!sweep_tick_codex_probe_with(
                &binary,
                account,
                5,
                &state_dir,
                now + time::Duration::minutes(5)
            ));
            assert_eq!(
                probe_calls(&config_dir),
                1,
                "the dedupe window must stop a second probe cold"
            );
        }

        #[test]
        fn a_seat_that_launches_by_command_is_never_probed() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            seed_low_gauge_and_fresh_session(&config_dir, &state_dir);
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "command = \"codex-alt\"");
            let account = &config.accounts["codex-main"];

            assert!(!sweep_tick_codex_probe_with(
                &binary,
                account,
                5,
                &state_dir,
                mv_core::time::now_utc()
            ));
            assert_eq!(probe_calls(&config_dir), 0);
        }

        #[test]
        fn a_healthy_gauge_above_the_floor_is_never_probed() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            let mut map = mv_state::gauge::GaugeMap::new();
            map.insert(
                "chatgpt".to_string(),
                mv_state::gauge::GaugeEntry {
                    percent_left: Some(50.0),
                    ..Default::default()
                },
            );
            mv_state::gauge::save(&state_dir, &map).unwrap();
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "");
            let account = &config.accounts["codex-main"];

            assert!(!sweep_tick_codex_probe_with(
                &binary,
                account,
                5,
                &state_dir,
                mv_core::time::now_utc()
            ));
            assert_eq!(probe_calls(&config_dir), 0);
        }

        #[test]
        fn no_gauge_reading_at_all_is_never_probed() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "");
            let account = &config.accounts["codex-main"];

            assert!(!sweep_tick_codex_probe_with(
                &binary,
                account,
                5,
                &state_dir,
                mv_core::time::now_utc()
            ));
        }

        #[test]
        fn a_non_codex_or_non_chatgpt_seat_is_never_probed() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("claude-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            let content = format!(
                "[accounts.claude-main]\n\
                 kind = \"claude\"\n\
                 config_dir_var = \"CLAUDE_CONFIG_DIR\"\n\
                 config_dir = \"{}\"\n\
                 command = \"claude\"\n",
                config_dir.display()
            );
            let config =
                mv_config::load::load_config_str(&content, Path::new("config.toml")).unwrap();
            let account = &config.accounts["claude-main"];

            assert!(!sweep_tick_codex_probe_with(
                Path::new("codex"),
                account,
                5,
                &state_dir,
                mv_core::time::now_utc()
            ));
        }

        #[test]
        fn a_zero_dispatch_min_quota_percent_disables_probing() {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("codex-home");
            let state_dir = root.path().join("state");
            std::fs::create_dir_all(&config_dir).unwrap();
            std::fs::create_dir_all(&state_dir).unwrap();
            seed_low_gauge_and_fresh_session(&config_dir, &state_dir);
            let binary = write_fake_codex(&root.path().join("bin"));
            let config = chatgpt_account_config(&config_dir, "");
            let account = &config.accounts["codex-main"];

            assert!(!sweep_tick_codex_probe_with(
                &binary,
                account,
                0,
                &state_dir,
                mv_core::time::now_utc()
            ));
            assert_eq!(probe_calls(&config_dir), 0);
        }
    }
}

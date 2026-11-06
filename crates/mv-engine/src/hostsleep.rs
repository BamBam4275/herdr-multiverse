//! Host sleep-prevention around long-running work (`pmset` on macOS).
//!
//! This module owns the **gate** (when to bother calling `pmset` at all),
//! the **probe** (an injectable `pmset -g log` subprocess call, card
//! `20261102-b-32b6bb`), the **parser** (log text → Sleep/Wake intervals),
//! and — via [`report`]/[`report_with`] — the orchestration that composes
//! all three with `mv_state::hostsleep_store`'s marker/pending-queue
//! persistence. What is still missing is the periodic call site: no
//! `commands::sweep` edit is in this task's write scope (that file belongs
//! to another running lane), so [`report`]'s own doc comment names the
//! one line still needed there instead of this task adding it; see this
//! task's `result.md`.
//!
//! **Confidence note:** the exact `pmset -g log` date/timezone text format
//! was not independently verified against a captured real log in this task
//! (no fixture available, and `AGENTS.md` forbids reading the frozen
//! Python reference). `parse_line`'s assumed shape (`YYYY-MM-DD
//! HH:MM:SS ±HHMM` then a tab, then event, then a tab, then message) is
//! this module's best-effort reading of `spec-seats-quotas.md`'s
//! `<date> <time> <tz>\t<event>\t<message>` description.
//!
//! [`report`] (card `20261102-b-32b6bb`) is the orchestration this module's
//! own earlier doc note said was still missing: it composes the gate, the
//! probe, the parser, and `mv_state::hostsleep_store`'s marker/queue
//! persistence into the one call `cmd_sweep`'s Rust port needs — see that
//! function's own doc comment for the one-line call site still needed in
//! `commands::sweep` (excluded from this task's write scope; noted in this
//! task's `result.md` instead of edited directly).

use std::path::Path;
use std::time::Duration;

use time::OffsetDateTime;

/// Resolve the token used to invoke `pmset`: `$MULTIVERSE_PMSET_BIN` when
/// set and non-blank, else the bare `pmset` token. Mirrors
/// `HERDR_BIN_PATH`'s own override shape (`mv_herdr::argv::herdr_binary`)
/// so a conformance scenario can substitute a scripted fake for the real
/// macOS-only binary, and assert on `check`/`doctor`'s idle-sleep warning
/// line directly instead of `normalize.py` filtering it out of every
/// comparison (card `20261102-b-32b6bb`).
pub fn pmset_binary() -> String {
    resolve_pmset_binary(std::env::var("MULTIVERSE_PMSET_BIN").ok())
}

fn resolve_pmset_binary(raw: Option<String>) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value,
        _ => "pmset".to_string(),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PmsetError {
    #[error("pmset probe failed: {0}")]
    Probe(#[from] mv_herdr::error::HerdrError),
    #[error("pmset -g log exited with status {0}")]
    Exit(i32),
}

/// Run `pmset -g log` through [`pmset_binary`]'s injectable seam, returning
/// its raw stdout for [`parse_intervals`] to read. Bounded to a short
/// synchronous timeout, the same shape as `dockerreport::collect_with`'s
/// probe.
pub fn run_pmset_log() -> Result<String, PmsetError> {
    run_pmset_log_with(&pmset_binary())
}

/// Same as [`run_pmset_log`], but against an explicit binary — the seam a
/// test or conformance scenario calls directly with a scripted fake.
pub fn run_pmset_log_with(binary: &str) -> Result<String, PmsetError> {
    let command = mv_herdr::argv::HerdrCommand {
        argv: vec![binary.to_string(), "-g".into(), "log".into()],
        purpose: "read the pmset sleep/wake log".into(),
        stdin: None,
        expects_json: false, // Plain pmset text, not a Herdr envelope.
    };
    let output = mv_herdr::run::run(&command, Some(Duration::from_secs(10)))?;
    if output.returncode != 0 {
        return Err(PmsetError::Exit(output.returncode));
    }
    Ok(output.stdout)
}

pub const MIN_INTERVAL_MINUTES: i64 = 5;
pub const MAX_LOG_LINES: usize = 20_000;
pub const SWEEP_GAP_MINUTES: i64 = 15;
pub const PMSET_STALE_MINUTES: i64 = 30;

/// One `Sleep` → `Wake` interval. `lower_bound` is `true` when the opening
/// `Sleep` line fell outside the capped log window: reported as "slept
/// from before `end`" rather than silently dropped, so `start` is `None`
/// in that case. `power` is the closing `Wake` line's own power source —
/// `"AC"`, `"battery"`, or `"unknown"` (`power_source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SleepInterval {
    pub start: Option<OffsetDateTime>,
    pub end: OffsetDateTime,
    pub lower_bound: bool,
    pub power: String,
}

/// `AC`, `battery`, or `unknown` from a closing `Wake` line's own message
/// (`hostsleep.py::_power_source`).
pub fn power_source(message: &str) -> &'static str {
    if message.contains("Using AC") {
        "AC"
    } else if message.contains("Using BATT") {
        "battery"
    } else {
        "unknown"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Sleep,
    Wake,
    DarkWake,
    Other,
}

fn classify_event(event: &str) -> Event {
    let normalized = event.trim().to_ascii_lowercase();
    if normalized.contains("darkwake") {
        Event::DarkWake
    } else if normalized.contains("wake") {
        Event::Wake
    } else if normalized.starts_with("sleep") {
        Event::Sleep
    } else {
        Event::Other
    }
}

/// Split one `pmset -g log` line into its timestamp, event name, and
/// message: `<date> <time> <tz>\t<event>\t<message>`.
fn parse_line(line: &str) -> Option<(OffsetDateTime, &str, &str)> {
    let mut tab_parts = line.splitn(3, '\t');
    let timestamp_part = tab_parts.next()?.trim();
    let event = tab_parts.next()?.trim();
    let message = tab_parts.next().unwrap_or("").trim();
    let instant = parse_pmset_timestamp(timestamp_part)?;
    Some((instant, event, message))
}

/// The first parsed timestamp in `log_text`, regardless of event type —
/// the earliest instant the [`MAX_LOG_LINES`] cap kept. Used to tell a
/// capped window (its earliest line postdates the sweep watermark) from
/// one that already reaches back far enough that nothing relevant could
/// have been cut off (`hostsleep.py::report`'s own `earliest`).
pub fn earliest_timestamp(log_text: &str) -> Option<OffsetDateTime> {
    log_text
        .lines()
        .take(MAX_LOG_LINES)
        .find_map(|line| parse_line(line).map(|(instant, _, _)| instant))
}

fn parse_pmset_timestamp(segment: &str) -> Option<OffsetDateTime> {
    let mut parts = segment.split_whitespace();
    let date = parts.next()?;
    let time_part = parts.next()?;
    let tz = parts.next()?;

    let mut date_fields = date.split('-');
    let year: i32 = date_fields.next()?.parse().ok()?;
    let month: u8 = date_fields.next()?.parse().ok()?;
    let day: u8 = date_fields.next()?.parse().ok()?;

    let mut time_fields = time_part.split(':');
    let hour: u8 = time_fields.next()?.parse().ok()?;
    let minute: u8 = time_fields.next()?.parse().ok()?;
    let second: u8 = time_fields.next()?.parse().ok()?;

    let negative = tz.starts_with('-');
    let digits = tz.trim_start_matches(['+', '-']);
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let tz_hour: i8 = digits[0..2].parse().ok()?;
    let tz_minute: i8 = digits[2..4].parse().ok()?;
    let sign: i8 = if negative { -1 } else { 1 };
    let offset = time::UtcOffset::from_hms(sign * tz_hour, sign * tz_minute, 0).ok()?;

    let date =
        time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day).ok()?;
    let time = time::Time::from_hms(hour, minute, second).ok()?;
    Some(time::PrimitiveDateTime::new(date, time).assume_offset(offset))
}

/// Build Sleep→Wake intervals from already-capped log text (callers pass at
/// most the last [`MAX_LOG_LINES`] lines). A bare `DarkWake`, or a second
/// `Sleep` while one is already open, never opens/closes an interval on its
/// own. Anything under [`MIN_INTERVAL_MINUTES`] is dropped. A `Wake` with
/// no matching prior `Sleep` in this text means the sleep started before
/// the window began — reported as a lower-bound interval rather than
/// silently dropped.
pub fn parse_intervals(log_text: &str) -> Vec<SleepInterval> {
    let mut open: Option<OffsetDateTime> = None;
    let mut out = Vec::new();

    for line in log_text.lines().take(MAX_LOG_LINES) {
        let Some((instant, event, message)) = parse_line(line) else {
            continue;
        };
        match classify_event(event) {
            Event::Sleep => {
                if open.is_none() {
                    open = Some(instant);
                }
                // A second Sleep while one is already open is ignored;
                // the first opening instant is kept.
            }
            Event::Wake => match open.take() {
                Some(start) => {
                    let minutes = (instant - start).whole_minutes();
                    if minutes >= MIN_INTERVAL_MINUTES {
                        out.push(SleepInterval {
                            start: Some(start),
                            end: instant,
                            lower_bound: false,
                            power: power_source(message).to_string(),
                        });
                    }
                }
                None => out.push(SleepInterval {
                    start: None,
                    end: instant,
                    lower_bound: true,
                    power: power_source(message).to_string(),
                }),
            },
            Event::DarkWake | Event::Other => {}
        }
    }

    out
}

/// Total known sleep minutes across `intervals`. A lower-bound interval's
/// true duration is unknown (its start fell outside the parsed window), so
/// it contributes `0` rather than a guess — this undercounts rather than
/// risks an inflated figure feeding a stuck-task/expiry calculation.
pub fn total_sleep_minutes(intervals: &[SleepInterval]) -> i64 {
    intervals
        .iter()
        .filter_map(|interval| {
            let start = interval.start?;
            Some((interval.end - start).whole_minutes().max(0))
        })
        .sum()
}

/// Whether to bother running `pmset -g log` at all: only when the gap since
/// the last watermark exceeds [`SWEEP_GAP_MINUTES`], or the gap since
/// `pmset` was last actually invoked exceeds [`PMSET_STALE_MINUTES`].
pub fn should_run_pmset(
    last_sweep: Option<OffsetDateTime>,
    last_pmset: Option<OffsetDateTime>,
    now: OffsetDateTime,
) -> bool {
    let sweep_gap = last_sweep.map_or(i64::MAX, |t| (now - t).whole_minutes());
    let pmset_gap = last_pmset.map_or(i64::MAX, |t| (now - t).whole_minutes());
    sweep_gap > SWEEP_GAP_MINUTES || pmset_gap > PMSET_STALE_MINUTES
}

/// Advance a watermark monotonically: `max(since, now)`, never regressing
/// past the previous watermark even if the wall clock steps backward (an
/// NTP correction).
pub fn advance_watermark(
    previous: Option<OffsetDateTime>,
    since: OffsetDateTime,
    now: OffsetDateTime,
) -> OffsetDateTime {
    let candidate = since.max(now);
    match previous {
        Some(prev) => candidate.max(prev),
        None => candidate,
    }
}

/// One `report()` call's result: the printable lines, whether a scan
/// actually ran (`authoritative` — `false` when the gate blocked the call
/// or the probe failed), and whether it was durably queued for a
/// notification pass to consume (`persisted` — only ever `true` when the
/// caller passed `apply`). Mirrors `hostsleep.py`'s `SleepReport`, a
/// `list[str]` subclass there; a plain struct here, since Rust has no
/// listlike-subclass idiom worth reaching for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SleepReport {
    pub lines: Vec<String>,
    pub authoritative: bool,
    pub persisted: bool,
}

/// On the very first call for a project — no stored watermark yet — how
/// far back to look (`hostsleep.py::FIRST_RUN_LOOKBACK`, 2 hours).
pub const FIRST_RUN_LOOKBACK_MINUTES: i64 = 120;

fn format_instant(instant: OffsetDateTime) -> String {
    let instant = instant.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        instant.year(),
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second(),
    )
}

/// The one orchestrating call a sweep makes once per tick: gate, probe,
/// parse, format, and (when `apply`) persist. Calls [`run_pmset_log`] for
/// the probe; see [`report_with`] for the injectable-probe version every
/// test in this module uses instead, so no test ever shells out for real.
///
/// **The one-line call site this task could not add.** `commands::sweep`
/// is excluded from this task's write scope (another lane owns it — see
/// this task's `result.md`). Its owner should call this once per sweep
/// tick, after the Docker housekeeping block and before the per-project
/// loop (`hostsleep.py`'s own "host-wide, not per-project" placement),
/// against the `multiverse` hub project's own root — skipping the call
/// entirely when that project is not configured, the same "nowhere to
/// keep state" posture this function's own callers already take for other
/// unanswerable gaps — then print every line in the returned
/// [`SleepReport`] the same way the Docker block's own lines are printed.
/// This call must never affect `cmd_sweep`'s own exit code, matching
/// `hostsleep.py`'s "a missing `pmset` binary ... collapses this block to
/// nothing at all" contract:
///
/// ```ignore
/// let report = mv_engine::hostsleep::report(&hub_project.path, OffsetDateTime::now_utc(), apply);
/// for line in &report.lines {
///     println!("{line}");
/// }
/// ```
pub fn report(project_root: &Path, now: OffsetDateTime, apply: bool) -> SleepReport {
    report_with(project_root, now, apply, run_pmset_log)
}

/// [`report`], but with the `pmset -g log` probe replaced by `log_fn` — the
/// seam every test in this module calls instead of [`report`] itself.
/// `Err` from `log_fn` folds to the same "say nothing, touch nothing" outcome
/// as a missing binary or non-zero exit already does in
/// `hostsleep.py::_pmset_log`.
pub fn report_with(
    project_root: &Path,
    now: OffsetDateTime,
    apply: bool,
    log_fn: impl FnOnce() -> Result<String, PmsetError>,
) -> SleepReport {
    let now = now.to_offset(time::UtcOffset::UTC);

    // Non-blocking, like `hostsleep.py::_sweep_lock`: a call that loses the
    // race to an overlapping one returns empty and leaves every marker
    // untouched, rather than both calls reading the same stale watermark.
    let lock_path = mv_state::hostsleep_store::lock_path(project_root);
    let Ok(_guard) = mv_core::lock::acquire(&lock_path, "hostsleep", Duration::ZERO) else {
        return SleepReport::default();
    };

    let since = mv_state::hostsleep_store::read_timestamp(
        project_root,
        mv_state::hostsleep_store::STATE_FILENAME,
    );
    let last_pmset_run = mv_state::hostsleep_store::read_timestamp(
        project_root,
        mv_state::hostsleep_store::PMSET_STATE_FILENAME,
    );
    if !should_run_pmset(since, last_pmset_run, now) {
        return SleepReport::default();
    }
    let since = since.unwrap_or_else(|| now - time::Duration::minutes(FIRST_RUN_LOOKBACK_MINUTES));

    let Ok(log_text) = log_fn() else {
        return SleepReport::default();
    };

    let earliest = earliest_timestamp(&log_text);
    let mut lines = Vec::new();
    for interval in parse_intervals(&log_text) {
        match interval.start {
            None => {
                // The opening `Sleep` fell outside the capped window. If
                // the window's own earliest line already reaches back to
                // or before `since`, the real start is in already-reported
                // territory; otherwise the window was cut short of
                // `since` and this sleep would otherwise vanish rather
                // than merely go unreported until the next tick.
                let Some(earliest) = earliest else { continue };
                if earliest <= since {
                    continue;
                }
                let minutes = (interval.end - earliest).whole_minutes();
                if minutes < MIN_INTERVAL_MINUTES {
                    continue;
                }
                lines.push(format!(
                    "host: slept from before {}Z-{}Z ({minutes}+ min, {})",
                    format_instant(earliest),
                    format_instant(interval.end),
                    interval.power,
                ));
            }
            Some(start) => {
                if start <= since {
                    continue;
                }
                let minutes = (interval.end - start).whole_minutes();
                lines.push(format!(
                    "host: slept {}Z-{}Z ({minutes} min, {})",
                    format_instant(start),
                    format_instant(interval.end),
                    interval.power,
                ));
            }
        }
    }

    // A dry run (`apply == false`) reports without moving the window, or
    // the next real sweep would never mention the sleeps this preview
    // already showed.
    let persisted = if apply {
        // Publish before advancing the watermark: another full sweep may
        // win notify's lock after this one has already consumed the log.
        let queued = mv_state::hostsleep_store::queue_pending(project_root, lines.clone()).is_ok();
        let _ = mv_state::hostsleep_store::write_timestamp(
            project_root,
            mv_state::hostsleep_store::STATE_FILENAME,
            advance_watermark(None, since, now),
        );
        let _ = mv_state::hostsleep_store::write_timestamp(
            project_root,
            mv_state::hostsleep_store::PMSET_STATE_FILENAME,
            advance_watermark(last_pmset_run, now, now),
        );
        queued
    } else {
        false
    };

    SleepReport {
        lines,
        authoritative: true,
        persisted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn line(date_time_tz: &str, event: &str, message: &str) -> String {
        format!("{date_time_tz}\t{event}\t{message}")
    }

    #[test]
    fn pmset_binary_honors_the_env_override() {
        assert_eq!(
            resolve_pmset_binary(Some("/path/to/fake-pmset".to_string())),
            "/path/to/fake-pmset"
        );
    }

    #[test]
    fn pmset_binary_falls_back_to_the_bare_token_when_unset_or_blank() {
        assert_eq!(resolve_pmset_binary(Some(String::new())), "pmset");
        assert_eq!(resolve_pmset_binary(Some("   ".to_string())), "pmset");
        assert_eq!(resolve_pmset_binary(None), "pmset");
    }

    #[cfg(unix)]
    #[test]
    fn isolated_probe_runs_only_the_injected_binary_and_propagates_failure() {
        use crate::directives::tests::TempDir;
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let binary = dir.0.join("fake-pmset");
        std::fs::write(
            &binary,
            "#!/bin/sh\n[ \"$#\" -eq 2 ] || exit 9\n[ \"$1\" = -g ] || exit 9\n[ \"$2\" = log ] || exit 9\nprintf '%s\\n' '2026-11-02 01:00:00 +0000\\tSleep\\tstart'\n",
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = run_pmset_log_with(&binary.to_string_lossy()).unwrap();
        assert!(stdout.contains("Sleep"));

        std::fs::write(&binary, "#!/bin/sh\nexit 7\n").unwrap();
        assert!(matches!(
            run_pmset_log_with(&binary.to_string_lossy()),
            Err(PmsetError::Exit(7))
        ));
        assert!(run_pmset_log_with(&dir.0.join("missing").to_string_lossy()).is_err());
    }

    #[test]
    fn parses_a_sleep_wake_pair_into_one_interval() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 07:00:00 +0000", "Wake", "end"),
        ]
        .join("\n");
        let intervals = parse_intervals(&log);
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].start, Some(datetime!(2026-11-02 01:00:00 UTC)));
        assert_eq!(intervals[0].end, datetime!(2026-11-02 07:00:00 UTC));
        assert!(!intervals[0].lower_bound);
    }

    #[test]
    fn intervals_under_the_minimum_are_dropped() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 01:02:00 +0000", "Wake", "end"),
        ]
        .join("\n");
        assert!(parse_intervals(&log).is_empty());
    }

    #[test]
    fn a_bare_darkwake_never_opens_or_closes_an_interval() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "DarkWake", "x"),
            line("2026-11-02 02:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 08:00:00 +0000", "Wake", "end"),
        ]
        .join("\n");
        let intervals = parse_intervals(&log);
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].start, Some(datetime!(2026-11-02 02:00:00 UTC)));
    }

    #[test]
    fn a_second_sleep_while_one_is_open_keeps_the_first() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 01:30:00 +0000", "Sleep", "redundant"),
            line("2026-11-02 08:00:00 +0000", "Wake", "end"),
        ]
        .join("\n");
        let intervals = parse_intervals(&log);
        assert_eq!(intervals.len(), 1);
        assert_eq!(intervals[0].start, Some(datetime!(2026-11-02 01:00:00 UTC)));
    }

    #[test]
    fn a_wake_with_no_prior_sleep_is_a_lower_bound_not_dropped() {
        let log = line("2026-11-02 08:00:00 +0000", "Wake", "end");
        let intervals = parse_intervals(&log);
        assert_eq!(intervals.len(), 1);
        assert!(intervals[0].lower_bound);
        assert_eq!(intervals[0].start, None);
        assert_eq!(intervals[0].end, datetime!(2026-11-02 08:00:00 UTC));
    }

    #[test]
    fn total_sleep_minutes_sums_known_duration_intervals() {
        let intervals = vec![
            SleepInterval {
                start: Some(datetime!(2026-11-02 01:00:00 UTC)),
                end: datetime!(2026-11-02 02:00:00 UTC),
                lower_bound: false,
                power: "AC".to_string(),
            },
            SleepInterval {
                start: None,
                end: datetime!(2026-11-02 03:00:00 UTC),
                lower_bound: true,
                power: "unknown".to_string(),
            },
        ];
        // 60 minutes known + 0 for the unknown-duration lower bound.
        assert_eq!(total_sleep_minutes(&intervals), 60);
    }

    #[test]
    fn should_run_pmset_gates_on_either_marker_being_stale() {
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert!(should_run_pmset(None, None, now));
        let fresh_sweep = now - time::Duration::minutes(1);
        let fresh_pmset = now - time::Duration::minutes(1);
        assert!(!should_run_pmset(Some(fresh_sweep), Some(fresh_pmset), now));
        let stale_sweep = now - time::Duration::minutes(SWEEP_GAP_MINUTES + 1);
        assert!(should_run_pmset(Some(stale_sweep), Some(fresh_pmset), now));
        let stale_pmset = now - time::Duration::minutes(PMSET_STALE_MINUTES + 1);
        assert!(should_run_pmset(Some(fresh_sweep), Some(stale_pmset), now));
    }

    #[test]
    fn watermark_never_regresses_on_a_backward_clock_step() {
        let prev = datetime!(2026-11-02 12:00:00 UTC);
        let backward_now = datetime!(2026-11-02 11:00:00 UTC);
        let advanced = advance_watermark(Some(prev), backward_now, backward_now);
        assert_eq!(advanced, prev);
    }

    #[test]
    fn watermark_advances_to_the_later_of_since_and_now() {
        let since = datetime!(2026-11-02 11:00:00 UTC);
        let now = datetime!(2026-11-02 12:00:00 UTC);
        assert_eq!(advance_watermark(None, since, now), now);
    }

    #[test]
    fn garbage_lines_are_skipped_without_panicking() {
        let log = ["not a pmset line at all", "also garbage\twith\ttabs"].join("\n");
        assert!(parse_intervals(&log).is_empty());
    }

    #[test]
    fn power_source_reads_ac_or_batt_from_the_wake_message_else_unknown() {
        assert_eq!(power_source("DarkWake to FullWake Using AC"), "AC");
        assert_eq!(power_source("Wake Using BATT"), "battery");
        assert_eq!(power_source("Wake"), "unknown");
    }

    #[test]
    fn closing_wake_messages_set_each_intervals_power_source() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 07:00:00 +0000", "Wake", "Using BATT"),
        ]
        .join("\n");
        let intervals = parse_intervals(&log);
        assert_eq!(intervals[0].power, "battery");
    }

    #[test]
    fn earliest_timestamp_is_the_first_parsed_line_regardless_of_event() {
        let log = [
            line("2026-11-02 01:00:00 +0000", "DarkWake", "x"),
            line("2026-11-02 02:00:00 +0000", "Sleep", "start"),
        ]
        .join("\n");
        assert_eq!(
            earliest_timestamp(&log),
            Some(datetime!(2026-11-02 01:00:00 UTC))
        );
        assert_eq!(earliest_timestamp("garbage"), None);
    }

    fn temp_project(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mv-engine-hostsleep-report-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn report_with_runs_on_first_call_and_reports_an_interval_past_the_lookback() {
        let root = temp_project("first-call");
        // No stored watermark: the 2-hour first-run lookback puts `since`
        // at 2026-11-02 05:30:00 UTC, before this interval's own start.
        let now = datetime!(2026-11-02 07:30:00 UTC);
        let log = [
            line("2026-11-02 06:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 07:00:00 +0000", "Wake", "Using AC"),
        ]
        .join("\n");
        let report = report_with(&root, now, true, || Ok(log.clone()));
        assert!(report.authoritative);
        assert!(report.persisted);
        assert_eq!(
            report.lines,
            vec!["host: slept 2026-11-02T06:00:00Z-2026-11-02T07:00:00Z (60 min, AC)"]
        );
        assert_eq!(
            mv_state::hostsleep_store::read_timestamp(
                &root,
                mv_state::hostsleep_store::STATE_FILENAME
            ),
            Some(now)
        );
        assert_eq!(
            mv_state::hostsleep_store::read_pending(&root).unwrap(),
            vec![report.lines]
        );
    }

    #[test]
    fn report_with_skips_an_interval_already_covered_by_the_watermark() {
        let root = temp_project("already-covered");
        let since = datetime!(2026-11-02 08:00:00 UTC);
        mv_state::hostsleep_store::write_timestamp(
            &root,
            mv_state::hostsleep_store::STATE_FILENAME,
            since,
        )
        .unwrap();
        // Stale enough for the gate to still call pmset, but the interval
        // itself started before `since` and must not be re-reported.
        let now = since + time::Duration::minutes(SWEEP_GAP_MINUTES + 1);
        let log = [
            line("2026-11-02 01:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 07:00:00 +0000", "Wake", "Using AC"),
        ]
        .join("\n");
        let report = report_with(&root, now, true, || Ok(log.clone()));
        assert!(report.lines.is_empty());
    }

    #[test]
    fn report_with_skips_the_probe_entirely_while_both_markers_are_fresh() {
        let root = temp_project("gated");
        let now = datetime!(2026-11-02 12:00:00 UTC);
        mv_state::hostsleep_store::write_timestamp(
            &root,
            mv_state::hostsleep_store::STATE_FILENAME,
            now,
        )
        .unwrap();
        mv_state::hostsleep_store::write_timestamp(
            &root,
            mv_state::hostsleep_store::PMSET_STATE_FILENAME,
            now,
        )
        .unwrap();
        let report = report_with(&root, now, true, || {
            panic!("the gate should never call the probe here")
        });
        assert_eq!(report, SleepReport::default());
    }

    #[test]
    fn report_with_a_failed_probe_leaves_every_marker_untouched() {
        let root = temp_project("probe-failed");
        let now = datetime!(2026-11-02 12:00:00 UTC);
        let report = report_with(&root, now, true, || Err(PmsetError::Exit(1)));
        assert_eq!(report, SleepReport::default());
        assert_eq!(
            mv_state::hostsleep_store::read_timestamp(
                &root,
                mv_state::hostsleep_store::STATE_FILENAME
            ),
            None
        );
    }

    #[test]
    fn report_with_a_dry_run_reports_without_persisting() {
        let root = temp_project("dry-run");
        // No stored watermark: the 2-hour first-run lookback puts `since`
        // at 2026-11-02 05:30:00 UTC, before this interval's own start.
        let now = datetime!(2026-11-02 07:30:00 UTC);
        let log = [
            line("2026-11-02 06:00:00 +0000", "Sleep", "start"),
            line("2026-11-02 07:00:00 +0000", "Wake", "Using AC"),
        ]
        .join("\n");
        let report = report_with(&root, now, false, || Ok(log.clone()));
        assert!(report.authoritative);
        assert!(!report.persisted);
        assert!(!report.lines.is_empty());
        assert_eq!(
            mv_state::hostsleep_store::read_timestamp(
                &root,
                mv_state::hostsleep_store::STATE_FILENAME
            ),
            None
        );
        assert!(mv_state::hostsleep_store::read_pending(&root)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn report_with_reports_a_capped_window_as_a_lower_bound() {
        let root = temp_project("lower-bound");
        // First-run lookback puts `since` at 2026-11-02 10:00:00 UTC.
        let now = datetime!(2026-11-02 12:00:00 UTC);
        // No opening Sleep in this window: the real sleep began before it,
        // at or before the window's own earliest (non-Sleep) line.
        let log = [
            line("2026-11-02 11:00:00 +0000", "DarkWake", "x"),
            line("2026-11-02 11:30:00 +0000", "Wake", "Using BATT"),
        ]
        .join("\n");
        let report = report_with(&root, now, true, || Ok(log.clone()));
        assert_eq!(
            report.lines,
            vec!["host: slept from before 2026-11-02T11:00:00Z-2026-11-02T11:30:00Z (30+ min, battery)"]
        );
    }
}

//! `presence [--json]`: report operator presence from macOS's HID idle
//! timer.
//!
//! Ports Python's `_hid_idle_seconds`/`cmd_presence` (`cli.py` ~11072):
//! shells out to `ioreg -c IOHIDSystem`, reads the `"HIDIdleTime"`
//! nanosecond counter, and reports active/away/unknown with exit 0/1/2.
//! Any probe failure (missing binary, non-zero exit, timeout, no match)
//! reports `unknown`/exit 2 — the same value a macOS probe failure
//! reports, so a non-macOS host is simply a probe that always "fails"
//! (`architecture.md` §5: "`2` … Usage error, or an inconclusive
//! presence check").

use clap::ArgMatches;

use crate::exit::{Failure, ERROR, OK, UNKNOWN};

/// Python's `PRESENCE_AWAY_AFTER_SECONDS` (`cli.py` ~10949): five minutes.
const PRESENCE_AWAY_AFTER_SECONDS: f64 = 5.0 * 60.0;

/// Python's `HID_IDLE_PROBE_TIMEOUT_SECONDS` (`cli.py` ~10948).
#[cfg(target_os = "macos")]
const HID_IDLE_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Pull the nanosecond `"HIDIdleTime"` reading out of `ioreg -c
/// IOHIDSystem`'s stdout, mirroring Python's
/// `re.search(r'"HIDIdleTime"\s*=\s*(\d+)', result.stdout)`: the first
/// match, any amount of whitespace around `=`, digits only.
fn parse_hid_idle_nanos(stdout: &str) -> Option<u64> {
    const KEY: &str = "\"HIDIdleTime\"";
    let after_key = &stdout[stdout.find(KEY)? + KEY.len()..];
    let after_eq = after_key.trim_start().strip_prefix('=')?;
    let digits: String = after_eq
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// Run `ioreg -c IOHIDSystem` with a bounded timeout and return its
/// stdout, or `None` on any spawn error, non-zero exit, or timeout —
/// mirroring Python's `runner.run` catching `DispatchError`/`OSError`
/// and `cmd_presence` treating a non-`ok` result as a failed probe.
#[cfg(target_os = "macos")]
fn run_ioreg() -> Option<String> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Instant;

    // Matches `mv-herdr::run`'s poll loop: `try_wait` never blocks, so
    // the same thread can both watch for exit and enforce the timeout,
    // while a second thread drains stdout concurrently so a large dump
    // can't fill the pipe and deadlock the child.
    const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

    let mut child = Command::new("ioreg")
        .args(["-c", "IOHIDSystem"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout_pipe = child.stdout.take()?;
    let stdout_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if start.elapsed() >= HID_IDLE_PROBE_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break None,
        }
    };

    status.filter(|status| status.success())?;
    let stdout_bytes = stdout_handle.join().ok()?;
    Some(String::from_utf8_lossy(&stdout_bytes).into_owned())
}

/// Card `20261103-b-15d3ca`: `pub(super)` (was crate-private) so
/// `commands::sweep`'s own picker-alert gate — Python's `_mike_is_idle`,
/// consulted at `_report_blocked_picker`'s one-minute threshold rather than
/// this command's own five-minute `PRESENCE_AWAY_AFTER_SECONDS` — can reuse
/// this exact probe instead of shelling out to `ioreg` a second way.
#[cfg(target_os = "macos")]
pub(crate) fn hid_idle_seconds() -> Option<f64> {
    let nanos = parse_hid_idle_nanos(&run_ioreg()?)?;
    Some(nanos as f64 / 1_000_000_000.0)
}

/// No HID idle probe exists off macOS; a probe that cannot run is
/// exactly as "failed" as one that ran and found nothing.
#[cfg(not(target_os = "macos"))]
pub(crate) fn hid_idle_seconds() -> Option<f64> {
    None
}

/// Render `cmd_presence`'s report for a given idle reading, byte-for-byte
/// matching Python's `json.dumps({...}, sort_keys=True)` and
/// `f"Mike HID idle: {idle} seconds ({status})"`. Pulled out of `run` so
/// every branch (unknown/active/away, json/text) is testable without a
/// real HID probe.
fn render(idle_seconds: Option<f64>, json: bool) -> (String, u8) {
    let (status, code) = match idle_seconds {
        None => ("unknown", UNKNOWN),
        Some(seconds) if seconds >= PRESENCE_AWAY_AFTER_SECONDS => ("away", ERROR),
        Some(_) => ("active", OK),
    };
    let text = if json {
        let idle = idle_seconds
            .map(|seconds| format!("{seconds:?}"))
            .unwrap_or_else(|| "null".to_string());
        format!("{{\"idle_seconds\": {idle}, \"status\": \"{status}\"}}")
    } else {
        let idle = idle_seconds
            .map(|seconds| format!("{seconds:.1}"))
            .unwrap_or_else(|| "unknown".to_string());
        format!("Mike HID idle: {idle} seconds ({status})")
    };
    (text, code)
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    let (text, code) = render(hid_idle_seconds(), args.get_flag("json"));
    println!("{text}");
    if code == OK {
        Ok(())
    } else {
        Err(Failure::Reported(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::command;

    fn matches(words: &[&str]) -> ArgMatches {
        let m = command()
            .try_get_matches_from(std::iter::once("multiverse").chain(words.iter().copied()))
            .unwrap();
        m.subcommand_matches("presence").unwrap().clone()
    }

    #[test]
    fn parses_the_first_hid_idle_time_regardless_of_surrounding_whitespace() {
        for (stdout, expected) in [
            (
                r#"    | | |   "HIDIdleTime" = 1464917385250"#,
                1464917385250,
            ),
            (r#""HIDIdleTime"=0"#, 0),
            (r#""HIDIdleTime"   =   42"#, 42),
            (
                "junk before\n\"HIDIdleTime\" = 7\nother = \"HIDIdleTime\" = 999",
                7,
            ),
        ] {
            assert_eq!(parse_hid_idle_nanos(stdout), Some(expected), "{stdout:?}");
        }
    }

    #[test]
    fn missing_or_malformed_hid_idle_time_is_not_a_match() {
        for stdout in ["", "no such key here", "\"HIDIdleTime\" = \"not a number\""] {
            assert_eq!(parse_hid_idle_nanos(stdout), None, "{stdout:?}");
        }
    }

    #[test]
    fn render_reports_unknown_exit_2_with_no_probe_reading() {
        let (json, code) = render(None, true);
        assert_eq!(json, r#"{"idle_seconds": null, "status": "unknown"}"#);
        assert_eq!(code, UNKNOWN);

        let (text, code) = render(None, false);
        assert_eq!(text, "Mike HID idle: unknown seconds (unknown)");
        assert_eq!(code, UNKNOWN);
    }

    #[test]
    fn render_reports_active_below_the_away_threshold() {
        let (json, code) = render(Some(125.0), true);
        assert_eq!(json, r#"{"idle_seconds": 125.0, "status": "active"}"#);
        assert_eq!(code, OK);

        let (text, code) = render(Some(125.5), false);
        assert_eq!(text, "Mike HID idle: 125.5 seconds (active)");
        assert_eq!(code, OK);
    }

    #[test]
    fn render_reports_away_at_and_above_the_five_minute_threshold() {
        // Python's `_mike_is_idle` uses `>=`, so the boundary itself is away.
        let (json, code) = render(Some(PRESENCE_AWAY_AFTER_SECONDS), true);
        assert_eq!(json, r#"{"idle_seconds": 300.0, "status": "away"}"#);
        assert_eq!(code, ERROR);

        let (text, code) = render(Some(301.25), false);
        assert_eq!(text, "Mike HID idle: 301.2 seconds (away)");
        assert_eq!(code, ERROR);
    }

    #[test]
    fn run_s_exit_code_always_matches_one_of_the_three_documented_outcomes() {
        // `render`'s own tests pin the exact printed text for each reading;
        // this only checks `run`'s plumbing (args -> probe -> exit code),
        // since the live idle reading (and so active-vs-away) can't be
        // pinned from a test.
        for row in [&["presence"][..], &["presence", "--json"]] {
            match run(&matches(row)) {
                Ok(()) => {}
                Err(err @ Failure::Reported(code)) => {
                    assert!(matches!(code, ERROR | UNKNOWN), "{row:?}: {code}");
                    assert_eq!(err.to_string(), "");
                }
                Err(other) => panic!("{row:?}: unexpected failure variant {other:?}"),
            }
        }
    }

    // Regression test for card 20261102-b-e6835b: the Rust `presence`
    // command used to never probe HID idle at all and always answered
    // `unknown`/exit 2. On macOS (this crate's only real target for the
    // probe) a working `ioreg` must now produce a real reading.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_probe_actually_reads_hid_idle_time() {
        assert!(
            hid_idle_seconds().is_some(),
            "expected a real HID idle reading from ioreg, got none"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn non_macos_has_no_probe_and_always_reports_unknown() {
        assert_eq!(hid_idle_seconds(), None);
    }
}

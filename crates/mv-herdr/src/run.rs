//! Subprocess execution against `$HERDR_BIN_PATH`: pane/agent reads
//! tailed at 2 MiB, every other command's stdout and all stderr capped
//! at 8 MiB with a typed error past it, timeouts and a missing binary
//! mapped to typed errors, never `sh -c`.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::argv::{
    herdr_binary, HerdrCommand, DEFAULT_TIMEOUT_SECONDS, MAX_OUTPUT_BYTES, MAX_PANE_READ_BYTES,
};
use crate::envelope::{try_json_object, CommandResult, EXIT_OK};
use crate::error::HerdrError;

/// How often the wait loop polls the child for exit, while still being
/// responsive enough that a sub-30ms test timeout is observed promptly.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Whether the workspace `.cargo/config.toml` (forced on for every
/// `cargo build`/`test`/`run`, card `20261103-b-1da16f`) forbids falling
/// back to a real `herdr` on PATH. Read fresh on every call, matching
/// `herdr_binary()`'s own "never cached" rule.
fn forbids_real_herdr() -> bool {
    forbids_real_herdr_from(std::env::var("MV_TEST_FORBID_REAL_HERDR").ok().as_deref())
}

fn forbids_real_herdr_from(raw: Option<&str>) -> bool {
    raw.is_some_and(|value| !value.trim().is_empty() && value.trim() != "0")
}

/// Keep the newest complete UTF-8 text within `max_bytes`.
fn tail_text(text: &str, max_bytes: usize) -> String {
    let bytes = text.as_bytes();
    if bytes.len() <= max_bytes {
        return text.to_string();
    }
    let start = bytes.len() - max_bytes;
    let mut boundary = start;
    while boundary < bytes.len() && !text.is_char_boundary(boundary) {
        boundary += 1;
    }
    text[boundary..].to_string()
}

/// Read `pipe`, keeping at most `max_bytes` of the newest data in memory.
/// Pane/agent reads must not `read_to_end` a huge transcript and only
/// tail afterwards — that still O(n)s the whole buffer. Only a tailed
/// read uses this: it is fine to silently drop the oldest bytes because
/// the caller only ever wants the latest pane state, never the full
/// transcript.
fn read_bounded(pipe: &mut impl Read, max_bytes: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > max_bytes {
                    let drain = buf.len() - max_bytes;
                    buf.drain(..drain);
                }
            }
            Err(_) => break,
        }
    }
    buf
}

/// Read `pipe` up to `max_bytes`, continuing to drain (and discard) any
/// bytes past the cap so a hostile or runaway child cannot block forever
/// on a full pipe waiting for us to catch up. The second element is
/// `true` once the cap was exceeded. Card `20261103-b-57c876`: JSON
/// listings and stderr must not be silently truncated like a tailed
/// pane/agent read — truncating JSON at an arbitrary byte boundary
/// produces invalid JSON, not a smaller valid one, so `run` turns a
/// `true` here into a typed error instead of risking that.
fn read_capped(pipe: &mut impl Read, max_bytes: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = max_bytes.saturating_sub(buf.len());
                if remaining > 0 {
                    let take = remaining.min(n);
                    buf.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    (buf, truncated)
}

/// Kill the child's process group (the child is started with
/// `process_group(0)`, so its pgid equals its pid) then reap it. Killing
/// only the direct child leaves a `sleep`/pager grandchild running after
/// a timeout; waiting after the group kill is what prevents a zombie.
fn kill_and_reap(child: &mut std::process::Child) {
    let pid = child.id();
    let _ = Command::new("/bin/kill")
        .args(["-KILL", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
    let _ = child.wait();
}

/// Whether `argv[1..3]` names a tailed read: `agent read` or `pane read`.
/// Checked on the argv shape alone, regardless of which binary sits at
/// `argv[0]` — a diagnostic read must never retain an unbounded buffer,
/// herdr binary or not.
fn is_tailed_read(argv: &[String]) -> bool {
    argv.len() >= 3
        && matches!(
            (argv[1].as_str(), argv[2].as_str()),
            ("agent", "read") | ("pane", "read")
        )
}

fn purpose_or_default(command: &HerdrCommand) -> String {
    if command.purpose.is_empty() {
        "herdr command".to_string()
    } else {
        command.purpose.clone()
    }
}

/// The default-timeout decision, pulled out as a pure function so it is
/// testable without waiting on the real 30-second default: Herdr calls
/// get a bounded default when the caller passes none; anything else gets
/// no default at all.
fn effective_timeout(is_herdr: bool, requested: Option<Duration>) -> Option<Duration> {
    requested.or_else(|| {
        if is_herdr {
            Some(Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS))
        } else {
            None
        }
    })
}

fn classify_envelope(payload: &Option<Value>, command: &HerdrCommand) -> Option<String> {
    let purpose = purpose_or_default(command);
    match payload {
        None => Some(format!("{purpose} returned non-JSON output")),
        Some(value) => match value.get("result") {
            None => Some(format!("{purpose} returned JSON without a 'result' field")),
            Some(Value::Object(_)) => None,
            Some(_) => Some(format!(
                "{purpose} returned JSON with a non-object 'result' field"
            )),
        },
    }
}

/// Run one Herdr command and capture its result.
///
/// No shell, ever: the argv vector goes straight to `exec`, so nothing
/// can be word-split or glob-expanded into a different command than the
/// one built, and there is no pipeline whose exit status could mask the
/// binary's own.
///
/// `command.stdin`, when set, is fed to the child exactly as given;
/// `None` behaves as if no stdin were piped at all, rather than
/// inheriting this process's stdin.
///
/// Herdr calls get a bounded default subprocess timeout
/// ([`DEFAULT_TIMEOUT_SECONDS`]) unless `timeout` is given; callers may
/// pass a longer timeout only for a documented slower operation. A
/// missing or non-executable binary becomes [`HerdrError::MissingBinary`]
/// rather than a raw OS error, and an elapsed timeout becomes
/// [`HerdrError::Timeout`]: a hung `herdr agent prompt` must leave a
/// typed trace of its own failure, not a silent hang indistinguishable
/// from a dispatch still in progress. A zero-exit reply that is
/// non-JSON or lacks `result` is *not* raised — it comes back as a
/// `CommandResult` with `protocol_error` set, so a fleet reader cannot
/// mistake it for an empty listing but a caller can still inspect it.
pub fn run(command: &HerdrCommand, timeout: Option<Duration>) -> Result<CommandResult, HerdrError> {
    let is_herdr = command
        .argv
        .first()
        .map(|first| *first == herdr_binary())
        .unwrap_or(false);
    let timeout_budget = effective_timeout(is_herdr, timeout);

    let binary = command.argv.first().cloned().unwrap_or_default();
    if binary == "herdr" && forbids_real_herdr() {
        return Err(HerdrError::RealHerdrForbidden {
            purpose: purpose_or_default(command),
        });
    }
    let mut child_command = Command::new(&binary);
    if command.argv.len() > 1 {
        child_command.args(&command.argv[1..]);
    }
    // Card 20261103-b-1da16f: never let the child — fake or real — see
    // the pane this process is itself running in. A wrapper script that
    // needs its own `HERDR_BIN_PATH`/`MV_FAKE_HERDR_STATE` (the fixtures
    // in `council.rs`/`runner.rs`) re-`export`s them before `exec`, which
    // overrides whatever is stripped here, so this costs those fixtures
    // nothing.
    for (key, _) in std::env::vars() {
        if key.starts_with("HERDR_") {
            child_command.env_remove(key);
        }
    }
    child_command.stdin(if command.stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    child_command.stdout(Stdio::piped());
    child_command.stderr(Stdio::piped());
    // Own process group so a timeout can kill grandchildren (a hung
    // `herdr` that exec'd a pager, or a test stub that `sleep`s) without
    // signalling this process's group.
    let _ = child_command.process_group(0);

    let mut child = child_command
        .spawn()
        .map_err(|source| HerdrError::MissingBinary {
            binary: binary.clone(),
            source,
        })?;

    if let Some(input) = command.stdin.clone() {
        if let Some(mut stdin) = child.stdin.take() {
            thread::spawn(move || {
                let _ = stdin.write_all(input.as_bytes());
            });
        }
    }

    let mut stdout_pipe = child.stdout.take().expect("stdout was piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr was piped");
    let tailed = is_tailed_read(&command.argv);
    let stdout_handle = thread::spawn(move || {
        if tailed {
            (read_bounded(&mut stdout_pipe, MAX_PANE_READ_BYTES), false)
        } else {
            read_capped(&mut stdout_pipe, MAX_OUTPUT_BYTES)
        }
    });
    let stderr_handle = thread::spawn(move || read_capped(&mut stderr_pipe, MAX_OUTPUT_BYTES));

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if let Some(limit) = timeout_budget {
                    if start.elapsed() >= limit {
                        kill_and_reap(&mut child);
                        break None;
                    }
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => {
                if let Some(limit) = timeout_budget {
                    if start.elapsed() >= limit {
                        kill_and_reap(&mut child);
                        break None;
                    }
                }
                break child.wait().ok();
            }
        }
    };

    let (stdout_bytes, stdout_truncated) = stdout_handle.join().unwrap_or_default();
    let (stderr_bytes, stderr_truncated) = stderr_handle.join().unwrap_or_default();

    // A size overrun must win over a timeout: a slow-to-drain pipe under
    // load can let the timeout fire after the cap was already exceeded
    // (`kill_and_reap` only stops the child, it doesn't undo the cap
    // having been hit), and that must still report `OutputTooLarge`, not
    // a `Timeout` that hides the real reason the child never finished.
    if stdout_truncated || stderr_truncated {
        return Err(HerdrError::OutputTooLarge {
            purpose: purpose_or_default(command),
            limit_bytes: MAX_OUTPUT_BYTES,
        });
    }

    let Some(status) = status else {
        return Err(HerdrError::Timeout {
            purpose: purpose_or_default(command),
            timeout_secs: timeout_budget.unwrap_or_default().as_secs_f64(),
        });
    };

    let mut stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    if is_tailed_read(&command.argv) {
        stdout = tail_text(&stdout, MAX_PANE_READ_BYTES);
    }

    let returncode = status.code().unwrap_or(-1);
    let payload = try_json_object(&stdout);

    let protocol_error = if is_herdr && command.expects_json && returncode == EXIT_OK {
        classify_envelope(&payload, command)
    } else {
        None
    };

    Ok(CommandResult {
        argv: command.argv.clone(),
        returncode,
        stdout,
        stderr,
        payload,
        protocol_error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::argv::test_support::HerdrBinPathGuard;
    use crate::argv::{agent_list, agent_read, pane_run};

    fn echo(text: &str) -> HerdrCommand {
        HerdrCommand::new(vec!["/bin/echo".to_string(), text.to_string()])
    }

    /// A timestamp alone collides when two tests race this call within the
    /// same clock tick (observed: concurrent `fake_herdr_printing` callers
    /// landing on the same path, one test's cleanup deleting the script out
    /// from under the other's spawn, surfacing as `MissingBinary` rather
    /// than the error either test was asserting on). The counter guarantees
    /// every call in this process gets a distinct suffix regardless of
    /// clock resolution.
    fn unique_suffix() -> u128 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        (nanos << 32) | u128::from(count)
    }

    fn make_executable(path: &std::path::Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(path, perms).unwrap();
    }

    /// Write a throwaway `#!/bin/sh` script with a literal, trusted body
    /// and return its path. Left on disk for the OS to reap with the
    /// rest of the temp directory; these are single-digit-KB text files
    /// in a handful of tests.
    fn fake_herdr_script(body: &str) -> std::path::PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mv-herdr-test-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        make_executable(&path);
        path
    }

    /// Write `content` to a data file and return a script that `cat`s it
    /// verbatim. Unlike interpolating `content` straight into a shell
    /// script body, this never has to worry about shell-quoting
    /// arbitrary bytes (quotes, backslashes, huge buffers) correctly —
    /// `cat` reproduces the file exactly, with no escaping to get wrong.
    fn fake_herdr_printing(content: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let suffix = unique_suffix();
        let mut data_path = std::env::temp_dir();
        data_path.push(format!(
            "mv-herdr-test-data-{}-{suffix}",
            std::process::id()
        ));
        std::fs::write(&data_path, content).unwrap();

        let mut script_path = std::env::temp_dir();
        script_path.push(format!(
            "mv-herdr-test-script-{}-{suffix}",
            std::process::id()
        ));
        std::fs::write(
            &script_path,
            format!("#!/bin/sh\ncat '{}'\n", data_path.display()),
        )
        .unwrap();
        make_executable(&script_path);
        (script_path, data_path)
    }

    // --- effective_timeout (pure) --------------------------------------

    #[test]
    fn herdr_calls_get_the_bounded_default_timeout() {
        assert_eq!(
            effective_timeout(true, None),
            Some(Duration::from_secs_f64(DEFAULT_TIMEOUT_SECONDS))
        );
    }

    #[test]
    fn non_herdr_calls_get_no_default_timeout() {
        assert_eq!(effective_timeout(false, None), None);
    }

    #[test]
    fn an_explicit_timeout_always_wins() {
        let requested = Some(Duration::from_secs(5));
        assert_eq!(effective_timeout(true, requested), requested);
        assert_eq!(effective_timeout(false, requested), requested);
    }

    // --- run(), against a real subprocess -------------------------------

    #[test]
    fn run_executes_and_parses_json() {
        let command = echo(r#"{"result": {"type": "ok"}}"#);
        let result = run(&command, None).unwrap();
        assert!(result.ok());
        assert_eq!(
            result.payload,
            Some(serde_json::json!({"result": {"type": "ok"}}))
        );
    }

    #[test]
    fn run_reports_a_nonzero_exit_without_raising() {
        let command = HerdrCommand::new(vec!["/usr/bin/false".to_string()]);
        let result = run(&command, None).unwrap();
        assert!(!result.ok());
        assert_eq!(result.message(), "exit 1");
    }

    #[test]
    fn non_json_output_leaves_payload_empty() {
        let result = run(&echo("not json"), None).unwrap();
        assert!(result.ok());
        assert_eq!(result.payload, None);
    }

    #[test]
    fn pane_reads_keep_only_the_newest_bounded_buffer() {
        let text = "old\n".repeat(100) + "newest\n" + &"x".repeat(MAX_PANE_READ_BYTES + 100);
        let (script, data) = fake_herdr_printing(&text);
        let command = HerdrCommand::new(vec![
            script.to_string_lossy().into_owned(),
            "agent".to_string(),
            "read".to_string(),
            "agent".to_string(),
        ]);

        let result = run(&command, None).unwrap();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&data).ok();

        assert!(result.stdout.len() <= MAX_PANE_READ_BYTES);
        assert!(result.stdout.ends_with(&"x".repeat(100)));
    }

    /// Card `20261103-b-57c876`: unlike a pane/agent read, a JSON
    /// listing's stdout must not be silently tailed — a huge or hostile
    /// reply is refused outright rather than parsed from a truncated,
    /// invalid JSON tail.
    #[test]
    fn a_huge_json_listing_is_refused_rather_than_silently_truncated() {
        let huge = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let (script, data) = fake_herdr_printing(&huge);
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());

        let error = run(&agent_list(), None).unwrap_err();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&data).ok();

        match error {
            HerdrError::OutputTooLarge { limit_bytes, .. } => {
                assert_eq!(limit_bytes, MAX_OUTPUT_BYTES);
            }
            other => panic!("expected OutputTooLarge, got {other:?}"),
        }
    }

    /// Same cap, applied to stderr regardless of what command ran.
    #[test]
    fn huge_stderr_is_refused_rather_than_silently_truncated() {
        let huge = "x".repeat(MAX_OUTPUT_BYTES + 100);
        let (script, data) = fake_herdr_printing(&huge);
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat '{}' >&2\n", data.display()),
        )
        .unwrap();
        make_executable(&script);
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());

        let error = run(&agent_list(), None).unwrap_err();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&data).ok();

        match error {
            HerdrError::OutputTooLarge { limit_bytes, .. } => {
                assert_eq!(limit_bytes, MAX_OUTPUT_BYTES);
            }
            other => panic!("expected OutputTooLarge, got {other:?}"),
        }
    }

    #[test]
    fn herdr_exit_zero_without_result_is_an_error() {
        let script = fake_herdr_script("printf '%s' '{}'");
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());

        let result = run(&agent_list(), None).unwrap();
        std::fs::remove_file(&script).ok();

        assert!(!result.ok());
        assert!(result.message().contains("without a 'result' field"));
    }

    #[test]
    fn creation_commands_reject_malformed_envelopes() {
        for reply in ["not json", "{}", r#"{"result": null}"#, r#"{"result": []}"#] {
            let (script, data) = fake_herdr_printing(reply);
            let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());

            let result = run(&agent_list(), None).unwrap();
            std::fs::remove_file(&script).ok();
            std::fs::remove_file(&data).ok();

            assert!(!result.ok(), "reply {reply:?} should not be ok");
            assert!(result.protocol_error.is_some());
        }
    }

    #[test]
    fn plain_text_herdr_replies_stay_ok_for_commands_that_print_text() {
        let script = fake_herdr_script("printf 'ok\\n'");
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());

        assert!(run(&pane_run("w1:p1", "unset FOO"), None).unwrap().ok());
        assert!(run(&agent_read("mv-fix-cl", None, None), None)
            .unwrap()
            .ok());

        let listing = run(&agent_list(), None).unwrap();
        std::fs::remove_file(&script).ok();
        assert!(!listing.ok());
        assert!(listing.message().contains("non-JSON"));
    }

    #[test]
    fn a_successful_result_has_no_error_code() {
        let result = run(&echo("{}"), None).unwrap();
        assert_eq!(result.error_code(), None);
    }

    #[test]
    fn run_feeds_command_stdin_to_the_real_subprocess() {
        let mut command = HerdrCommand::new(vec!["/bin/cat".to_string()]);
        command.stdin = Some("hello from stdin\n".to_string());
        let result = run(&command, None).unwrap();
        assert!(result.ok());
        assert_eq!(result.stdout, "hello from stdin\n");
    }

    #[test]
    fn run_with_no_stdin_set_does_not_hang_or_send_anything() {
        let command = HerdrCommand::new(vec!["/bin/cat".to_string(), "/dev/null".to_string()]);
        let result = run(&command, Some(Duration::from_secs(10))).unwrap();
        assert!(result.ok());
        assert_eq!(result.stdout, "");
    }

    #[test]
    fn run_errors_when_the_binary_is_missing() {
        let command = HerdrCommand::new(vec!["/no/such/binary-xyz".to_string()]);
        let error = run(&command, None).unwrap_err();
        assert!(matches!(error, HerdrError::MissingBinary { .. }));
        assert!(error.to_string().contains("/no/such/binary-xyz"));
    }

    /// Card 20261103-b-1da16f's structural fix: the bare `herdr` token
    /// (what `herdr_binary()` returns when `HERDR_BIN_PATH` is unset) must
    /// never reach `spawn` while `MV_TEST_FORBID_REAL_HERDR` is set — the
    /// workspace `.cargo/config.toml` forces it on for every `cargo
    /// build`/`test`/`run`, so this is the always-on case in practice.
    #[test]
    fn run_refuses_the_bare_herdr_token_when_forbidden() {
        assert!(forbids_real_herdr_from(Some("1")));
        assert!(!forbids_real_herdr_from(Some("0")));
        assert!(!forbids_real_herdr_from(Some("")));
        assert!(!forbids_real_herdr_from(Some("  ")));
        assert!(!forbids_real_herdr_from(None));
        let _guard = HerdrBinPathGuard::unset();
        let command = HerdrCommand::new(vec![
            "herdr".to_string(),
            "pane".to_string(),
            "list".to_string(),
        ]);
        let result = run(&command, None);
        assert!(matches!(
            result.unwrap_err(),
            HerdrError::RealHerdrForbidden { .. }
        ));
    }

    #[test]
    fn run_errors_when_the_binary_is_not_executable() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mv-herdr-test-not-executable-{}",
            std::process::id()
        ));
        std::fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o644);
        std::fs::set_permissions(&path, perms).unwrap();

        let expected = path.to_string_lossy().into_owned();
        let command = HerdrCommand::new(vec![expected.clone()]);
        let error = run(&command, None).unwrap_err();
        std::fs::remove_file(&path).ok();

        assert!(matches!(error, HerdrError::MissingBinary { .. }));
        assert!(error.to_string().contains(expected.as_str()));
    }

    #[test]
    fn a_slow_binary_times_out_with_a_typed_error() {
        let script = fake_herdr_script("sleep 1");
        let command = HerdrCommand::new(vec![script.to_string_lossy().into_owned()]);

        let error = run(&command, Some(Duration::from_millis(10))).unwrap_err();
        std::fs::remove_file(&script).ok();

        match error {
            HerdrError::Timeout { timeout_secs, .. } => {
                assert!((timeout_secs - 0.01).abs() < 1e-9);
            }
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    fn process_state(pid: u32) -> Option<String> {
        let output = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "state="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if state.is_empty() {
            None
        } else {
            Some(state)
        }
    }

    #[test]
    fn a_timeout_kills_grandchild_processes_and_leaves_no_zombie() {
        let suffix = unique_suffix();
        let mut pidfile = std::env::temp_dir();
        pidfile.push(format!("mv-herdr-pgid-{}-{suffix}", std::process::id()));
        let script = fake_herdr_script(&format!(
            "sleep 60 &\nprintf '%s\\n%s\\n' \"$$\" \"$!\" > '{}'\nwait\n",
            pidfile.display()
        ));
        let command = HerdrCommand::new(vec![script.to_string_lossy().into_owned()]);
        // 2s, not 500ms: `run` starts its timeout clock only after
        // `spawn()` returns, but the script still has to be scheduled far
        // enough to fork `sleep` and write the pidfile before the clock
        // runs out. On a host busy with other concurrently-running
        // subprocess-spawning tests, that scheduling alone can eat
        // hundreds of ms; a 500ms budget made that race, not the kill
        // logic under test, the thing that failed. 2s keeps this well
        // under the 60s sleep so a real timeout (not natural exit) is
        // still what we are observing.
        let error = run(&command, Some(Duration::from_secs(2))).unwrap_err();
        assert!(matches!(error, HerdrError::Timeout { .. }));

        let recorded = std::fs::read_to_string(&pidfile).unwrap_or_default();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&pidfile).ok();
        let mut pids = recorded.lines().filter_map(|line| line.parse::<u32>().ok());
        let parent = pids
            .next()
            .expect("script should have recorded its pid before the timeout");
        let grandchild = pids
            .next()
            .expect("script should have recorded the sleep pid");

        for _ in 0..40 {
            thread::sleep(Duration::from_millis(25));
            let parent_state = process_state(parent);
            let child_state = process_state(grandchild);
            for (pid, state) in [(parent, &parent_state), (grandchild, &child_state)] {
                if state.as_deref().is_some_and(|s| s.starts_with('Z')) {
                    panic!("pid {pid} is a zombie ({state:?})");
                }
            }
            if parent_state.is_none() && child_state.is_none() {
                return;
            }
        }
        panic!(
            "timeout left processes running: parent={parent} ({:?}) grandchild={grandchild} ({:?})",
            process_state(parent),
            process_state(grandchild)
        );
    }

    #[test]
    fn read_bounded_drops_the_oldest_bytes_while_reading() {
        let data = [b'a'; 100];
        let mut cursor = std::io::Cursor::new(data);
        let out = read_bounded(&mut cursor, 16);
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|byte| *byte == b'a'));
        let mut empty = std::io::Cursor::new([]);
        assert!(read_bounded(&mut empty, 16).is_empty());
    }

    #[test]
    fn read_capped_flags_truncation_instead_of_silently_tailing() {
        let data = [b'a'; 100];
        let mut cursor = std::io::Cursor::new(data);
        let (out, truncated) = read_capped(&mut cursor, 16);
        assert_eq!(out.len(), 16);
        assert!(out.iter().all(|byte| *byte == b'a'));
        assert!(truncated);

        let mut small = std::io::Cursor::new([b'a'; 10]);
        let (out, truncated) = read_capped(&mut small, 16);
        assert_eq!(out.len(), 10);
        assert!(!truncated);
    }

    #[test]
    fn extra_json_fields_on_a_zero_exit_listing_are_not_a_protocol_error() {
        let script = fake_herdr_script(
            r#"printf '%s' '{"id":"cli:agent:list","extra":true,"result":{"type":"agent_list","agents":[]}}'"#,
        );
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());
        let result = run(&agent_list(), None).unwrap();
        std::fs::remove_file(&script).ok();
        assert!(result.ok());
        assert!(result.protocol_error.is_none());
        assert_eq!(
            result
                .payload
                .as_ref()
                .and_then(|payload| payload.get("extra")),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn a_nonzero_exit_with_json_on_stdout_is_captured_not_raised() {
        let (script, data) =
            fake_herdr_printing(r#"{"error":{"code":"timeout","message":"wait expired"}}"#);
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());
        // The stub prints JSON then exits 1. `printf` is exit 0, so wrap.
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat '{}'\nexit 1\n", data.display()),
        )
        .unwrap();
        make_executable(&script);

        let result = run(&agent_list(), None).unwrap();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&data).ok();

        assert!(!result.ok());
        assert_eq!(result.returncode, 1);
        assert!(result.protocol_error.is_none());
        assert_eq!(
            result.error_code(),
            Some(crate::envelope::HerdrErrorCode::Timeout)
        );
    }

    #[test]
    fn missing_pane_error_envelope_is_a_failed_result_not_a_panic() {
        let (script, data) = fake_herdr_printing(
            r#"{"error":{"code":"fake_herdr_error","message":"pane not found: w9:p9"}}"#,
        );
        let _guard = HerdrBinPathGuard::set(&script.to_string_lossy());
        std::fs::write(
            &script,
            format!("#!/bin/sh\ncat '{}' >&2\nexit 1\n", data.display()),
        )
        .unwrap();
        make_executable(&script);

        let command = crate::argv::pane_get("w9:p9");
        let result = run(&command, None).unwrap();
        std::fs::remove_file(&script).ok();
        std::fs::remove_file(&data).ok();

        assert!(!result.ok());
        assert!(result.message().contains("pane not found"));
        assert!(result.error_code().is_none());
    }
}

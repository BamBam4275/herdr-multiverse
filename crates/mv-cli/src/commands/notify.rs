//! `multiverse notify`: adapt CLI requests to the shared rate-limited sender.
//! Delivery errors are advisory: send/flush I/O and network failure exit 0
//! (Python `cmd_notify`). Missing `--project`/`--title`/`--message` without
//! `--flush` is a usage error and exits 2. Missing topic configuration
//! never starts a subprocess or network request.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use clap::ArgMatches;
use mv_state::notify::{self, Outcome, Priority, QueuedNotification, SendResult};

use super::doctor::{resolve_config_path, state_dir_from_config};
use crate::exit::{Failure, UNKNOWN};

fn curl_quote(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
}

/// `notify.py`'s `_ntfy_url` (`notify.py:321-324`): `MULTIVERSE_NTFY_URL`,
/// else the default ntfy server, slash-stripped. Empty is treated as unset,
/// same posture as [`resolve_topic`]/[`resolve_token`].
fn ntfy_url() -> String {
    ntfy_url_from(std::env::var("MULTIVERSE_NTFY_URL").ok().as_deref())
}

fn ntfy_url_from(raw: Option<&str>) -> String {
    let base = match raw.map(str::trim) {
        Some(value) if !value.is_empty() => value,
        _ => "https://ntfy.sh",
    };
    base.trim_end_matches('/').to_string()
}

/// Curl configuration goes over stdin so message/topic data is absent from
/// the process argv. This adapter owns I/O; rate/dedup/outbox rules live in
/// state. `pub(super)` (card `20261103-b-15d3ca`): `commands::sweep`'s own
/// picker alert reuses this exact push rather than a second curl
/// invocation of its own — via [`push`], which carries no token (that lane
/// does not own wiring an authenticated push through `sweep.rs`).
pub(crate) fn push(
    topic: Option<&str>,
    record: &QueuedNotification,
    priority: &str,
    tags: &[String],
) -> bool {
    push_with_token(topic, None, record, priority, tags)
}

/// [`push`], plus an optional `Authorization: Bearer` header
/// (`notify.py:361-421`'s `_read_token`) and ntfy's own title/body bounds
/// (`notify.py:403-406`'s own `_bound_notification` call in `ntfy_push`) —
/// applied again here even though [`notify::send`] already bounded the
/// record, since a delivered send's body can grow past the limit again once
/// the suppressed-streak suffix is appended.
pub(crate) fn push_with_token(
    topic: Option<&str>,
    token: Option<&str>,
    record: &QueuedNotification,
    priority: &str,
    tags: &[String],
) -> bool {
    let Some(topic) = topic else { return false };
    let config = curl_config(&ntfy_url(), topic, token, record, priority, tags);
    let Ok(mut child) = Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            "-K",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(config.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

/// The `curl -K -` config text itself, pulled out of [`push_with_token`] so
/// it can be checked without spawning a real `curl` or reaching any
/// network — this crate never sends a real ntfy push from a test.
fn curl_config(
    ntfy_url: &str,
    topic: &str,
    token: Option<&str>,
    record: &QueuedNotification,
    priority: &str,
    tags: &[String],
) -> String {
    let title = notify::truncate_title(&record.title);
    let message = notify::truncate_body(&record.message);
    let mut config = format!(
        "url = \"{}/{}\"\nheader = \"Title: {}\"\nheader = \"Priority: {}\"\ndata-binary = \"{}\"\n",
        curl_quote(ntfy_url),
        curl_quote(topic),
        curl_quote(&title),
        curl_quote(priority),
        curl_quote(&message),
    );
    if !tags.is_empty() {
        config.push_str(&format!(
            "header = \"Tags: {}\"\n",
            curl_quote(&tags.join(","))
        ));
    }
    if let Some(token) = token {
        config.push_str(&format!(
            "header = \"Authorization: Bearer {}\"\n",
            curl_quote(token)
        ));
    }
    config
}

/// `MULTIVERSE_NTFY_TOPIC_FILE`, else `<config dir>/ntfy-topic`, trimmed
/// and empty-as-absent. Pulled out of `run` (card `20261103-b-15d3ca`) so
/// `commands::sweep`'s own picker alert — which has no `--topic` CLI flag
/// of its own to read an override from — resolves the topic the same way.
pub(crate) fn resolve_topic(config_path: &Path) -> Option<String> {
    let topic_path = std::env::var_os("MULTIVERSE_NTFY_TOPIC_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config_path.with_file_name("ntfy-topic"));
    let topic = std::fs::read_to_string(topic_path).ok()?;
    let trimmed = topic.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `MULTIVERSE_NTFY_TOKEN_FILE`, else `<config dir>/ntfy-token`, trimmed and
/// empty-as-absent — `notify.py`'s own `_read_token` (`notify.py:367-375`).
/// `None` means anonymous, the same decision Python documents at
/// `notify.py:399-401`. `pub(crate)`, mirroring [`resolve_topic`], for a
/// future caller outside this module (`commands::sweep`'s own picker alert
/// resolves its topic this same way but is out of this task's write scope —
/// see this task's `result.md`/`revise`).
pub(crate) fn resolve_token(config_path: &Path) -> Option<String> {
    let token_path = std::env::var_os("MULTIVERSE_NTFY_TOKEN_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| config_path.with_file_name("ntfy-token"));
    let token = std::fs::read_to_string(token_path).ok()?;
    let trimmed = token.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn outcome(result: SendResult) -> &'static str {
    if result.unreachable {
        eprintln!("notify: ntfy unreachable, queued to outbox");
    }
    match result.outcome {
        Outcome::Sent => "sent",
        Outcome::Suppressed => "suppressed",
        Outcome::Queued => "queued",
        Outcome::Stale => "stale",
    }
}

/// Queued notifications carry project/title straight from whatever queued
/// them, so sanitize both before they reach the operator's terminal.
fn flush_line(record: &QueuedNotification, outcome_str: &str) -> String {
    format!(
        "{}: {}: {}",
        mv_board::layout::sanitize_title(&record.project),
        mv_board::layout::sanitize_title(&record.title),
        outcome_str
    )
}

fn missing_notify_fields(args: &ArgMatches) -> bool {
    args.get_one::<String>("project").is_none()
        || args.get_one::<String>("title").is_none()
        || args.get_one::<String>("message").is_none()
}

fn usage_missing_fields() -> Failure {
    eprintln!(
        "notify: --project, --title, and --message are all required (or pass --flush on its own)"
    );
    Failure::Reported(UNKNOWN)
}

fn run_with_state(
    args: &ArgMatches,
    state_dir: &Path,
    transport: &mut impl FnMut(&QueuedNotification) -> bool,
) -> Result<(), Failure> {
    if args.get_flag("flush") {
        for (record, result) in notify::flush(state_dir, transport)
            .map_err(|error| Failure::Operation(error.to_string()))?
        {
            let outcome_str = outcome(result);
            println!("{}", flush_line(&record, outcome_str));
        }
        return Ok(());
    }
    let (Some(project), Some(title), Some(message)) = (
        args.get_one::<String>("project"),
        args.get_one::<String>("title"),
        args.get_one::<String>("message"),
    ) else {
        return Err(usage_missing_fields());
    };
    let episode_key = args.get_one::<String>("episode-key").cloned();
    let episode_state = args.get_one::<String>("episode-state").cloned();
    let (episode_key, episode_state) = match (episode_key, episode_state) {
        (Some(key), Some(path)) => (Some(key), Some(path)),
        _ => (None, None),
    };
    let tags: Vec<String> = args
        .get_many::<String>("tag")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let priority_str = args
        .get_one::<String>("priority")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let record = QueuedNotification {
        project: project.clone(),
        title: title.clone(),
        message: message.clone(),
        priority: priority_str,
        dedup_key: episode_key.clone().filter(|key| key != title),
        queued_at: mv_core::time::now_stamp(),
        kind: None,
        subject: None,
        tags,
        episode_key,
        episode_state,
    };
    let priority = if args
        .get_one::<String>("priority")
        .is_some_and(|p| p == "urgent")
    {
        Priority::Urgent
    } else {
        Priority::Normal
    };
    let result = notify::send(state_dir, &record, priority, transport)
        .map_err(|error| Failure::Operation(error.to_string()))?;
    println!("notify: {}", outcome(result));
    Ok(())
}

pub(crate) fn run(args: &ArgMatches) -> Result<(), Failure> {
    // Usage is decided before config: Python `cmd_notify` never calls
    // `_load(args)`, and a missing trio must exit 2 even when no fleet
    // config exists. Delivery/network/I/O stay exit 0.
    if !args.get_flag("flush") && missing_notify_fields(args) {
        return Err(usage_missing_fields());
    }
    // Notify accepts a bare project string and does not load project config.
    let result = (|| {
        let (config_path, _) =
            resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
        let state_dir = state_dir_from_config(&config_path);
        let topic = resolve_topic(&config_path);
        let token = resolve_token(&config_path);
        let tags: Vec<String> = args
            .get_many::<String>("tag")
            .map(|values| values.cloned().collect())
            .unwrap_or_default();
        run_with_state(args, &state_dir, &mut |record| {
            // A flushed record carries its own originally-chosen priority
            // (`record.priority`); a fresh (non-flush) send's record was
            // just built from this same `args`, so reading it back off the
            // record rather than `args` again is the one priority source
            // both paths share (`notify.py`'s own `record.get("priority",
            // "default")` replay, `notify.py:1139`).
            let tags = if record.tags.is_empty() {
                tags.as_slice()
            } else {
                record.tags.as_slice()
            };
            push_with_token(
                topic.as_deref(),
                token.as_deref(),
                record,
                &record.priority,
                tags,
            )
        })
    })();
    match result {
        Err(error @ Failure::Reported(_)) => Err(error),
        Err(error) => {
            eprintln!("notify: {error}");
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use crate::args;

    #[test]
    fn notify_queues_and_flushes_in_isolated_state() {
        let state = tempfile::tempdir().unwrap();
        for argv in [
            vec!["notify", "--project", "p", "--title", "t", "--message", "m"],
            vec!["notify", "--flush"],
        ] {
            let matches = args::command()
                .try_get_matches_from(std::iter::once("multiverse").chain(argv))
                .unwrap();
            let sub = matches.subcommand_matches("notify").unwrap();
            assert!(super::run_with_state(sub, state.path(), &mut |_| false).is_ok());
        }
        let queued =
            mv_state::notify::load_outbox(&state.path().join("notify-outbox.jsonl")).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].project, "p");
        assert_eq!(queued[0].title, "t");
        assert_eq!(queued[0].message, "m");
    }

    #[test]
    fn missing_project_title_or_message_is_usage_exit_two() {
        let matches = args::command()
            .try_get_matches_from(["multiverse", "notify"])
            .unwrap();
        let sub = matches.subcommand_matches("notify").unwrap();
        let error = super::run(sub).expect_err("bare notify must not exit 0");
        assert!(matches!(
            error,
            crate::exit::Failure::Reported(crate::exit::UNKNOWN)
        ));
        assert_eq!(error.code(), 2);

        let state = tempfile::tempdir().unwrap();
        let error = super::run_with_state(sub, state.path(), &mut |_| false)
            .expect_err("run_with_state must refuse the missing trio");
        assert!(matches!(
            error,
            crate::exit::Failure::Reported(crate::exit::UNKNOWN)
        ));
    }

    #[test]
    fn no_topic_does_not_invoke_the_transport() {
        let record = mv_state::notify::QueuedNotification {
            project: "p".into(),
            title: "t".into(),
            message: "m".into(),
            priority: "default".into(),
            dedup_key: None,
            queued_at: "".into(),
            kind: None,
            subject: None,
            tags: Vec::new(),
            episode_key: None,
            episode_state: None,
        };
        assert!(!super::push(None, &record, "normal", &[]));
        assert_eq!(super::curl_quote("a\"\nb\\c\r"), "a\\\"\\nb\\\\c\\r");
    }

    #[test]
    fn flush_line_strips_escape_sequences_from_project_and_title() {
        let record = mv_state::notify::QueuedNotification {
            project: "p\u{1b}]0;pwned\u{7}roject".into(),
            title: "t\u{1b}[2Jitle".into(),
            message: "m".into(),
            priority: "default".into(),
            dedup_key: None,
            queued_at: "".into(),
            kind: None,
            subject: None,
            tags: Vec::new(),
            episode_key: None,
            episode_state: None,
        };
        let line = super::flush_line(&record, "sent");
        assert!(!line.contains('\u{1b}'));
        assert_eq!(line, "project: title: sent");
    }

    #[test]
    fn tags_are_repeatable_and_episode_state_requires_the_key() {
        let state = tempfile::tempdir().unwrap();
        let matches = args::command()
            .try_get_matches_from([
                "multiverse",
                "notify",
                "--project",
                "p",
                "--title",
                "t",
                "--message",
                "m",
                "--tag",
                "a",
                "--tag",
                "b",
                "--episode-key",
                "host-sleep",
                "--episode-state",
                "episodes.json",
            ])
            .unwrap();
        let sub = matches.subcommand_matches("notify").unwrap();
        super::run_with_state(sub, state.path(), &mut |_| false).unwrap();
        let queued =
            mv_state::notify::load_outbox(&state.path().join("notify-outbox.jsonl")).unwrap();
        assert_eq!(queued[0].tags, vec!["a", "b"]);
        assert_eq!(queued[0].episode_key.as_deref(), Some("host-sleep"));
        assert_eq!(queued[0].episode_state.as_deref(), Some("episodes.json"));

        let state = tempfile::tempdir().unwrap();
        let matches = args::command()
            .try_get_matches_from([
                "multiverse",
                "notify",
                "--project",
                "p",
                "--title",
                "solo",
                "--message",
                "m",
                "--episode-key",
                "alone",
            ])
            .unwrap();
        let sub = matches.subcommand_matches("notify").unwrap();
        super::run_with_state(sub, state.path(), &mut |_| false).unwrap();
        let queued =
            mv_state::notify::load_outbox(&state.path().join("notify-outbox.jsonl")).unwrap();
        assert!(queued[0].episode_key.is_none());
        assert!(queued[0].episode_state.is_none());
    }

    fn sample_record() -> mv_state::notify::QueuedNotification {
        mv_state::notify::QueuedNotification {
            project: "p".into(),
            title: "t".into(),
            message: "m".into(),
            priority: "default".into(),
            dedup_key: None,
            queued_at: "".into(),
            kind: None,
            subject: None,
            tags: Vec::new(),
            episode_key: None,
            episode_state: None,
        }
    }

    /// Scout gap #20: `notify.py:361-421`'s `_read_token` adds an
    /// `Authorization: Bearer` header when a token is configured.
    #[test]
    fn curl_config_adds_a_bearer_header_only_when_a_token_is_given() {
        let record = sample_record();
        let without_token =
            super::curl_config("https://ntfy.sh", "topic", None, &record, "default", &[]);
        assert!(!without_token.contains("Authorization"));

        let with_token = super::curl_config(
            "https://ntfy.sh",
            "topic",
            Some("secret-token"),
            &record,
            "default",
            &[],
        );
        assert!(with_token.contains("header = \"Authorization: Bearer secret-token\""));
    }

    /// Scout gap #25: `notify.py:321-324`'s `_ntfy_url` honors
    /// `MULTIVERSE_NTFY_URL` instead of the hardcoded default.
    #[test]
    fn ntfy_url_from_honors_an_override_and_strips_a_trailing_slash() {
        assert_eq!(super::ntfy_url_from(None), "https://ntfy.sh");
        assert_eq!(super::ntfy_url_from(Some("")), "https://ntfy.sh");
        assert_eq!(
            super::ntfy_url_from(Some("https://ntfy.example.com/")),
            "https://ntfy.example.com"
        );
    }

    /// Scout gap #21: ntfy's own title/body limits are enforced again at
    /// the push boundary, since a delivered body can grow past them once
    /// the suppressed-streak suffix (`notify.rs`'s own `send`) is appended.
    #[test]
    fn curl_config_truncates_an_oversized_title_and_body() {
        let mut record = sample_record();
        record.title = "x".repeat(mv_state::notify::TITLE_MAX_CHARS + 20);
        record.message = "y".repeat(mv_state::notify::BODY_MAX_BYTES + 20);
        let config = super::curl_config("https://ntfy.sh", "topic", None, &record, "default", &[]);
        assert!(!config.contains(&"x".repeat(mv_state::notify::TITLE_MAX_CHARS + 20)));
        assert!(config.contains("…[truncated]"));
    }
}

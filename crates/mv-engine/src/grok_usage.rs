//! Grok-specific usage parsing, kept separate from the shared
//! [`crate::usage`] model because its CLI output shape differs.
//!
//! Grok's CLI keeps no real token counter — the Python original's own
//! module docstring states this plainly. Every value this module produces
//! is an **estimate**, never a measured count; callers must not present it
//! as equivalent to Claude's or Codex's gauge readings.

use std::path::{Path, PathBuf};

/// `loops × chat_history_bytes / 8` — the estimate formula, with integer
/// (floor) division matching Python's `//`.
pub fn estimate_tokens(loops: u64, chat_history_bytes: u64) -> u64 {
    loops.saturating_mul(chat_history_bytes) / 8
}

/// `<GROK_HOME>/sessions/<urlencoded-cwd>/<session-id>/`, the directory a
/// Grok session's `summary.json`/`events.jsonl`/`chat_history.jsonl` live
/// under.
pub fn session_dir(grok_home: &Path, cwd: &str, session_id: &str) -> PathBuf {
    grok_home
        .join("sessions")
        .join(percent_encode(cwd))
        .join(session_id)
}

/// Minimal RFC 3986 "unreserved" percent-encoder: letters, digits, and
/// `-_.~` pass through unescaped, everything else (including `/`, since the
/// whole path collapses into one directory segment) becomes `%XX`. No
/// external crate for this — the alphabet is small and fixed, and adding a
/// url-encoding dependency for one call site is not worth it.
///
/// **Confidence note:** the exact percent-encoding convention (in
/// particular, whether `/` is escaped or kept as a path separator) was not
/// independently verified against a captured real Grok session directory
/// in this task — see this task's `result.md`. This function assumes the
/// whole cwd collapses to a single directory segment, consistent with
/// "urlencoded-cwd" being one path component in the spec's own wording.
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match *byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// How far back a session is worth reading at all — both the summary
/// windows and `--detail`'s per-day/per-project tables live inside it.
pub const WINDOW_DAYS: u64 = 7;

/// One calendar day's (UTC) sessions and estimated tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyUsage {
    pub day: String,
    pub sessions: u64,
    pub estimated_tokens: u64,
}

/// One project's sessions and estimated tokens over [`WINDOW_DAYS`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectUsage {
    pub project: String,
    pub sessions: u64,
    pub estimated_tokens: u64,
}

/// `accounts usage --detail grok`'s per-day and per-project tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageDetail {
    pub by_day: Vec<DailyUsage>,
    pub by_project: Vec<ProjectUsage>,
}

/// The per-day/per-project detail tables, or `None` when the scan finds
/// no sessions inside [`WINDOW_DAYS`].
pub fn detail(grok_home: &Path) -> Option<UsageDetail> {
    let sessions = scan_sessions(grok_home, std::time::SystemTime::now());
    if sessions.is_empty() {
        return None;
    }
    Some(detail_from_sessions(&sessions))
}

/// Sessions and estimated tokens inside one trailing window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageWindow {
    pub sessions: u64,
    pub estimated_tokens: u64,
    /// `(project, session count)`, busiest first, ties broken by name.
    /// Empty unless the caller asked for a ranking (`top_projects > 0`).
    pub top_projects: Vec<(String, u64)>,
}

/// What `accounts usage` shows for grok: two trailing windows off one scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageSummary {
    pub last_24h: UsageWindow,
    pub last_7d: UsageWindow,
}

/// The `accounts usage` summary for one grok account, or `None` when the
/// scan finds no sessions inside [`WINDOW_DAYS`].
pub fn summarize(grok_home: &Path) -> Option<UsageSummary> {
    let now = std::time::SystemTime::now();
    let sessions = scan_sessions(grok_home, now);
    if sessions.is_empty() {
        return None;
    }
    Some(UsageSummary {
        last_24h: window_from(&sessions, now, 24, 3),
        last_7d: window_from(&sessions, now, 24 * WINDOW_DAYS, 0),
    })
}

fn window_from(
    sessions: &[GrokSession],
    now: std::time::SystemTime,
    hours: u64,
    top_projects: usize,
) -> UsageWindow {
    let cutoff = now
        .checked_sub(std::time::Duration::from_secs(hours.saturating_mul(3600)))
        .unwrap_or(std::time::UNIX_EPOCH);
    let matched: Vec<&GrokSession> = sessions
        .iter()
        .filter(|session| session.when >= cutoff)
        .collect();
    let estimated_tokens = matched.iter().map(|session| session.estimated_tokens).sum();
    let mut top = Vec::new();
    if top_projects > 0 {
        let mut counts: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
        for session in &matched {
            if let Some(project) = &session.project {
                *counts.entry(project.clone()).or_insert(0) += 1;
            }
        }
        let mut ranked: Vec<(String, u64)> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(top_projects);
        top = ranked;
    }
    UsageWindow {
        sessions: matched.len() as u64,
        estimated_tokens,
        top_projects: top,
    }
}

struct GrokSession {
    when: std::time::SystemTime,
    project: Option<String>,
    estimated_tokens: u64,
}

fn scan_sessions(grok_home: &Path, now: std::time::SystemTime) -> Vec<GrokSession> {
    let cutoff = now
        .checked_sub(std::time::Duration::from_secs(WINDOW_DAYS * 24 * 60 * 60))
        .unwrap_or(std::time::UNIX_EPOCH);
    let sessions_root = grok_home.join("sessions");
    let cwd_dirs = match std::fs::read_dir(&sessions_root) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };
    let mut sessions = Vec::new();
    for cwd_dir in cwd_dirs.flatten() {
        let cwd_path = cwd_dir.path();
        if !cwd_path.is_dir() {
            continue;
        }
        let encoded_cwd = cwd_dir.file_name().to_string_lossy().into_owned();
        let session_dirs = match std::fs::read_dir(&cwd_path) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for session_dir in session_dirs.flatten() {
            let path = session_dir.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(session) = read_session(&path, &encoded_cwd, cutoff) {
                sessions.push(session);
            }
        }
    }
    sessions
}

fn read_session(
    session_dir: &Path,
    encoded_cwd: &str,
    cutoff: std::time::SystemTime,
) -> Option<GrokSession> {
    let summary = read_json_object(&session_dir.join("summary.json"));
    let mut cwd: Option<String> = None;
    let mut when: Option<std::time::SystemTime> = None;
    if let Some(summary) = &summary {
        if let Some(raw) = summary.get("cwd").and_then(serde_json::Value::as_str) {
            if !raw.is_empty() {
                cwd = Some(raw.to_string());
            }
        }
        if let Some(raw) = summary
            .get("created_at")
            .and_then(serde_json::Value::as_str)
        {
            when = parse_timestamp(raw);
        }
    }
    if when.is_none() {
        when = session_dir
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok());
    }
    let when = when?;
    if when < cutoff {
        return None;
    }
    let cwd = cwd.unwrap_or_else(|| percent_decode(encoded_cwd));
    let project = project_from_cwd(&cwd);
    let loops = count_loop_started(&session_dir.join("events.jsonl"));
    let chat_bytes = file_size(&session_dir.join("chat_history.jsonl"));
    Some(GrokSession {
        when,
        project,
        estimated_tokens: ((loops as f64) * (chat_bytes as f64) / 8.0).round() as u64, // Python GrokSession.estimated_tokens uses round, not floor.
    })
}

fn detail_from_sessions(sessions: &[GrokSession]) -> UsageDetail {
    let mut by_day_counts: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    let mut by_project_counts: std::collections::BTreeMap<String, (u64, u64)> =
        std::collections::BTreeMap::new();
    for session in sessions {
        let day = utc_date(session.when);
        let day_entry = by_day_counts.entry(day).or_insert((0, 0));
        day_entry.0 += 1;
        day_entry.1 += session.estimated_tokens;
        if let Some(project) = &session.project {
            let project_entry = by_project_counts.entry(project.clone()).or_insert((0, 0));
            project_entry.0 += 1;
            project_entry.1 += session.estimated_tokens;
        }
    }
    let mut by_day: Vec<DailyUsage> = by_day_counts
        .into_iter()
        .map(|(day, (sessions, estimated_tokens))| DailyUsage {
            day,
            sessions,
            estimated_tokens,
        })
        .collect();
    by_day.sort_by(|a, b| b.day.cmp(&a.day));
    let mut by_project: Vec<ProjectUsage> = by_project_counts
        .into_iter()
        .map(|(project, (sessions, estimated_tokens))| ProjectUsage {
            project,
            sessions,
            estimated_tokens,
        })
        .collect();
    by_project.sort_by(|a, b| {
        b.sessions
            .cmp(&a.sessions)
            .then_with(|| a.project.cmp(&b.project))
    });
    UsageDetail { by_day, by_project }
}

fn read_json_object(path: &Path) -> Option<serde_json::Value> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value.is_object().then_some(value)
}

fn parse_timestamp(raw: &str) -> Option<std::time::SystemTime> {
    let candidate =
        if raw.ends_with('Z') || raw.contains('+') || raw.rfind('-').is_some_and(|i| i > 10) {
            raw.to_string()
        } else {
            format!("{raw}Z")
        };
    let parsed =
        time::OffsetDateTime::parse(&candidate, &time::format_description::well_known::Rfc3339)
            .ok()?;
    let secs = parsed.unix_timestamp();
    if secs < 0 {
        return None;
    }
    Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64))
}

fn utc_date(when: std::time::SystemTime) -> String {
    let secs = when
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    time::OffsetDateTime::from_unix_timestamp(secs)
        .map(|dt| {
            format!(
                "{:04}-{:02}-{:02}",
                dt.year(),
                u8::from(dt.month()),
                dt.day()
            )
        })
        .unwrap_or_else(|_| "1970-01-01".into())
}

fn project_from_cwd(cwd: &str) -> Option<String> {
    let marker = "/Code/";
    let start = cwd.find(marker)? + marker.len();
    let rest = &cwd[start..];
    if rest.is_empty() {
        return None;
    }
    let name = rest.split('/').next().unwrap_or(rest);
    (!name.is_empty()).then(|| name.to_string())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(value) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(value);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn count_loop_started(path: &Path) -> u64 {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut count = 0;
    for line in text.lines() {
        if !line.contains("loop_started") {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            if value.get("type").and_then(serde_json::Value::as_str) == Some("loop_started") {
                count += 1;
            }
        }
    }
    count
}

fn file_size(path: &Path) -> u64 {
    path.metadata().map(|meta| meta.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_floors_like_python_integer_division() {
        assert_eq!(estimate_tokens(3, 100), 37); // 300/8 = 37.5 -> 37
        assert_eq!(estimate_tokens(0, 1_000), 0);
        assert_eq!(estimate_tokens(1, 7), 0); // 7/8 = 0
    }

    #[test]
    fn unreserved_characters_pass_through_unescaped() {
        assert_eq!(percent_encode("abc-XYZ_123.~"), "abc-XYZ_123.~");
    }

    #[test]
    fn slash_and_space_are_percent_encoded() {
        assert_eq!(percent_encode("/a b"), "%2Fa%20b");
    }

    #[test]
    fn session_dir_joins_grok_home_sessions_cwd_and_id() {
        let dir = session_dir(
            Path::new("/home/user/.grok"),
            "/home/user/project",
            "sess-1",
        );
        assert_eq!(
            dir,
            Path::new("/home/user/.grok/sessions/%2Fhome%2Fuser%2Fproject/sess-1")
        );
    }

    #[test]
    fn project_from_cwd_reads_the_code_segment() {
        assert_eq!(
            project_from_cwd("/home/dev/Code/demo/src"),
            Some("demo".into())
        );
        assert_eq!(project_from_cwd("/home/dev/Code/demo"), Some("demo".into()));
        assert_eq!(project_from_cwd("/home/dev/Code"), None);
    }

    #[test]
    fn detail_returns_none_when_there_are_no_sessions() {
        let dir = tempfile::tempdir().unwrap();
        assert!(detail(dir.path()).is_none());
    }

    #[test]
    fn detail_groups_recent_sessions_by_day_and_project() {
        let dir = tempfile::tempdir().unwrap();
        let session = session_dir(dir.path(), "/home/dev/Code/demo", "s1");
        std::fs::create_dir_all(&session).unwrap();
        let now = time::OffsetDateTime::now_utc();
        let created = now
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        std::fs::write(
            session.join("summary.json"),
            format!(r#"{{"cwd":"/home/dev/Code/demo","created_at":"{created}"}}"#),
        )
        .unwrap();
        std::fs::write(
            session.join("events.jsonl"),
            r#"{"type":"loop_started"}
{"type":"other"}
{"type":"loop_started"}
"#,
        )
        .unwrap();
        std::fs::write(session.join("chat_history.jsonl"), "xxxxxxxx").unwrap();
        let detail = detail(dir.path()).expect("recent session");
        assert_eq!(detail.by_day.len(), 1);
        assert_eq!(detail.by_day[0].sessions, 1);
        assert_eq!(detail.by_project[0].project, "demo");
        assert_eq!(detail.by_project[0].sessions, 1);
        // 2 loops × 8 bytes / 8 = 2
        assert_eq!(detail.by_day[0].estimated_tokens, 2);
    }
}

//! Vendor usage parsers feeding the gauge store.
//!
//! Claude: reads `<config_dir>/.claude.json`'s `cachedUsageUtilization`
//! block for the already-computed five-hour/seven-day utilization, and
//! separately sums token usage out of session transcripts. Parsing here is
//! pure — every function takes already-read JSON/text, not a path — so the
//! file-walking (finding `.claude.json`, enumerating
//! `<config_dir>/projects/*/*.jsonl`) and the separate `usage-cache.json` /
//! `usage-files-cache.json` caches described in `spec-seats-quotas.md` and
//! `audit-dispatch-tasks-seats.md` finding 2 are **not** covered here: no
//! `mv-state` file in this lane's list owns a usage-cache store (only
//! `breaker-state.json`, `seat-state.json`, `gauge-state.json` do), so that
//! caching layer is left for whichever file ends up owning it — see this
//! task's `result.md`.

use serde_json::Value;

/// `cachedUsageUtilization.utilization.{five_hour,seven_day}` plus the
/// cache key `cachedUsageUtilization.fetchedAtMs` (epoch milliseconds) —
/// keyed by the payload's own `fetchedAtMs`, **not** `.claude.json`'s file
/// mtime. Keying off mtime was the `53ebe31` regression this must not
/// reintroduce: an unrelated write to `.claude.json` must not look like a
/// fresh usage reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClaudeUtilization {
    pub five_hour_percent: f64,
    pub seven_day_percent: f64,
    pub fetched_at_ms: i64,
}

/// Parse a `.claude.json`-shaped payload. Returns `None` on any missing or
/// malformed field — fail open, the same posture as every other reader in
/// this subsystem.
pub fn parse_claude_utilization(claude_json: &Value) -> Option<ClaudeUtilization> {
    let node = claude_json.get("cachedUsageUtilization")?;
    let utilization = node.get("utilization")?;
    Some(ClaudeUtilization {
        five_hour_percent: utilization.get("five_hour")?.as_f64()?,
        seven_day_percent: utilization.get("seven_day")?.as_f64()?,
        fetched_at_ms: node.get("fetchedAtMs")?.as_i64()?,
    })
}

/// Sum every numeric field of one assistant message's `usage` block —
/// matches "sum the usage block" without hardcoding which vendor fields
/// (`input_tokens`, `output_tokens`, cache-creation/read counters, …) a
/// future Claude Code release adds or removes.
fn message_usage_tokens(usage: &Value) -> u64 {
    usage
        .as_object()
        .map(|fields| fields.values().filter_map(Value::as_u64).sum())
        .unwrap_or(0)
}

/// Sum every `assistant` message's `usage` block across one transcript's
/// JSONL lines, filtering to messages timestamped (epoch milliseconds) at
/// or after `since_epoch_ms` — callers pass "24h ago" for the behavior this
/// ports (`_claude_tokens_last_24h`). A line that fails to parse, or that
/// has no `assistant`-typed `message.usage` block, is skipped rather than
/// aborting the whole scan: one corrupt line must not blind the rest of
/// the transcript. A line with no usable `timestamp` field is kept (fail
/// open toward counting it) rather than silently excluded.
pub fn sum_assistant_tokens_since(transcript_jsonl: &str, since_epoch_ms: i64) -> u64 {
    transcript_jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|record| record.get("type").and_then(Value::as_str) == Some("assistant"))
        .filter(|record| {
            record
                .get("timestamp_ms")
                .and_then(Value::as_i64)
                .is_none_or(|ts| ts >= since_epoch_ms)
        })
        .filter_map(|record| record.get("message")?.get("usage").cloned())
        .map(|usage| message_usage_tokens(&usage))
        .sum()
}

/// Depth-first search for the first `total_token_usage` object — tolerant
/// of whatever envelope wraps it. Real evidence places this at
/// `payload.info.total_token_usage`, but this never assumes that exact
/// path. Mirrors Python's `limits._find_total_token_usage`.
fn find_total_token_usage(value: &Value) -> Option<&Value> {
    match value {
        Value::Object(fields) => {
            if let Some(found) = fields.get("total_token_usage") {
                if found.is_object() {
                    return Some(found);
                }
            }
            fields.values().find_map(find_total_token_usage)
        }
        Value::Array(items) => items.iter().find_map(find_total_token_usage),
        _ => None,
    }
}

/// One Codex session log's cumulative `(input_tokens, output_tokens)` —
/// the LAST `total_token_usage` reading the log's lines hold, read
/// newest-line-first. Mirrors Python's `limits._codex_session_tokens`:
/// a session's `total_token_usage` is already cumulative for the whole
/// session (`input_tokens` already includes `cached_input_tokens`), so the
/// newest reading already *is* the session's running total — never summed
/// a second time across lines within one file. `None` when no line in the
/// log carries a well-formed reading.
pub fn codex_session_tokens(log_jsonl: &str) -> Option<(u64, u64)> {
    for line in log_jsonl.lines().rev() {
        let line = line.trim();
        if line.is_empty() || !line.contains("total_token_usage") {
            continue;
        }
        let Ok(parsed) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(usage) = find_total_token_usage(&parsed) else {
            continue;
        };
        let input_tokens = usage.get("input_tokens").and_then(Value::as_u64);
        let output_tokens = usage.get("output_tokens").and_then(Value::as_u64);
        if let (Some(input_tokens), Some(output_tokens)) = (input_tokens, output_tokens) {
            return Some((input_tokens, output_tokens));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_a_well_formed_cached_usage_utilization_block() {
        let payload = json!({
            "cachedUsageUtilization": {
                "fetchedAtMs": 1_700_000_000_000i64,
                "utilization": {"five_hour": 12.5, "seven_day": 44.0},
            }
        });
        let parsed = parse_claude_utilization(&payload).unwrap();
        assert_eq!(parsed.five_hour_percent, 12.5);
        assert_eq!(parsed.seven_day_percent, 44.0);
        assert_eq!(parsed.fetched_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn cache_key_is_fetched_at_ms_not_a_file_mtime() {
        // This module never touches a file mtime at all; the struct's only
        // timestamp field is `fetched_at_ms`, read from the payload itself.
        let payload = json!({
            "cachedUsageUtilization": {
                "fetchedAtMs": 42,
                "utilization": {"five_hour": 1.0, "seven_day": 1.0},
            }
        });
        assert_eq!(
            parse_claude_utilization(&payload).unwrap().fetched_at_ms,
            42
        );
    }

    #[test]
    fn missing_block_fails_open_to_none() {
        assert!(parse_claude_utilization(&json!({})).is_none());
        assert!(parse_claude_utilization(&json!({"cachedUsageUtilization": {}})).is_none());
    }

    #[test]
    fn sums_assistant_usage_tokens_across_lines() {
        let transcript = [
            r#"{"type":"assistant","timestamp_ms":100,"message":{"usage":{"input_tokens":10,"output_tokens":5}}}"#,
            r#"{"type":"user","timestamp_ms":100,"message":{"usage":{"input_tokens":999}}}"#,
            r#"{"type":"assistant","timestamp_ms":200,"message":{"usage":{"input_tokens":3}}}"#,
        ]
        .join("\n");
        assert_eq!(sum_assistant_tokens_since(&transcript, 0), 18);
    }

    #[test]
    fn filters_out_messages_before_the_cutoff() {
        let transcript = [
            r#"{"type":"assistant","timestamp_ms":50,"message":{"usage":{"input_tokens":10}}}"#,
            r#"{"type":"assistant","timestamp_ms":500,"message":{"usage":{"input_tokens":20}}}"#,
        ]
        .join("\n");
        assert_eq!(sum_assistant_tokens_since(&transcript, 100), 20);
    }

    #[test]
    fn a_malformed_line_does_not_abort_the_whole_scan() {
        let transcript = [
            "not json at all",
            r#"{"type":"assistant","timestamp_ms":100,"message":{"usage":{"input_tokens":7}}}"#,
        ]
        .join("\n");
        assert_eq!(sum_assistant_tokens_since(&transcript, 0), 7);
    }

    #[test]
    fn codex_session_tokens_reads_the_newest_total_token_usage_reading() {
        let log = [
            r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":10,"output_tokens":2}}}}"#,
            r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":40,"output_tokens":9}}}}"#,
        ]
        .join("\n");
        assert_eq!(codex_session_tokens(&log), Some((40, 9)));
    }

    #[test]
    fn codex_session_tokens_tolerates_a_direct_envelope() {
        let log = r#"{"total_token_usage":{"input_tokens":5,"output_tokens":1}}"#;
        assert_eq!(codex_session_tokens(log), Some((5, 1)));
    }

    #[test]
    fn codex_session_tokens_skips_malformed_lines_and_keeps_scanning() {
        let log = [
            r#"{"type":"event_msg","payload":{"info":{"total_token_usage":{"input_tokens":3,"output_tokens":1}}}}"#,
            "not json at all",
        ]
        .join("\n");
        assert_eq!(codex_session_tokens(&log), Some((3, 1)));
    }

    #[test]
    fn codex_session_tokens_is_none_without_a_usable_reading() {
        assert_eq!(codex_session_tokens(""), None);
        assert_eq!(codex_session_tokens("not json at all"), None);
        assert_eq!(
            codex_session_tokens(r#"{"total_token_usage":{"input_tokens":"not a number"}}"#),
            None
        );
    }
}

//! Task directory parsing: `brief.md`, `result.md`, `status`, `revise`,
//! `retire`, and tolerance for whatever else a past agent dropped there.
//!
//! `mv-state::tasks` (crates/mv-state/src/tasks.rs, 3345 lines) has since
//! landed, but as an operational lifecycle module (sweep, lease expiry,
//! supersede, live Herdr agent queries) built to run against a live
//! fleet, not a structural auditor built to walk a static corpus
//! snapshot. Evaluated for the swap; decision is **no swap**:
//!
//! - Its closest equivalent, `read_status_verbose`, retries a read up to
//!   3 times to absorb a live truncate-then-write race. A `collect.sh`
//!   snapshot is never mid-write, so the retry buys nothing here, and its
//!   `StatusRead::Absent` on an unreadable file collapses "invalid UTF-8"
//!   into the same bucket as "file missing" — this runner's
//!   `classify_status` keeps `status.tolerated_invalid_utf8` as its own
//!   counted variant, a distinction `read_status_verbose` cannot make.
//! - There is no `mv-state::tasks` equivalent at all for `retire`-line
//!   shape (`<task-id-or-branch-ref> <40-hex sha>`) or for
//!   [`SANCTIONED_FILES`] tolerance — both are oracle-specific contracts,
//!   not something a production reader needs to validate about its own
//!   writes.
//!
//! No call site here depends on anything `mv-state::tasks` offers; this
//! file's own parsing stays as the independent check.

use std::path::Path;

use crate::finding::Report;

const SANCTIONED_FILES: &[&str] = &[
    "brief.md",
    "result.md",
    "status",
    "revise",
    "retire",
    "scratch-pane",
];

/// Loose task-id shape: an 8-digit date, one or more `-`-joined
/// lowercase/digit segments (old names are a single role word; Council
/// task dirs observed live add an account-short segment, e.g.
/// `20260907-council-cl-1bee5f`), ending in a 6-hex-digit suffix.
///
/// Also accepts the pre-parity Rust council mint
/// `{date}-council-{digest}-{seat}` (e.g. `20260907-council-1bee5f-cl`):
/// the digest sat before the account short, so a trailing-hex check
/// alone would reject on-disk history that still carries that order.
pub fn looks_like_task_id(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 8 || !bytes[..8].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let Some(rest) = s.get(8..) else { return false };
    let Some(rest) = rest.strip_prefix('-') else {
        return false;
    };
    let Some((head, last)) = rest.rsplit_once('-') else {
        return false;
    };
    if last.len() == 6 && last.bytes().all(|b| b.is_ascii_hexdigit()) {
        return true;
    }
    looks_like_legacy_rust_council_id(head, last)
}

/// `{date}-council-{6hex}-{seat}` where `seat` is a non-empty, not-
/// all-digit account short. All-digit last segments stay a retry
/// suffix (`-2`) for `check_task_dir`, not a seat.
fn looks_like_legacy_rust_council_id(head: &str, seat: &str) -> bool {
    if seat.is_empty()
        || !seat
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        || seat.bytes().all(|b| b.is_ascii_digit())
    {
        return false;
    }
    let Some((role, digest)) = head.rsplit_once('-') else {
        return false;
    };
    role == "council" && digest.len() == 6 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `multiverse/tasks.py:750-788`'s contract, per `spec-on-disk-contract.md`
/// §B: exact-match `done|blocked|failed` (case-folded, trimmed) is the
/// only way a status counts as terminal. Anything else — a concatenated
/// value like `donedone`, empty content, or invalid UTF-8 — folds to
/// "absent", not an error (`_read_task_status` retries then returns
/// `None`). So a malformed status file is never a [`Report::fail`] here;
/// it is always a counted variant, matching the reference's own tolerant
/// behavior.
fn classify_status(raw: &[u8]) -> &'static str {
    let Ok(text) = std::str::from_utf8(raw) else {
        return "status.tolerated_invalid_utf8";
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "done" => "status.done",
        "blocked" => "status.blocked",
        "failed" => "status.failed",
        "" => "status.tolerated_empty",
        _ => "status.tolerated_garbage",
    }
}

/// `retire`'s documented shape (`spec-on-disk-contract.md` §B):
/// `<stranded-task-id> <40-hex-char sha>` per non-blank line. Real fleet
/// data (a sampled project's `.multiverse/tasks/<task-id>/retire`) shows
/// the first field is actually the full branch ref (`multiverse/<task-id>`, the naming convention
/// `mv-state::worktrees`'s doc comment names), not a bare task id — the
/// spec doc's "task-id" wording was approximate. Accept either.
fn check_retire(path: &Path, content: &str, report: &mut Report) {
    for (idx, raw_line) in content.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(id), Some(sha), None) = (parts.next(), parts.next(), parts.next()) else {
            report.fail(
                path,
                Some(idx + 1),
                format!("retire line is not `<task-id> <sha>`: {line:?}"),
            );
            continue;
        };
        let bare_id = id.strip_prefix("multiverse/").unwrap_or(id);
        if !looks_like_task_id(bare_id) {
            report.fail(
                path,
                Some(idx + 1),
                format!("retire line's task id is malformed: {id:?}"),
            );
        } else if bare_id != id {
            report.note("tasks.retire_id_is_branch_ref");
        }
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            report.fail(
                path,
                Some(idx + 1),
                format!("retire line's sha is not 40 hex chars: {sha:?}"),
            );
        }
    }
}

/// Checks one task directory (either a project's own `tasks/<id>/`, or a
/// worktree's nested `.multiverse/tasks/<id>/` — same contract either
/// way). `dir_name` is the task id implied by the directory's own name,
/// used only to flag an obviously mismatched `retire`/`brief.md`, not
/// re-derived from file content.
pub fn check_task_dir(dir: &Path, dir_name: &str, report: &mut Report) {
    if looks_like_task_id(dir_name) {
        report.note("tasks.dir_name_task_id_shaped");
    } else if let Some((base, suffix)) = dir_name.rsplit_once('-') {
        // Real fleet data (sampled 2026-11-02; 137 occurrences across the
        // seven projects; synthetic examples: `20000101-reviewer-000001-2`,
        // `20000101-reviewer-000002-6`) shows a bare task id can carry a
        // trailing `-<retry number>` — a redispatch/retry counter, not
        // documented in `spec-on-disk-contract.md`. Counted distinctly
        // from a genuinely unshaped name rather than lumped in with it.
        if looks_like_task_id(base)
            && !suffix.is_empty()
            && suffix.bytes().all(|b| b.is_ascii_digit())
        {
            report.note("tasks.dir_name_task_id_with_retry_suffix");
        } else {
            report.note("tasks.dir_name_not_task_id_shaped");
        }
    } else {
        report.note("tasks.dir_name_not_task_id_shaped");
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if !SANCTIONED_FILES.contains(&name) {
            report.note("tasks.unsanctioned_file_tolerated");
            continue;
        }

        let Ok(bytes) = std::fs::read(&path) else {
            report.fail(&path, None, "could not read task file");
            continue;
        };

        match name {
            "status" => report.note(classify_status(&bytes)),
            "brief.md" => match String::from_utf8(bytes) {
                Ok(_) => report.note("tasks.brief_present"),
                Err(_) => report.fail(&path, None, "brief.md is not valid UTF-8"),
            },
            "result.md" => match String::from_utf8(bytes) {
                Ok(_) => report.note("tasks.result_present"),
                Err(_) => report.fail(&path, None, "result.md is not valid UTF-8"),
            },
            "revise" => match String::from_utf8(bytes) {
                Ok(text) => report.note(if text.trim().is_empty() {
                    "tasks.revise_empty"
                } else {
                    "tasks.revise_present"
                }),
                Err(_) => report.fail(&path, None, "revise is not valid UTF-8"),
            },
            "retire" => match String::from_utf8(bytes) {
                Ok(text) => {
                    report.note("tasks.retire_present");
                    check_retire(&path, &text, report);
                }
                Err(_) => report.fail(&path, None, "retire is not valid UTF-8"),
            },
            "scratch-pane" => report.note("tasks.scratch_pane_present"),
            _ => unreachable!("filtered by SANCTIONED_FILES above"),
        }
    }
}

/// Walks a project's `tasks/` directory, checking each task dir in turn.
pub fn check_tasks_root(tasks_root: &Path, report: &mut Report) {
    let Ok(entries) = std::fs::read_dir(tasks_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        check_task_dir(&path, &name, report);
    }
}

#[cfg(test)]
mod tests {
    use super::looks_like_task_id;

    #[test]
    fn accepts_python_council_seat_then_digest() {
        assert!(looks_like_task_id("20260907-council-cl-1bee5f"));
    }

    #[test]
    fn accepts_legacy_rust_council_digest_then_seat() {
        assert!(looks_like_task_id("20260907-council-1bee5f-cl"));
        assert!(looks_like_task_id("20261105-council-799a31-ca"));
    }

    #[test]
    fn accepts_an_ordinary_role_id() {
        assert!(looks_like_task_id("20261102-fixer-abc123"));
        assert!(looks_like_task_id("20000101-explorer-000001"));
    }

    #[test]
    fn rejects_a_retry_suffix_as_the_id_itself() {
        // `check_task_dir` classifies `…-2` as a retry of a shaped base,
        // so the bare id with the counter attached must stay unshaped.
        assert!(!looks_like_task_id("20000101-reviewer-000001-2"));
        assert!(!looks_like_task_id("20260907-council-1bee5f-cl-2"));
        assert!(!looks_like_task_id("20260907-council-cl-1bee5f-2"));
    }
}

//! `activity.cursor` / `.lastseen` / `.lastack` checks.
//!
//! `mv-state::activity` has since landed as a real module, with its own
//! `read_cursor`/`read_marker`/`read_lastack`. Evaluated for the swap;
//! decision is **no swap**, for the same oracle-independence reason
//! `routing_log.rs`'s module doc gives in detail, plus two concrete
//! mismatches found while checking:
//!
//! - `read_cursor` takes a `ProjectLog`, whose high-water `seq` comes from
//!   `mv_state::log`'s order-agnostic parser — calling it here would make
//!   this check depend on the exact parsing posture `routing_log.rs`
//!   decided against, instead of the high-water `seq` this runner already
//!   computes with its own strict parser.
//! - `read_marker`/`read_lastack` split on `'\t'` with `splitn(2/3, ..)`,
//!   so extra tab-separated fields past `<seq>` (or past
//!   `<count>\t<through_seq>`) fold into the trailing timestamp field
//!   silently; `check_lastseen`/`check_lastack` below require an exact
//!   field count and count any extra as malformed. Swapping would weaken
//!   what shows up as `activity.lastseen_tolerated_malformed` /
//!   `activity.lastack_tolerated_malformed` — a real divergence to
//!   notice even for files this runner already treats as tolerant,
//!   diagnostic-only.
//!
//! The cursor rule below is the one this lane's input audit
//! (`audit-mail-backlog-board-disk.md`, P1) corrected: a cursor ahead of
//! the log's own high-water `seq` is a log-reset/replacement generation
//! mismatch, not an empty inbox, and must fail closed exactly like a
//! malformed cursor — not be silently accepted as "nothing unread".

use std::path::Path;

use crate::finding::Report;

/// Checks `activity.cursor` against the routing.log high-water `seq`
/// already computed by [`crate::routing_log::check_routing_log`]. Missing
/// file is cursor 0 (no finding); malformed or out-of-range content is a
/// fail-closed finding, per the corrected spec text.
pub fn check_cursor(file: &Path, high_water_seq: u64, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let trimmed = text.trim_end_matches('\n');
    match trimmed.parse::<i64>() {
        Ok(n) if n < 0 => {
            report.fail(file, None, format!("activity.cursor is negative: {n}"));
        }
        Ok(n) if (n as u64) > high_water_seq => {
            report.fail(
                file,
                None,
                format!(
                    "activity.cursor={n} is ahead of routing.log's high-water seq={high_water_seq} — log reset/replacement, not an empty inbox"
                ),
            );
        }
        Ok(_) => report.note("activity.cursor_ok"),
        Err(_) => {
            report.fail(
                file,
                None,
                format!("activity.cursor is not an integer: {trimmed:?}"),
            );
        }
    }
}

/// `<seq>\t<iso>\n` — diagnostic only, tolerant of corruption
/// (`spec-on-disk-contract.md` §B, `tests/test_activity.py:1458`'s "a
/// corrupt marker file still lets inbox deliver mail"). A malformed file
/// is a counted variant, never a finding.
pub fn check_lastseen(file: &Path, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let fields: Vec<&str> = text.trim_end_matches('\n').split('\t').collect();
    let ok = fields.len() == 2 && fields[0].parse::<u64>().is_ok();
    report.note(if ok {
        "activity.lastseen_ok"
    } else {
        "activity.lastseen_tolerated_malformed"
    });
}

/// `<count>\t<seq>\t<iso>\n` — same tolerant posture as `.lastseen`.
pub fn check_lastack(file: &Path, report: &mut Report) {
    let Ok(text) = std::fs::read_to_string(file) else {
        return;
    };
    let fields: Vec<&str> = text.trim_end_matches('\n').split('\t').collect();
    let ok =
        fields.len() == 3 && fields[0].parse::<u64>().is_ok() && fields[1].parse::<u64>().is_ok();
    report.note(if ok {
        "activity.lastack_ok"
    } else {
        "activity.lastack_tolerated_malformed"
    });
}

//! The STATUS row: SEATS, OWED REPLIES, MAIL, and NEXUS LAST PASS.

use crate::accounts::{AccountSnapshot, AccountState};
use crate::layout::{
    box_bottom, box_text, box_top_with_title, card_row_dims, cell_width, ljust_visible, truncate,
    SECTION_CARD_MIN_WIDTH,
};

/// `workers_snapshot`-derived seat counts for the SEATS card.
/// `live_agents`/`open_seats`/`working` are `None` exactly when Herdr
/// could not be asked — `seat_cap` is config and stays known either way,
/// matching `board.py`'s `WorkersSnapshot.max_concurrent`, which is never
/// gated on `herdr_ok` (card `20261102-b-48f352`: the whole card used to
/// disappear instead of rendering with `?` placeholders).
#[derive(Debug, Clone)]
pub struct SeatsSnapshot {
    /// Panes whose Herdr `agent_status` is literally `working`.
    pub working: Option<u32>,
    /// Held slots (`workers.workers`) — live non-orchestrator agents.
    pub live_agents: Option<u32>,
    pub open_seats: Option<u32>,
    pub seat_cap: u32,
    pub unrecognized: Vec<String>,
}

/// One unanswered `asked` entry for the OWED REPLIES card.
///
/// `project` is the holder (who owes), `sender` is who is owed — the
/// `'<owes> owes <is-owed> a reply to #<seq> (<age>)'` line
/// `board.py`'s `_owed_reply_card_lines` prints.
#[derive(Debug, Clone)]
pub struct OwedReply {
    pub project: String,
    pub sender: String,
    pub seq: u64,
    pub wait_text: String,
    pub warn: bool,
}

/// Per-project unread mail for the MAIL card.
#[derive(Debug, Clone)]
pub struct MailEntry {
    pub project: String,
    pub unread: u32,
    pub last_look_age: Option<String>,
}

/// One project's `orchestrator-pass` read for the LAST CYCLE card.
/// Clock fields are already `_compact_elapsed` strings (`"3h"`, `"?"`
/// lives as `None`).
#[derive(Debug, Clone)]
pub struct LastCycleEntry {
    pub project: String,
    pub mail_at: Option<String>,
    pub reconcile_at: Option<String>,
    pub backlog_at: Option<String>,
    pub overdue: bool,
    pub parked: bool,
}

fn section_card(title: &str, lines: &[String], width: usize) -> Vec<String> {
    let mut out = vec![box_top_with_title(title, width)];
    if lines.is_empty() {
        out.push(box_text("(none)", width));
    } else {
        for line in lines {
            out.push(box_text(line, width));
        }
    }
    out.push(box_bottom(width));
    out
}

fn seats_card_lines(
    seats: &SeatsSnapshot,
    accounts: &[AccountSnapshot],
    width: usize,
) -> Vec<String> {
    let fmt = |value: Option<u32>| value.map_or_else(|| "?".to_string(), |n| n.to_string());
    let mut lines = vec![
        format!("{} working", fmt(seats.working)),
        format!("{} held / {} cap", fmt(seats.live_agents), seats.seat_cap),
    ];
    for account in accounts {
        if account.state == AccountState::Available {
            continue;
        }
        let symbol = account.state.symbol();
        let detail = if account.state == AccountState::HalfOpen {
            match &account.detail {
                Some(detail) if !detail.is_empty() => {
                    format!("{} {detail}", account.state.label())
                }
                _ => account.state.label().to_string(),
            }
        } else {
            account.detail.clone().unwrap_or_default()
        };
        let text = if detail.is_empty() {
            format!("{symbol} {}", account.name)
        } else {
            format!("{symbol} {} {detail}", account.name)
        };
        lines.push(text);
    }
    if !seats.unrecognized.is_empty() {
        lines.push(format!("unrecognized: {}", seats.unrecognized.join(", ")));
    }
    section_card("SEATS", &lines, width)
}

fn owed_reply_card_lines(replies: &[OwedReply], width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let lines: Vec<String> = if inner < 28 {
        let mut counts: Vec<(String, u32)> = Vec::new();
        for reply in replies {
            if let Some(entry) = counts.iter_mut().find(|(p, _)| p == &reply.project) {
                entry.1 += 1;
            } else {
                counts.push((reply.project.clone(), 1));
            }
        }
        counts
            .into_iter()
            .map(|(project, n)| format!("{project}: {n} owed"))
            .collect()
    } else {
        replies
            .iter()
            .map(|r| {
                let prefix = if r.warn { "⚠ " } else { "" };
                let head = format!("{prefix}{} owes {}", r.project, r.sender);
                let tail = format!("#{} ({})", r.seq, r.wait_text);
                let full = format!("{head} a reply to {tail}");
                let condensed = format!("{head} {tail}");
                if cell_width(&full) <= inner {
                    full
                } else if cell_width(&condensed) <= inner {
                    condensed
                } else {
                    truncate(&condensed, inner)
                }
            })
            .collect()
    };
    section_card(&format!("OWED REPLIES ({})", replies.len()), &lines, width)
}

fn mail_card_lines(mail: &[MailEntry], width: usize) -> Vec<String> {
    let total: u32 = mail.iter().map(|m| m.unread).sum();
    let lines: Vec<String> = mail
        .iter()
        .map(|m| {
            let since = m.last_look_age.as_deref().unwrap_or("never opened");
            format!("{}  {} · {since}", m.project, m.unread)
        })
        .collect();
    section_card(&format!("MAIL ({total})"), &lines, width)
}

fn last_cycle_age(c: &LastCycleEntry, narrow: bool) -> String {
    if narrow {
        [
            c.mail_at.as_deref(),
            c.reconcile_at.as_deref(),
            c.backlog_at.as_deref(),
        ]
        .into_iter()
        .flatten()
        .next()
        .unwrap_or("?")
        .to_string()
    } else {
        format!(
            "{} · {} · {}",
            c.mail_at.as_deref().unwrap_or("?"),
            c.reconcile_at.as_deref().unwrap_or("?"),
            c.backlog_at.as_deref().unwrap_or("?"),
        )
    }
}

fn last_cycle_card_lines(cycles: &[LastCycleEntry], width: usize) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let narrow = inner < 28;
    let mut lines: Vec<String> = Vec::new();
    if !narrow && !cycles.is_empty() {
        lines.push(
            "mail = last inbox read · reconcile/backlog = last sweep · ⚠ = overdue".to_string(),
        );
    }
    for c in cycles {
        let age = last_cycle_age(c, narrow);
        if c.parked {
            lines.push(format!("{}  ✗ parked  {age}", c.project));
        } else if narrow {
            let status = if c.overdue { "warn" } else { "✓" };
            lines.push(format!("{}  {status}  {age}", c.project));
        } else {
            let status = if c.overdue { "⚠ overdue" } else { "✓" };
            lines.push(format!("{}  {status}  {age}", c.project));
        }
    }
    let title = "NEXUS LAST PASS";
    section_card(title, &lines, width)
}

/// Render all four STATUS cards, wrapping to further rows of the same
/// column count once `width` cannot hold them all at
/// [`SECTION_CARD_MIN_WIDTH`] — `board.py`'s `_card_row_dims`.
pub fn render_status_cards(
    seats: &SeatsSnapshot,
    owed_replies: &[OwedReply],
    mail: &[MailEntry],
    last_cycle: &[LastCycleEntry],
    accounts: &[AccountSnapshot],
    total_width: usize,
) -> Vec<String> {
    let (cols, per_card) = card_row_dims(
        4,
        total_width,
        SECTION_CARD_MIN_WIDTH.min(total_width.max(1)),
    );
    let blocks = [
        seats_card_lines(seats, accounts, per_card),
        owed_reply_card_lines(owed_replies, per_card),
        mail_card_lines(mail, per_card),
        last_cycle_card_lines(last_cycle, per_card),
    ];
    let mut out = Vec::new();
    for row in blocks.chunks(cols.max(1)) {
        let height = row.iter().map(|c| c.len()).max().unwrap_or(0);
        let padded: Vec<Vec<String>> = row
            .iter()
            .map(|block| pad_card_block(block, height, per_card))
            .collect();
        for i in 0..height {
            let joined = padded
                .iter()
                .map(|c| c.get(i).cloned().unwrap_or_else(|| " ".repeat(per_card)))
                .collect::<Vec<_>>()
                .join(" ");
            out.push(ljust_visible(&joined, total_width));
        }
    }
    out
}

/// Stretch a boxed card to `height` by inserting blank interior lines
/// before its bottom border, so every card sharing an n-up row keeps
/// its `└…┘` on the same printed row. Mirrors `board.py`'s
/// `_pad_card_block` (the STATUS row used to space-fill *after* a short
/// card closed, so OWED's bottom sat above SEATS/MAIL).
fn pad_card_block(block: &[String], height: usize, width: usize) -> Vec<String> {
    let pad = height.saturating_sub(block.len());
    if pad == 0 || block.is_empty() {
        return block.to_vec();
    }
    let blank = box_text("", width);
    let mut out = Vec::with_capacity(height);
    out.extend_from_slice(&block[..block.len() - 1]);
    out.extend(std::iter::repeat_n(blank, pad));
    out.push(block[block.len() - 1].clone());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::cell_width;

    fn seats() -> SeatsSnapshot {
        SeatsSnapshot {
            working: Some(2),
            live_agents: Some(3),
            open_seats: Some(1),
            seat_cap: 4,
            unrecognized: vec![],
        }
    }

    #[test]
    fn seats_card_shows_question_marks_when_herdr_could_not_be_asked() {
        let seats = SeatsSnapshot {
            working: None,
            live_agents: None,
            open_seats: None,
            seat_cap: 4,
            unrecognized: vec![],
        };
        let lines = seats_card_lines(&seats, &[], 40);
        assert!(lines.iter().any(|l| l.contains("? working")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("? held / 4 cap")),
            "{lines:?}"
        );
    }

    #[test]
    fn status_cards_never_overflow_the_requested_width_at_any_size() {
        let replies = vec![OwedReply {
            project: "herdr-multiverse".into(),
            sender: "sandbox".into(),
            seq: 1,
            wait_text: "2h".into(),
            warn: false,
        }];
        let mail = vec![MailEntry {
            project: "herdr-multiverse".into(),
            unread: 2,
            last_look_age: Some("1h ago".into()),
        }];
        let cycles = vec![LastCycleEntry {
            project: "herdr-multiverse".into(),
            mail_at: None,
            reconcile_at: Some("14:00".into()),
            backlog_at: None,
            overdue: true,
            parked: false,
        }];
        for width in [20usize, 24, 40, 80, 120, 200] {
            let lines = render_status_cards(&seats(), &replies, &mail, &cycles, &[], width);
            for line in &lines {
                assert!(cell_width(line) <= width.max(SECTION_CARD_MIN_WIDTH) * 4 + 4);
            }
        }
    }

    #[test]
    fn owed_replies_falls_back_to_an_inbox_count_below_the_narrow_floor() {
        let replies = vec![
            OwedReply {
                project: "proj".into(),
                sender: "a".into(),
                seq: 1,
                wait_text: "1h".into(),
                warn: false,
            },
            OwedReply {
                project: "proj".into(),
                sender: "b".into(),
                seq: 2,
                wait_text: "2h".into(),
                warn: false,
            },
        ];
        let lines = owed_reply_card_lines(&replies, SECTION_CARD_MIN_WIDTH);
        assert!(lines.iter().any(|l| l.contains("2 owed")));
    }

    #[test]
    fn last_cycle_overdue_flag_survives_narrow_condensation() {
        let cycles = vec![LastCycleEntry {
            project: "proj".into(),
            mail_at: None,
            reconcile_at: None,
            backlog_at: None,
            overdue: true,
            parked: false,
        }];
        let lines = last_cycle_card_lines(&cycles, SECTION_CARD_MIN_WIDTH);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("warn") || l.contains('\u{26a0}')),
            "{lines:?}"
        );
    }

    #[test]
    fn last_cycle_card_title_says_nexus_at_wide_and_narrow_widths() {
        let cycles = vec![LastCycleEntry {
            project: "proj".into(),
            mail_at: None,
            reconcile_at: None,
            backlog_at: None,
            overdue: false,
            parked: false,
        }];
        for width in [SECTION_CARD_MIN_WIDTH, 80] {
            let lines = last_cycle_card_lines(&cycles, width);
            let rendered = lines.join("\n");
            assert!(rendered.contains("NEXUS LAST PASS"), "{rendered:?}");
            assert!(!rendered.to_lowercase().contains("orch"), "{rendered:?}");
        }
    }

    #[test]
    fn osc_52_in_a_project_or_agent_name_does_not_reach_status_output() {
        let seats = SeatsSnapshot {
            working: Some(1),
            live_agents: Some(1),
            open_seats: Some(1),
            seat_cap: 2,
            unrecognized: vec!["evil\u{1b}]52;c;EVIL\u{07}agent".into()],
        };
        let replies = vec![OwedReply {
            project: "p\u{1b}]0;pwned\u{07}roj".into(),
            sender: "x".into(),
            seq: 1,
            wait_text: "1h".into(),
            warn: false,
        }];
        let lines = render_status_cards(&seats, &replies, &[], &[], &[], 200);
        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains('\u{07}'), "{line:?}");
            assert!(!line.contains("]52;"), "{line:?}");
            assert!(!line.contains("pwned"), "{line:?}");
        }
        assert!(lines.iter().any(|l| l.contains("unrecognized")));
        assert!(lines.iter().any(|l| l.contains("evilagent")));
        assert!(lines.iter().any(|l| l.contains("proj")));
    }

    #[test]
    fn shorter_status_cards_keep_their_bottom_border_on_the_tallest_row() {
        // SEATS has two body lines; OWED is empty (`(none)` only); MAIL
        // has one. Without interior padding OWED closed a row early and
        // the joined line filled with spaces instead of `│   │`.
        let seats = SeatsSnapshot {
            working: Some(0),
            live_agents: Some(0),
            open_seats: Some(3),
            seat_cap: 3,
            unrecognized: vec![],
        };
        let mail = vec![MailEntry {
            project: "sandbox".into(),
            unread: 0,
            last_look_age: Some("offline (no live orchestrator)".into()),
        }];
        let lines = render_status_cards(&seats, &[], &mail, &[], &[], 80);
        let bottoms: Vec<&String> = lines.iter().filter(|l| l.contains('└')).collect();
        assert!(
            !bottoms.is_empty(),
            "expected a bottom border, got {lines:?}"
        );
        assert_eq!(
            bottoms[0].matches('└').count(),
            3,
            "first status row should close SEATS, OWED, and MAIL together, got {:?}",
            bottoms[0]
        );
    }
}

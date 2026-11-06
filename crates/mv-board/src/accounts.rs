//! The ACCOUNTS row: one boxed card per configured account.

use crate::layout::{
    box_bottom, box_text, box_top_with_corner, box_top_with_title, card_row_dims, cell_width,
    ljust_visible, pad_to, truncate, ACCOUNT_CARD_DETAIL_MIN_WIDTH, ACCOUNT_CARD_LIVE_MIN_WIDTH,
    ACCOUNT_CARD_MIN_WIDTH,
};

/// An account's availability label — always exactly one of these, derived
/// by the caller from the same checks a real dispatch makes (selector
/// availability/breaker/open-count, plus the draining flag).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountState {
    Available,
    AtCap,
    Fenced,
    Draining,
    HalfOpen,
}

impl AccountState {
    pub fn label(self) -> &'static str {
        match self {
            AccountState::Available => "AVAILABLE",
            AccountState::AtCap => "AT CAP",
            AccountState::Fenced => "FENCED",
            AccountState::Draining => "DRAINING",
            AccountState::HalfOpen => "HALF-OPEN",
        }
    }

    /// `board.py`'s `_STATE_SYMBOL`.
    pub fn symbol(self) -> &'static str {
        match self {
            AccountState::Available => "✓",
            AccountState::AtCap | AccountState::HalfOpen => "◐",
            AccountState::Fenced => "✗",
            AccountState::Draining => "◔",
        }
    }
}

/// One account's resolved data, as the caller will populate it from
/// `mv-engine::{selector, breaker, seatstate, usage}`.
#[derive(Debug, Clone)]
pub struct AccountSnapshot {
    pub name: String,
    /// Display model (`account.model`, else the Builder role's model for
    /// this kind, else `vendor`) — the right half of the top-border title
    /// `name · model`.
    pub model: String,
    pub vendor: String,
    pub kind: String,
    pub state: AccountState,
    /// Live open-agent count. `None` when Herdr could not be asked.
    pub seats_open: Option<u32>,
    pub seats_free: Option<u32>,
    pub seats_total: Option<u32>,
    pub over_cap: bool,
    /// e.g. `"resets in 12m"` or a FENCED/DRAINING reason.
    pub detail: Option<String>,
    /// One line per active quota window, e.g. `"5h: 40%"`.
    pub quota_windows: Vec<String>,
    /// 24h token usage, already formatted for display (`"41.2M in · 0.9M
    /// out"`). `None` renders as `"tok 24h n/a"`.
    pub tokens_24h: Option<String>,
    /// Snapshot clock, printed on the card's own top-border corner.
    pub as_of: Option<String>,
}

fn account_card_title(account: &AccountSnapshot) -> String {
    let model = if account.model.is_empty() {
        account.vendor.as_str()
    } else {
        account.model.as_str()
    };
    if model.is_empty() {
        account.name.clone()
    } else {
        format!("{} · {model}", account.name)
    }
}

/// Top border: `name · model` on the left, `'as of HH:MM'` on the right
/// once there is one — `board.py`'s `_account_card_top`. The model is
/// the first thing dropped as the card narrows, then the corner clock.
fn account_card_top(account: &AccountSnapshot, width: usize) -> String {
    let title = account_card_title(account);
    let corner = account
        .as_of
        .as_deref()
        .filter(|when| !when.is_empty())
        .map(|when| format!("as of {when}"))
        .unwrap_or_default();
    if corner.is_empty() {
        return box_top_with_title(&title, width);
    }
    if let Some(combined) = box_top_with_corner(&title, &corner, width) {
        return combined;
    }
    if !account.model.is_empty() {
        if let Some(combined) = box_top_with_corner(&account.name, &corner, width) {
            return combined;
        }
    }
    box_top_with_title(&title, width)
}

fn account_state_line(account: &AccountSnapshot) -> String {
    let symbol = account.state.symbol();
    if account.state == AccountState::AtCap {
        let open = account
            .seats_open
            .map_or_else(|| "?".to_string(), |n| n.to_string());
        let cap = account
            .seats_total
            .map_or_else(|| "?".to_string(), |n| n.to_string());
        format!("{symbol} SEATS FULL {open}/{cap}")
    } else {
        format!("{symbol} {}", account.state.label())
    }
}

fn account_card_lines(account: &AccountSnapshot, width: usize) -> Vec<String> {
    let mut lines = vec![account_card_top(account, width)];
    lines.push(box_text(&account_state_line(account), width));

    if width >= ACCOUNT_CARD_LIVE_MIN_WIDTH {
        // `seats_total` is config (the cap), known whether or not Herdr
        // could be asked; `seats_open` needs a live open-agent count and
        // is `None` exactly when Herdr was unreachable. Python prints
        // `"seats {open}/{cap} free {free}"` with `?` for the unknown
        // half, same as the SEATS status card's own placeholder.
        if let Some(total) = account.seats_total {
            let open = account
                .seats_open
                .map_or_else(|| "?".to_string(), |n| n.to_string());
            let free = account
                .seats_free
                .map_or_else(|| "?".to_string(), |n| n.to_string());
            let mut seats_text = format!("seats {open}/{total} free {free}");
            if account.over_cap {
                seats_text.push_str(" ⚠ over cap");
            }
            lines.push(box_text(&seats_text, width));
        }
    }
    if width >= ACCOUNT_CARD_DETAIL_MIN_WIDTH {
        if let Some(detail) = &account.detail {
            if !detail.is_empty() {
                lines.push(box_text(detail, width));
            }
        }
        for window in &account.quota_windows {
            lines.push(box_text(window, width));
        }
        let tokens = match &account.tokens_24h {
            Some(tokens) if !tokens.is_empty() => format!("tok 24h {tokens}"),
            _ => "tok 24h n/a".to_string(),
        };
        lines.push(box_text(&tokens, width));
    }
    lines.push(box_bottom(width));
    lines
}

/// The unboxed `"ACCOUNTS" ... "fleet N/M"` line printed above the account
/// cards — mirrors `board.py`'s `_section_header`: `right` (the dynamic
/// fleet total) is never truncated, `left` (the static "ACCOUNTS" label)
/// gives way first, down to dropped entirely, once `width` cannot hold
/// both plus a one-cell gap. `running`/`capacity` are `None` only when
/// Herdr could not be asked — `cap` itself is config, not Herdr, state, so
/// a live-listing failure alone must never blank it out.
pub fn account_section_header(running: Option<u32>, capacity: Option<u32>, width: usize) -> String {
    let right = format!(
        "fleet {}/{}",
        running.map_or_else(|| "?".to_string(), |n| n.to_string()),
        capacity.map_or_else(|| "?".to_string(), |n| n.to_string()),
    );
    let right = truncate(&right, width);
    let remaining = width.saturating_sub(cell_width(&right));
    if remaining == 0 {
        return pad_to(&right, width);
    }
    let left = truncate("ACCOUNTS", remaining.saturating_sub(1));
    let gap = remaining.saturating_sub(cell_width(&left)).max(1);
    pad_to(&format!("{left}{}{right}", " ".repeat(gap)), width)
}

/// Render one boxed card per account, as many per row as fit at
/// `ACCOUNT_CARD_MIN_WIDTH`, wrapping the rest onto further rows of the
/// same column count (`board.py`'s `_card_row_dims`/`_layout_card_blocks`)
/// rather than cramming every account into one row and losing seats/quota/
/// token detail to a floor-width squeeze — confirmed against the real
/// fleet's 6+ configured accounts, which did exactly that at the fixed
/// 80-column placeholder width (this task's `result.md`).
pub fn render_account_cards(accounts: &[AccountSnapshot], total_width: usize) -> Vec<String> {
    if accounts.is_empty() {
        return Vec::new();
    }
    let (cols, per_card) = card_row_dims(
        accounts.len(),
        total_width,
        ACCOUNT_CARD_MIN_WIDTH.min(total_width.max(1)),
    );
    let blocks: Vec<Vec<String>> = accounts
        .iter()
        .map(|a| account_card_lines(a, per_card))
        .collect();
    let blank = box_text("", per_card);
    let mut out = Vec::new();
    for row in blocks.chunks(cols.max(1)) {
        let height = row.iter().map(|c| c.len()).max().unwrap_or(0);
        let padded: Vec<_> = row
            .iter()
            .map(|card| {
                let mut card = card.clone();
                while card.len() < height {
                    card.insert(card.len() - 1, blank.clone());
                }
                card
            })
            .collect();
        for i in 0..height {
            let joined = padded
                .iter()
                .map(|card| card[i].clone())
                .collect::<Vec<_>>()
                .join(" ");
            out.push(ljust_visible(&joined, total_width));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::cell_width;

    fn account(name: &str) -> AccountSnapshot {
        AccountSnapshot {
            name: name.into(),
            model: "gpt-test".into(),
            vendor: "codex".into(),
            kind: "codex".into(),
            state: AccountState::Available,
            seats_open: Some(2),
            seats_free: Some(2),
            seats_total: Some(4),
            over_cap: false,
            detail: Some("resets in 12m".into()),
            quota_windows: vec!["5h: 40%".into()],
            tokens_24h: None,
            as_of: Some("14:32".into()),
        }
    }

    #[test]
    fn account_cards_pad_inside_the_border_to_equal_heights() {
        let mut short = account("claude");
        short.detail = None;
        let long = account("codex");
        let lines = render_account_cards(&[short, long], 80);
        let bottom = lines.last().unwrap();
        assert_eq!(
            bottom.chars().filter(|ch| *ch == '└').count(),
            2,
            "{lines:?}"
        );
        for line in &lines[..lines.len() - 1] {
            assert!(!line.contains('└'), "{lines:?}");
        }
    }

    #[test]
    fn account_cards_never_overflow_the_requested_width_at_any_size() {
        let accounts = vec![account("claude"), account("codex"), account("grok")];
        for width in [20usize, 22, 30, 60, 100, 200] {
            let lines = render_account_cards(&accounts, width);
            for line in &lines {
                assert!(cell_width(line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn six_accounts_wrap_to_further_rows_instead_of_cramming_one_row() {
        // The real fleet shape this task's result.md investigates: six
        // accounts at the CLI's 80-column placeholder width used to squeeze
        // every card below `ACCOUNT_CARD_LIVE_MIN_WIDTH`/`_DETAIL_MIN_WIDTH`,
        // dropping seats/quota/token lines everywhere. Wrapping to three
        // per row instead keeps every card at a legible width.
        let accounts: Vec<AccountSnapshot> = [
            "claude",
            "codex",
            "codex-work",
            "grok",
            "local",
            "claude-work",
        ]
        .iter()
        .map(|name| account(name))
        .collect();
        let lines = render_account_cards(&accounts, 80);
        for line in &lines {
            assert!(cell_width(line) <= 80, "{line:?}");
        }
        assert!(
            lines.iter().any(|l| l.contains("seats 2/4 free")),
            "wrapped cards should still clear the seats-line floor: {lines:?}"
        );
    }

    #[test]
    fn account_card_top_embeds_the_clock_without_changing_the_border_width() {
        let account = account("claude");
        for width in [10usize, 16, 22, 40, 60] {
            let top = account_card_top(&account, width);
            assert_eq!(cell_width(&top), width);
        }
    }

    #[test]
    fn account_card_names_the_model_and_tok_24h_n_a() {
        let lines = render_account_cards(&[account("explorer-seat")], 80);
        assert!(
            lines.iter().any(|l| l.contains("explorer-seat · gpt-test")),
            "{lines:?}"
        );
        assert!(lines.iter().any(|l| l.contains("✓ AVAILABLE")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("tok 24h n/a")), "{lines:?}");
        assert!(
            lines.iter().any(|l| l.contains("seats 2/4 free 2")),
            "{lines:?}"
        );
    }

    #[test]
    fn every_account_label_is_one_of_the_five_fixed_strings() {
        for state in [
            AccountState::Available,
            AccountState::AtCap,
            AccountState::Fenced,
            AccountState::Draining,
            AccountState::HalfOpen,
        ] {
            assert!(
                ["AVAILABLE", "AT CAP", "FENCED", "DRAINING", "HALF-OPEN"].contains(&state.label())
            );
        }
    }

    #[test]
    fn account_section_header_shows_fleet_running_over_capacity() {
        let header = account_section_header(Some(4), Some(15), 40);
        assert_eq!(cell_width(&header), 40);
        assert!(header.starts_with("ACCOUNTS"), "{header:?}");
        assert!(header.trim_end().ends_with("fleet 4/15"), "{header:?}");
    }

    #[test]
    fn account_section_header_reads_unknown_running_as_a_question_mark() {
        let header = account_section_header(None, Some(15), 40);
        assert!(header.contains("fleet ?/15"), "{header:?}");
    }

    #[test]
    fn account_section_header_never_exceeds_the_requested_width() {
        for width in 0..30 {
            let header = account_section_header(Some(4), Some(15), width);
            assert!(cell_width(&header) <= width, "width {width}: {header:?}");
        }
    }

    #[test]
    fn narrow_cards_drop_detail_but_keep_the_state_label() {
        let accounts = vec![account("claude")];
        let lines = render_account_cards(&accounts, ACCOUNT_CARD_MIN_WIDTH);
        assert!(lines.iter().any(|l| l.contains("AVAILABLE")));
    }

    #[test]
    fn a_capped_seats_line_survives_an_unknown_open_count() {
        let mut claude = account("claude");
        claude.seats_open = None;
        claude.seats_free = None;
        let lines = render_account_cards(&[claude], 80);
        assert!(
            lines.iter().any(|l| l.contains("seats ?/4 free ?")),
            "{lines:?}"
        );
    }

    #[test]
    fn osc_52_in_an_account_name_or_detail_does_not_reach_output() {
        let mut claude = account("claude");
        claude.name = "claude\u{1b}]52;c;EVIL\u{07}".into();
        claude.detail = Some("\u{1b}]0;pwned\u{07}resets in 12m".into());
        let lines = render_account_cards(&[claude], 80);
        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains('\u{07}'), "{line:?}");
            assert!(!line.contains("]52;"), "{line:?}");
            assert!(!line.contains("pwned"), "{line:?}");
        }
        assert!(lines.iter().any(|l| l.contains("claude")));
        assert!(lines.iter().any(|l| l.contains("resets in 12m")));
    }
}

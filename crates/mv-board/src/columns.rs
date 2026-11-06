//! Column packing, sort order, and the BLOCKED/IN PROGRESS/BACKLOG layout.

use std::cmp::Ordering;

use crate::card::{Card, PlacedCard};
use crate::layout::{
    box_bottom, box_text, box_text_visible, box_top_with_title, card_bottom, card_top,
    ljust_visible, sanitize_title, wrap_two, AUTO_TWO_UP_MIN_WIDTH, MIN_COLUMN_WIDTH,
    STACKED_FALLBACK_LIMIT,
};

const BLOCKED_EMPHASIS_ON: &str = "\u{1b}[1;31m";
const BLOCKED_EMPHASIS_OFF: &str = "\u{1b}[0m";

fn wait_cmp(a: Option<u64>, b: Option<u64>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(x), Some(y)) => y.cmp(&x),
    }
}

/// IN PROGRESS order: longest-waiting first, unknown wait last.
fn in_progress_cmp(a: &Card, b: &Card) -> Ordering {
    wait_cmp(a.wait_seconds, b.wait_seconds)
}

/// BLOCKED order: dispatched-blocked and asked-unanswered cards share one
/// `(acknowledged, priority, wait)` shape so they interleave in one list —
/// unacknowledged always sorts before acknowledged.
fn blocked_cmp(a: &Card, b: &Card) -> Ordering {
    a.acknowledged
        .cmp(&b.acknowledged)
        .then_with(|| a.priority.unwrap_or(0).cmp(&b.priority.unwrap_or(0)))
        .then_with(|| wait_cmp(a.wait_seconds, b.wait_seconds))
}

/// BACKLOG order: priority, then creation order. The only priority-sorted
/// column — callers populate `Card::priority` from `created_order` via the
/// snapshot before calling this, since `Card` doesn't carry creation order
/// itself (display-only).
fn backlog_cmp(a: &Card, b: &Card) -> Ordering {
    a.priority
        .unwrap_or(u8::MAX)
        .cmp(&b.priority.unwrap_or(u8::MAX))
}

pub fn sort_in_progress(cards: &mut [Card]) {
    cards.sort_by(in_progress_cmp);
}

pub fn sort_blocked(cards: &mut [Card]) {
    cards.sort_by(blocked_cmp);
}

pub fn sort_backlog(cards: &mut [Card]) {
    cards.sort_by(backlog_cmp);
}

/// Same orders as above, for the merged fleet view's `PlacedCard`s.
pub fn sort_in_progress_placed(cards: &mut [PlacedCard]) {
    cards.sort_by(|a, b| in_progress_cmp(&a.card, &b.card));
}

pub fn sort_blocked_placed(cards: &mut [PlacedCard]) {
    cards.sort_by(|a, b| blocked_cmp(&a.card, &b.card));
}

pub fn sort_backlog_placed(cards: &mut [PlacedCard]) {
    cards.sort_by(|a, b| backlog_cmp(&a.card, &b.card));
}

fn priority_line(card: &Card) -> Option<String> {
    let mut segments = Vec::new();
    if let Some(priority) = card.priority {
        segments.push(format!("P{priority}"));
    }
    if let Some(due) = &card.due_text {
        segments.push(due.clone());
    }
    if card.overdue {
        segments.push("OVERDUE".to_string());
    }
    if segments.is_empty() {
        None
    } else {
        Some(segments.join(" "))
    }
}

/// One work card's boxed lines, matching `board.py`'s `_card_box_parts`:
/// top-right clock, id line, wrapped title, optional agent, wrapped
/// status, priority/due/OVERDUE, and a bottom rail of role + project short.
fn card_lines(card: &Card, width: usize, project_short: Option<&str>) -> Vec<String> {
    let inner = width.saturating_sub(4);
    let mut content: Vec<String> = Vec::new();
    content.push(sanitize_title(&card.id_line));
    let title = sanitize_title(&card.title);
    if !title.is_empty() {
        let (l1, l2) = wrap_two(&title, inner);
        content.push(l1);
        if let Some(l2) = l2 {
            content.push(l2);
        }
    }
    if let Some(agent) = card.agent.as_deref().filter(|agent| !agent.is_empty()) {
        content.push(sanitize_title(agent));
    }
    if let Some(status) = &card.status_text {
        let (s1, s2) = wrap_two(status, inner);
        if !s1.is_empty() {
            content.push(s1);
        }
        if let Some(s2) = s2 {
            content.push(s2);
        }
    }
    if let Some(priority) = priority_line(card) {
        content.push(priority);
    }
    let clock = card.corner_clock.as_deref().unwrap_or("");
    let age = card.corner_age.as_deref().unwrap_or("");
    let mut lines = vec![card_top(width, clock, age)];
    lines.extend(content.into_iter().map(|line| box_text(&line, width)));
    lines.push(card_bottom(
        width,
        project_short,
        card.role.as_deref().unwrap_or(""),
    ));
    lines
}

/// Rendered line count for one card at `width` — used to budget a column
/// against an available `height` before it's worth a full pack pass.
pub fn card_height(card: &Card, width: usize) -> usize {
    card_lines(card, width, None).len()
}

/// One card as the column renderer sees it: the card itself, optional
/// project color, optional project short for the bottom-right rail.
struct CardRef<'a> {
    card: &'a Card,
    color: Option<u8>,
    project_short: Option<&'a str>,
}

fn colorize(code: u8, s: &str) -> String {
    format!("\u{1b}[{code}m{s}\u{1b}[0m")
}

/// Width of each of `n` equal columns laid side by side with a single
/// space between neighbors, so `n * column_width + (n - 1) <= total_width`.
pub(crate) fn n_up_column_width(total_width: usize, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    total_width.saturating_sub(n - 1) / n
}

/// Width of each of `cols` cards sharing one row within a column, mirroring
/// `board.py`'s `_n_up_card_width`: `one_up_width` divided `cols` ways,
/// minus the `cols - 1` one-cell gaps between them, floored at 1 so a
/// cramped layout still renders rather than underflowing.
fn n_up_card_width(one_up_width: usize, cols: usize) -> usize {
    let cols = cols.max(1);
    (one_up_width.saturating_sub(cols - 1) / cols).max(1)
}

/// `--cards-per-row {auto,1,2}` (`board.py`'s `_resolve_cards_per_row`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CardsPerRow {
    #[default]
    Auto,
    One,
    Two,
}

impl CardsPerRow {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "1" => Some(Self::One),
            "2" => Some(Self::Two),
            _ => None,
        }
    }
}

/// `--status {blocked,in-progress,backlog}` column keys, in the board's
/// fixed left-to-right order. Filtering drops columns; it does not reorder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoardColumn {
    Blocked,
    InProgress,
    Backlog,
}

impl BoardColumn {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "blocked" => Some(Self::Blocked),
            "in-progress" => Some(Self::InProgress),
            "backlog" => Some(Self::Backlog),
            _ => None,
        }
    }

    fn header(self) -> &'static str {
        match self {
            Self::Blocked => "BLOCKED",
            Self::InProgress => "IN PROGRESS",
            Self::Backlog => "BACKLOG",
        }
    }
}

/// Which of the three columns `--status` kept. An all-false filter (no
/// `--status` given) shows every column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusFilter {
    pub blocked: bool,
    pub in_progress: bool,
    pub backlog: bool,
}

impl Default for StatusFilter {
    fn default() -> Self {
        Self::all()
    }
}

impl StatusFilter {
    pub fn all() -> Self {
        Self {
            blocked: true,
            in_progress: true,
            backlog: true,
        }
    }

    pub fn from_keys(keys: impl IntoIterator<Item = BoardColumn>) -> Self {
        let mut filter = Self {
            blocked: false,
            in_progress: false,
            backlog: false,
        };
        let mut any = false;
        for key in keys {
            any = true;
            match key {
                BoardColumn::Blocked => filter.blocked = true,
                BoardColumn::InProgress => filter.in_progress = true,
                BoardColumn::Backlog => filter.backlog = true,
            }
        }
        if any {
            filter
        } else {
            Self::all()
        }
    }

    fn selected(self) -> Vec<BoardColumn> {
        let wanted = [
            BoardColumn::Blocked,
            BoardColumn::InProgress,
            BoardColumn::Backlog,
        ];
        wanted
            .into_iter()
            .filter(|column| match column {
                BoardColumn::Blocked => self.blocked,
                BoardColumn::InProgress => self.in_progress,
                BoardColumn::Backlog => self.backlog,
            })
            .collect()
    }
}

/// `--cards-per-row auto`'s own resolution: two-up once each card would
/// still clear [`AUTO_TWO_UP_MIN_WIDTH`], else one-per-row. `1` and `2`
/// force that lane count regardless of width (`board.py`'s same three
/// modes).
fn resolve_cards_per_row(mode: CardsPerRow, one_up_width: usize) -> usize {
    match mode {
        CardsPerRow::One => 1,
        CardsPerRow::Two => 2,
        CardsPerRow::Auto => {
            if n_up_card_width(one_up_width, 2) >= AUTO_TWO_UP_MIN_WIDTH {
                2
            } else {
                1
            }
        }
    }
}

/// `block` (a card's own boxed lines) stretched to `height` by inserting
/// blank interior lines before its bottom border, so every card sharing an
/// n-up row keeps its bottom border aligned even when another card's title
/// wrapped to a second line. Mirrors `board.py`'s `_pad_card_block`.
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

/// `blocks` (each already boxed at `card_width`, one per card, with its own
/// color baked into every line already) laid out `cols` per row, row-major:
/// card 1 through `cols` on the first row, the rest on further rows of the
/// same `row_width` — never column-major, so a column's own wait/priority
/// sort still reads left-to-right then down. Mirrors `board.py`'s
/// `_layout_card_blocks`.
fn layout_card_blocks(
    blocks: &[Vec<String>],
    colors: &[Option<u8>],
    cols: usize,
    card_width: usize,
    row_width: usize,
) -> Vec<String> {
    let cols = cols.max(1);
    let mut out = Vec::new();
    for (group, color_group) in blocks.chunks(cols).zip(colors.chunks(cols)) {
        let height = group.iter().map(Vec::len).max().unwrap_or(0);
        let padded: Vec<Vec<String>> = group
            .iter()
            .zip(color_group)
            .map(|(block, color)| {
                let padded_block = pad_card_block(block, height, card_width);
                match color {
                    Some(code) => padded_block.iter().map(|l| colorize(*code, l)).collect(),
                    None => padded_block,
                }
            })
            .collect();
        for row in 0..height {
            let joined = padded
                .iter()
                .map(|p| p[row].as_str())
                .collect::<Vec<_>>()
                .join(" ");
            out.push(ljust_visible(&joined, row_width));
        }
    }
    out
}

/// A column's cards laid out `cols` per row within `row_width` — a lone
/// card in an otherwise-`cols`-wide row renders at its own narrower lane
/// width (`n_up_card_width`), not stretched to fill the row: the row it
/// shares with nothing else is still padded out to `row_width`, but the
/// card's own box stays the width every full row's lane would have given
/// it. Mirrors `board.py`'s `_n_up_rows`.
fn n_up_rows(cards: &[CardRef<'_>], row_width: usize, cols: usize) -> Vec<String> {
    let card_width = n_up_card_width(row_width, cols);
    let blocks: Vec<Vec<String>> = cards
        .iter()
        .map(|placed| card_lines(placed.card, card_width, placed.project_short))
        .collect();
    let colors: Vec<Option<u8>> = cards.iter().map(|placed| placed.color).collect();
    layout_card_blocks(&blocks, &colors, cols, card_width, row_width)
}

/// One column's top border, body lines, and bottom border, mirroring
/// `board.py`'s `_column_parts`. Kept apart from the top-level renderer so
/// the side-by-side case can pad every column's body to the same height
/// before closing its box — padding after the bottom border was written
/// would leave it stranded mid-column instead of at the foot.
///
/// `cards_per_row` is resolved here, once per column, from this column's
/// own width — the same width every column in one board shares (a
/// side-by-side board's columns are already equal width; a stacked
/// board's recurse to the same full `col_width` for each), so every
/// column still ends up with the one shared cards-per-row a real terminal
/// would show.
fn column_parts(
    header: &str,
    cards: &[CardRef<'_>],
    col_width: usize,
    limit: usize,
    emphasize: bool,
    resolved: usize,
    budget_rows: Option<usize>,
) -> (String, Vec<String>, String) {
    let inner_width = col_width.saturating_sub(4).max(1);

    let shown_count = budget_rows.map_or_else(
        || limit.min(cards.len()),
        |budget| pack_for_budget(cards, inner_width, resolved, budget),
    );
    let shown = &cards[..shown_count];
    let overflow = cards.len() - shown_count;

    let mut body: Vec<String> = Vec::new();
    if shown.is_empty() {
        body.push(box_text("(none)", col_width));
    } else {
        body.extend(
            n_up_rows(shown, inner_width, resolved)
                .into_iter()
                .map(|line| box_text_visible(&line, col_width)),
        );
    }
    if overflow > 0 {
        body.push(box_text(&format!("+{overflow} more"), col_width));
    }

    let mut top = box_top_with_title(header, col_width);
    let mut bottom = box_bottom(col_width);
    if emphasize {
        top = format!("{BLOCKED_EMPHASIS_ON}{top}{BLOCKED_EMPHASIS_OFF}");
        bottom = format!("{BLOCKED_EMPHASIS_ON}{bottom}{BLOCKED_EMPHASIS_OFF}");
    }
    (top, body, bottom)
}

/// Layout knobs for [`render_columns_with`] / [`render_columns_merged_with`].
#[derive(Debug, Clone, Copy)]
pub struct ColumnLayout {
    pub total_width: usize,
    pub limit: usize,
    /// Auto packing budget, including the column's top and bottom borders.
    pub height: Option<usize>,
    pub color: bool,
    pub cards_per_row: CardsPerRow,
    pub statuses: StatusFilter,
}

/// Shared implementation behind [`render_columns`] and
/// [`render_columns_merged`] — both just adapt their own card type into
/// `(&Card, Option<u8>)` pairs (a merged card's own project color, or
/// `None` for a single-project board) and hand them here.
///
/// `statuses` drops columns the way `board.py`'s `_selected_columns` does:
/// a single remaining column uses the full `total_width`, not a third of
/// it. `cards_per_row` is resolved once from that shared column width.
fn render_columns_generic(
    blocked: &[CardRef<'_>],
    in_progress: &[CardRef<'_>],
    backlog: &[CardRef<'_>],
    layout: ColumnLayout,
) -> Vec<String> {
    let selected = layout.statuses.selected();
    if selected.is_empty() {
        return Vec::new();
    }
    let n = selected.len();
    let column_for = |column: BoardColumn| -> (&'static str, &[CardRef<'_>], bool) {
        match column {
            BoardColumn::Blocked => (column.header(), blocked, layout.color),
            BoardColumn::InProgress => (column.header(), in_progress, false),
            BoardColumn::Backlog => (column.header(), backlog, false),
        }
    };
    if n > 1 && layout.total_width < n * MIN_COLUMN_WIDTH + n - 1 {
        let mut out = Vec::new();
        for column in selected {
            let (_, cards, _) = column_for(column);
            let mut child = layout;
            child.statuses = StatusFilter::from_keys([column]);
            child.height = layout.height.map(|height| height / n);
            if let Some(height) = child.height {
                let inner = layout.total_width.saturating_sub(4).max(1);
                let lanes = resolve_cards_per_row(layout.cards_per_row, inner);
                let width = n_up_card_width(inner, lanes);
                let first_height = cards
                    .iter()
                    .take(lanes)
                    .map(|placed| card_height(placed.card, width))
                    .max()
                    .unwrap_or(0);
                if !cards.is_empty() && first_height > height.saturating_sub(2) {
                    child.height = None;
                    child.limit = STACKED_FALLBACK_LIMIT.min(crate::layout::DEFAULT_LIMIT);
                }
            }
            out.extend(render_columns_generic(blocked, in_progress, backlog, child));
        }
        return out;
    }
    let column_width = n_up_column_width(layout.total_width, n);
    let budget_rows = layout.height.map(|height| height.saturating_sub(2));
    let adaptive = if layout.cards_per_row == CardsPerRow::Auto && layout.height.is_some() {
        auto_column_dimensions(
            &selected
                .iter()
                .map(|column| column_for(*column).1)
                .collect::<Vec<_>>(),
            layout.total_width,
            budget_rows.unwrap_or(0),
        )
    } else {
        None
    };
    let parts: Vec<_> = selected
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let (header, cards, emphasize) = column_for(*column);
            let (width, lanes) = adaptive
                .as_ref()
                .map(|dimensions| dimensions[index])
                .unwrap_or_else(|| {
                    (
                        column_width,
                        resolve_cards_per_row(
                            layout.cards_per_row,
                            column_width.saturating_sub(4).max(1),
                        ),
                    )
                });
            let limit = if layout.total_width < MIN_COLUMN_WIDTH && layout.height.is_none() {
                STACKED_FALLBACK_LIMIT
            } else {
                layout.limit
            };
            let (top, body, bottom) =
                column_parts(header, cards, width, limit, emphasize, lanes, budget_rows);
            (top, body, bottom, width)
        })
        .collect();
    let height = parts
        .iter()
        .map(|(_, body, _, _)| body.len())
        .max()
        .unwrap_or(0);
    let padded: Vec<_> = parts
        .iter()
        .map(|(_, body, _, width)| {
            let mut body = body.clone();
            body.resize(height, box_text("", *width));
            body
        })
        .collect();
    let mut lines = vec![parts
        .iter()
        .map(|(top, _, _, _)| top.as_str())
        .collect::<Vec<_>>()
        .join(" ")];
    for row in 0..height {
        lines.push(
            padded
                .iter()
                .map(|body| body[row].as_str())
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    lines.push(
        parts
            .iter()
            .map(|(_, _, bottom, _)| bottom.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    lines
}

/// Greedy whole-row packing, then reserve the overflow footer only when needed.
fn pack_for_budget(cards: &[CardRef<'_>], width: usize, lanes: usize, budget: usize) -> usize {
    let first = pack_n_up(cards, width, lanes, budget);
    if first == cards.len() {
        first
    } else {
        pack_n_up(cards, width, lanes, budget.saturating_sub(1))
    }
}

fn pack_n_up(cards: &[CardRef<'_>], width: usize, lanes: usize, budget: usize) -> usize {
    let lanes = lanes.max(1);
    let card_width = n_up_card_width(width, lanes);
    let mut used = 0;
    let mut count = 0;
    for group in cards.chunks(lanes) {
        let height = group
            .iter()
            .map(|placed| card_height(placed.card, card_width))
            .max()
            .unwrap_or(0);
        if used + height > budget {
            break;
        }
        used += height;
        count += group.len();
    }
    count
}

/// Give earlier columns only the lanes they need, and the last the remainder.
fn auto_column_dimensions(
    columns: &[&[CardRef<'_>]],
    width: usize,
    budget_rows: usize,
) -> Option<Vec<(usize, usize)>> {
    let n = columns.len();
    let budget = width.checked_add(1)?.checked_sub(4 * n)?;
    let lane_budget = budget / (AUTO_TWO_UP_MIN_WIDTH + 1);
    if lane_budget < n {
        return None;
    }
    let card_width = budget / lane_budget - 1;
    let mut assigned = 0;
    let mut widths = 0;
    let mut dimensions = Vec::new();
    for (index, cards) in columns.iter().enumerate() {
        let reserved = n - index - 1;
        let lanes = if reserved == 0 {
            lane_budget - assigned
        } else {
            let cap = (lane_budget - assigned - reserved).clamp(1, 2);
            (1..=cap)
                .find(|lanes| {
                    let row_width = card_width * lanes + lanes - 1;
                    pack_n_up(cards, row_width, *lanes, budget_rows) == cards.len()
                })
                .unwrap_or(cap)
        };
        let col_width = if reserved == 0 {
            width - widths - (n - 1)
        } else {
            4 + lanes * card_width + lanes - 1
        };
        dimensions.push((col_width, lanes));
        widths += col_width;
        assigned += lanes;
    }
    Some(dimensions)
}

fn placed_as_refs(cards: &[PlacedCard], color: bool) -> Vec<CardRef<'_>> {
    cards
        .iter()
        .map(|placed| CardRef {
            card: &placed.card,
            color: if color { placed.color } else { None },
            project_short: placed.project_short.as_deref(),
        })
        .collect()
}

/// `render_columns`, for the merged fleet view's `PlacedCard`s.
pub fn render_columns_merged(
    blocked: &[PlacedCard],
    in_progress: &[PlacedCard],
    backlog: &[PlacedCard],
    total_width: usize,
    limit: usize,
    color: bool,
) -> Vec<String> {
    render_columns_merged_with(
        blocked,
        in_progress,
        backlog,
        ColumnLayout {
            height: None,
            total_width,
            limit,
            color,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        },
    )
}

pub fn render_columns_merged_with(
    blocked: &[PlacedCard],
    in_progress: &[PlacedCard],
    backlog: &[PlacedCard],
    layout: ColumnLayout,
) -> Vec<String> {
    render_columns_generic(
        &placed_as_refs(blocked, layout.color),
        &placed_as_refs(in_progress, layout.color),
        &placed_as_refs(backlog, layout.color),
        layout,
    )
}

/// Lay BLOCKED/IN PROGRESS/BACKLOG out side by side when `total_width`
/// clears three `MIN_COLUMN_WIDTH`-wide columns, else stack them. If even
/// the stacked form is too narrow to be useful, cards-per-column drops to
/// `STACKED_FALLBACK_LIMIT` instead of the caller's ordinary `limit`.
fn card_as_refs(cards: &[Card]) -> Vec<CardRef<'_>> {
    cards
        .iter()
        .map(|card| CardRef {
            card,
            color: None,
            project_short: None,
        })
        .collect()
}

pub fn render_columns(
    blocked: &[Card],
    in_progress: &[Card],
    backlog: &[Card],
    total_width: usize,
    limit: usize,
    color: bool,
) -> Vec<String> {
    render_columns_with(
        blocked,
        in_progress,
        backlog,
        ColumnLayout {
            height: None,
            total_width,
            limit,
            color,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        },
    )
}

pub fn render_columns_with(
    blocked: &[Card],
    in_progress: &[Card],
    backlog: &[Card],
    layout: ColumnLayout,
) -> Vec<String> {
    render_columns_generic(
        &card_as_refs(blocked),
        &card_as_refs(in_progress),
        &card_as_refs(backlog),
        layout,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::card::{backlog_card, BacklogItemSnapshot, BacklogStatus};
    use crate::layout::{cell_width, visible_width};

    fn item(priority: u8, title: &str) -> BacklogItemSnapshot {
        BacklogItemSnapshot {
            id: "20261102-b-000000".into(),
            title: title.into(),
            priority,
            created_order: 0,
            due_text: None,
            overdue: false,
            role_hint: None,
            status: BacklogStatus::Open,
            has_human_annotation: false,
            elapsed_seconds: None,
            corner_clock: None,
            corner_age: None,
        }
    }

    #[test]
    fn row53_auto_packs_whole_rows_reserves_footer_and_shares_width() {
        let cards: Vec<Card> = (0..15)
            .map(|i| backlog_card(&item(1, &format!("card-{i:02}"))))
            .collect();
        let mut layout = ColumnLayout {
            total_width: 240,
            height: Some(30),
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        };
        let lines = render_columns_with(&[], &[], &cards, layout);
        assert!(
            lines.iter().any(|line| line.contains("card-14")),
            "last column gets spare lanes"
        );
        assert!(lines.len() <= 30);
        assert!(lines.iter().all(|line| cell_width(line) == 240));
        layout.cards_per_row = CardsPerRow::Two;
        layout.statuses = StatusFilter::from_keys([BoardColumn::Backlog]);
        layout.height = Some(13);
        let lines = render_columns_with(&[], &[], &cards, layout);
        assert!(
            lines.iter().any(|line| line.contains("+11 more")),
            "two complete card rows and one overflow footer: {lines:?}"
        );
        assert!(lines.len() <= 13);
        layout.total_width = 80;
        layout.height = Some(90);
        layout.statuses = StatusFilter::all();
        assert!(render_columns_with(&[], &[], &cards, layout).len() <= 90);
    }

    #[test]
    fn render_never_overflows_common_terminal_widths() {
        let cards: Vec<Card> = (0..5)
            .map(|i| backlog_card(&item(i, "a reasonably long backlog title here")))
            .collect();
        for width in [40usize, 80, 100, 120, 200] {
            let lines = render_columns(&[], &[], &cards, width, 10, false);
            for line in &lines {
                assert!(cell_width(line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn render_never_crashes_on_pathological_widths() {
        let cards: Vec<Card> = vec![backlog_card(&item(0, "x"))];
        for width in [0usize, 1, 2, 3, 4, 5] {
            let _ = render_columns(&cards, &cards, &cards, width, 10, true);
        }
    }

    #[test]
    fn every_line_is_the_same_cell_width_when_side_by_side() {
        let cards: Vec<Card> = vec![backlog_card(&item(0, "one")), backlog_card(&item(1, "two"))];
        let lines = render_columns(&cards, &cards, &cards, 150, 10, false);
        let width = cell_width(&lines[0]);
        for line in &lines {
            assert_eq!(cell_width(line), width);
        }
    }

    #[test]
    fn every_column_closes_with_its_own_bottom_border() {
        // Side by side (150 clears three 40-wide columns): the whole board
        // closes with one joined bottom-border row.
        let wide = render_columns(&[], &[], &[], 150, 10, false);
        assert!(wide.last().unwrap().contains('└'));
        // Stacked (80 does not): each of the three columns gets its own
        // closing border row. `ends_with('┘')` (not merely `contains('└')`)
        // rules out a card's own embedded bottom border, which never sits
        // at the end of a padded body row.
        let narrow = render_columns(&[], &[], &[], 80, 10, false);
        assert_eq!(narrow.iter().filter(|l| l.ends_with('┘')).count(), 3);
    }

    #[test]
    fn a_lone_card_in_a_two_up_row_renders_narrower_than_its_column() {
        // At the board's current fixed width (80), BLOCKED/IN PROGRESS/
        // BACKLOG always stack (80 < 3*40+2), and a stacked column's own
        // one-up width (76) clears `AUTO_TWO_UP_MIN_WIDTH` — so even a
        // single card packs two-up, at the narrower per-lane width
        // (`n_up_card_width(76, 2) == 37`), not stretched to the column's
        // own 80-cell width. This is `board.py`'s own behavior (a 4-card
        // BLOCKED column reduced to one narrow lane next to a blank one),
        // which this task ported.
        let mut asked = BacklogItemSnapshot {
            status: BacklogStatus::Asked,
            ..item(0, "which account should we use?")
        };
        asked.has_human_annotation = false;
        let card = backlog_card(&asked);
        let lines = render_columns(&[card], &[], &[], 80, 10, false);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("└────────────── asked ──────────────┘")),
            "{lines:#?}"
        );
    }

    #[test]
    fn two_cards_share_one_two_up_row_at_the_default_width() {
        let cards: Vec<Card> = vec![
            backlog_card(&item(0, "first")),
            backlog_card(&item(1, "second")),
        ];
        let lines = render_columns(&[], &[], &cards, 80, 10, false);
        // Both cards' titles land on the very same output row — proof
        // they packed two-up (row-major) rather than one-per-row.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("first") && l.contains("second")),
            "{lines:#?}"
        );
    }

    #[test]
    fn blocked_column_emphasis_survives_column_stacking_at_narrow_widths() {
        let blocked: Vec<Card> = vec![backlog_card(&item(0, "needs a human"))];
        let narrow = render_columns(&blocked, &[], &[], 30, 10, true);
        assert!(narrow.iter().any(|l| l.contains(BLOCKED_EMPHASIS_ON)));
        let wide = render_columns(&blocked, &[], &[], 150, 10, true);
        assert!(wide.iter().any(|l| l.contains(BLOCKED_EMPHASIS_ON)));
    }

    #[test]
    fn blocked_border_is_emphasized_even_when_blocked_is_empty() {
        let lines = render_columns(&[], &[], &[], 150, 10, true);
        assert!(lines[0].contains(BLOCKED_EMPHASIS_ON));
    }

    #[test]
    fn backlog_sorts_by_priority_then_creation_order() {
        let mut cards: Vec<Card> = vec![
            backlog_card(&item(2, "low priority")),
            backlog_card(&item(0, "high priority")),
        ];
        sort_backlog(&mut cards);
        assert_eq!(cards[0].title, "high priority");
    }

    #[test]
    fn an_unanswered_asked_card_closes_with_the_centered_asked_footer() {
        let mut asked = BacklogItemSnapshot {
            status: BacklogStatus::Asked,
            ..item(0, "which account should we use?")
        };
        asked.has_human_annotation = false;
        let card = backlog_card(&asked);
        let lines = card_lines(&card, 37, None);
        assert_eq!(
            lines.last().unwrap(),
            "└────────────── asked ──────────────┘"
        );
        assert!(
            lines.iter().any(|l| l.contains("20261102-b-000000")),
            "id line must render: {lines:#?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("P0")),
            "priority line must render: {lines:#?}"
        );
    }

    #[test]
    fn an_old_acknowledged_card_does_not_bury_a_newer_unacknowledged_one() {
        let mut old_ack = backlog_card(&item(0, "old, answered"));
        old_ack.acknowledged = true;
        let mut new_unack = backlog_card(&item(0, "new, unanswered"));
        new_unack.acknowledged = false;
        let mut cards = vec![old_ack, new_unack];
        sort_blocked(&mut cards);
        assert_eq!(cards[0].title, "new, unanswered");
    }

    #[test]
    fn stacked_fallback_uses_the_narrow_limit_not_the_default() {
        let cards: Vec<Card> = (0..5).map(|i| backlog_card(&item(i, "card"))).collect();
        let lines = render_columns(&[], &[], &cards, 20, 10, false);
        let more_line = lines.iter().find(|l| l.contains("+3 more"));
        assert!(more_line.is_some(), "expected a +3 more line: {lines:?}");
    }

    #[test]
    fn merged_view_colors_each_card_by_its_own_project() {
        let placed = vec![
            PlacedCard {
                card: backlog_card(&item(0, "[proj-a] task one")),
                project_short: Some("proj-a".into()),
                color: Some(92),
                agent_color: None,
            },
            PlacedCard {
                card: backlog_card(&item(0, "[proj-b] task two")),
                project_short: Some("proj-b".into()),
                color: Some(93),
                agent_color: None,
            },
        ];
        let lines = render_columns_merged(&[], &[], &placed, 150, 10, true);
        assert!(lines.iter().any(|l| l.contains("\u{1b}[92m")));
        assert!(lines.iter().any(|l| l.contains("\u{1b}[93m")));
    }

    #[test]
    fn merged_view_never_overflows_common_terminal_widths() {
        let placed: Vec<PlacedCard> = (0..4)
            .map(|i| PlacedCard {
                card: backlog_card(&item(i, "a merged fleet card title")),
                project_short: Some("proj".into()),
                color: Some(92),
                agent_color: None,
            })
            .collect();
        for width in [40usize, 80, 150] {
            let lines = render_columns_merged(&placed, &placed, &placed, width, 10, true);
            for line in &lines {
                assert!(visible_width(line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn merged_view_rows_stay_byte_bound_even_two_up_and_colored() {
        // Two differently-colored cards sharing one two-up row must still
        // produce a line whose *visible* width (ANSI stripped) matches
        // every other line's — not a raw byte count inflated by escapes.
        let placed = vec![
            PlacedCard {
                card: backlog_card(&item(0, "alpha")),
                project_short: Some("a".into()),
                color: Some(92),
                agent_color: None,
            },
            PlacedCard {
                card: backlog_card(&item(0, "bravo")),
                project_short: Some("b".into()),
                color: Some(93),
                agent_color: None,
            },
        ];
        let lines = render_columns_merged(&[], &[], &placed, 80, 10, true);
        let data_line = lines
            .iter()
            .find(|l| l.contains("alpha") && l.contains("bravo"))
            .expect("alpha and bravo should share one two-up row");
        assert_eq!(visible_width(data_line), 80);
    }

    #[test]
    fn status_filter_drops_columns_and_a_single_column_uses_full_width() {
        let cards: Vec<Card> = vec![backlog_card(&item(0, "only-backlog"))];
        let all = render_columns(&[], &[], &cards, 150, 10, false);
        assert!(all.iter().any(|l| l.contains("BLOCKED")));
        assert!(all.iter().any(|l| l.contains("IN PROGRESS")));
        assert!(all.iter().any(|l| l.contains("BACKLOG")));

        let filtered = render_columns_with(
            &[],
            &[],
            &cards,
            ColumnLayout {
                height: None,
                total_width: 150,
                limit: 10,
                color: false,
                cards_per_row: CardsPerRow::Auto,
                statuses: StatusFilter::from_keys([BoardColumn::Backlog]),
            },
        );
        assert!(filtered.iter().any(|l| l.contains("BACKLOG")));
        assert!(filtered.iter().all(|l| !l.contains("BLOCKED")));
        assert!(filtered.iter().all(|l| !l.contains("IN PROGRESS")));
        let top = filtered
            .iter()
            .find(|l| l.contains("BACKLOG"))
            .expect("backlog header");
        assert_eq!(visible_width(top), 150);
    }

    #[test]
    fn cards_per_row_one_keeps_cards_stacked_when_auto_would_go_two_up() {
        let cards: Vec<Card> = vec![
            backlog_card(&item(0, "first-card-title")),
            backlog_card(&item(1, "second-card-title")),
        ];
        let two_up = render_columns_with(
            &[],
            &[],
            &cards,
            ColumnLayout {
                height: None,
                total_width: 150,
                limit: 10,
                color: false,
                cards_per_row: CardsPerRow::Two,
                statuses: StatusFilter::from_keys([BoardColumn::Backlog]),
            },
        );
        let one_up = render_columns_with(
            &[],
            &[],
            &cards,
            ColumnLayout {
                height: None,
                total_width: 150,
                limit: 10,
                color: false,
                cards_per_row: CardsPerRow::One,
                statuses: StatusFilter::from_keys([BoardColumn::Backlog]),
            },
        );
        let two_up_shares = two_up
            .iter()
            .any(|l| l.contains("first-card-title") && l.contains("second-card-title"));
        let one_up_shares = one_up
            .iter()
            .any(|l| l.contains("first-card-title") && l.contains("second-card-title"));
        assert!(
            two_up_shares,
            "forced two-up should share a row: {two_up:?}"
        );
        assert!(!one_up_shares, "forced one-up should stack: {one_up:?}");
    }

    #[test]
    fn osc_52_in_a_card_title_does_not_reach_output() {
        let card = backlog_card(&item(0, "hello\u{1b}]52;c;QUJDRA==\u{07}world"));
        let lines = render_columns(&[], &[], &[card], 80, 10, false);
        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains('\u{07}'), "{line:?}");
            assert!(!line.contains("]52;"), "{line:?}");
        }
        assert!(lines
            .iter()
            .any(|l| l.contains("hello") && l.contains("world")));
    }

    #[test]
    fn osc_52_in_due_text_and_role_does_not_reach_output() {
        let mut asked = item(0, "safe title");
        asked.due_text = Some("\u{1b}]52;c;EVIL\u{07}tomorrow".into());
        asked.role_hint = Some("\u{1b}]0;pwned\u{07}Builder".into());
        let card = backlog_card(&asked);
        let lines = card_lines(&card, 40, None);
        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains('\u{07}'), "{line:?}");
            assert!(!line.contains("]52;"), "{line:?}");
            assert!(!line.contains("pwned"), "{line:?}");
        }
        assert!(lines.iter().any(|l| l.contains("tomorrow")));
        assert!(lines.iter().any(|l| l.contains("Builder")));
    }

    #[test]
    fn csi_erase_in_a_title_cannot_clear_the_screen() {
        let card = backlog_card(&item(0, "keep\u{1b}[2Jme"));
        let lines = card_lines(&card, 40, None);
        for line in &lines {
            assert!(!line.contains('\u{1b}'), "{line:?}");
            assert!(!line.contains("[2J"), "{line:?}");
        }
        assert!(lines.iter().any(|l| l.contains("keep") && l.contains("me")));
    }
}

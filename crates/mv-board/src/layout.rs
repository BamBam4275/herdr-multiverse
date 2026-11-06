//! Cell-width math, truncation/wrapping, and box drawing.
//!
//! Python measures card text with `unicodedata.east_asian_width`/`combining`
//! (wide/fullwidth = 2 cells, combining marks = 0, everything else 1), never
//! `len()`. `unicode-width` already implements the same UAX #11 rule, so we
//! use it instead of hand-rolling the East-Asian-width table.

use unicode_width::UnicodeWidthChar;

/// Minimum column width before `render_columns` falls back to stacking.
pub const MIN_COLUMN_WIDTH: usize = 40;

/// `--cards-per-row auto` only goes two-up once each card clears this.
pub const AUTO_TWO_UP_MIN_WIDTH: usize = 34;

/// Default number of cards shown per column before "+N more".
pub const DEFAULT_LIMIT: usize = 10;

/// Limit used when even the stacked fallback is starved for width.
pub const STACKED_FALLBACK_LIMIT: usize = 2;

/// Shared floor for the ACCOUNTS cards.
pub const ACCOUNT_CARD_MIN_WIDTH: usize = 22;
pub const ACCOUNT_CARD_LIVE_MIN_WIDTH: usize = 24;
pub const ACCOUNT_CARD_DETAIL_MIN_WIDTH: usize = 22;

/// Shared floor for the SEATS/OWED REPLIES/MAIL/LAST CYCLE cards.
pub const SECTION_CARD_MIN_WIDTH: usize = 20;

/// A pane considered abandoned past this many minutes.
pub const STALE_AFTER_MINUTES: i64 = 30;

/// The real terminal width behind this process's stdout, or `None` when
/// neither `COLUMNS` nor the underlying `ioctl` can answer (piped,
/// redirected, or off a tty) — the one piece of Python's
/// `shutil.get_terminal_size(fallback=(80, 24)).columns` this crate can
/// answer on its own, since only `mv-board` carries a crossterm
/// dependency (`mv-cli`'s own module doc on `commands::board` explains why
/// `cmd_board` cannot query this itself). `shutil.get_terminal_size` reads
/// `COLUMNS` from the environment first and only falls through to the
/// real tty query when it is absent or not a positive integer, so a
/// caller (or a test fixture) that exports `COLUMNS` wins over whatever
/// the actual terminal reports, same as Python. The `(80, 24)` fallback
/// itself is left to the caller rather than baked in here, so a caller
/// wanting a different fallback is not forced to carry this crate's
/// opinion about it (card `20261102-b-2c4922`).
pub fn terminal_width() -> Option<usize> {
    terminal_width_from(std::env::var("COLUMNS").ok().as_deref())
}

fn terminal_width_from(columns: Option<&str>) -> Option<usize> {
    columns
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|columns| *columns > 0)
        .or_else(|| {
            crossterm::terminal::size()
                .ok()
                .map(|(columns, _rows)| columns as usize)
        })
}

/// Terminal rows, with the same positive `LINES` override as Python's terminal probe.
pub fn terminal_height() -> Option<usize> {
    std::env::var("LINES")
        .ok()
        .and_then(|rows| rows.parse::<usize>().ok())
        .filter(|rows| *rows > 0)
        .or_else(|| {
            crossterm::terminal::size()
                .ok()
                .map(|(_, rows)| rows as usize)
        })
}

/// True when the board should emit SGR color. `--no-color` wins; a
/// non-empty `NO_COLOR` env var (<https://no-color.org/>) also disables
/// color, regardless of its value.
pub fn color_enabled(cli_no_color: bool) -> bool {
    color_enabled_from(cli_no_color, std::env::var("NO_COLOR").ok().as_deref())
}

fn color_enabled_from(cli_no_color: bool, no_color: Option<&str>) -> bool {
    if cli_no_color {
        return false;
    }
    !matches!(no_color, Some(value) if !value.is_empty())
}

/// How many of `n` equal-width cards fit on one row of `width` cells, and
/// each card's own width — mirrors `board.py`'s `_card_row_dims`: as many
/// columns as fit at `min_width`, folding toward fewer (wrapping the rest
/// onto further rows of that same column count, the caller's job) only
/// once `width` cannot hold that many at `min_width`. Never fewer than one
/// column, even when `width` alone is still short of `min_width` at that
/// point — degrades to a narrower card rather than dropping one.
pub fn card_row_dims(n: usize, width: usize, min_width: usize) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let mut cols = n;
    while cols > 1 && width.saturating_sub(cols - 1) / cols < min_width {
        cols -= 1;
    }
    let card_width = (width.saturating_sub(cols - 1) / cols).max(1);
    (cols, card_width)
}

/// Cell width of a single character, matching Python's `_char_width`.
pub fn char_width(c: char) -> usize {
    if c.is_control() {
        return 0;
    }
    UnicodeWidthChar::width(c).unwrap_or(0)
}

/// Cell width of a string: the sum of each character's cell width, not
/// `str::len()` or `chars().count()`.
pub fn cell_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

/// Skip a CSI sequence (`…` through a final byte in 0x40–0x7E).
fn skip_csi(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) {
    for c in chars.by_ref() {
        if ('\u{40}'..='\u{7e}').contains(&c) {
            break;
        }
    }
}

/// Skip an OSC/DCS/APC/PM/SOS string until BEL, ST (`ESC \`), or 8-bit ST.
fn skip_until_st_or_bel(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) {
    while let Some(c) = chars.next() {
        match c {
            '\u{07}' | '\u{9c}' => break,
            '\u{1b}' if chars.peek() == Some(&'\\') => {
                chars.next();
                break;
            }
            _ => {}
        }
    }
}

/// Consume the rest of an ISO-6429 sequence after the ESC introducer.
fn skip_after_esc(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>) {
    match chars.next() {
        Some('[') => skip_csi(chars),
        Some(']') | Some('P') | Some('X') | Some('^') | Some('_') => skip_until_st_or_bel(chars),
        Some(_) | None => {}
    }
}

fn skip_c1(chars: &mut std::iter::Peekable<impl Iterator<Item = char>>, introducer: char) {
    match introducer {
        '\u{9b}' => skip_csi(chars),
        '\u{90}' | '\u{98}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => skip_until_st_or_bel(chars),
        _ => {}
    }
}

fn is_c1_introducer(c: char) -> bool {
    matches!(
        c,
        '\u{90}' | '\u{98}' | '\u{9b}' | '\u{9d}' | '\u{9e}' | '\u{9f}'
    )
}

fn is_line_separator(c: char) -> bool {
    c == '\u{2028}' || c == '\u{2029}'
}

/// Strip CSI/OSC/DCS (and 8-bit C1) sequences, then fold leftover control
/// characters. Titles collapse every control — including newline — to a
/// space so a card cannot shear the rows below it. Multiline dumps keep
/// `\n` and `\t`; `\r` is dropped in front of `\n` and otherwise becomes
/// a space, so a carriage-return overwrite cannot hide earlier text.
fn sanitize_chars(s: &str, keep_newlines: bool) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_after_esc(&mut chars);
            continue;
        }
        if is_c1_introducer(c) {
            skip_c1(&mut chars, c);
            continue;
        }
        if c == '\r' {
            if keep_newlines && chars.peek() == Some(&'\n') {
                continue;
            }
            out.push(' ');
            continue;
        }
        if c == '\n' || c == '\t' {
            if keep_newlines {
                out.push(c);
            } else {
                out.push(' ');
            }
            continue;
        }
        if c.is_control() || is_line_separator(c) {
            out.push(if keep_newlines && is_line_separator(c) {
                '\n'
            } else {
                ' '
            });
            continue;
        }
        out.push(c);
    }
    out
}

/// Strip ANSI/OSC sequences and fold remaining controls to spaces, so a
/// title (or any other single-line untrusted field) cannot inject
/// clipboard writes, title changes, or a literal newline that shears
/// the rows below it.
pub fn sanitize_title(s: &str) -> String {
    sanitize_chars(s, false)
}

/// Strip ANSI/OSC sequences from a multi-line dump (result.md, pane
/// text, inspect output) while keeping real newlines.
pub fn sanitize_multiline(s: &str) -> String {
    sanitize_chars(s, true)
}

/// Clip `s` to at most `max_width` cells, appending `"…"` when it had to
/// clip. Untrusted input is sanitized first so a zero-width ESC sequence
/// cannot ride through the clip unchanged. Never returns a string wider
/// than `max_width`.
pub fn truncate(s: &str, max_width: usize) -> String {
    let s = sanitize_title(s);
    if max_width == 0 {
        return String::new();
    }
    if cell_width(&s) <= max_width {
        return s;
    }
    if max_width == 1 {
        return "…".to_string();
    }
    let budget = max_width - 1;
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = char_width(c);
        if used + w > budget {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

/// Cell width ignoring ANSI escape sequences. Counts SGR used by this
/// crate's own colorizer, and also skips OSC/DCS/C1 so a leaked sequence
/// cannot inflate or deflate the measured width.
pub fn visible_width(s: &str) -> usize {
    let mut width = 0;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            skip_after_esc(&mut chars);
            continue;
        }
        if is_c1_introducer(c) {
            skip_c1(&mut chars, c);
            continue;
        }
        width += char_width(c);
    }
    width
}

/// Pad or clip `s` to exactly `width` cells, left-aligned.
pub fn pad_to(s: &str, width: usize) -> String {
    let clipped = truncate(s, width);
    let pad = width.saturating_sub(cell_width(&clipped));
    format!("{clipped}{}", " ".repeat(pad))
}

/// Right-pad `s` to `width` cells, measuring with [`visible_width`] so a
/// string that already carries SGR color codes (an n-up row joining
/// colored card blocks) still lands on the right column. Never truncates —
/// mirrors Python's `_cell_ljust`, used only where the caller's own math
/// already guarantees `s` fits.
pub fn ljust_visible(s: &str, width: usize) -> String {
    let pad = width.saturating_sub(visible_width(s));
    format!("{s}{}", " ".repeat(pad))
}

/// One boxed content line, `width` cells wide, for a line that may already
/// carry embedded SGR color codes — [`box_text`]'s ANSI-aware twin, needed
/// once a column body line can be a joined, per-card-colored n-up row
/// rather than always-plain text. Matches `board.py`'s `_box_text`: one
/// space of padding inside each pipe (`"│ " … " │"`), inner width
/// `width - 4`, with the same degenerate floors at `width < 4`.
pub fn box_text_visible(line: &str, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "│".to_string(),
        2 | 3 => format!("│{}│", " ".repeat(width - 2)),
        n => {
            let inner = n - 4;
            format!("│ {} │", ljust_visible(line, inner))
        }
    }
}

/// Split `s` at the last space within `budget` cells, or at the raw cell
/// width when no space is found, returning `(first, rest)`.
pub fn break_at_width(s: &str, budget: usize) -> (String, String) {
    if cell_width(s) <= budget {
        return (s.to_string(), String::new());
    }
    let mut last_space_byte: Option<usize> = None;
    let mut used = 0usize;
    let mut cut_byte = s.len();
    let mut cut_set = false;
    for (idx, c) in s.char_indices() {
        let w = char_width(c);
        if used + w > budget {
            cut_byte = idx;
            cut_set = true;
            break;
        }
        if c == ' ' {
            last_space_byte = Some(idx);
        }
        used += w;
    }
    if !cut_set {
        cut_byte = s.len();
    }
    if let Some(space_idx) = last_space_byte {
        let first = s[..space_idx].to_string();
        let rest = s[space_idx + 1..].to_string();
        (first, rest)
    } else {
        let first = s[..cut_byte].to_string();
        let rest = s[cut_byte..].to_string();
        (first, rest)
    }
}

/// Wrap `s` onto at most two lines of `width` cells each, truncating the
/// second line (with an ellipsis) rather than producing a third.
pub fn wrap_two(s: &str, width: usize) -> (String, Option<String>) {
    let sanitized = sanitize_title(s);
    if cell_width(&sanitized) <= width {
        return (sanitized, None);
    }
    let (first, rest) = break_at_width(&sanitized, width);
    if rest.is_empty() {
        return (first, None);
    }
    (first, Some(truncate(&rest, width)))
}

/// Top border of a box `width` cells wide. Degenerates gracefully instead
/// of overflowing when `width` is too small for a real border.
pub fn box_top(width: usize) -> String {
    box_border(width, '┌', '┐')
}

/// Bottom border of a box `width` cells wide.
pub fn box_bottom(width: usize) -> String {
    box_border(width, '└', '┘')
}

/// Top border of a box `width` cells wide, with ` title ` embedded near
/// the left edge — a column's own boxed heading (`"┌─ BLOCKED ────…┐"`),
/// mirroring `board.py`'s `_box_top(title, width)`: exactly one dash
/// before the label, the rest after, so the label reads as left-aligned
/// rather than centered the way [`box_bottom_with_label`]'s footer is.
/// Falls back to the plain, unlabeled border below `width < 5` (no room
/// for even `"┌ … ┐"`), and truncates the title before that floor once it
/// alone would not fit.
pub fn box_top_with_title(title: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    if width < 5 {
        return box_top(width);
    }
    let title = sanitize_title(title);
    let mut label = format!(" {title} ");
    if cell_width(&label) > width - 3 {
        label = format!(" {} ", truncate(&title, (width - 5).max(1)));
    }
    let dashes = (width - 2).saturating_sub(cell_width(&label));
    let left = usize::from(dashes > 0);
    let right = dashes - left;
    format!("┌{}{label}{}┐", "─".repeat(left), "─".repeat(right))
}

/// Bottom border of a box `width` cells wide, with ` label ` centered in
/// it (same total cell width as a plain [`box_bottom`]) — the "asked"
/// footer on a BLOCKED backlog card. Falls back to a plain border when
/// there isn't room for the label plus at least one dash on each side.
pub fn box_bottom_with_label(label: &str, width: usize) -> String {
    if width < 6 {
        return box_bottom(width);
    }
    let inner = width - 2;
    let max_label = inner.saturating_sub(4);
    if max_label == 0 {
        return box_bottom(width);
    }
    let label = truncate(label, max_label);
    let middle = format!(" {label} ");
    let used = cell_width(&middle);
    if used + 2 > inner {
        return box_bottom(width);
    }
    let remaining = inner - used;
    let left = remaining / 2;
    let right = remaining - left;
    format!("└{}{middle}{}┘", "─".repeat(left), "─".repeat(right))
}

fn box_border(width: usize, left: char, right: char) -> String {
    match width {
        0 => String::new(),
        1 => left.to_string(),
        n => {
            let mut s = String::with_capacity(n);
            s.push(left);
            s.push_str(&"─".repeat(n - 2));
            s.push(right);
            s
        }
    }
}

/// One boxed content line, `width` cells wide: `"│ " + padded/clipped
/// text + " │"`, matching `board.py`'s `_box_text`. Below `width < 4`
/// there is no room for both pipes and their padding spaces, so this
/// degrades the same way `_box_top`/`_box_bottom` do rather than
/// overflowing.
pub fn box_text(line: &str, width: usize) -> String {
    match width {
        0 => String::new(),
        1 => "│".to_string(),
        2 | 3 => format!("│{}│", " ".repeat(width - 2)),
        n => {
            let inner = n - 4;
            format!("│ {} │", pad_to(line, inner))
        }
    }
}

/// `board.py`'s `_box_top_with_corner`: a titled top border with a
/// second, right-aligned label (`"┌─ explorer-seat · gpt-test ── as of
/// 04:53 ┐"`). `None` when `width` cannot hold both labels with at least
/// one dash after `"┌"` and at least one between them — the caller then
/// drops the least essential piece (the model, then the corner).
pub fn box_top_with_corner(title: &str, corner: &str, width: usize) -> Option<String> {
    if width < 5 {
        return None;
    }
    let inner = width - 2;
    let title = sanitize_title(title);
    let corner = sanitize_title(corner);
    let left_label = format!(" {title} ");
    let right_label = format!(" {corner} ");
    let left_w = cell_width(&left_label);
    let right_w = cell_width(&right_label);
    if left_w + right_w + 2 > inner {
        return None;
    }
    let dashes = inner - left_w - right_w;
    let lead = 1;
    let mid = dashes - lead;
    Some(format!(
        "┌{}{left_label}{}{right_label}┐",
        "─".repeat(lead),
        "─".repeat(mid)
    ))
}

/// A work card's own top border, carrying `_card_corner`'s `(time, age)`
/// pair right-aligned — e.g. `'┌────── 04:08 · 12m ┐'`. Empty `time_text`
/// draws a plain [`box_top`]. Age drops first as `width` narrows, then
/// the whole label, matching `board.py`'s `_card_top`.
pub fn card_top(width: usize, time_text: &str, age_text: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "┌".to_string();
    }
    let time_text = sanitize_title(time_text);
    let age_text = sanitize_title(age_text);
    if time_text.is_empty() {
        return box_top(width);
    }
    let inner = width - 2;
    let mut label = if age_text.is_empty() {
        format!(" {time_text} ")
    } else {
        format!(" {time_text} · {age_text} ")
    };
    if cell_width(&label) > inner.saturating_sub(1) {
        label = format!(" {time_text} ");
        if cell_width(&label) > inner.saturating_sub(1) {
            return box_top(width);
        }
    }
    let dashes = inner.saturating_sub(cell_width(&label));
    format!("┌{}{label}┐", "─".repeat(dashes))
}

/// A work card's bottom border: the role centred, the project's `short`
/// at the right corner. Labels drop rather than squeeze when the width
/// will not hold them, and the role goes first because the project is
/// the one a merged board cannot do without (`board.py`'s
/// `_card_bottom_segments`).
pub fn card_bottom(width: usize, short: Option<&str>, role: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "└".to_string();
    }
    if width == 2 {
        return "└┘".to_string();
    }
    let inner = width - 2;
    let short = short.map(sanitize_title).filter(|s| !s.is_empty());
    let role = sanitize_title(role);
    let mut short_label = match short {
        Some(short) => format!(" {short} "),
        None => String::new(),
    };
    let mut role_label = if role.is_empty() {
        String::new()
    } else {
        format!(" {role} ")
    };
    if cell_width(&role_label) + cell_width(&short_label) + 2 > inner {
        role_label.clear();
    }
    if cell_width(&short_label) + 2 > inner {
        short_label.clear();
    }
    let region = inner.saturating_sub(cell_width(&short_label));
    let lead = if role_label.is_empty() {
        region
    } else {
        (region.saturating_sub(cell_width(&role_label)) / 2).max(1)
    };
    let trail = region
        .saturating_sub(cell_width(&role_label))
        .saturating_sub(lead);
    format!(
        "└{}{role_label}{}{short_label}┘",
        "─".repeat(lead),
        "─".repeat(trail)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_width_never_panics_with_or_without_a_real_tty() {
        // `cargo test` has no tty, so this is almost always `None` here —
        // the point is only that querying it is infallible from the
        // caller's perspective, matching `shutil.get_terminal_size`'s own
        // "always returns, falls back on failure" contract.
        let _ = terminal_width_from(None);
    }

    #[test]
    fn terminal_width_honors_columns_env_var_like_shutil_get_terminal_size() {
        assert_eq!(terminal_width_from(Some("200")), Some(200));
    }

    #[test]
    fn terminal_width_ignores_a_non_positive_or_unparseable_columns() {
        assert_ne!(terminal_width_from(Some("0")), Some(0));
        assert_ne!(terminal_width_from(Some("not-a-number")), Some(0));
    }

    #[test]
    fn card_row_dims_wraps_to_fewer_columns_once_min_width_would_be_violated() {
        // Six accounts at width 80, floor 22: 6/5/4 columns each undercut
        // the floor, 3 columns gives (80-2)/3 = 26, which clears it.
        assert_eq!(card_row_dims(6, 80, 22), (3, 26));
    }

    #[test]
    fn card_row_dims_never_drops_below_one_column() {
        assert_eq!(card_row_dims(4, 10, 22), (1, 10));
    }

    #[test]
    fn card_row_dims_keeps_everything_on_one_row_when_it_fits() {
        assert_eq!(card_row_dims(2, 80, 22), (2, 39));
    }

    #[test]
    fn card_row_dims_of_zero_cards_is_zero_by_zero() {
        assert_eq!(card_row_dims(0, 80, 22), (0, 0));
    }

    #[test]
    fn truncate_never_exceeds_max_width() {
        for width in 0..40 {
            let s = "a lane card title that runs on for a very long while";
            assert!(cell_width(&truncate(s, width)) <= width);
        }
    }

    #[test]
    fn truncate_leaves_short_strings_untouched() {
        assert_eq!(truncate("short", 20), "short");
    }

    #[test]
    fn cjk_title_width_is_two_cells_per_glyph() {
        // Each CJK glyph below is double-width, so four of them is 8 cells.
        assert_eq!(cell_width("ソートカ"), 8);
        assert!(cell_width(&truncate("ソートカード", 5)) <= 5);
    }

    #[test]
    fn box_top_never_exceeds_the_requested_width() {
        for width in 0..64 {
            assert!(cell_width(&box_top(width)) <= width, "width {width}");
        }
    }

    #[test]
    fn box_bottom_never_exceeds_the_requested_width() {
        for width in 0..64 {
            assert!(cell_width(&box_bottom(width)) <= width, "width {width}");
        }
    }

    #[test]
    fn box_bottom_with_label_centers_the_label_at_the_recorded_width() {
        assert_eq!(
            box_bottom_with_label("asked", 37),
            "└────────────── asked ──────────────┘"
        );
    }

    #[test]
    fn box_bottom_with_label_never_exceeds_the_requested_width() {
        for width in 0..64 {
            assert!(
                cell_width(&box_bottom_with_label("asked", width)) <= width,
                "width {width}"
            );
        }
    }

    #[test]
    fn box_bottom_with_label_falls_back_to_a_plain_border_when_too_narrow() {
        assert_eq!(box_bottom_with_label("asked", 5), box_bottom(5));
    }

    #[test]
    fn box_text_never_exceeds_the_requested_width() {
        for width in 0..64 {
            let line = "status: blocked, needs a human to decide something";
            assert!(cell_width(&box_text(line, width)) <= width, "width {width}");
        }
    }

    #[test]
    fn box_text_pads_short_lines_to_the_full_width() {
        assert_eq!(cell_width(&box_text("hi", 10)), 10);
    }

    #[test]
    fn box_text_uses_one_space_of_inner_padding_like_python() {
        assert_eq!(box_text("hi", 10), "│ hi     │");
    }

    #[test]
    fn card_top_right_aligns_the_clock_and_age() {
        let top = card_top(37, "04:08", "12m");
        assert_eq!(cell_width(&top), 37);
        assert!(top.ends_with(" 04:08 · 12m ┐"), "{top:?}");
    }

    #[test]
    fn card_top_drops_the_age_before_the_clock() {
        let top = card_top(12, "04:08", "12m");
        assert_eq!(cell_width(&top), 12);
        assert!(top.contains("04:08"), "{top:?}");
        assert!(!top.contains("12m"), "{top:?}");
    }

    #[test]
    fn card_bottom_centers_the_role_with_no_project_short() {
        assert_eq!(
            card_bottom(37, None, "asked"),
            "└────────────── asked ──────────────┘"
        );
    }

    #[test]
    fn card_bottom_puts_the_project_short_in_the_right_corner() {
        let bottom = card_bottom(37, Some("sbx"), "asked");
        assert_eq!(cell_width(&bottom), 37);
        assert!(bottom.contains(" asked "), "{bottom:?}");
        assert!(bottom.ends_with(" sbx ┘"), "{bottom:?}");
    }

    #[test]
    fn box_top_with_corner_places_both_labels() {
        let top = box_top_with_corner("explorer-seat · gpt-test", "as of 04:53", 50).unwrap();
        assert_eq!(cell_width(&top), 50);
        assert!(top.starts_with("┌─ explorer-seat · gpt-test "), "{top:?}");
        assert!(top.ends_with(" as of 04:53 ┐"), "{top:?}");
    }

    #[test]
    fn sanitize_title_removes_line_and_paragraph_separators() {
        let dirty = "line one\u{2028}line two\nline three";
        let clean = sanitize_title(dirty);
        assert!(!clean.contains('\u{2028}'));
        assert!(!clean.contains('\n'));
    }

    #[test]
    fn wrap_two_never_produces_a_third_line() {
        let long = "this title is long enough that it would need three or more lines at a narrow width if nothing stopped it from doing so";
        let (first, second) = wrap_two(long, 12);
        assert!(cell_width(&first) <= 12);
        if let Some(second) = second {
            assert!(cell_width(&second) <= 12);
        }
    }

    #[test]
    fn visible_width_ignores_ansi_sgr_sequences() {
        let colored = "\u{1b}[1;31mBLOCKED\u{1b}[0m";
        assert_eq!(visible_width(colored), cell_width("BLOCKED"));
    }

    #[test]
    fn ljust_visible_pads_past_embedded_ansi_codes() {
        let colored = "\u{1b}[92mhi\u{1b}[0m";
        let padded = ljust_visible(colored, 5);
        assert_eq!(visible_width(&padded), 5);
        assert!(padded.starts_with(colored));
    }

    #[test]
    fn ljust_visible_never_truncates() {
        assert_eq!(ljust_visible("already long", 3), "already long");
    }

    #[test]
    fn box_text_visible_wraps_a_colored_line_without_miscounting_width() {
        let colored = "\u{1b}[92mhi\u{1b}[0m";
        let wrapped = box_text_visible(colored, 10);
        assert_eq!(visible_width(&wrapped), 10);
        assert!(wrapped.contains(colored));
    }

    #[test]
    fn wrap_two_breaks_at_the_last_space_in_budget() {
        let (first, rest) = break_at_width("alpha beta gamma", 11);
        assert_eq!(first, "alpha beta");
        assert_eq!(rest, "gamma");
    }

    #[test]
    fn box_top_with_title_embeds_the_label_near_the_left_edge() {
        let top = box_top_with_title("BLOCKED", 40);
        assert!(top.starts_with("┌─ BLOCKED "), "{top:?}");
        assert_eq!(cell_width(&top), 40);
    }

    #[test]
    fn box_top_with_title_never_exceeds_the_requested_width_at_any_size() {
        for width in 0..64 {
            assert!(
                cell_width(&box_top_with_title("BACKLOG", width)) <= width,
                "width {width}"
            );
        }
    }

    #[test]
    fn box_top_with_title_falls_back_to_a_plain_border_when_too_narrow() {
        assert_eq!(box_top_with_title("BLOCKED", 4), box_top(4));
    }

    fn assert_no_escapes(s: &str) {
        assert!(!s.contains('\u{1b}'), "ESC survived: {s:?}");
        assert!(!s.contains('\u{07}'), "BEL survived: {s:?}");
        assert!(
            !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t'),
            "{s:?}"
        );
    }

    #[test]
    fn sanitize_title_strips_osc_52_clipboard_writes() {
        let dirty = "hello\u{1b}]52;c;QUJDRA==\u{07}world";
        let clean = sanitize_title(dirty);
        assert_no_escapes(&clean);
        assert_eq!(clean, "helloworld");
    }

    #[test]
    fn sanitize_title_strips_osc_0_title_changes_and_csi_erase() {
        let dirty = "\u{1b}]0;pwned\u{07}keep\u{1b}[2Jme";
        let clean = sanitize_title(dirty);
        assert_no_escapes(&clean);
        assert_eq!(clean, "keepme");
        assert!(!clean.contains("[2J"));
    }

    #[test]
    fn sanitize_title_strips_8bit_c1_csi() {
        let dirty = "pre\u{9b}31mRED\u{9b}0mpost";
        let clean = sanitize_title(dirty);
        assert_no_escapes(&clean);
        assert_eq!(clean, "preREDpost");
    }

    #[test]
    fn sanitize_title_is_idempotent() {
        let dirty = "a\u{1b}]52;c;xx\u{07}b\nc";
        let once = sanitize_title(dirty);
        assert_eq!(once, sanitize_title(&once));
    }

    #[test]
    fn sanitize_multiline_keeps_newlines_and_strips_osc() {
        let dirty = "line one\n\u{1b}]52;c;QUJDRA==\u{07}line two\n";
        let clean = sanitize_multiline(dirty);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{07}'));
        assert_eq!(clean, "line one\nline two\n");
    }

    #[test]
    fn sanitize_multiline_drops_cr_so_it_cannot_overwrite_a_line() {
        let dirty = "secret\rok  \n";
        let clean = sanitize_multiline(dirty);
        assert_eq!(clean, "secret ok  \n");
        let crlf = sanitize_multiline("keep\r\nthis");
        assert_eq!(crlf, "keep\nthis");
    }

    #[test]
    fn truncate_strips_zero_width_escapes_before_clipping() {
        let dirty = "\u{1b}]52;c;EVIL\u{07}visible";
        let clipped = truncate(dirty, 20);
        assert_no_escapes(&clipped);
        assert_eq!(clipped, "visible");
    }

    #[test]
    fn emoji_and_cjk_titles_never_overflow_the_clip_budget() {
        let s = "hi 😀 ソートカ 🎉";
        for width in 0..24 {
            assert!(cell_width(&truncate(s, width)) <= width);
        }
        assert_eq!(cell_width("\u{FFFD}"), 1);
    }

    #[test]
    fn visible_width_skips_osc_as_well_as_sgr() {
        let osc = "ab\u{1b}]52;c;xx\u{07}cd";
        assert_eq!(visible_width(osc), cell_width("abcd"));
    }

    #[test]
    fn color_enabled_honors_no_color_flag_and_env() {
        assert!(color_enabled_from(false, None));
        assert!(!color_enabled_from(true, None));
        assert!(!color_enabled_from(false, Some("1")));
        assert!(color_enabled_from(false, Some("")));
    }
}

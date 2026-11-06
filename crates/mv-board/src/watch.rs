//! `board --watch`: a ratatui view driven by a crossterm poll loop.
//!
//! Still pure rendering — this module takes a [`BoardSource`] the caller
//! implements (wrapping whatever `mv-state`/`mv-engine`/Herdr reads the CLI
//! lane needs) and never touches `.multiverse/` itself. The one-shot
//! `board` command stays the plain text report from [`crate::render`]; this
//! module just re-prints those same lines into a ratatui `Paragraph` inside
//! an alternate screen, and redraws on state changes or the refresh interval —
//! never merely because elapsed-time text or quota numbers
//! ticked over (callers are expected to exclude those from the fingerprint,
//! the same exclusion `board.py`'s `watch_fingerprint` makes).

use std::io;
use std::time::Duration;

use crossterm::cursor::Show;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

/// Opaque fingerprint of "what would change the screen". Two polls with
/// the same fingerprint must not redraw.
pub type Fingerprint = u64;

/// A watch loop's data source. `mv-board` defines the trait; a real
/// implementation (reading tasks/backlog/Herdr state) belongs to the CLI
/// lane, same as the one-shot renderer's inputs.
pub trait BoardSource {
    /// Produce the next frame's lines and a fingerprint of the state they
    /// came from.
    fn poll(&mut self) -> (Vec<String>, Fingerprint);
}

/// True when `current` differs from the last fingerprint drawn — the
/// loop's entire "redraw only on change" rule, pulled out so it's testable
/// without a real terminal.
pub fn should_redraw(last: Option<Fingerprint>, current: Fingerprint) -> bool {
    last != Some(current)
}

/// Render `lines` as one `Paragraph` filling the frame.
///
/// `ratatui` 0.30 made `Backend::Error` an associated type rather than
/// fixing it to `io::Error` (`TestBackend`'s is `Infallible`), so this
/// maps whatever the backend returns into an `io::Error` rather than
/// widening every caller's return type to a generic error.
pub fn draw<B>(terminal: &mut Terminal<B>, lines: &[String]) -> io::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let text = lines.join("\n");
    terminal
        .draw(|frame| {
            let paragraph = Paragraph::new(text);
            frame.render_widget(paragraph, frame.area());
        })
        .map_err(io::Error::other)?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchAction {
    Quit,
    Resize,
    Ignore,
}

fn classify_event(event: &Event) -> WatchAction {
    match event {
        Event::Key(key) => {
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
                || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                WatchAction::Quit
            } else {
                WatchAction::Ignore
            }
        }
        Event::Resize(_, _) => WatchAction::Resize,
        _ => WatchAction::Ignore,
    }
}

#[cfg(test)]
fn is_quit(event: &Event) -> bool {
    classify_event(event) == WatchAction::Quit
}

fn watch_loop<B>(
    terminal: &mut Terminal<B>,
    source: &mut impl BoardSource,
    tick: Duration,
) -> io::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    watch_loop_with(
        terminal,
        source,
        tick,
        |timeout| {
            if event::poll(timeout)? {
                Ok(Some(event::read()?))
            } else {
                Ok(None)
            }
        },
        std::time::Instant::now,
    )
}

fn watch_loop_with<B>(
    terminal: &mut Terminal<B>,
    source: &mut impl BoardSource,
    tick: Duration,
    mut read_event: impl FnMut(Duration) -> io::Result<Option<Event>>,
    mut now: impl FnMut() -> std::time::Instant,
) -> io::Result<()>
where
    B: Backend,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (lines, fingerprint) = source.poll();
    draw(terminal, &lines)?;
    let mut last_fingerprint = Some(fingerprint);
    let mut next_refresh = now() + tick;
    loop {
        let timeout = Duration::from_secs(1).min(next_refresh.saturating_duration_since(now()));
        if let Some(ev) = read_event(timeout)? {
            match classify_event(&ev) {
                WatchAction::Quit => return Ok(()),
                WatchAction::Resize => last_fingerprint = None,
                WatchAction::Ignore => {}
            }
        }
        let (lines, fingerprint) = source.poll();
        let current = now();
        if redraw_due(last_fingerprint, fingerprint, current >= next_refresh) {
            draw(terminal, &lines)?;
            last_fingerprint = Some(fingerprint);
            next_refresh = current + tick;
        }
    }
}

fn redraw_due(last: Option<Fingerprint>, current: Fingerprint, interval_elapsed: bool) -> bool {
    interval_elapsed || should_redraw(last, current)
}

/// Restores cooked mode, the primary screen, and the cursor on every
/// exit path — including a panic inside `watch_loop` — so a failed
/// `disable_raw_mode` cannot skip `LeaveAlternateScreen` the way the
/// previous `?` chain did.
struct TerminalRestore;

impl Drop for TerminalRestore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut stdout = io::stdout();
        let _ = execute!(stdout, LeaveAlternateScreen, Show);
    }
}

/// Run the `--watch` loop against a real terminal: an alternate screen,
/// crossterm event polling on `tick`, and `q`/Esc/Ctrl-C to quit. Restores
/// the terminal on the way out even if `source` or the draw call errors.
pub fn run(mut source: impl BoardSource, tick: Duration) -> io::Result<()> {
    enable_raw_mode()?;
    let _restore = TerminalRestore;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    watch_loop(&mut terminal, &mut source, tick)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn row10_watch_paints_before_waiting_for_the_first_event() {
        use crossterm::event::KeyEvent;
        use std::cell::Cell;
        let polls = Cell::new(0);
        struct Source<'a>(&'a Cell<usize>);
        impl BoardSource for Source<'_> {
            fn poll(&mut self) -> (Vec<String>, Fingerprint) {
                self.0.set(self.0.get() + 1);
                (vec!["first frame".into()], 1)
            }
        }
        let mut terminal = Terminal::new(TestBackend::new(40, 5)).unwrap();
        watch_loop_with(
            &mut terminal,
            &mut Source(&polls),
            Duration::from_secs(300),
            |timeout| {
                assert!(polls.get() > 0, "first frame must be painted immediately");
                assert!(
                    timeout <= Duration::from_secs(1),
                    "probe at least once a second"
                );
                Ok(Some(Event::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::NONE,
                ))))
            },
            std::time::Instant::now,
        )
        .unwrap();
    }

    #[test]
    fn row10_watch_redraws_on_interval_even_with_an_unchanged_fingerprint() {
        assert!(!redraw_due(Some(1), 1, false));
        assert!(redraw_due(Some(1), 1, true));
        assert!(redraw_due(Some(1), 2, false));
    }

    #[test]
    fn should_redraw_is_false_only_when_the_fingerprint_repeats() {
        assert!(should_redraw(None, 1));
        assert!(!should_redraw(Some(1), 1));
        assert!(should_redraw(Some(1), 2));
    }

    #[test]
    fn draw_never_panics_at_a_small_terminal_size() {
        let backend = TestBackend::new(20, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        let lines: Vec<String> = vec!["BLOCKED".into(), "(none)".into()];
        draw(&mut terminal, &lines).unwrap();
    }

    #[test]
    fn draw_renders_the_first_line_into_the_buffer() {
        let backend = TestBackend::new(40, 5);
        let mut terminal = Terminal::new(backend).unwrap();
        draw(&mut terminal, &["hello board".to_string()]).unwrap();
        let buffer = terminal.backend().buffer();
        let row: String = (0..11).map(|x| buffer[(x, 0)].symbol()).collect();
        assert_eq!(row, "hello board");
    }

    #[test]
    fn is_quit_matches_q_and_esc_but_not_other_keys() {
        use crossterm::event::KeyEvent;
        let q = Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
        let esc = Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let other = Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(is_quit(&q));
        assert!(is_quit(&esc));
        assert!(is_quit(&ctrl_c));
        assert!(!is_quit(&other));
    }

    #[test]
    fn classify_event_treats_resize_as_a_forced_redraw_not_a_quit() {
        let resize = Event::Resize(40, 12);
        assert_eq!(classify_event(&resize), WatchAction::Resize);
        assert!(!is_quit(&resize));
    }

    #[test]
    fn draw_never_panics_at_a_1x1_or_very_wide_terminal() {
        for (w, h) in [(1u16, 1u16), (2, 2), (400, 3)] {
            let backend = TestBackend::new(w, h);
            let mut terminal = Terminal::new(backend).unwrap();
            let lines: Vec<String> = vec!["BLOCKED".into(), "(none)".into()];
            draw(&mut terminal, &lines).unwrap();
        }
    }
}

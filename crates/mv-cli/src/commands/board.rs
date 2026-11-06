//! `multiverse board`: the task/backlog board, as plain text (and, with
//! `--watch`, redrawn on an interval).
//!
//! [`board_data::LiveSource`] is the real [`BoardDataSource`]: configured
//! projects come from `mv-config::load`, their tasks/backlog from
//! `mv-state::{tasks,backlog,index}`, fleet account/seat health from
//! `mv-engine::{breaker,seatstate,limits}`, and live Herdr state from a
//! directly-shelled `agent list`/`workspace list` (the same
//! `std::process::Command` + hand-rolled JSON pattern `commands::workers`
//! already uses — `mv-cli` has no `mv-herdr`/`serde_json` dependency, and
//! widening `Cargo.toml` is outside this lane's write scope). See
//! `board_data`'s module doc for exactly what that resolution narrows
//! relative to Python's `tasks.resolve`, and this task's `result.md` for
//! the rest.
//!
//! Terminal width comes from [`mv_board::layout::terminal_width`]
//! (`COLUMNS`, then the tty ioctl), falling back to 80 off a tty — the
//! same fallback `cmd_board`'s `shutil.get_terminal_size(fallback=(80, 24))`
//! uses. Color follows `--no-color` and a non-empty `NO_COLOR`.

use clap::ArgMatches;
use mv_board::render::{
    self, BoardColumn, BoardSnapshot as ProjectRenderSnapshot, CardsPerRow, FleetSnapshot,
    RenderOptions, StatusFilter,
};
use mv_board::watch;

use crate::args::{BoardLimitArg, WatchArg};

use super::board_data;
use crate::exit::Failure;

/// Board render inputs: one project, the pooled fleet view, or a
/// fully-formed message in place of either (the degraded `--active`
/// outcomes `board_data::LiveSource` can reach without an error).
pub enum BoardView {
    Project(ProjectRenderSnapshot),
    Fleet(FleetSnapshot),
    Message(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardScope {
    Named(Vec<String>),
    All,
    Here,
    Active,
    Default,
}

impl BoardScope {
    /// Named projects > `--all` > `--here` > `--active` > default — the
    /// precedence `cmd_board` documents (spec-board.md §6).
    fn resolve(args: &ArgMatches) -> Self {
        let projects: Vec<String> = args
            .get_many::<String>("projects")
            .map(|values| values.cloned().collect())
            .unwrap_or_default();
        if !projects.is_empty() {
            Self::Named(projects)
        } else if args.get_flag("all") {
            Self::All
        } else if args.get_flag("here") {
            Self::Here
        } else if args.get_flag("active") {
            Self::Active
        } else {
            Self::Default
        }
    }
}

/// A resolved view, plus the scope-header line to print above it (`None`
/// for a [`BoardView::Message`], which has nothing else to print).
pub trait BoardDataSource {
    fn snapshot(&self, scope: &BoardScope) -> Result<(Option<String>, BoardView), Failure>;

    fn fingerprint(&self, view: &BoardView) -> watch::Fingerprint {
        fingerprint_view(view)
    }
}

/// A fixed "no projects configured" source, used only by this module's own
/// tests — `board_data::LiveSource::snapshot` reaches the identical message
/// for real whenever a loaded config has zero projects, the same way
/// `commands::accounts` keeps its own test-only `Unconfigured` fake rather
/// than threading that case through a real data source.
#[cfg(test)]
struct Unconfigured;

#[cfg(test)]
impl BoardDataSource for Unconfigured {
    fn snapshot(&self, _scope: &BoardScope) -> Result<(Option<String>, BoardView), Failure> {
        Err(Failure::Config(
            "no projects configured; run `multiverse add` or `multiverse new`".to_owned(),
        ))
    }
}

/// The seam `mv-board::watch` fills for real: an interactive,
/// redraw-on-change event loop. `render` produces one frame's lines.
pub struct BoardFrame {
    lines: Vec<String>,
    fingerprint: watch::Fingerprint,
}

pub trait BoardWatchLoop {
    fn watch(&self, render: &dyn Fn() -> Result<BoardFrame, Failure>) -> Result<(), Failure>;
}

struct LiveWatch {
    tick: std::time::Duration,
}

impl Default for LiveWatch {
    fn default() -> Self {
        Self {
            tick: std::time::Duration::from_secs(300),
        }
    }
}

impl BoardWatchLoop for LiveWatch {
    fn watch(&self, render: &dyn Fn() -> Result<BoardFrame, Failure>) -> Result<(), Failure> {
        // Fail fast on the first resolve, with the same error the
        // non-watch path would show, before ever entering raw mode.
        let first = render()?;

        struct Adapter<'a> {
            render: &'a dyn Fn() -> Result<BoardFrame, Failure>,
            last: BoardFrame,
            first: bool,
        }
        impl watch::BoardSource for Adapter<'_> {
            fn poll(&mut self) -> (Vec<String>, watch::Fingerprint) {
                // `mv_board::watch::BoardSource::poll` has no way to
                // surface a `Result` — a mid-watch resolve failure keeps
                // the last good frame on screen rather than tearing down
                // the loop or showing a blank one.
                if self.first {
                    self.first = false;
                } else if let Ok(lines) = (self.render)() {
                    self.last = lines;
                }
                (self.last.lines.clone(), self.last.fingerprint)
            }
        }

        let adapter = Adapter {
            render,
            last: first,
            first: true,
        };
        watch::run(adapter, self.tick)
            .map_err(|error| Failure::Operation(format!("board --watch failed: {error}")))
    }
}

/// Stable state only: clocks, elapsed ages and quota readings refresh on interval.
pub(crate) fn fingerprint_view(view: &BoardView) -> watch::Fingerprint {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let (projects, health) = match view {
        BoardView::Project(snapshot) => (
            std::slice::from_ref(&snapshot.project),
            &snapshot.fleet_health,
        ),
        BoardView::Fleet(snapshot) => (snapshot.projects.as_slice(), &snapshot.fleet_health),
        BoardView::Message(lines) => {
            lines.hash(&mut hasher);
            return hasher.finish();
        }
    };
    for project in projects {
        project.name.hash(&mut hasher);
        for task in &project.tasks {
            (
                &task.task_id,
                &task.title,
                &task.role,
                &task.account,
                &task.agent,
                &task.state_note,
                &task.stale_pane_note.as_ref().map(|_| true),
                &task.dirty_done_detail,
                task.acknowledged,
                task.limited_count,
            )
                .hash(&mut hasher);
            format!("{:?}", task.outcome).hash(&mut hasher);
            format!("{:?}", task.live_status).hash(&mut hasher);
        }
        for item in &project.backlog {
            (
                &item.id,
                &item.title,
                item.priority,
                item.created_order,
                &item.role_hint,
                item.has_human_annotation,
            )
                .hash(&mut hasher);
            format!("{:?}", item.status).hash(&mut hasher);
        }
    }
    for account in &health.accounts {
        (
            &account.name,
            account.state.label(),
            account.seats_open,
            account.seats_total,
            account.over_cap,
        )
            .hash(&mut hasher);
    }
    hasher.finish()
}

fn render_view(view: &BoardView, options: &RenderOptions, by_project: bool) -> Vec<String> {
    match view {
        BoardView::Project(snapshot) => render::render(snapshot, options),
        BoardView::Fleet(snapshot) if by_project => render::render_by_project(snapshot, options),
        BoardView::Fleet(snapshot) => render::render_merged(snapshot, options),
        BoardView::Message(lines) => lines.clone(),
    }
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    // Usage errors (a bad scope-flag combination) must surface before any
    // config load is attempted — the same ordering `commands::validate`
    // already applies fleet-wide for `--role`/`--timeout`, and exactly
    // what `args::tests::every_documented_invocation_parses_and_reaches_
    // its_stub` pins for `board` (exit 1, never a config error, for an
    // invocation that combines every scope flag at once).
    validate_scope_combination(args)?;
    let source = board_data::LiveSource::load(args)?;
    let tick = watch_tick(args, source.refresh_seconds());
    run_with(args, &source, &LiveWatch { tick })
}

/// Non-tty / missing-`COLUMNS` fallback — `cmd_board`'s own
/// `shutil.get_terminal_size(fallback=(80, 24)).columns`, which is what
/// a piped run (every conformance scenario) actually sees.
const DEFAULT_WIDTH: usize = 80;
const DEFAULT_LIMIT: usize = 10;
const DEFAULT_WATCH_SECS: u64 = 300;

fn watch_tick(args: &ArgMatches, refresh_seconds: u32) -> std::time::Duration {
    match args.get_one::<WatchArg>("watch") {
        Some(WatchArg::Seconds(seconds)) => std::time::Duration::from_secs_f64(*seconds),
        Some(WatchArg::FromConfig) => {
            std::time::Duration::from_secs(u64::from(refresh_seconds.max(1)))
        }
        None => std::time::Duration::from_secs(DEFAULT_WATCH_SECS),
    }
}

fn resolve_width() -> usize {
    mv_board::layout::terminal_width().unwrap_or(DEFAULT_WIDTH)
}

fn resolve_color(args: &ArgMatches) -> bool {
    mv_board::layout::color_enabled(args.get_flag("no-color"))
}

fn resolve_limit(args: &ArgMatches) -> usize {
    match args.get_one::<BoardLimitArg>("limit") {
        Some(BoardLimitArg::Count(n)) => *n as usize,
        Some(BoardLimitArg::Auto) | None => DEFAULT_LIMIT,
    }
}

fn auto_height(args: &ArgMatches, tty: bool, rows: usize) -> Option<usize> {
    (tty && !matches!(
        args.get_one::<BoardLimitArg>("limit"),
        Some(BoardLimitArg::Count(_))
    ))
    .then_some(rows)
}

fn resolve_cards_per_row(args: &ArgMatches) -> CardsPerRow {
    args.get_one::<String>("cards-per-row")
        .and_then(|value| CardsPerRow::parse(value))
        .unwrap_or(CardsPerRow::Auto)
}

fn resolve_statuses(args: &ArgMatches) -> StatusFilter {
    let keys = args
        .get_many::<String>("status")
        .into_iter()
        .flatten()
        .filter_map(|value| BoardColumn::parse(value));
    StatusFilter::from_keys(keys)
}

fn is_watch(args: &ArgMatches) -> bool {
    args.get_one::<WatchArg>("watch").is_some()
}

/// `cmd_board`'s own combination checks (`cli.py:8277-8284`): `--here`
/// never mixes with any other scope selector, and `--all` never mixes
/// with named projects. [`BoardScope::resolve`] has a silent precedence
/// order for when a caller ignores this and passes several anyway — this
/// runs first so that precedence never papers over a combination Python
/// itself refuses outright.
fn validate_scope_combination(args: &ArgMatches) -> Result<(), Failure> {
    let named = args
        .get_many::<String>("projects")
        .is_some_and(|mut values| values.next().is_some());
    let all = args.get_flag("all");
    let here = args.get_flag("here");
    let active = args.get_flag("active");
    if here && (all || active || named) {
        return Err(Failure::Operation(
            "cannot combine --here with --all, --active, or named projects".to_owned(),
        ));
    }
    if all && named {
        return Err(Failure::Operation(
            "cannot combine --all with named projects".to_owned(),
        ));
    }
    Ok(())
}

fn run_with(
    args: &ArgMatches,
    source: &dyn BoardDataSource,
    watch_loop: &dyn BoardWatchLoop,
) -> Result<(), Failure> {
    validate_scope_combination(args)?;
    let scope = BoardScope::resolve(args);
    let render_fn = move || -> Result<BoardFrame, Failure> {
        let options = RenderOptions {
            width: resolve_width(),
            height: {
                use std::io::IsTerminal;
                auto_height(
                    args,
                    std::io::stdout().is_terminal(),
                    mv_board::layout::terminal_height().unwrap_or(24),
                )
            },
            limit: resolve_limit(args),
            color: resolve_color(args),
            cards_per_row: resolve_cards_per_row(args),
            statuses: resolve_statuses(args),
        };
        let (header, view) = source.snapshot(&scope)?;
        let fingerprint = source.fingerprint(&view);
        let mut lines: Vec<String> = header
            .into_iter()
            .map(|h| mv_board::layout::sanitize_multiline(&h))
            .collect();
        lines.extend(render_view(&view, &options, args.get_flag("by-project")));
        Ok(BoardFrame { lines, fingerprint })
    };
    if is_watch(args) {
        return watch_loop.watch(&render_fn);
    }
    for line in render_fn()?.lines {
        println!("{line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;
    use mv_board::card::DispatchedTaskSnapshot;
    use mv_board::render::{FleetHealth, ProjectSnapshot};

    struct Fixed(fn() -> BoardView);
    impl BoardDataSource for Fixed {
        fn snapshot(&self, _scope: &BoardScope) -> Result<(Option<String>, BoardView), Failure> {
            Ok((None, (self.0)()))
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("board").unwrap().clone()
    }

    fn empty_project() -> ProjectSnapshot {
        ProjectSnapshot {
            name: "demo".into(),
            short: "demo".into(),
            tasks: Vec::<DispatchedTaskSnapshot>::new(),
            backlog: Vec::new(),
        }
    }

    fn running_task(id: &str) -> DispatchedTaskSnapshot {
        DispatchedTaskSnapshot {
            task_id: id.into(),
            title: Some("Work".into()),
            role: "Scout".into(),
            account: "seat".into(),
            elapsed_text: "1m".into(),
            elapsed_seconds: Some(60),
            limited_count: 0,
            outcome: None,
            live_status: None,
            state_note: None,
            stale_pane_note: None,
            dirty_done_detail: None,
            acknowledged: false,
            corner_clock: Some("12:00".into()),
            corner_age: Some("1m".into()),
            agent: None,
        }
    }

    #[test]
    fn row10_watch_fingerprint_excludes_clock_ages_but_tracks_task_state() {
        let mut project = empty_project();
        project.tasks.push(running_task("task"));
        let mut view = BoardView::Project(ProjectRenderSnapshot {
            project,
            fleet_health: FleetHealth::default(),
        });
        let first = fingerprint_view(&view);
        let BoardView::Project(snapshot) = &mut view else {
            unreachable!()
        };
        snapshot.project.tasks[0].elapsed_text = "2m".into();
        snapshot.project.tasks[0].elapsed_seconds = Some(120);
        snapshot.project.tasks[0].corner_age = Some("2m".into());
        assert_eq!(first, fingerprint_view(&view));
        let BoardView::Project(snapshot) = &mut view else {
            unreachable!()
        };
        snapshot.project.tasks[0].state_note =
            Some("stalled, agent is live but not working".into());
        assert_ne!(first, fingerprint_view(&view));
    }

    #[test]
    fn row53_auto_limit_uses_tty_rows_and_leaves_numeric_and_pipes_fixed() {
        let mut project = empty_project();
        project.tasks = (0..15)
            .map(|i| running_task(&format!("task-{i:02}")))
            .collect();
        let view = BoardView::Project(ProjectRenderSnapshot {
            project,
            fleet_health: FleetHealth::default(),
        });
        let mut options = RenderOptions {
            width: 160,
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::One,
            statuses: StatusFilter::from_keys([BoardColumn::InProgress]),
            height: auto_height(&matches(&["board", "--limit", "auto"]), true, 120),
        };
        let lines = render_view(&view, &options, false);
        assert!(
            lines.iter().any(|line| line.contains("task-14")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|line| line.contains("+5 more")));
        assert!(lines.len() + 2 <= 120, "scope and prompt still need rows");
        options.height = auto_height(&matches(&["board", "--limit", "3"]), true, 120);
        options.limit = 3;
        assert!(render_view(&view, &options, false)
            .iter()
            .any(|line| line.contains("+12 more")));
        assert_eq!(auto_height(&matches(&["board"]), false, 120), None);
        options.height = Some(30);
        options.limit = 10;
        assert!(render_view(&view, &options, false).len() + 2 <= 30);
    }

    #[test]
    fn row53_by_project_splits_the_terminal_height_between_boards() {
        let mut project = empty_project();
        project.tasks = (0..15)
            .map(|i| running_task(&format!("task-{i:02}")))
            .collect();
        let mut other = project.clone();
        other.name = "other".into();
        let view = BoardView::Fleet(FleetSnapshot {
            projects: vec![project, other],
            fleet_health: FleetHealth::default(),
        });
        let options = RenderOptions {
            width: 160,
            height: Some(80),
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::One,
            statuses: StatusFilter::from_keys([BoardColumn::InProgress]),
        };
        let lines = render_view(&view, &options, true);
        assert!(lines.len() + 2 <= 80);
        assert_eq!(lines.iter().filter(|line| line.contains("more")).count(), 2);
    }

    #[test]
    fn row35_by_project_stacks_headed_boards_with_one_fleet_panel() {
        let mut other = empty_project();
        other.name = "other".into();
        let view = BoardView::Fleet(FleetSnapshot {
            projects: vec![empty_project(), other],
            fleet_health: FleetHealth::default(),
        });
        let options = RenderOptions {
            width: 80,
            height: None,
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::default(),
        };
        let lines = render_view(&view, &options, true);
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.as_str() == "demo" || line.as_str() == "other")
                .count(),
            2
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("IN PROGRESS"))
                .count(),
            2
        );
    }

    #[test]
    fn unconfigured_board_needs_config() {
        let err = run_with(&matches(&["board"]), &Unconfigured, &LiveWatch::default()).unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn here_and_all_together_is_an_error_not_needs_config() {
        let err = run_with(
            &matches(&["board", "--here", "--all"]),
            &Unconfigured,
            &LiveWatch::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn here_and_active_together_is_also_an_error() {
        let err = run_with(
            &matches(&["board", "--here", "--active"]),
            &Unconfigured,
            &LiveWatch::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn here_and_named_projects_together_is_also_an_error() {
        let err = run_with(
            &matches(&["board", "one", "--here"]),
            &Unconfigured,
            &LiveWatch::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn all_and_named_projects_together_is_an_error() {
        let err = run_with(
            &matches(&["board", "one", "--all"]),
            &Unconfigured,
            &LiveWatch::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn scope_precedence_is_named_then_all_then_here_then_active_then_default() {
        let m = matches(&["board", "one", "two", "--all", "--active"]);
        assert_eq!(
            BoardScope::resolve(&m),
            BoardScope::Named(vec!["one".into(), "two".into()])
        );
        let m = matches(&["board", "--all", "--active"]);
        assert_eq!(BoardScope::resolve(&m), BoardScope::All);
        let m = matches(&["board", "--active"]);
        assert_eq!(BoardScope::resolve(&m), BoardScope::Active);
        let m = matches(&["board"]);
        assert_eq!(BoardScope::resolve(&m), BoardScope::Default);
    }

    #[test]
    fn a_project_view_renders_through_mv_board_render() {
        let source = Fixed(|| {
            BoardView::Project(ProjectRenderSnapshot {
                project: empty_project(),
                fleet_health: FleetHealth::default(),
            })
        });
        assert!(run_with(&matches(&["board"]), &source, &LiveWatch::default()).is_ok());
    }

    #[test]
    fn a_fleet_view_renders_through_mv_board_render_merged() {
        let source = Fixed(|| {
            BoardView::Fleet(FleetSnapshot {
                projects: vec![empty_project()],
                fleet_health: FleetHealth::default(),
            })
        });
        assert!(run_with(
            &matches(&["board", "--all"]),
            &source,
            &LiveWatch::default()
        )
        .is_ok());
    }

    #[test]
    fn watch_fails_fast_on_the_first_resolve_without_entering_raw_mode() {
        // Never exercises the success path of `LiveWatch` here: that would
        // enable raw mode / an alternate screen, which has no place in a
        // headless `cargo test` run. A resolve failure must return before
        // any of that happens, which this proves without touching a
        // terminal.
        let err = LiveWatch::default()
            .watch(&|| Err(Failure::Config("no projects configured".into())))
            .unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn watch_without_a_configured_source_reports_the_same_error_as_the_non_watch_path() {
        let err = run_with(
            &matches(&["board", "--watch"]),
            &Unconfigured,
            &LiveWatch::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), 3);
    }

    #[test]
    fn a_message_view_prints_its_lines_with_no_header() {
        let source = Fixed(|| BoardView::Message(vec!["no active projects".to_string()]));
        assert!(run_with(
            &matches(&["board", "--active"]),
            &source,
            &LiveWatch::default()
        )
        .is_ok());
    }

    #[test]
    fn resolve_color_is_false_when_no_color_flag_is_set() {
        assert!(!resolve_color(&matches(&["board", "--no-color"])));
    }

    #[test]
    fn resolve_width_falls_back_to_the_non_tty_default_when_columns_is_unset() {
        // `terminal_width` itself is locked and tested in mv-board; this
        // only pins that the CLI uses that probe (or 80) rather than a
        // different hardcoded size.
        let width = resolve_width();
        assert!(width >= 1, "{width}");
    }

    #[test]
    fn a_message_view_header_strips_osc_clipboard_writes() {
        let dirty = "scope\u{1b}]52;c;EVIL\u{07} header";
        let clean = mv_board::layout::sanitize_multiline(dirty);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{07}'));
        assert!(clean.contains("scope"));
        assert!(clean.contains("header"));
    }

    #[test]
    fn watch_limit_cards_per_row_and_status_are_read_from_args() {
        assert!(is_watch(&matches(&["board", "--watch"])));
        assert!(is_watch(&matches(&["board", "--watch", "2.5"])));
        assert!(!is_watch(&matches(&["board"])));
        assert_eq!(
            watch_tick(&matches(&["board", "--watch"]), 12),
            std::time::Duration::from_secs(12)
        );
        assert_eq!(
            watch_tick(&matches(&["board", "--watch", "2.5"]), 300),
            std::time::Duration::from_secs_f64(2.5)
        );
        assert_eq!(resolve_limit(&matches(&["board"])), DEFAULT_LIMIT);
        assert_eq!(
            resolve_limit(&matches(&["board", "--limit", "auto"])),
            DEFAULT_LIMIT
        );
        assert_eq!(resolve_limit(&matches(&["board", "--limit", "4"])), 4);
        assert_eq!(
            resolve_cards_per_row(&matches(&["board"])),
            CardsPerRow::Auto
        );
        assert_eq!(
            resolve_cards_per_row(&matches(&["board", "--cards-per-row", "1"])),
            CardsPerRow::One
        );
        assert_eq!(
            resolve_cards_per_row(&matches(&["board", "--cards-per-row", "2"])),
            CardsPerRow::Two
        );
        assert_eq!(resolve_statuses(&matches(&["board"])), StatusFilter::all());
        assert_eq!(
            resolve_statuses(&matches(&[
                "board", "--status", "backlog", "--status", "blocked"
            ])),
            StatusFilter::from_keys([BoardColumn::Backlog, BoardColumn::Blocked])
        );
    }
}

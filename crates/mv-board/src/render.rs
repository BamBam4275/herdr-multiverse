//! `render`/`render_merged`: the parseable one-shot text report used
//! by the one-shot `board` command and the `multiverse-board` plugin
//! action.
//!
//! Neither function does any I/O. Both take an already-resolved snapshot —
//! the CLI lane builds one from `mv-state`/`mv-engine`/a live Herdr
//! `agent list` and hands it here. Nothing in this module reads
//! `.multiverse/`, and nothing here prints the worktree-discovery
//! "ignored: not a lane" diagnostic — that belongs to `mv-state::worktrees`
//! alone, never to board output.

use crate::accounts::{account_section_header, render_account_cards, AccountSnapshot};
use crate::card::{
    split_backlog, split_dispatched, BacklogItemSnapshot, Card, DispatchedTaskSnapshot, PlacedCard,
};
use crate::columns::{
    render_columns_merged_with, render_columns_with, sort_backlog, sort_backlog_placed,
    sort_blocked, sort_blocked_placed, sort_in_progress, sort_in_progress_placed, ColumnLayout,
};
use crate::layout::sanitize_title;

pub use crate::columns::{BoardColumn, CardsPerRow, StatusFilter};
use crate::status_cards::{
    render_status_cards, LastCycleEntry, MailEntry, OwedReply, SeatsSnapshot,
};

/// A bright, dark-terminal-readable 4-entry palette. Red/bright-blue and
/// the non-bright set are excluded as unreadable (the same rationale
/// `board.py`'s `_PALETTE` records, from a reported screenshot).
const PROJECT_PALETTE: [u8; 4] = [92, 93, 95, 97];

/// Every agent renders in the same color — a past per-agent hash was
/// reverted as unreadable/arbitrary.
pub const AGENT_COLOR: u8 = 96;

/// Hash a project name into one of the four palette colors. Uses
/// `crc32fast` (matching `zlib.crc32`), not a salted-per-process `hash()`,
/// so the same project gets the same color across runs.
pub fn project_color(name: &str) -> u8 {
    let crc = crc32fast::hash(name.as_bytes());
    PROJECT_PALETTE[(crc % PROJECT_PALETTE.len() as u32) as usize]
}

/// One project's resolved tasks and backlog, plus its configured short
/// name (board doesn't invent project abbreviations; the CLI lane already
/// has that from config).
#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    pub name: String,
    pub short: String,
    pub tasks: Vec<DispatchedTaskSnapshot>,
    pub backlog: Vec<BacklogItemSnapshot>,
}

/// Shared fleet-health data behind the ACCOUNTS/STATUS rows — the same for
/// a single-project `render` and the pooled `render_merged`.
#[derive(Debug, Clone, Default)]
pub struct FleetHealth {
    pub accounts: Vec<AccountSnapshot>,
    /// The ACCOUNTS row's own `"fleet N/M"` header — `running` is `None`
    /// only when Herdr could not be asked; `capacity` comes from config and
    /// stays known even then (`board.py`'s `WorkersSnapshot.max_concurrent`
    /// is never gated on `herdr_ok`).
    pub accounts_running: Option<u32>,
    pub accounts_capacity: Option<u32>,
    pub seats: Option<SeatsSnapshot>,
    pub owed_replies: Vec<OwedReply>,
    pub mail: Vec<MailEntry>,
    pub last_cycle: Vec<LastCycleEntry>,
}

/// Inputs for `render`: one project's board.
#[derive(Debug, Clone)]
pub struct BoardSnapshot {
    pub project: ProjectSnapshot,
    pub fleet_health: FleetHealth,
}

/// Inputs for `render_merged`: the pooled fleet view.
#[derive(Debug, Clone)]
pub struct FleetSnapshot {
    pub projects: Vec<ProjectSnapshot>,
    pub fleet_health: FleetHealth,
}

/// Terminal geometry and display flags, computed by the caller the same
/// way `cmd_board` does today (subtract header/ACCOUNTS/STATUS line counts
/// from `shutil.get_terminal_size()`, or a fixed default off a tty).
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    pub width: usize,
    /// Whole terminal height for tty auto packing; None keeps a fixed card limit.
    pub height: Option<usize>,
    /// Cards shown per column before "+N more".
    pub limit: usize,
    pub color: bool,
    pub cards_per_row: CardsPerRow,
    pub statuses: StatusFilter,
}

fn render_accounts(health: &FleetHealth, width: usize) -> Vec<String> {
    if health.accounts.is_empty() {
        return Vec::new();
    }
    let mut out = vec![account_section_header(
        health.accounts_running,
        health.accounts_capacity,
        width,
    )];
    out.extend(render_account_cards(&health.accounts, width));
    out
}

fn render_status(health: &FleetHealth, width: usize) -> Vec<String> {
    let Some(seats) = &health.seats else {
        return Vec::new();
    };
    render_status_cards(
        seats,
        &health.owed_replies,
        &health.mail,
        &health.last_cycle,
        &health.accounts,
        width,
    )
}

/// Render one project's board: ACCOUNTS row, the project name, the
/// BLOCKED / IN PROGRESS / BACKLOG columns, then the STATUS row —
/// `cli.cmd_board`'s own print order (`print(account_lines)`, then
/// `"\n{project.name}"` + `render()`, then `status_lines`).
pub fn render(snapshot: &BoardSnapshot, options: &RenderOptions) -> Vec<String> {
    let mut out = render_accounts(&snapshot.fleet_health, options.width);
    out.push(String::new());
    out.push(sanitize_title(&snapshot.project.name));

    let status = render_status(&snapshot.fleet_health, options.width);
    let height = options
        .height
        .map(|rows| rows.saturating_sub(out.len() + status.len() + 2));
    out.extend(project_columns(&snapshot.project, options, height));
    out.extend(status);
    out
}

fn project_columns(
    project: &ProjectSnapshot,
    options: &RenderOptions,
    height: Option<usize>,
) -> Vec<String> {
    let (mut blocked, mut in_progress) = split_dispatched(&project.tasks);
    let (backlog_blocked, mut backlog): (Vec<Card>, Vec<Card>) = split_backlog(&project.backlog);
    blocked.extend(backlog_blocked);
    sort_blocked(&mut blocked);
    sort_in_progress(&mut in_progress);
    sort_backlog(&mut backlog);
    render_columns_with(
        &blocked,
        &in_progress,
        &backlog,
        ColumnLayout {
            total_width: options.width,
            limit: options.limit,
            height,
            color: options.color,
            cards_per_row: options.cards_per_row,
            statuses: options.statuses,
        },
    )
}

/// Stack project boards, with ACCOUNTS and STATUS printed once for the fleet.
pub fn render_by_project(fleet: &FleetSnapshot, options: &RenderOptions) -> Vec<String> {
    let mut out = render_accounts(&fleet.fleet_health, options.width);
    let status = render_status(&fleet.fleet_health, options.width);
    let count = fleet.projects.len();
    let height = options
        .height
        .map(|rows| rows.saturating_sub(out.len() + status.len() + 2 * count + 2) / count.max(1));
    for project in &fleet.projects {
        out.push(String::new());
        out.push(sanitize_title(&project.name));
        out.extend(project_columns(project, options, height));
    }
    out.extend(status);
    out
}

fn place_cards(cards: Vec<Card>, project: &ProjectSnapshot) -> Vec<PlacedCard> {
    let color = project_color(&project.name);
    cards
        .into_iter()
        .map(|card| {
            let agent_color = card.agent.as_ref().map(|_| AGENT_COLOR);
            PlacedCard {
                card,
                project_short: Some(project.short.clone()),
                color: Some(color),
                agent_color,
            }
        })
        .collect()
}

/// Render the pooled fleet view: every project's BLOCKED/IN PROGRESS/
/// BACKLOG cards interleaved into one set of three columns, each card
/// tagged with its project's short name and color.
pub fn render_merged(fleet: &FleetSnapshot, options: &RenderOptions) -> Vec<String> {
    // Merged fleet view: accounts, then columns immediately (no per-project
    // heading), then the STATUS row — `cli.cmd_board`'s non-`--by-project`
    // branch.
    let mut out = render_accounts(&fleet.fleet_health, options.width);

    let mut blocked: Vec<PlacedCard> = Vec::new();
    let mut in_progress: Vec<PlacedCard> = Vec::new();
    let mut backlog: Vec<PlacedCard> = Vec::new();

    for project in &fleet.projects {
        let (mut p_blocked, p_in_progress) = split_dispatched(&project.tasks);
        let (bl_blocked, bl_backlog) = split_backlog(&project.backlog);
        p_blocked.extend(bl_blocked);

        blocked.extend(place_cards(p_blocked, project));
        in_progress.extend(place_cards(p_in_progress, project));
        backlog.extend(place_cards(bl_backlog, project));
    }

    sort_blocked_placed(&mut blocked);
    sort_in_progress_placed(&mut in_progress);
    sort_backlog_placed(&mut backlog);

    let status = render_status(&fleet.fleet_health, options.width);
    let height = options
        .height
        .map(|rows| rows.saturating_sub(out.len() + status.len() + 2));
    out.extend(render_columns_merged_with(
        &blocked,
        &in_progress,
        &backlog,
        ColumnLayout {
            total_width: options.width,
            height,
            limit: options.limit,
            color: options.color,
            cards_per_row: options.cards_per_row,
            statuses: options.statuses,
        },
    ));
    out.extend(status);
    out
}

#[cfg(test)]
const SNAPSHOT_80: [&str; 23] = [
    "",
    "demo",
    "┌─ BLOCKED ────────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────┐ ┌───────────────────────────────────┐  │",
    "│ │ 20261102-fixer-aaaaaa             │ │ 20261102-b-111111                 │  │",
    "│ │ work on 20261102-fixer-aaaaaa     │ │ which account should we use?      │  │",
    "│ │ claude 5m                         │ │ P1                                │  │",
    "│ └───────────── Builder ─────────────┘ └────────────── asked ──────────────┘  │",
    "└──────────────────────────────────────────────────────────────────────────────┘",
    "┌─ IN PROGRESS ────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────┐                                        │",
    "│ │ 20261102-fixer-bbbbbb             │                                        │",
    "│ │ work on 20261102-fixer-bbbbbb     │                                        │",
    "│ │ claude 5m                         │                                        │",
    "│ └───────────── Builder ─────────────┘                                        │",
    "└──────────────────────────────────────────────────────────────────────────────┘",
    "┌─ BACKLOG ────────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────┐                                        │",
    "│ │ 20261102-b-222222                 │                                        │",
    "│ │ write the docs                    │                                        │",
    "│ │ P2                                │                                        │",
    "│ └───────────────────────────────────┘                                        │",
    "└──────────────────────────────────────────────────────────────────────────────┘",
];
#[cfg(test)]
const SNAPSHOT_120: [&str; 23] = [
    "",
    "demo",
    "┌─ BLOCKED ────────────────────────────────────────────────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────────────────────────┐ ┌───────────────────────────────────────────────────────┐  │",
    "│ │ 20261102-fixer-aaaaaa                                 │ │ 20261102-b-111111                                     │  │",
    "│ │ work on 20261102-fixer-aaaaaa                         │ │ which account should we use?                          │  │",
    "│ │ claude 5m                                             │ │ P1                                                    │  │",
    "│ └─────────────────────── Builder ───────────────────────┘ └──────────────────────── asked ────────────────────────┘  │",
    "└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘",
    "┌─ IN PROGRESS ────────────────────────────────────────────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────────────────────────┐                                                            │",
    "│ │ 20261102-fixer-bbbbbb                                 │                                                            │",
    "│ │ work on 20261102-fixer-bbbbbb                         │                                                            │",
    "│ │ claude 5m                                             │                                                            │",
    "│ └─────────────────────── Builder ───────────────────────┘                                                            │",
    "└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘",
    "┌─ BACKLOG ────────────────────────────────────────────────────────────────────────────────────────────────────────────┐",
    "│ ┌───────────────────────────────────────────────────────┐                                                            │",
    "│ │ 20261102-b-222222                                     │                                                            │",
    "│ │ write the docs                                        │                                                            │",
    "│ │ P2                                                    │                                                            │",
    "│ └───────────────────────────────────────────────────────┘                                                            │",
    "└──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────┘",
];
#[cfg(test)]
const SNAPSHOT_160: [&str; 14] = [
    "",
    "demo",
    "┌─ BLOCKED ────────────────────────────────────────┐ ┌─ IN PROGRESS ────────────────────────────────────┐ ┌─ BACKLOG ────────────────────────────────────────┐",
    "│ ┌──────────────────────────────────────────────┐ │ │ ┌──────────────────────────────────────────────┐ │ │ ┌──────────────────────────────────────────────┐ │",
    "│ │ 20261102-fixer-aaaaaa                        │ │ │ │ 20261102-fixer-bbbbbb                        │ │ │ │ 20261102-b-222222                            │ │",
    "│ │ work on 20261102-fixer-aaaaaa                │ │ │ │ work on 20261102-fixer-bbbbbb                │ │ │ │ write the docs                               │ │",
    "│ │ claude 5m                                    │ │ │ │ claude 5m                                    │ │ │ │ P2                                           │ │",
    "│ └────────────────── Builder ───────────────────┘ │ │ └────────────────── Builder ───────────────────┘ │ │ └──────────────────────────────────────────────┘ │",
    "│ ┌──────────────────────────────────────────────┐ │ │                                                  │ │                                                  │",
    "│ │ 20261102-b-111111                            │ │ │                                                  │ │                                                  │",
    "│ │ which account should we use?                 │ │ │                                                  │ │                                                  │",
    "│ │ P1                                           │ │ │                                                  │ │                                                  │",
    "│ └─────────────────── asked ────────────────────┘ │ │                                                  │ │                                                  │",
    "└──────────────────────────────────────────────────┘ └──────────────────────────────────────────────────┘ └──────────────────────────────────────────────────┘",
];
#[cfg(test)]
const SNAPSHOT_200: [&str; 14] = [
    "",
    "demo",
    "┌─ BLOCKED ──────────────────────────────────────────────────────┐ ┌─ IN PROGRESS ──────────────────────────────────────────────────┐ ┌─ BACKLOG ──────────────────────────────────────────────────────┐",
    "│ ┌────────────────────────────────────────────────────────────┐ │ │ ┌────────────────────────────────────────────────────────────┐ │ │ ┌────────────────────────────────────────────────────────────┐ │",
    "│ │ 20261102-fixer-aaaaaa                                      │ │ │ │ 20261102-fixer-bbbbbb                                      │ │ │ │ 20261102-b-222222                                          │ │",
    "│ │ work on 20261102-fixer-aaaaaa                              │ │ │ │ work on 20261102-fixer-bbbbbb                              │ │ │ │ write the docs                                             │ │",
    "│ │ claude 5m                                                  │ │ │ │ claude 5m                                                  │ │ │ │ P2                                                         │ │",
    "│ └───────────────────────── Builder ──────────────────────────┘ │ │ └───────────────────────── Builder ──────────────────────────┘ │ │ └────────────────────────────────────────────────────────────┘ │",
    "│ ┌────────────────────────────────────────────────────────────┐ │ │                                                                │ │                                                                │",
    "│ │ 20261102-b-111111                                          │ │ │                                                                │ │                                                                │",
    "│ │ which account should we use?                               │ │ │                                                                │ │                                                                │",
    "│ │ P1                                                         │ │ │                                                                │ │                                                                │",
    "│ └────────────────────────── asked ───────────────────────────┘ │ │                                                                │ │                                                                │",
    "└────────────────────────────────────────────────────────────────┘ └────────────────────────────────────────────────────────────────┘ └────────────────────────────────────────────────────────────────┘",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::accounts::AccountState;
    use crate::card::{LiveAgentStatus, TaskOutcome};
    use crate::layout::{cell_width, visible_width};

    fn task(
        id: &str,
        outcome: Option<TaskOutcome>,
        live: Option<LiveAgentStatus>,
    ) -> DispatchedTaskSnapshot {
        DispatchedTaskSnapshot {
            task_id: id.into(),
            title: Some(format!("work on {id}")),
            role: "Builder".into(),
            account: "claude".into(),
            elapsed_text: "5m".into(),
            elapsed_seconds: Some(300),
            limited_count: 0,
            outcome,
            live_status: live,
            state_note: None,
            stale_pane_note: None,
            dirty_done_detail: None,
            acknowledged: false,
            corner_clock: None,
            corner_age: None,
            agent: None,
        }
    }

    fn project(name: &str, tasks: Vec<DispatchedTaskSnapshot>) -> ProjectSnapshot {
        ProjectSnapshot {
            name: name.into(),
            short: name.chars().take(4).collect(),
            tasks,
            backlog: Vec::new(),
        }
    }

    #[test]
    fn render_never_overflows_and_project_color_is_stable() {
        let a = project(
            "alpha",
            vec![task(
                "20261102-fixer-a",
                None,
                Some(LiveAgentStatus::Running),
            )],
        );
        let snapshot = BoardSnapshot {
            project: a,
            fleet_health: FleetHealth::default(),
        };
        let options = RenderOptions {
            width: 120,
            height: None,
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        };
        let lines = render(&snapshot, &options);
        for line in &lines {
            assert!(cell_width(line) <= 120, "{line:?}");
        }
        assert_eq!(project_color("alpha"), project_color("alpha"));
    }

    #[test]
    fn render_merged_tags_every_card_with_its_project_short_name() {
        let fleet = FleetSnapshot {
            projects: vec![
                project(
                    "alpha",
                    vec![task("20261102-fixer-a", Some(TaskOutcome::Blocked), None)],
                ),
                project(
                    "bravo",
                    vec![task("20261102-fixer-b", Some(TaskOutcome::Blocked), None)],
                ),
            ],
            fleet_health: FleetHealth::default(),
        };
        let options = RenderOptions {
            width: 150,
            height: None,
            limit: 10,
            color: true,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        };
        let lines = render_merged(&fleet, &options);
        assert!(
            lines.iter().any(|l| l.contains("alph")),
            "merged footer must name alpha's short: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("brav")),
            "merged footer must name bravo's short: {lines:?}"
        );
        for line in &lines {
            assert!(visible_width(line) <= 150, "{line:?}");
        }
    }

    fn backlog_item(
        id: &str,
        title: &str,
        priority: u8,
        status: crate::card::BacklogStatus,
    ) -> BacklogItemSnapshot {
        BacklogItemSnapshot {
            id: id.into(),
            title: title.into(),
            priority,
            created_order: 0,
            due_text: None,
            overdue: false,
            role_hint: None,
            status,
            has_human_annotation: false,
            elapsed_seconds: None,
            corner_clock: None,
            corner_age: None,
        }
    }

    /// A board with one card in each of BLOCKED (a stuck dispatch),
    /// BLOCKED (an unanswered ask), IN PROGRESS, and BACKLOG — enough of a
    /// mix to exercise n-up packing (two BLOCKED cards) and the plain
    /// single-card columns together.
    fn mixed_board() -> ProjectSnapshot {
        let mut project = project(
            "demo",
            vec![
                task("20261102-fixer-aaaaaa", Some(TaskOutcome::Blocked), None),
                task(
                    "20261102-fixer-bbbbbb",
                    None,
                    Some(LiveAgentStatus::Running),
                ),
            ],
        );
        project.backlog = vec![
            backlog_item(
                "20261102-b-111111",
                "which account should we use?",
                1,
                crate::card::BacklogStatus::Asked,
            ),
            backlog_item(
                "20261102-b-222222",
                "write the docs",
                2,
                crate::card::BacklogStatus::Open,
            ),
        ];
        project
    }

    /// Pins `render`'s output at the common terminal widths this task was
    /// asked to keep byte-stable: a regression guard against the n-up
    /// packing this task ported (`board.py`'s `_column_parts`/`_n_up_rows`/
    /// `_resolve_cards_per_row`) silently drifting again. Not a claim that
    /// these are the only correct strings — a deliberate change to the
    /// layout should update them deliberately, the same way any other
    /// pinned-output test is updated.
    #[test]
    fn snapshot_render_at_common_terminal_widths() {
        let snapshot = BoardSnapshot {
            project: mixed_board(),
            fleet_health: FleetHealth::default(),
        };
        for width in [80usize, 120, 160, 200] {
            let options = RenderOptions {
                width,
                height: None,
                limit: 10,
                color: false,
                cards_per_row: CardsPerRow::Auto,
                statuses: StatusFilter::all(),
            };
            let lines = render(&snapshot, &options);
            for line in &lines {
                assert!(cell_width(line) <= width, "width {width}: {line:?}");
            }
            let golden: &[&str] = match width {
                80 => &SNAPSHOT_80,
                120 => &SNAPSHOT_120,
                160 => &SNAPSHOT_160,
                200 => &SNAPSHOT_200,
                _ => unreachable!(),
            };
            assert_eq!(lines, golden, "width {width}");
        }
    }

    #[test]
    fn render_never_emits_the_not_a_lane_diagnostic() {
        let snapshot = BoardSnapshot {
            project: project("alpha", vec![]),
            fleet_health: FleetHealth {
                accounts: vec![AccountSnapshot {
                    name: "claude".into(),
                    model: String::new(),
                    vendor: String::new(),
                    kind: String::new(),
                    state: AccountState::Available,
                    seats_open: Some(1),
                    seats_free: Some(1),
                    seats_total: Some(2),
                    over_cap: false,
                    detail: None,
                    quota_windows: vec![],
                    tokens_24h: None,
                    as_of: None,
                }],
                accounts_running: None,
                accounts_capacity: None,
                seats: None,
                owed_replies: vec![],
                mail: vec![],
                last_cycle: vec![],
            },
        };
        let options = RenderOptions {
            width: 100,
            height: None,
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        };
        let lines = render(&snapshot, &options);
        assert!(lines.iter().all(|l| !l.contains("not a lane")));
    }

    fn assert_no_injected_escapes(lines: &[String]) {
        for line in lines {
            assert!(!line.contains('\u{1b}'), "ESC survived: {line:?}");
            assert!(!line.contains('\u{07}'), "BEL survived: {line:?}");
            assert!(
                !line.chars().any(|c| matches!(
                    c,
                    '\u{90}' | '\u{98}' | '\u{9b}' | '\u{9d}' | '\u{9e}' | '\u{9f}'
                )),
                "C1 introducer survived: {line:?}"
            );
            assert!(!line.contains("]52;"), "{line:?}");
            assert!(!line.contains("[2J"), "{line:?}");
            assert!(!line.contains("pwned"), "{line:?}");
            assert!(!line.contains("QUJDRA"), "{line:?}");
        }
    }

    /// Injects OSC 52 / OSC 0 / CSI / C1 into every untrusted field the
    /// board actually prints from a task or backlog card — title, agent
    /// (pane name), and pane-derived status notes — and asserts none of
    /// those sequences survive in the rendered frame.
    #[test]
    fn untrusted_task_title_card_title_agent_and_pane_text_cannot_inject_terminal_sequences() {
        let osc52 = "\u{1b}]52;c;QUJDRA==\u{07}";
        let osc0 = "\u{1b}]0;pwned\u{07}";
        let csi = "\u{1b}[2J";
        let c1 = "\u{9b}2K";

        let mut dispatched = task(
            "20261102-fixer-cccccc",
            None,
            Some(LiveAgentStatus::Running),
        );
        dispatched.title = Some(format!("task{osc52}title"));
        dispatched.agent = Some(format!("agent{csi}name"));
        dispatched.state_note = Some(format!("note{osc0}live"));
        dispatched.stale_pane_note = Some(format!("pane{c1}text"));

        let mut project = project("demo", vec![dispatched]);
        project.name = format!("proj{osc52}ect");
        project.backlog = vec![backlog_item(
            "20261102-b-333333",
            &format!("card{osc0}title"),
            1,
            crate::card::BacklogStatus::Open,
        )];

        let snapshot = BoardSnapshot {
            project,
            fleet_health: FleetHealth::default(),
        };
        let options = RenderOptions {
            width: 80,
            height: None,
            limit: 10,
            color: false,
            cards_per_row: CardsPerRow::Auto,
            statuses: StatusFilter::all(),
        };
        let lines = render(&snapshot, &options);
        assert_no_injected_escapes(&lines);
        let joined = lines.join("\n");
        assert!(joined.contains("tasktitle"), "{joined}");
        assert!(joined.contains("cardtitle"), "{joined}");
        assert!(joined.contains("agentname"), "{joined}");
        assert!(joined.contains("panetext"), "{joined}");
        assert!(joined.contains("notelive"), "{joined}");
        assert!(joined.contains("project"), "{joined}");
    }
}

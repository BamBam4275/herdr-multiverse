//! `Card`/`PlacedCard`: the per-item renderable unit, and the input
//! snapshot types a caller (the CLI lane, eventually) fills in from
//! resolved task/backlog data. `mv-board` never reads `.multiverse/`
//! itself — these structs are the whole contract with that side effect.

/// Which board column a card belongs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Blocked,
    InProgress,
    Backlog,
}

/// Why a dispatched lane has stopped moving, for the BLOCKED note text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveAgentStatus {
    Running,
    Awaiting,
    Unrecorded,
    Stalled,
    /// Herdr itself reports the pane as blocked.
    HerdrBlocked,
    /// The prompt classifier caught a dialog Herdr mislabels idle/done/unknown.
    PromptBlocked,
}

/// A task's recorded terminal outcome, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskOutcome {
    Done,
    Blocked,
    Failed,
}

/// A dispatched task, already resolved by the caller from `mv-state::tasks`
/// plus a live Herdr `agent list` snapshot. Field names mirror
/// `board.py`'s `_Card` inputs (`account elapsed [limited ×N]` status text,
/// `_state_note`/`_stale_pane_note`/dirty-done detail).
#[derive(Debug, Clone)]
pub struct DispatchedTaskSnapshot {
    pub task_id: String,
    pub title: Option<String>,
    /// Display role name. Left as an opaque string rather than an enum:
    /// role display naming (old vs. new names) is `mv-config`'s call, not
    /// board's — see `result.md` for why this crate doesn't take a side.
    pub role: String,
    pub account: String,
    pub elapsed_text: String,
    /// Seconds elapsed, used only for sort order (longest-waiting first).
    /// `None` sorts last, matching Python's "unknown last".
    pub elapsed_seconds: Option<u64>,
    pub limited_count: u32,
    pub outcome: Option<TaskOutcome>,
    pub live_status: Option<LiveAgentStatus>,
    /// `"awaiting, no live agent"` / `"unrecorded, needs --reconcile"` / etc.
    pub state_note: Option<String>,
    /// `"stale: pane gone <age>"` past `STALE_AFTER_MINUTES`.
    pub stale_pane_note: Option<String>,
    /// Set when outcome is `done` but the lane has uncommitted changes.
    pub dirty_done_detail: Option<String>,
    pub acknowledged: bool,
    /// Pre-formatted top-right corner clock, from `finished_at` or
    /// `dispatched_at` (`board.py`'s `_task_card.corner_when`).
    pub corner_clock: Option<String>,
    pub corner_age: Option<String>,
    /// Live Herdr pane name, when one is recorded. Empty means the
    /// agent line is omitted — the account still lives on `status_text`.
    pub agent: Option<String>,
}

/// A backlog item's current lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklogStatus {
    Open,
    Asked,
    Done,
}

/// A backlog card, resolved by the caller from `mv-state::backlog`.
#[derive(Debug, Clone)]
pub struct BacklogItemSnapshot {
    pub id: String,
    pub title: String,
    pub priority: u8,
    /// Monotonic creation order; lower sorts earlier within a priority.
    pub created_order: u64,
    pub due_text: Option<String>,
    pub overdue: bool,
    pub role_hint: Option<String>,
    pub status: BacklogStatus,
    /// Only meaningful when `status == Asked`: an `asked` item with a human
    /// annotation reads as answered and moves to BACKLOG, not BLOCKED.
    pub has_human_annotation: bool,
    pub elapsed_seconds: Option<u64>,
    /// Pre-formatted top-right corner clock, from `item.updated_at`.
    pub corner_clock: Option<String>,
    pub corner_age: Option<String>,
}

/// One renderable card: a dispatched task or a backlog item, already
/// reduced to display text.
#[derive(Debug, Clone)]
pub struct Card {
    pub id_line: String,
    pub title: String,
    pub agent: Option<String>,
    pub status_text: Option<String>,
    pub priority: Option<u8>,
    pub due_text: Option<String>,
    pub overdue: bool,
    pub role: Option<String>,
    /// Top-right corner clock (`"04:08"` / `"yday 04:08"` / `"Oct 12"`),
    /// already formatted by the caller from `corner_when`. Empty means
    /// the card draws a plain top border.
    pub corner_clock: Option<String>,
    /// Top-right corner age (`"12m"`), paired with [`Self::corner_clock`].
    /// Dropped first as the card narrows (`board.py`'s `_card_top`).
    pub corner_age: Option<String>,
    /// Sort-only: not rendered.
    pub(crate) wait_seconds: Option<u64>,
    pub(crate) acknowledged: bool,
}

/// A `Card` placed into the merged fleet view, carrying the per-project
/// color tag. `render`'s single-project cards leave these blank.
#[derive(Debug, Clone)]
pub struct PlacedCard {
    pub card: Card,
    pub project_short: Option<String>,
    pub color: Option<u8>,
    pub agent_color: Option<u8>,
}

fn status_text(snapshot: &DispatchedTaskSnapshot) -> String {
    // `board.py`'s `_task_card`: account, compact elapsed, then
    // `limited ×N` with no brackets — role lives on the bottom rail.
    let mut parts = vec![snapshot.account.as_str(), snapshot.elapsed_text.as_str()];
    let limited = format!("limited ×{}", snapshot.limited_count);
    if snapshot.limited_count > 0 {
        parts.push(&limited);
    }
    parts.join(" ")
}

/// Build the renderable card for one dispatched task.
pub fn task_card(snapshot: &DispatchedTaskSnapshot) -> Card {
    let mut notes = Vec::new();
    if let Some(note) = &snapshot.state_note {
        notes.push(note.clone());
    }
    if let Some(note) = &snapshot.stale_pane_note {
        notes.push(note.clone());
    }
    if let Some(detail) = &snapshot.dirty_done_detail {
        notes.push(format!(
            "done but lane has uncommitted changes({detail}): commit, or record failed/superseded"
        ));
    } else if snapshot.acknowledged {
        notes.push("acknowledged".to_string());
    }
    let mut status = status_text(snapshot);
    if !notes.is_empty() {
        // Python joins notes with `", "`, not an em-dash.
        status.push_str(", ");
        status.push_str(&notes.join(", "));
    }
    Card {
        id_line: snapshot.task_id.clone(),
        title: snapshot.title.clone().unwrap_or_default(),
        agent: snapshot.agent.clone(),
        status_text: Some(status),
        priority: None,
        due_text: None,
        overdue: false,
        role: Some(snapshot.role.clone()).filter(|role| !role.is_empty()),
        corner_clock: snapshot.corner_clock.clone(),
        corner_age: snapshot.corner_age.clone(),
        wait_seconds: snapshot.elapsed_seconds,
        acknowledged: snapshot.acknowledged,
    }
}

/// Build the renderable card for one backlog item. An unanswered `asked`
/// item puts `"asked"` in the bottom-rail role slot (`board.py`'s
/// `_backlog_card`); an answered one puts `"answered"` and sits in
/// BACKLOG. The top-right corner is the item's `updated_at` clock, not
/// the role word.
pub fn backlog_card(item: &BacklogItemSnapshot) -> Card {
    let role = match item.status {
        BacklogStatus::Asked if item.has_human_annotation => Some("answered".to_string()),
        BacklogStatus::Asked => Some("asked".to_string()),
        _ => item.role_hint.clone().filter(|role| !role.is_empty()),
    };
    let due_text = item.due_text.as_ref().map(|due| {
        if due.starts_with("due ") {
            due.clone()
        } else {
            format!("due {due}")
        }
    });
    Card {
        id_line: item.id.clone(),
        title: item.title.clone(),
        agent: None,
        status_text: None,
        priority: Some(item.priority),
        due_text,
        overdue: item.overdue,
        role,
        corner_clock: item.corner_clock.clone(),
        corner_age: item.corner_age.clone(),
        wait_seconds: item.elapsed_seconds,
        acknowledged: item.has_human_annotation,
    }
}

/// Split dispatched tasks into (BLOCKED, IN PROGRESS). A resolved
/// `done`/`failed` outcome renders in neither column.
///
/// `dirty_done_detail` and a `"pane gone, status "`-prefixed `state_note`
/// each force a card to BLOCKED ahead of every other check, matching
/// `board.py`'s `_split_dispatched`: `if t.dirty_done or t.reason.
/// startswith("pane gone, status "): blocked.append(t)` comes first in
/// that function too, before it even asks `_effective_status`. Both cases
/// carry no recorded `blocked` outcome — the status file says `done`, or
/// the task is merely UNRECORDED — so without this bypass they fall
/// through to IN PROGRESS and sit there needing a decision nobody is
/// shown (critic finding 6).
pub fn split_dispatched(tasks: &[DispatchedTaskSnapshot]) -> (Vec<Card>, Vec<Card>) {
    let mut blocked = Vec::new();
    let mut in_progress = Vec::new();
    for task in tasks {
        let pane_confirmed_gone = task
            .state_note
            .as_deref()
            .is_some_and(|note| note.starts_with("pane gone, status "));
        if task.dirty_done_detail.is_some() || pane_confirmed_gone {
            blocked.push(task_card(task));
            continue;
        }
        if let Some(outcome) = task.outcome {
            if outcome == TaskOutcome::Done || outcome == TaskOutcome::Failed {
                continue;
            }
            if outcome == TaskOutcome::Blocked {
                blocked.push(task_card(task));
                continue;
            }
        }
        let herdr_says_blocked = matches!(
            task.live_status,
            Some(LiveAgentStatus::HerdrBlocked) | Some(LiveAgentStatus::PromptBlocked)
        );
        if herdr_says_blocked {
            blocked.push(task_card(task));
        } else {
            in_progress.push(task_card(task));
        }
    }
    (blocked, in_progress)
}

/// Split backlog items into (BLOCKED, BACKLOG). `done` renders in neither.
pub fn split_backlog(items: &[BacklogItemSnapshot]) -> (Vec<Card>, Vec<Card>) {
    let mut blocked = Vec::new();
    let mut backlog = Vec::new();
    for item in items {
        match item.status {
            BacklogStatus::Done => continue,
            BacklogStatus::Open => backlog.push(backlog_card(item)),
            BacklogStatus::Asked => {
                if item.has_human_annotation {
                    backlog.push(backlog_card(item));
                } else {
                    blocked.push(backlog_card(item));
                }
            }
        }
    }
    (blocked, backlog)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatched(
        outcome: Option<TaskOutcome>,
        live: Option<LiveAgentStatus>,
    ) -> DispatchedTaskSnapshot {
        DispatchedTaskSnapshot {
            task_id: "20261102-fixer-abc123".into(),
            title: Some("do the thing".into()),
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

    #[test]
    fn a_blocked_task_lands_in_the_blocked_column_only() {
        let t = dispatched(Some(TaskOutcome::Blocked), None);
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert_eq!(blocked.len(), 1);
        assert!(in_progress.is_empty());
    }

    #[test]
    fn a_herdr_blocked_live_agent_lands_in_blocked_not_in_progress() {
        let t = dispatched(None, Some(LiveAgentStatus::HerdrBlocked));
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert_eq!(blocked.len(), 1);
        assert!(in_progress.is_empty());
    }

    #[test]
    fn a_prompt_blocked_running_task_lands_in_blocked_with_its_note() {
        let mut t = dispatched(None, Some(LiveAgentStatus::PromptBlocked));
        t.state_note = Some("stalled, agent is live but not working".into());
        let (blocked, _) = split_dispatched(&[t]);
        assert_eq!(blocked.len(), 1);
        assert!(blocked[0]
            .status_text
            .as_ref()
            .unwrap()
            .contains("stalled, agent is live but not working"));
    }

    #[test]
    fn a_dirty_done_task_lands_in_blocked_even_with_no_recorded_outcome() {
        let mut t = dispatched(None, None);
        t.dirty_done_detail = Some("1 modified + 0 untracked: foo.rs".into());
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert_eq!(blocked.len(), 1);
        assert!(in_progress.is_empty());
        assert!(blocked[0]
            .status_text
            .as_ref()
            .unwrap()
            .contains("done but lane has uncommitted changes"));
    }

    #[test]
    fn a_confirmed_gone_pane_lands_in_blocked_even_with_no_recorded_outcome() {
        let mut t = dispatched(None, None);
        t.state_note = Some("pane gone, status done".into());
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert_eq!(blocked.len(), 1);
        assert!(in_progress.is_empty());
    }

    #[test]
    fn an_unrecorded_pane_with_no_gone_confirmation_still_lands_in_progress() {
        let mut t = dispatched(None, None);
        t.state_note = Some("unrecorded, needs --reconcile".into());
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert!(blocked.is_empty());
        assert_eq!(in_progress.len(), 1);
    }

    #[test]
    fn a_done_outcome_renders_in_neither_column() {
        let t = dispatched(Some(TaskOutcome::Done), None);
        let (blocked, in_progress) = split_dispatched(&[t]);
        assert!(blocked.is_empty());
        assert!(in_progress.is_empty());
    }

    fn backlog_item(status: BacklogStatus, annotated: bool) -> BacklogItemSnapshot {
        BacklogItemSnapshot {
            id: "20261102-b-def456".into(),
            title: "investigate the thing".into(),
            priority: 1,
            created_order: 0,
            due_text: None,
            overdue: false,
            role_hint: None,
            status,
            has_human_annotation: annotated,
            elapsed_seconds: None,
            corner_clock: None,
            corner_age: None,
        }
    }

    #[test]
    fn an_asked_item_is_absent_from_backlog_and_present_in_blocked() {
        let item = backlog_item(BacklogStatus::Asked, false);
        let (blocked, backlog) = split_backlog(&[item]);
        assert_eq!(blocked.len(), 1);
        assert!(backlog.is_empty());
    }

    #[test]
    fn an_unanswered_asked_item_carries_the_asked_footer() {
        let card = backlog_card(&backlog_item(BacklogStatus::Asked, false));
        assert_eq!(card.role.as_deref(), Some("asked"));
    }

    #[test]
    fn an_answered_asked_item_has_the_answered_footer() {
        let card = backlog_card(&backlog_item(BacklogStatus::Asked, true));
        assert_eq!(card.role.as_deref(), Some("answered"));
    }

    #[test]
    fn an_open_item_has_no_asked_footer() {
        let card = backlog_card(&backlog_item(BacklogStatus::Open, false));
        assert_eq!(card.role, None);
    }

    #[test]
    fn an_answered_ask_moves_to_backlog() {
        let item = backlog_item(BacklogStatus::Asked, true);
        let (blocked, backlog) = split_backlog(&[item]);
        assert!(blocked.is_empty());
        assert_eq!(backlog.len(), 1);
    }

    #[test]
    fn a_done_backlog_item_renders_in_neither_column() {
        let item = backlog_item(BacklogStatus::Done, false);
        let (blocked, backlog) = split_backlog(&[item]);
        assert!(blocked.is_empty());
        assert!(backlog.is_empty());
    }
}

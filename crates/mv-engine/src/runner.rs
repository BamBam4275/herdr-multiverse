//! Runs a planned dispatch: acquires the fleet lock, starts the
//! Herdr agent, releases before the readiness wait.
//!
//! The planner, lock, and worktree modules are separate ownership lanes.
//! [`DispatchStore`] is their required adapter: it must allocate/rebind the
//! task under the project lock and create/adopt/verify writer worktrees.
//! Neither this module nor a Herdr state alone declares a task successful.

use std::collections::HashSet;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use mv_herdr::argv::{self, HerdrCommand};
use mv_herdr::envelope::{pane_id_from, AgentState, CommandResult, HerdrErrorCode};
use serde_json::Value;
use time::OffsetDateTime;

use crate::blocking_prompt;
use crate::codex_trust;
use crate::codex_worktree_permissions;
use crate::limits;

#[derive(Debug, thiserror::Error)]
pub enum RunnerError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Herdr(#[from] mv_herdr::error::HerdrError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("agent {agent} in pane {pane} needs attention: {reason}")]
    NeedsAttention {
        agent: String,
        pane: String,
        reason: String,
    },
}

/// Fully bound launch, returned by `DispatchStore::prepare` while both locks
/// are held. `native_args` starts with native model/effort arguments from
/// `mv-herdr` and provider permission/deny rules `prepare` already applied;
/// [`dispatch`] appends the Codex worktree permissions override to it once
/// `prepare`'s own worktree creation means there is a lane to describe (see
/// `apply_codex_worktree_permissions`).
#[derive(Debug, Clone)]
pub struct PreparedLaunch {
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub task_id: String,
    pub agent_name: String,
    pub role: String,
    pub kind: String,
    pub model: String,
    pub seat: String,
    pub brief: String,
    pub native_args: Vec<String>,
    /// Account environment; `dispatch` binds MULTIVERSE_TASK_ID to this task.
    pub environment: Vec<(String, String)>,
    pub clear_auth: Vec<String>,
    /// Trusted configuration command, including any leading env assignments.
    pub command: Option<String>,
}

/// State integration boundary, with no permissive production default.
/// Guards must own their locks and unlock on drop. All callbacks up through
/// `record_started` run with the fleet lock held; `prepare` also holds the
/// project lock. `prepare` must recheck admission/halt/seat limits, preserve
/// review/exclusion decisions, bind a free task id, verify the lane, copy
/// inputs, and prepare Codex trust/permissions before returning. Policy and
/// Git lifecycle stay in their owning modules rather than being guessed here.
pub trait DispatchStore {
    type Guard;
    fn lock_fleet(&mut self) -> Result<Self::Guard, RunnerError>;
    fn lock_project(&mut self) -> Result<Self::Guard, RunnerError>;
    /// Re-verify concurrency, project floors/ceilings, and the selected
    /// seat's own cap against *current* fleet state — a fresh chain walk,
    /// not the plan-time answer — while the fleet lock is held and before
    /// the project lock/[`prepare`](Self::prepare) even runs. Python's
    /// `concurrency_or_raise` + `project_slots_or_raise` +
    /// `recheck_seat_cap` (`runner.py:2440-2452`). The default is a no-op:
    /// a store with nothing to recheck yet must not start refusing every
    /// dispatch; a real adapter overrides this with
    /// `crate::dispatch::check_fleet_capacity` and a fresh selector chain
    /// walk.
    fn recheck_admission(&mut self) -> Result<(), RunnerError> {
        Ok(())
    }
    fn prepare(&mut self) -> Result<PreparedLaunch, RunnerError>;
    fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError>;
    /// Recompute collision names from the original project/role/seat when
    /// available, including the shorter project budget for each instance.
    fn unique_agent_name(
        &self,
        base: &str,
        taken: &HashSet<String>,
    ) -> Result<String, RunnerError> {
        unique_agent_name(base, taken)
    }
    /// Append the `event=dispatch` routing-log line before any pane exists
    /// — Python's `append_routing_log` (`runner.py:2628-2635`), called
    /// right after the agent's name is minted so the record names the
    /// agent that will actually run. Unlike [`record_started`](Self::record_started),
    /// a failure here is not best-effort in Python: `append_routing_log` is
    /// called with no exception guard there, so a real adapter should let
    /// this propagate. The default `Ok(())` only covers a store with no
    /// routing log wired in yet.
    fn record_dispatch(&mut self, launch: &PreparedLaunch) -> Result<(), RunnerError> {
        let _ = launch;
        Ok(())
    }
    /// Re-read the fleet halt immediately before the pane is created, in
    /// case the fleet was halted mid-plan. Python's equivalent check runs
    /// in the CLI just before calling the runner (`cli.py:3708-3726`);
    /// `crate::dispatch::check_halt_before_pane_creation`'s own doc
    /// comment places this port's call site here instead — "immediately
    /// before the runner would create a pane" — since this port's CLI does
    /// not call the runner yet. The default `Ok(())` only covers a store
    /// with no halt file wired in yet.
    fn check_halt_before_pane_creation(&mut self) -> Result<(), RunnerError> {
        Ok(())
    }
    /// Append-only started/routing record; failures become warnings.
    /// `started_at` is this run's own staleness floor (`Dispatched::started_at`,
    /// captured before the prompt was ever sent), logged as `event=started`'s
    /// own timestamp rather than the wall time a store happens to call this
    /// at. A fast agent's `prompt` can write its status file before this
    /// append runs; stamping the log with "now" instead of `started_at` can
    /// then land the logged second *after* that write's own second, and
    /// every other reader of this line (`read_writer_status`'s `since`
    /// cutoff, `supersede_collision_predecessor` among them) would misread
    /// that already-written status as a stale leftover from an earlier
    /// occupant of this task id and default its outcome away from what
    /// actually happened.
    fn record_started(
        &mut self,
        launch: &PreparedLaunch,
        pane: &str,
        started_at: SystemTime,
    ) -> Result<(), RunnerError>;
    /// Record this dispatch as `failed-to-start` once a start or prompt
    /// never lands, so `tasks.resolve` sees a terminal outcome instead of
    /// AWAITING forever and the fleet-cap slot it held is released.
    /// Python's `_record_failed_to_start` (`runner.py:2388-2401`), called
    /// from every failure point between `event=dispatch` and
    /// `event=started` — including a blocked/needs-attention pane, which
    /// Python records the same way since it has no separate case for it.
    /// `pane` is empty when the failure happened before a pane existed.
    /// Best effort like `record_started`: a logging failure here must not
    /// replace the dispatch error that caused it. The default `Ok(())`
    /// only covers a store with no routing log wired in yet.
    fn record_failed_to_start(
        &mut self,
        launch: &PreparedLaunch,
        pane: &str,
    ) -> Result<(), RunnerError> {
        let _ = (launch, pane);
        Ok(())
    }
    /// Trip the pool backing `launch`'s seat, given a settled/failed pane's
    /// own vendor-exhaustion [`limits::Verdict`] and the pane text it was
    /// read from. Python's one production call site for an automatic trip
    /// — `_maybe_trip_breaker`'s `breaker.trip(...)` (`runner.py:3119-3141`),
    /// reached from both of `dispatch()`'s own failure paths
    /// (`runner.py:2908-2920`'s `except Exception`, which is this crate's
    /// `dispatch`'s own failure branch below). `breaker::trip`'s
    /// `TripRequest` already derives the cooldown from `verdict.kind`,
    /// escalates repeated trips of `verdict.rule`, and writes `pane_text` as
    /// evidence given a project root — a real adapter only has to resolve
    /// `launch.seat`/`launch.kind` to a pool/vendor and a breaker config
    /// directory, none of which this crate has a type for. Returns whether a
    /// trip actually happened (`Ok(true)`), so a caller can log/report it;
    /// the default `Ok(false)` only covers a store with no breaker wired in
    /// yet — a verdict was still correctly classified either way, so this is
    /// never treated as an error on its own.
    ///
    /// Tripping the breaker here is this port's whole answer to "fall back
    /// to the next seat in the chain": Python's `dispatch()` never retries
    /// itself with a different seat either (confirmed against
    /// `runner.py:2908-3176` — no reselect call anywhere in
    /// `_maybe_trip_breaker` or its callers). A tripped pool is simply
    /// skipped by the *next* chain walk (`selector::select`'s
    /// `BreakerQuery`, already breaker-aware), the same way Python's own
    /// `AccountSelector.select` skips it afterward.
    fn trip_breaker(
        &mut self,
        launch: &PreparedLaunch,
        verdict: &limits::Verdict,
        pane_text: &str,
        now: OffsetDateTime,
    ) -> Result<bool, RunnerError> {
        let _ = (launch, verdict, pane_text, now);
        Ok(false)
    }
    /// Run configured setup outside the locks. The adapter owns its bounded
    /// subprocess and redacted lane-setup.log. Failure prevents prompting.
    fn setup_lane(&mut self, launch: &PreparedLaunch) -> Result<(), RunnerError>;
    /// Release any half-open trial on a failed start, exactly once. Storage
    /// errors must not replace the original dispatch error.
    fn release_trial(&mut self) -> Result<(), RunnerError>;
    /// Surface best-effort cleanup failures without masking the launch error.
    fn warn(&mut self, message: &str);
    /// Whether a failed dispatch should leave its pane open for a human to
    /// read — Python's `DispatchRunner.keep_pane_on_failure`, which
    /// defaults to `True` (`runner.py:2344-2346`): the screen is often the
    /// only evidence of what went wrong, and closing it is destructive.
    /// Distinct from `close_pane_when_done`, a success-only flag this crate
    /// does not own yet.
    fn keep_pane_on_failure(&self) -> bool {
        true
    }
    /// How long to retry `agent start` against a shell that has not
    /// finished coming up — Python's `SHELL_READY_RETRY_SECONDS`
    /// (`runner.py:127`), 5 seconds in production. A store overrides this
    /// downward only for tests; production has no reason to.
    fn shell_ready_retry(&self) -> Duration {
        Duration::from_secs(5)
    }
}

pub trait Herdr {
    fn call(
        &mut self,
        command: &HerdrCommand,
        timeout: Duration,
    ) -> Result<CommandResult, RunnerError>;
}

pub struct ProcessHerdr;

impl Herdr for ProcessHerdr {
    fn call(
        &mut self,
        command: &HerdrCommand,
        timeout: Duration,
    ) -> Result<CommandResult, RunnerError> {
        Ok(mv_herdr::run::run(command, Some(timeout))?)
    }
}

#[derive(Debug, Clone)]
pub struct Timing {
    pub readiness: Duration,
    pub prompt: Duration,
    pub poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            readiness: Duration::from_secs(120),
            prompt: Duration::from_secs(120),
            poll: Duration::from_secs(2),
        }
    }
}

#[derive(Debug)]
pub struct Dispatched {
    pub launch: PreparedLaunch,
    pub pane: String,
    pub baseline_seq: u64,
    pub warnings: Vec<String>,
    /// Wall-clock time this run's brief was written — [`wait_for_result`]'s
    /// staleness floor. Mirrors Python's `_status_predates_run`
    /// (`tasks.py:811-839`): a status file older than this run's own start
    /// is a leftover from a previous occupant of this task id, never this
    /// run's own result (`critic-history-compat.md` finding 6).
    pub started_at: SystemTime,
}

const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

fn checked(
    herdr: &mut impl Herdr,
    command: HerdrCommand,
    timeout: Duration,
) -> Result<CommandResult, RunnerError> {
    let result = herdr.call(&command, timeout)?;
    if !result.ok() {
        return Err(RunnerError::Refused(format!(
            "{}: {}",
            command.purpose,
            result.message()
        )));
    }
    if command.expects_json
        && !result
            .payload
            .as_ref()
            .is_some_and(|p| p["result"].is_object())
    {
        return Err(RunnerError::Refused(format!(
            "{}: missing JSON result",
            command.purpose
        )));
    }
    Ok(result)
}

fn validate(launch: &PreparedLaunch) -> Result<(), RunnerError> {
    if !mv_state::paths::is_valid_task_id(&launch.task_id) || launch.agent_name.trim().is_empty() {
        return Err(RunnerError::Refused("invalid task id or agent name".into()));
    }
    if ![
        "nexus",
        "scout",
        "builder",
        "archivist",
        "critic",
        "seer",
        "council",
        "artisan",
        "watcher",
    ]
    .contains(&launch.role.as_str())
    {
        return Err(RunnerError::Refused(
            "launch requires a current role name".into(),
        ));
    }
    if !launch.cwd.is_absolute() || !launch.project_root.is_absolute() {
        return Err(RunnerError::Refused("launch paths must be absolute".into()));
    }
    if launch.kind == "claude"
        && launch
            .command
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
    {
        return Err(RunnerError::Refused(
            "Claude requires an account command wrapper".into(),
        ));
    }
    if launch.clear_auth.iter().any(|name| {
        name.is_empty()
            || name.as_bytes()[0].is_ascii_digit()
            || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
    }) {
        return Err(RunnerError::Refused("invalid auth variable name".into()));
    }
    Ok(())
}

fn task_dir(launch: &PreparedLaunch) -> PathBuf {
    launch.cwd.join(".multiverse/tasks").join(&launch.task_id)
}

/// Fresh task directories prevent accidental overwrite on redispatch. The
/// store must rebind collisions while holding its project lock. Fsync the
/// brief and its directory before any pane or prompt can observe it.
fn write_brief(launch: &PreparedLaunch) -> Result<PathBuf, RunnerError> {
    let state = launch.cwd.join(".multiverse");
    let tasks = state.join("tasks");
    for path in [&state, &tasks] {
        match DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(path)?.is_dir() {
                    return Err(RunnerError::Refused(
                        "task parent is not a real directory".into(),
                    ));
                }
                // Critic finding 11: Python always `chmod(0o700)`s `tasks/`
                // and the task dir, even when they already existed — a
                // pre-existing 0755 `.multiverse`/`tasks` (e.g. the shared
                // checkout a reader uses) must not stay world-readable just
                // because this dispatch did not create it.
                fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
            }
            Err(e) => return Err(e.into()),
        }
    }
    let directory = task_dir(launch);
    DirBuilder::new().mode(0o700).create(&directory)?;
    let brief = directory.join("brief.md");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(&brief)?;
    file.write_all(launch.brief.as_bytes())?;
    file.sync_all()?;
    File::open(&directory)?.sync_all()?;
    File::open(&tasks)?.sync_all()?;
    File::open(&state)?.sync_all()?;
    File::open(&launch.cwd)?.sync_all()?;
    Ok(brief)
}

#[path = "naming.rs"]
pub mod naming;

pub fn unique_agent_name(base: &str, taken: &HashSet<String>) -> Result<String, RunnerError> {
    if !taken.contains(base) {
        return Ok(base.into());
    }
    for instance in 2..100 {
        let name = format!("{base}-{instance}");
        if !taken.contains(&name) {
            return Ok(name);
        }
    }
    Err(RunnerError::Refused(format!(
        "no free agent name for {base}"
    )))
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Quotes a config `command` word by word before it is spliced into a
/// `pane run` script (Herdr's own argv takes a single script string; there
/// is no argv seam to hand it instead). A bare `NAME=value` prefix — the
/// documented env-wrapper syntax, e.g. `CACHE=1 claude-interactive` — must
/// keep its `NAME=` unquoted for the shell to recognize it as an
/// assignment at all: quoting the whole word (`'NAME=value'`) makes the
/// shell look for a program literally named that instead of setting a
/// variable. Quoting only the value still blocks injection through it.
/// Every word once the prefix ends, including the binary/function name
/// itself, is quoted in full, the same way `native_args` already are —
/// quoting survives function-name lookup (quote removal happens before a
/// command word is resolved), so a configured shell-function wrapper still
/// runs.
fn quote_command_words(command: &str) -> String {
    let mut quoted = Vec::new();
    let mut in_prefix = true;
    for word in command.split_whitespace() {
        if in_prefix {
            if let Some((name, value)) = word.split_once('=') {
                if is_shell_identifier(name) {
                    quoted.push(format!("{name}={}", quote(value)));
                    continue;
                }
            }
            in_prefix = false;
        }
        quoted.push(quote(word));
    }
    quoted.join(" ")
}

fn is_shell_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub fn pane_label(launch: &PreparedLaunch) -> String {
    let program = launch
        .command
        .as_deref()
        .and_then(|command| command.split_whitespace().find(|word| !word.contains('=')))
        .unwrap_or(&launch.kind);
    format!(
        "{} · {} · {} · {} · {} {}",
        launch.agent_name, program, launch.model, launch.seat, launch.role, launch.task_id
    )
}

/// Python's `_start_agent` (`runner.py:3526-3534`): persists Codex's own
/// directory-trust decision for this launch's worktree and project root
/// before the process is started, so the launch never parks on Codex's
/// interactive trust prompt. A no-op for any other `kind`. `launch.
/// environment` always carries `(account.config_dir_var, account.
/// config_dir value)` as its first pair (`RealDispatchStore::prepare`),
/// which for `kind == "codex"` is always `CODEX_HOME` — `mv_config::load`
/// refuses a codex account whose `config_dir_var` is anything else.
fn ensure_codex_trust(launch: &PreparedLaunch) -> Result<(), RunnerError> {
    if launch.kind != "codex" {
        return Ok(());
    }
    let config_dir = launch
        .environment
        .iter()
        .find(|(key, _)| key == "CODEX_HOME")
        .map(|(_, value)| PathBuf::from(value))
        .ok_or_else(|| {
            RunnerError::Refused(format!(
                "codex launch for {:?} has no CODEX_HOME in its environment",
                launch.cwd
            ))
        })?;
    codex_trust::ensure_codex_trusted(&config_dir, &launch.cwd, &launch.project_root).map_err(|e| {
        RunnerError::Refused(format!(
            "could not ensure Codex trust for {}: {e}",
            launch.cwd.display()
        ))
    })
}

/// Port of Python's `_launch_commands` applying `herdr.
/// codex_worktree_permissions` to `agent_args` (`runner.py:3474-3478`): a
/// no-op for any other `kind`, and `None` for a reader dispatch or a
/// plan-time call (`codex_worktree_permissions`'s own doc comment) — by
/// this point in [`dispatch`], `launch.cwd` is the lane `store.prepare()`
/// already created, so the override is computed here rather than at
/// `prepare` time, when the worktree the override describes may not exist
/// yet. Appended after whatever `prepare` already put in `native_args`,
/// the same order Python's `[*agent_args, "-c", permissions]` uses.
fn apply_codex_worktree_permissions(launch: &mut PreparedLaunch) {
    if launch.kind != "codex" {
        return;
    }
    if let Some(permissions) =
        codex_worktree_permissions::codex_worktree_permissions(&launch.cwd, &launch.project_root)
    {
        launch.native_args.push("-c".to_string());
        launch.native_args.push(permissions);
    }
}

#[derive(Debug)]
struct Rectangle {
    pane: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

fn rectangles(result: &CommandResult) -> Option<Vec<Rectangle>> {
    let panes = result.payload.as_ref()?["result"]["layout"]["panes"].as_array()?;
    panes
        .iter()
        .map(|p| {
            // Card `20261102-b-01b66f`: each pane's geometry is nested under
            // its own `rect` object (Python's `herdr.pane_layout`/
            // `_where_to_split`/`_row_panes` all read `pane.get("rect")`,
            // never a flat `x`/`y`/`width`/`height` on the pane entry
            // itself) — reading the flat fields silently finds nothing,
            // which always sent `placement`/`equalize` down their
            // layout-unavailable fallback: every dispatch split "right" off
            // the caller's own pane forever, which is exactly Mike's report
            // of panes piling up in the hub tab instead of stacking in a
            // column.
            let rect = &p["rect"];
            let rectangle = Rectangle {
                pane: p["pane_id"].as_str()?.into(),
                x: rect["x"].as_f64()?,
                y: rect["y"].as_f64()?,
                width: rect["width"].as_f64()?,
                height: rect["height"].as_f64()?,
            };
            (rectangle.x.is_finite()
                && rectangle.y.is_finite()
                && rectangle.width.is_finite()
                && rectangle.height.is_finite()
                && rectangle.width > 0.0
                && rectangle.height > 0.0)
                .then_some(rectangle)
        })
        .collect()
}

/// `None` means "split `--current`", matching Python's own `None` return
/// from `_where_to_split` (`runner.py:728-794`) for both of its own
/// believe-neither-the-layout-nor-our-id cases: the origin pane missing
/// from the layout here mirrors Python's `mine not in rects`.
fn placement(current: &str, layout: &[Rectangle]) -> (Option<String>, &'static str) {
    let Some(origin) = layout.iter().find(|r| r.pane == current) else {
        return (None, "right");
    };
    // Reuse the nearest specialist column to the right, stacking below its
    // lowest pane. With no such column, make the first right-hand split.
    let right = layout
        .iter()
        .filter(|r| r.x >= origin.x + origin.width - 1.0)
        .min_by(|a, b| a.x.total_cmp(&b.x));
    let Some(column) = right else {
        return (Some(current.into()), "right");
    };
    let bottom = layout
        .iter()
        .filter(|r| (r.x - column.x).abs() <= 1.0)
        .max_by(|a, b| (a.y + a.height).total_cmp(&(b.y + b.height)))
        .unwrap_or(column);
    (Some(bottom.pane.clone()), "down")
}

fn equalize(herdr: &mut impl Herdr, pane: &str) {
    // Cosmetic, bounded, and fail-open: neither an older Herdr without the
    // layout surface nor a resize refusal should lose a running dispatch.
    for _ in 0..10 {
        let Ok(result) = checked(herdr, argv::pane_layout(pane), COMMAND_TIMEOUT) else {
            return;
        };
        let Some(mut layout) = rectangles(&result) else {
            return;
        };
        for _ in 0..4 {
            let Some(target) = layout.iter().find(|r| r.pane == pane) else {
                return;
            };
            let rows: Vec<_> = layout
                .iter()
                .filter(|r| (r.x - target.x).abs() <= 1.0)
                .collect();
            if rows.len() < 2 {
                return;
            }
            let total: f64 = rows.iter().map(|r| r.height).sum();
            let share = total / rows.len() as f64;
            let Some(row) = rows.iter().max_by(|a, b| {
                (share - a.height)
                    .abs()
                    .total_cmp(&(share - b.height).abs())
            }) else {
                return;
            };
            let delta = share - row.height;
            if delta.abs() <= 1.0 {
                return;
            }
            let Ok(resized) = checked(
                herdr,
                argv::pane_resize(&row.pane, "down", delta / total),
                COMMAND_TIMEOUT,
            ) else {
                return;
            };
            let Some(resize) = resized.payload.as_ref().map(|p| &p["result"]["resize"]) else {
                return;
            };
            if resize["changed"].as_bool() != Some(true) {
                return;
            }
            let synthetic = CommandResult {
                payload: Some(serde_json::json!({"result": {"layout": resize["layout"]}})),
                ..resized.clone()
            };
            let Some(next) = rectangles(&synthetic) else {
                break;
            };
            layout = next;
        }
    }
}

/// Panes sharing `anchor`'s row, sorted left to right — Python's
/// `_row_panes` (`runner.py:803-825`). Exact `y` equality, not the
/// tolerance-based grouping `equalize` above uses for columns: Python reads
/// the same rects a fresh `pane layout` or resize reply reports, with no
/// slack of its own. `None` when `anchor` is not in `layout` at all,
/// mirroring Python's own `anchor_pane not in rects` case.
fn row_panes(layout: &[Rectangle], anchor: &str) -> Option<Vec<Rectangle>> {
    let anchor_y = layout.iter().find(|r| r.pane == anchor)?.y;
    let mut row: Vec<Rectangle> = layout
        .iter()
        .filter(|r| r.y == anchor_y)
        .map(|r| Rectangle {
            pane: r.pane.clone(),
            x: r.x,
            y: r.y,
            width: r.width,
            height: r.height,
        })
        .collect();
    row.sort_by(|a, b| a.x.total_cmp(&b.x));
    Some(row)
}

/// Evens out the widths of every pane sharing `anchor`'s row — Python's
/// `equalize_row` (`runner.py:829-920`). Best-effort, same posture as
/// `equalize` above: a bad reply, a row of one, or herdr itself missing is
/// a silent no-op, never a reason to fail the dispatch this tidies up
/// after. Never touches a pane outside `anchor`'s own row.
///
/// One `pane layout` read seeds the row; every pass after that reads the
/// row straight back off each resize's own reply, never re-polling
/// layout, same as Python. Each pass walks the row left to right one
/// boundary at a time: the first resize on a boundary uses the whole
/// row's width as its distance estimate (a deliberate undershoot per
/// Python's own comment, since a boundary's containing split is narrower
/// than the full row for every pane but the first), and each resize after
/// that recalibrates from how far the previous one actually moved the
/// divider. Stops once every width is within 1 column of an equal share,
/// a resize comes back `changed: false`, or 10 passes have run.
fn equalize_row(herdr: &mut impl Herdr, anchor: &str) {
    let Ok(result) = checked(herdr, argv::pane_layout(anchor), COMMAND_TIMEOUT) else {
        return;
    };
    let Some(layout) = rectangles(&result) else {
        return;
    };
    let Some(mut row) = row_panes(&layout, anchor) else {
        return;
    };
    if row.len() < 2 {
        return;
    }

    for _ in 0..10 {
        let count = row.len();
        let left_edge = row[0].x;
        let last = &row[count - 1];
        let total_width = last.x + last.width - left_edge;
        let target_share = total_width / count as f64;
        if row.iter().all(|p| (p.width - target_share).abs() <= 1.0) {
            return;
        }

        for i in 0..count - 1 {
            let target_boundary = (left_edge + (i + 1) as f64 * total_width / count as f64).round();
            let mut span = total_width;
            for _ in 0..4 {
                let delta = target_boundary - row[i + 1].x;
                if delta.abs() <= 1.0 {
                    break;
                }
                // `--amount` must be positive: `direction` alone says which
                // way the divider goes. Move it with the pane on the side
                // it travels toward, whose own edge it is.
                let (pane, direction) = if delta > 0.0 {
                    (row[i].pane.clone(), "right")
                } else {
                    (row[i + 1].pane.clone(), "left")
                };
                let amount = (delta.abs() / span).min(0.5);
                let before = row[i + 1].x;
                let Ok(resized) = checked(
                    herdr,
                    argv::pane_resize(&pane, direction, amount),
                    COMMAND_TIMEOUT,
                ) else {
                    break;
                };
                let Some(resize) = resized.payload.as_ref().map(|p| &p["result"]["resize"]) else {
                    break;
                };
                if resize["changed"].as_bool() == Some(false) {
                    break;
                }
                let synthetic = CommandResult {
                    payload: Some(serde_json::json!({"result": {"layout": resize["layout"]}})),
                    ..resized.clone()
                };
                let Some(new_layout) = rectangles(&synthetic) else {
                    break;
                };
                let Some(new_row) = row_panes(&new_layout, anchor) else {
                    break;
                };
                if new_row.len() != count {
                    break;
                }
                row = new_row;
                let moved = (row[i + 1].x - before).abs();
                if moved == 0.0 {
                    break;
                }
                span = moved / amount;
            }
        }
    }
}

/// Close `pane`, then best-effort tidy whatever row it leaves behind —
/// Python's `DispatchRunner.teardown` + `equalize_row` (`runner.py:3208-3225`,
/// `:829-920`), called once a `--wait` dispatch or council turn settles with
/// a usable `done` result (`self.close_pane_when_done and live.status is
/// TaskStatus.DONE`). `anchor` is the caller's own long-lived pane
/// (`HERDR_PANE_ID`); an empty anchor skips the tidy read, the same way
/// Python's own `if ":" in mine` guard does. The closed pane's own row can
/// no longer be read by its id once it is gone (`pane layout` takes
/// `--pane`, and there is no `--tab`), so this equalizes from the
/// orchestrator's own long-lived pane instead — the same anchor
/// `_where_to_split` already trusts for the same reason.
pub fn teardown(herdr: &mut impl Herdr, pane: &str, anchor: &str) {
    let _ = checked(herdr, argv::pane_close(pane), COMMAND_TIMEOUT);
    if anchor.is_empty() {
        return;
    }
    equalize_row(herdr, anchor);
}

/// Python's `_DIALOG_MENU_MARKERS`, verbatim: the fixed vocabulary an open
/// `AskUserQuestion` picker's own footer shows, however Herdr classifies the
/// pane's `agent_status` (card `20261102-b-5b1325` — such a pane reads
/// `done`, not `blocked`, so the trust/approval needles in
/// [`attention_reason`] never catch it on their own).
const DIALOG_MENU_MARKERS: &[&str] = &[
    "enter to select",
    "esc to cancel",
    "enter confirm",
    "esc not now",
    "↑/↓ choose",
];

/// Python's `_DIALOG_MENU_TAIL_LINES`: only the pane's last few non-blank
/// lines are scanned, so a pane that merely scrolled past this vocabulary
/// earlier is not mistaken for one still parked on it.
const DIALOG_MENU_TAIL_LINES: usize = 6;

/// Whether `text`'s last few non-blank lines show an open question-menu
/// footer — Python's `dialog_or_menu_reason` menu-tail scan
/// (`tasks.py:1884-1893`).
fn menu_reason(text: &str) -> bool {
    let tail: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let start = tail.len().saturating_sub(DIALOG_MENU_TAIL_LINES);
    let joined = tail[start..].join("\n").to_lowercase();
    DIALOG_MENU_MARKERS
        .iter()
        .any(|marker| joined.contains(marker))
}

/// Return a reason to leave the pane open for a human. Never send keystrokes
/// to trust/approval dialogs or an interactive menu.
pub fn attention_reason(text: &str) -> Option<&'static str> {
    let lower = text.to_lowercase();
    if [
        "do you trust",
        "trust this folder",
        "trust this workspace",
        "trust the files",
        "trust this directory",
        "trust project",
        "workspace trust",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some("trust dialog")
    } else if [
        "permission required",
        "approve this",
        "allow this",
        "requires approval",
        "select an option",
        "choose an option",
        "would you like to proceed",
        "would you like to allow",
        "press enter to continue",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
    {
        Some("approval dialog or menu")
    } else if menu_reason(text) {
        Some("menu")
    } else {
        None
    }
}

/// The vendor-specific half of Python's `tasks.dialog_or_menu_reason`
/// (`tasks.py:1854-1894`): a billing, model-switch, sandbox-approval, or CLI
/// update dialog [`attention_reason`]'s own generic trust/approval wording
/// does not recognise. `kind` is `PreparedLaunch::kind` ("codex"/"claude"/
/// "grok"); same fail-open contract as [`blocking_prompt::classify_prompt`]
/// itself — an unrecognised `kind` or no matching shape yields `None`.
/// The menu-tail scan half of `dialog_or_menu_reason` lives in
/// [`attention_reason`] (via [`menu_reason`]) so every `visible` caller —
/// reseed, first prompt, heartbeat, sweep nudge — refuses an open
/// AskUserQuestion picker. `_codex_trust_prompt` is already covered by
/// [`attention_reason`]'s own "trust dialog" branch for every `kind`, not
/// just codex.
fn vendor_dialog_reason(text: &str, kind: &str) -> Option<String> {
    blocking_prompt::classify_prompt(text, kind).map(|prompt| format!("dialog ({})", prompt.rule))
}

/// Python's `tasks.dialog_or_menu_reason` (`tasks.py:1854-1894`): vendor
/// billing/model-switch/approval shapes first, then the generic
/// trust/approval/menu scan. A nudge (and any other "about to type into
/// this pane" caller) uses this *before* `agent prompt`, so a parked
/// dialog is never sent keystrokes.
pub fn dialog_or_menu_reason(text: &str, kind: &str) -> Option<String> {
    if let Some(reason) = vendor_dialog_reason(text, kind) {
        return Some(reason);
    }
    match attention_reason(text) {
        Some("menu") => Some("menu".into()),
        Some(other) => Some(format!("dialog ({other})")),
        None => None,
    }
}

fn update_screen(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "update available",
        "updating codex",
        "update codex",
        "new version of codex",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn attention(agent: &str, pane: &str, reason: impl Into<String>) -> RunnerError {
    RunnerError::NeedsAttention {
        agent: agent.into(),
        pane: pane.into(),
        reason: reason.into(),
    }
}

fn snapshot(value: &Value, agent: &str, pane: &str) -> Option<(AgentState, u64)> {
    if value["name"].as_str() != Some(agent) || value["pane_id"].as_str() != Some(pane) {
        return None;
    }
    Some((
        value["agent_status"].as_str()?.parse().ok()?,
        value["state_change_seq"].as_u64()?,
    ))
}

fn pause(start: Instant, budget: Duration, poll: Duration) -> bool {
    let remaining = budget.saturating_sub(start.elapsed());
    if remaining.is_zero() {
        return false;
    }
    thread::sleep(poll.min(remaining));
    true
}

/// `pub(crate)` (card `20261102-b-b4ede0`) so `crate::sweep_nudges` can
/// reuse this exact dialog-or-unreadable gate — the same read, the same
/// [`attention_reason`] classification — before it ever sends a canned
/// nudge into a pane, rather than re-deriving a second copy of it.
pub(crate) fn visible(
    herdr: &mut impl Herdr,
    agent: &str,
    pane: &str,
) -> Result<String, RunnerError> {
    let result = checked(
        herdr,
        argv::pane_read(pane, Some("visible"), None),
        COMMAND_TIMEOUT,
    )?;
    if let Some(reason) = attention_reason(&result.stdout) {
        return Err(attention(agent, pane, reason));
    }
    if result.stdout.trim().is_empty() {
        return Err(RunnerError::Refused("pane is unreadable".into()));
    }
    Ok(result.stdout)
}

/// Refuse a first prompt until the pane is clear of a billing, model-switch,
/// or other vendor dialog [`visible`]'s own generic trust/approval check
/// does not recognise. Python's `_wait_for_prompt_pane` (`runner.py:3607-
/// 3651`), minus its own retry loop: by the time this runs, [`wait_ready`]
/// has already retried/waited for a readable pane Herdr itself reports
/// ready, so this is one more read rather than a second poll loop. Only
/// called from [`dispatch`]'s own first-prompt path — Python's own call
/// site is just as singular (`_start_agent`'s caller, never `_prompt_once`'s
/// own mid-retry dialog check, which has no equivalent here; see
/// [`deliver_prompt_to`]'s doc comment).
fn refuse_if_parked_on_dialog(
    herdr: &mut impl Herdr,
    agent: &str,
    pane: &str,
    kind: &str,
) -> Result<(), RunnerError> {
    let text = visible(herdr, agent, pane)?;
    if let Some(reason) = vendor_dialog_reason(&text, kind) {
        return Err(attention(agent, pane, reason));
    }
    Ok(())
}

/// A plain, best-effort pane read for breaker classification, mirroring
/// `_maybe_trip_breaker`'s own `herdr.agent_read` (`runner.py:3061-3071`): a
/// read failure or empty text is "nothing to classify", not a dispatch
/// failure in its own right, so this returns `None` rather than propagating
/// a [`RunnerError`] — the caller already has its own dispatch error to
/// report and must not have it replaced by a diagnostic read's own failure.
fn read_for_classification(herdr: &mut impl Herdr, pane: &str) -> Option<String> {
    let result = checked(
        herdr,
        argv::pane_read(pane, Some("visible"), None),
        COMMAND_TIMEOUT,
    )
    .ok()?;
    (!result.stdout.trim().is_empty()).then_some(result.stdout)
}

pub fn wait_ready(
    herdr: &mut impl Herdr,
    agent: &str,
    pane: &str,
    timing: &Timing,
) -> Result<u64, RunnerError> {
    let start = Instant::now();
    loop {
        // Read even a not-yet-detected agent's pane: startup can be parked at
        // a trust dialog before Herdr gives it a usable agent state.
        let screen = visible(herdr, agent, pane);
        if let Err(error @ RunnerError::NeedsAttention { .. }) = screen {
            return Err(error);
        }
        if let Ok(result) = checked(herdr, argv::agent_list(), COMMAND_TIMEOUT) {
            if let Some(agents) = result
                .payload
                .as_ref()
                .and_then(|p| p["result"]["agents"].as_array())
            {
                if let Some((state, seq)) = agents.iter().find_map(|a| snapshot(a, agent, pane)) {
                    if state.is_ready_for_input()
                        && screen.as_ref().is_ok_and(|s| !update_screen(s))
                    {
                        return Ok(seq);
                    }
                }
            }
        }
        if !pause(start, timing.readiness, timing.poll) {
            return Err(RunnerError::Refused(format!(
                "agent {agent} in pane {pane} did not become readable and ready"
            )));
        }
    }
}

/// Wait only for Herdr to notice the pane in `agent list`, in any state —
/// the weaker condition `agent rename` needs, not the full
/// `is_ready_for_input` [`wait_ready`] waits for. Python's
/// `wait_until_ready(..., require_ready=False)` (`runner.py:3328-3366`).
/// Renaming before this returns can fail with "agent target <pane> not
/// found" the moment Herdr has not yet classified a freshly run seat
/// command (A29).
fn wait_detected(herdr: &mut impl Herdr, pane: &str, timing: &Timing) -> Result<(), RunnerError> {
    let start = Instant::now();
    loop {
        if let Ok(result) = checked(herdr, argv::agent_list(), COMMAND_TIMEOUT) {
            if let Some(agents) = result
                .payload
                .as_ref()
                .and_then(|p| p["result"]["agents"].as_array())
            {
                if agents.iter().any(|a| a["pane_id"].as_str() == Some(pane)) {
                    return Ok(());
                }
            }
        }
        if !pause(start, timing.readiness, timing.poll) {
            return Err(RunnerError::Refused(format!(
                "Herdr never detected an agent in pane {pane}"
            )));
        }
    }
}

/// Codex's own self-update screen clearing, a hardcoded wait independent of
/// [`Timing`] — Python's `CODEX_UPDATE_WAIT_SECONDS = 120.0`
/// (`runner.py:132`), not a per-caller tunable there either.
const CODEX_UPDATE_WAIT: Duration = Duration::from_secs(120);

/// Wait for a Codex self-update screen to clear. Python's
/// `_wait_for_codex_update` (`runner.py:3422-3429`): fail-open on a
/// diagnostic read error, since this is a secondary check inside a start
/// retry, not the dispatch's primary evidence.
fn wait_for_codex_update(herdr: &mut impl Herdr, agent: &str, pane: &str, timing: &Timing) -> bool {
    let start = Instant::now();
    loop {
        let still_updating = visible(herdr, agent, pane).is_ok_and(|text| update_screen(&text));
        if !still_updating {
            return true;
        }
        if !pause(start, CODEX_UPDATE_WAIT, timing.poll) {
            return false;
        }
    }
}

/// Retry `agent start` against a shell that has not finished coming up yet
/// — Python's `_start_agent` retry loop (`runner.py:3567-3592`):
/// "agent target pane ... is not an available shell" can race a shell
/// that simply has not accepted the pane's own `pane run` line yet. Retries
/// for `budget` (Python's `SHELL_READY_RETRY_SECONDS`, `store.shell_ready_retry()`
/// in this port), with a Codex self-update screen getting its own,
/// separately-bounded wait rather than counting against that budget.
/// `result` is the already-attempted first call; this only runs again if
/// that one failed.
fn retry_agent_start(
    herdr: &mut impl Herdr,
    launch: &PreparedLaunch,
    pane: &str,
    timing: &Timing,
    budget: Duration,
    mut result: CommandResult,
) -> Result<CommandResult, RunnerError> {
    let start = Instant::now();
    while !result.ok() {
        let codex_updating = launch.kind == "codex"
            && matches!(result.error_code(), Some(HerdrErrorCode::AgentNotReady))
            && visible(herdr, &launch.agent_name, pane).is_ok_and(|text| update_screen(&text));
        if codex_updating {
            if !wait_for_codex_update(herdr, &launch.agent_name, pane, timing) {
                return Err(RunnerError::Refused(format!(
                    "Codex self-update in pane {pane} did not clear within {CODEX_UPDATE_WAIT:?}; \
                     leaving the pane open for inspection"
                )));
            }
        } else if !pause(start, budget, timing.poll) {
            break;
        }
        result = herdr.call(
            &argv::agent_start(
                &launch.agent_name,
                &launch.kind,
                pane,
                Some(60_000),
                &launch.native_args,
            ),
            Duration::from_secs(65),
        )?;
    }
    Ok(result)
}

/// Sequence movement, rather than a pre-existing idle/done state, is the
/// completion gate. A blocked sample must hold still for a second poll.
#[derive(Default)]
pub struct TurnGate {
    blocked_seq: Option<u64>,
}

impl TurnGate {
    pub fn observe(&mut self, baseline: u64, state: AgentState, seq: u64) -> bool {
        if state == AgentState::Blocked {
            let settled = self.blocked_seq == Some(seq);
            self.blocked_seq = Some(seq);
            settled
        } else {
            self.blocked_seq = None;
            seq > baseline && state.is_ready_for_input()
        }
    }
}

/// The glyph each supported `kind`'s own idle composer opens a line with.
/// `codex` draws `›`; `claude` and `grok` both draw `❯` — confirmed for
/// grok against its own bordered composer in
/// `tests/fixtures/rate-limit/grok-weekly-quota-remaining.txt`, where a
/// lone `❯` sits inside the input box. Any other/unrecognised `kind`
/// falls back to the same `❯`, the majority shape — `PreparedLaunch::kind`
/// only ever carries one of the three above in practice.
fn composer_marker(kind: &str) -> &'static str {
    match kind {
        "codex" => "›",
        _ => "❯",
    }
}

/// The last `COMPOSER_TAIL_CHARS` characters of `prompt`, char-boundary
/// safe. `prompt` is a single line with no newlines of its own to anchor
/// a line-based match, so when a composer's finite height has scrolled
/// its opening line out of view, this distinctive tail — it always ends
/// in three real, backtick-quoted file paths — is what's left to look
/// for.
const COMPOSER_TAIL_CHARS: usize = 48;

fn distinctive_tail(prompt: &str) -> &str {
    match prompt.char_indices().rev().nth(COMPOSER_TAIL_CHARS - 1) {
        Some((at, _)) => &prompt[at..],
        None => prompt,
    }
}

/// Below this length, [`nudge_stuck_composer`] never presses the bare
/// Enter it would otherwise offer a stuck composer — no matter what a
/// fresh read shows. `distinctive_tail` above leans on a long prompt
/// always ending in three real, backtick-quoted file paths: nothing else
/// would coincidentally reproduce that, so a composer-text match is
/// trustworthy evidence the prompt is still sitting there, unsubmitted.
/// A short prompt carries no such distinctive content, and Claude Code
/// can offer a ghost (inline, not-yet-accepted) suggestion at a fresh
/// idle composer that repeats a just-run short nudge verbatim (card
/// `20261106-b-e08cef`) — that reads, in the composer's own region,
/// exactly like the same nudge still sitting there unsubmitted, and pane
/// text alone cannot tell the two apart. Reusing `COMPOSER_TAIL_CHARS` as
/// the cutoff ties the two together: the same length below which a
/// composer match stops being distinctive is where it stops being
/// trustworthy enough to act on, too. Below it, a genuinely stuck short
/// paste is left unresolved rather than risk re-submitting a nudge that
/// already ran.
const GHOST_ECHO_RISK_CHARS: usize = COMPOSER_TAIL_CHARS;

/// Composer-specific evidence that `prompt` has not yet been submitted:
/// not "appears anywhere in the pane" but "appears in the input-box
/// region" — the pane's own text from the *last* occurrence of `kind`'s
/// own composer marker onward. Reviewer HOLD `20261106-reviewer-731eab`:
/// a brief retained in scrollback after a real submission still satisfies
/// a whole-pane `contains`, so the one bare Enter below it unlocks can
/// land on whatever the agent's *next* turn shows at its own fresh
/// prompt — including editor ghost text. Scrollback sits *above* the
/// pane's last marker, never after it, so it is excluded by construction
/// once the search starts there instead of at byte 0.
///
/// A pane with no marker at all (an unrecognisable screen) is never
/// "still unsubmitted" either — [`nudge_stuck_composer`] must not press
/// Enter into a screen it cannot read as a composer, only into one it has
/// positive evidence is still holding the prompt.
fn prompt_still_in_composer(text: &str, prompt: &str, kind: &str) -> bool {
    let Some(start) = text.rfind(composer_marker(kind)) else {
        return false;
    };
    let region = &text[start..];
    region.contains(prompt) || region.contains(distinctive_tail(prompt))
}

/// Hand `task`'s brief to the agent Herdr just started, and verify it was
/// actually submitted — not merely that `agent prompt` returned, and not
/// merely that `state_change_seq` moved.
///
/// Card `20261106-b-19efc6`: a codex pane dispatched by the Python fleet
/// binary sat idle ~70 minutes with the brief still sitting, unsubmitted,
/// in its own input box. `agent prompt` had returned success and
/// `state_change_seq` had already moved — Python's own `_prompt_landed`
/// doc (`runner.py:3660-3680`) names exactly this shape: a composer can
/// produce a lifecycle change from the keystrokes alone and never submit.
/// No status file was ever written, and nothing here would have noticed.
///
/// A thin formatter over [`deliver_prompt_to`], which carries every safety
/// rule this needs — the composer-specific landing check, the fresh-idle
/// gate before a bare Enter, the vendor-dialog refusal — so a first
/// dispatch and a later nudge/reprompt (card `20261106-b-f174b0`) share one
/// implementation instead of two that can drift.
fn deliver_prompt(
    herdr: &mut impl Herdr,
    task: &Dispatched,
    brief: &Path,
    timing: &Timing,
) -> Result<(), RunnerError> {
    let prompt = format!("Read `{}` and execute it. Follow its write scope exactly. Write your findings to `{}` and then write `done`, `blocked`, or `failed` to `{}`.",
        brief.display(), brief.with_file_name("result.md").display(), brief.with_file_name("status").display());
    deliver_prompt_to(
        herdr,
        &task.launch.agent_name,
        &task.pane,
        &prompt,
        &task.launch.kind,
        task.baseline_seq,
        timing,
    )
}

/// Whether `prompt` has actually landed with `name`/`pane`: the agent is
/// `Working`, or `state_change_seq` has moved past `baseline_seq` *and*
/// `prompt` is no longer [`prompt_still_in_composer`] — not merely absent
/// from the whole pane, which scrollback can satisfy long after a real
/// submission (reviewer HOLD `20261106-reviewer-731eab`). A moved sequence
/// alone is not enough — see [`deliver_prompt_to`]'s own doc comment (cards
/// `20261106-b-19efc6`, `20261106-b-f174b0`). `kind` lets a vendor-specific
/// dialog that replaced the prompt (so it no longer reads as landed) be
/// recognised as exactly that, not mistaken for a landed turn.
///
/// `Ok(false)` is a claim that the polling budget actually observed the
/// agent and saw it not yet landed — never a stand-in for "nothing could be
/// read." Second Critic HOLD `20261106-reviewer-f7aaf4`: a budget whose
/// every `agent get` failed has zero successful reads, not negative
/// evidence, and [`deliver_prompt_to`] treats `Ok(false)` as grounds to go
/// on and consider a bare Enter — so a budget that never once read the
/// agent's state returns `Err` instead of silently passing for "not
/// submitted".
fn confirm_submitted(
    herdr: &mut impl Herdr,
    name: &str,
    pane: &str,
    prompt: &str,
    kind: &str,
    baseline_seq: u64,
    timing: &Timing,
) -> Result<bool, RunnerError> {
    let start = Instant::now();
    let mut read_ever_succeeded = false;
    loop {
        if let Ok(result) = checked(herdr, argv::agent_get(name), COMMAND_TIMEOUT) {
            if let Some((state, seq)) = result
                .payload
                .as_ref()
                .and_then(|p| snapshot(&p["result"]["agent"], name, pane))
            {
                read_ever_succeeded = true;
                // A working agent is enough even when Codex freezes seq.
                if state == AgentState::Working {
                    visible(herdr, name, pane)?;
                    return Ok(true);
                }
                if seq > baseline_seq && state.is_terminal_for_turn() {
                    // A failed read is not evidence of landing either way
                    // (`visible` propagates it), so `?` here, not a
                    // best-effort `read_for_classification` swallow.
                    // `visible` itself only screens the generic trust/
                    // approval/menu shapes; the vendor-specific check below
                    // catches a billing/model-switch dialog that replaced
                    // the brief entirely, which would otherwise read as
                    // "no longer contains `prompt`" and pass for landed.
                    let text = visible(herdr, name, pane)?;
                    if let Some(reason) = vendor_dialog_reason(&text, kind) {
                        return Err(attention(name, pane, reason));
                    }
                    if !prompt_still_in_composer(&text, prompt, kind) {
                        return Ok(true);
                    }
                    // Still sitting there, unsubmitted — the sequence moved
                    // from the keystrokes alone (card `20261106-b-19efc6`).
                }
            }
        }
        if !pause(start, timing.prompt / 2, timing.poll) {
            if !read_ever_succeeded {
                return Err(RunnerError::Refused(format!(
                    "prompt not submitted — could not read agent {name:?} state in pane \
                     {pane:?} at all while confirming submission; nothing further was sent.\n\
                     The pane is still open; read it to see what the agent is showing."
                )));
            }
            return Ok(false);
        }
    }
}

/// Deliver `prompt` to the already-running agent `name` in `pane` and
/// confirm it actually landed — not merely that `agent prompt` returned,
/// and not merely that `state_change_seq` moved.
///
/// The "prompt-landing confirmation primitive usable outside dispatch" card
/// `20261102-b-a01bbc` found missing — two-attempt retry, `visible`/
/// `attention` dialog-or-blocked detection before sending — plus the same
/// composer-specific submission check `deliver_prompt` itself relies on
/// (`confirm_submitted`, one bare-Enter nudge via `nudge_stuck_composer`
/// when a send is accepted but never lands). `deliver_prompt` is a formatter
/// over this function, not a second copy of it (card `20261106-b-f174b0`):
/// before this function grew `kind` and the same check, a stuck paste on
/// either of its own call sites — `commands::activity::nudge_one`'s nudge
/// and `commands::start`'s `--reseed` re-point — could sit unsubmitted
/// exactly like the codex pane card `20261106-b-19efc6` describes, with
/// nothing here to notice. `kind` (`PreparedLaunch::kind`/`Account::kind`)
/// is what makes that verification possible for a shared primitive: it lets
/// a vendor-specific (billing/model-switch) dialog be refused before ever
/// pressing Enter, the same way a first dispatch already could.
pub fn deliver_prompt_to(
    herdr: &mut impl Herdr,
    name: &str,
    pane: &str,
    prompt: &str,
    kind: &str,
    baseline_seq: u64,
    timing: &Timing,
) -> Result<(), RunnerError> {
    let mut last_error = "prompt did not land".to_string();
    for _ in 0..2 {
        visible(herdr, name, pane)?;
        let accepted = match herdr.call(
            &argv::agent_prompt(name, prompt, false, None),
            Duration::from_secs(120),
        ) {
            Ok(result) => {
                if matches!(
                    result.error_code(),
                    Some(HerdrErrorCode::AgentBlocked | HerdrErrorCode::AgentNotReady)
                ) {
                    return Err(attention(name, pane, result.message()));
                }
                if result.ok() {
                    true
                } else {
                    last_error = result.message();
                    false
                }
            }
            // A transport timeout can occur after delivery. Observe
            // activity before retrying, so a landed turn is not submitted
            // twice.
            Err(error) => {
                last_error = error.to_string();
                false
            }
        };

        if confirm_submitted(herdr, name, pane, prompt, kind, baseline_seq, timing)? {
            return Ok(());
        }

        if !accepted {
            // Herdr never even accepted this send: nothing was typed, so a
            // second full attempt (if one remains) is the right mitigation.
            continue;
        }

        // Herdr accepted the send, yet the agent never left idle and the
        // composer never emptied within the landing window. Give it one
        // chance to submit with a bare Enter — never a second full retype —
        // and return whatever that decides; there is no further outer retry
        // once a send has actually been accepted.
        return nudge_stuck_composer(herdr, name, pane, prompt, kind, baseline_seq, timing);
    }
    Err(RunnerError::Refused(last_error))
}

/// Give a composer that accepted `prompt`'s keystrokes but never landed one
/// chance to submit, and decide — never guess — whether it is safe to press
/// Enter into it. Shared by [`deliver_prompt`] (via [`deliver_prompt_to`])
/// so both the first dispatch and every later nudge/reprompt enforce the
/// same rules instead of risking a second, drifting copy (card
/// `20261106-b-f174b0`).
///
/// Reads the pane fresh: a trust/billing/model-switch dialog or an open menu
/// ([`dialog_or_menu_reason`]) is never answered with a key, and a composer
/// that no longer shows `prompt` in its own region
/// ([`prompt_still_in_composer`]) needs nothing further — scrollback can
/// retain a submitted prompt indefinitely (reviewer HOLD
/// `20261106-reviewer-731eab`), so only the composer's own region counts.
///
/// A composer still showing `prompt` is not, on its own, licence to press
/// Enter (Second Critic HOLD `20261106-reviewer-f7aaf4`): a *fresh*
/// `agent get`, taken right here, must show the agent genuinely idle/done
/// first. `Working` on that fresh read is the same "already landed" signal
/// [`confirm_submitted`] itself treats as success — the prompt reached the
/// agent's own turn late — so it returns `Ok(())` without ever touching the
/// key. Any other state, or a read that fails outright, refuses the Enter
/// and fails loudly instead of gambling a keystroke into whatever the agent
/// is actually doing.
///
/// Nor is a composer match licence to press Enter when `prompt` is short
/// (card `20261106-b-e08cef`, [`GHOST_ECHO_RISK_CHARS`]): a Claude Code
/// ghost suggestion that repeats a just-run short nudge verbatim, once its
/// own turn has already completed, reads in the composer's own region
/// exactly like that same nudge still sitting there unsubmitted — pane
/// text alone cannot tell the two apart. Below that length this function
/// never touches the key at all, no matter what the fresh read shows.
fn nudge_stuck_composer(
    herdr: &mut impl Herdr,
    name: &str,
    pane: &str,
    prompt: &str,
    kind: &str,
    baseline_seq: u64,
    timing: &Timing,
) -> Result<(), RunnerError> {
    let text = match read_for_classification(herdr, pane) {
        Some(text) => text,
        None => {
            return Err(RunnerError::Refused(format!(
                "prompt not submitted — could not read agent {name:?} in pane {pane:?} \
                 to confirm whether the prompt ever reached its own turn; nothing \
                 further was sent.\n\
                 The pane is still open; read it to see what the agent is showing."
            )));
        }
    };
    if let Some(reason) = dialog_or_menu_reason(&text, kind) {
        // Never press Enter into a trust/billing/model-switch dialog or an
        // open menu — only the agent's own unanswered composer, still
        // showing exactly the prompt it was handed, earns a nudge.
        return Err(attention(name, pane, reason));
    }
    if prompt.chars().count() < GHOST_ECHO_RISK_CHARS {
        // Too short to trust a composer-text match at all (see
        // `GHOST_ECHO_RISK_CHARS`): a ghost suggestion echoing a just-run
        // nudge this size is plausible, and nothing else here can tell
        // that apart from a genuine stuck paste. Never gamble the
        // keystroke — nothing further is sent, same as a composer that has
        // already moved on.
        return Ok(());
    }
    if !prompt_still_in_composer(&text, prompt, kind) {
        // The composer moved on between the poll above and this read — a
        // slow settle, not a stuck paste — or the pane is not recognisable
        // as a composer at all. Either way, never a bare Enter: nothing
        // further to send.
        return Ok(());
    }

    let fresh_state = checked(herdr, argv::agent_get(name), COMMAND_TIMEOUT)
        .ok()
        .and_then(|result| {
            result
                .payload
                .as_ref()
                .and_then(|p| snapshot(&p["result"]["agent"], name, pane))
        })
        .map(|(state, _)| state);
    match fresh_state {
        Some(state) if state.is_ready_for_input() => {}
        Some(AgentState::Working) => return Ok(()),
        Some(state) => {
            return Err(attention(
                name,
                pane,
                format!(
                    "agent state is {state}, not idle; refusing to press Enter into its own composer"
                ),
            ));
        }
        None => {
            return Err(RunnerError::Refused(format!(
                "prompt not submitted — could not confirm agent {name:?} in pane {pane:?} is \
                 idle before pressing Enter; nothing further was sent.\n\
                 The pane is still open; read it to see what the agent is showing."
            )));
        }
    }

    herdr.call(
        &argv::agent_prompt(name, "", false, None),
        Duration::from_secs(120),
    )?;
    if confirm_submitted(herdr, name, pane, prompt, kind, baseline_seq, timing)? {
        return Ok(());
    }

    Err(RunnerError::Refused(format!(
        "prompt not submitted — agent {name:?} in pane {pane:?} still shows the prompt \
         sitting in its own input box after the prompt and one Enter; nothing further \
         was sent.\n\
         The pane is still open; read it to see what the agent is showing."
    )))
}

/// Whether a no-resend heartbeat landed, or only went unconfirmed —
/// [`deliver_heartbeat`]'s result. Unlike [`deliver_prompt_to`], an
/// unconfirmed landing is not an error: the send already happened and must
/// never be repeated, so there is nothing left to retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heartbeat {
    Landed,
    Unconfirmed { last_status: String },
}

/// Deliver `prompt` to the already-idle/done agent `name` in `pane` exactly
/// once, then only watch for it landing — never a second `agent prompt`
/// call. Card `20261102-b-4f86db` (finding 1): `sweep --tick`'s hub
/// heartbeat, ported from Python's `_sweep_tick_orchestrator`
/// (`cli.py:11281-11480`), which sends `SWEEP_TICK_PROMPT` at most once
/// per project per tick and is explicit that an unconfirmed landing still
/// "counts as ticked: no re-send, no keys" — a repeating hub-cron heartbeat
/// is exactly the case where [`deliver_prompt_to`]'s own two-attempt retry
/// (right for a one-shot dispatch/reseed prompt) would risk typing the tick
/// text into a live pane twice.
///
/// Shares [`deliver_prompt_to`]'s own dialog/menu guard (`visible`, via
/// [`attention_reason`]) before sending, and its own landing condition
/// (`Working`, or `state_change_seq` past `baseline_seq` and terminal-for-
/// turn) while watching — just without the retry loop around either.
pub fn deliver_heartbeat(
    herdr: &mut impl Herdr,
    name: &str,
    pane: &str,
    prompt: &str,
    baseline_seq: u64,
    timing: &Timing,
) -> Result<Heartbeat, RunnerError> {
    visible(herdr, name, pane)?;
    let sent = herdr.call(
        &argv::agent_prompt(name, prompt, false, None),
        Duration::from_secs(120),
    )?;
    if matches!(
        sent.error_code(),
        Some(HerdrErrorCode::AgentBlocked | HerdrErrorCode::AgentNotReady)
    ) {
        return Err(attention(name, pane, sent.message()));
    }
    if !sent.ok() {
        return Err(RunnerError::Refused(sent.message()));
    }

    let start = Instant::now();
    let mut last_status = "unknown".to_string();
    loop {
        if let Ok(result) = checked(herdr, argv::agent_get(name), COMMAND_TIMEOUT) {
            if let Some((state, seq)) = result
                .payload
                .as_ref()
                .and_then(|p| snapshot(&p["result"]["agent"], name, pane))
            {
                last_status = state.as_str().to_string();
                if state == AgentState::Working
                    || (seq > baseline_seq && state.is_terminal_for_turn())
                {
                    return Ok(Heartbeat::Landed);
                }
            }
        }
        if !pause(start, timing.prompt, timing.poll) {
            break;
        }
    }
    Ok(Heartbeat::Unconfirmed { last_status })
}

/// Whether [`start_dispatch`] should take the fleet lock itself.
///
/// [`FleetLock::Acquire`] is the ordinary single-dispatch path: take the
/// lock, recheck admission, start, drop the lock before the prompt.
/// [`FleetLock::AlreadyHeld`] is Python's `DispatchRunner(config=None)`
/// (`council.py:591-606`): the caller already holds `_fleet_lock` and has
/// already run the batch-level cap/capacity checks, so a second acquire
/// would be thread-local-reentrant (`held: None`) and would *not* drop the
/// caller's lock before the prompt — the defect critic-grok3 row 4 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetLock {
    Acquire,
    AlreadyHeld,
}

/// Agent started and countable, but not yet prompted — the state Python
/// has once `_start_agent` returns and `_fleet_lock` has released
/// (`runner.py:2724-2731`).
pub struct StartedDispatch {
    launch: PreparedLaunch,
    pane: String,
    brief: PathBuf,
    started_at: SystemTime,
}

/// Start one already-planned dispatch. The caller supplies the current pane
/// explicitly (normally HERDR_PANE_ID), so tests cannot inherit a live pane.
pub fn dispatch(
    store: &mut impl DispatchStore,
    herdr: &mut impl Herdr,
    current_pane: &str,
    timing: &Timing,
) -> Result<Dispatched, RunnerError> {
    let started = start_dispatch(store, herdr, current_pane, timing, FleetLock::Acquire)?;
    prompt_dispatch(store, herdr, started, timing)
}

/// Pane creation and agent start, up through the moment the new agent is
/// countable. Drops a lock this call itself acquired before returning;
/// does not acquire (and therefore cannot drop) a lock the caller still
/// holds.
pub fn start_dispatch(
    store: &mut impl DispatchStore,
    herdr: &mut impl Herdr,
    current_pane: &str,
    timing: &Timing,
    lock: FleetLock,
) -> Result<StartedDispatch, RunnerError> {
    let mut owned_pane = None;
    let mut owned_agent = None;
    // Set once `record_dispatch` succeeds — the agent now has a name and a
    // routing-log line naming it, so any later failure in this closure owes
    // it a matching `failed-to-start` record (finding 4).
    let mut dispatched: Option<PreparedLaunch> = None;
    let attempt = (|| {
        if current_pane.trim().is_empty() {
            return Err(RunnerError::Refused(
                "dispatch requires HERDR_PANE_ID".into(),
            ));
        }
        let fleet = match lock {
            FleetLock::Acquire => Some(store.lock_fleet()?),
            FleetLock::AlreadyHeld => None,
        };
        // Fresh admission recheck against *current* fleet state, not the
        // plan-time answer, while the fleet lock just taken is held and
        // before the project lock/`prepare` even run (finding 5). Skipped
        // when the caller already holds the lock and has done the
        // batch-level checks (Python `DispatchRunner(config=None)`).
        if matches!(lock, FleetLock::Acquire) {
            store.recheck_admission()?;
        }
        let started_at = SystemTime::now();
        let (mut launch, brief) = {
            let _project = store.lock_project()?;
            let launch = store.prepare()?;
            validate(&launch)?;
            let brief = write_brief(&launch)?;
            (launch, brief)
        };
        launch
            .environment
            .retain(|(key, _)| key != "MULTIVERSE_TASK_ID");
        launch
            .environment
            .push(("MULTIVERSE_TASK_ID".into(), launch.task_id.clone()));
        let mut taken = store.unfinished_agent_names()?;
        if let Ok(result) = checked(herdr, argv::agent_list(), COMMAND_TIMEOUT) {
            if let Some(agents) = result
                .payload
                .as_ref()
                .and_then(|p| p["result"]["agents"].as_array())
            {
                taken.extend(
                    agents
                        .iter()
                        .filter_map(|a| a["name"].as_str().map(str::to_owned)),
                );
            }
        }
        launch.agent_name = store.unique_agent_name(&launch.agent_name, &taken)?;
        // Before the agent exists, so anything that appears afterwards is
        // its doing (finding 4; Python's `append_routing_log`).
        store.record_dispatch(&launch)?;
        dispatched = Some(launch.clone());
        let env: Vec<_> = launch
            .environment
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let layout = checked(herdr, argv::pane_layout(current_pane), COMMAND_TIMEOUT)
            .ok()
            .and_then(|r| rectangles(&r));
        let (split_pane, direction) = layout
            .as_ref()
            .map_or((None, "right"), |panes| placement(current_pane, panes));
        // The fleet could have been halted since planning; this is the last
        // check before a pane is actually created (finding 5).
        store.check_halt_before_pane_creation()?;
        let split = checked(
            herdr,
            argv::pane_split(
                &launch.cwd,
                &env,
                split_pane.as_deref(),
                split_pane.is_none(),
                Some(direction),
                false,
            ),
            COMMAND_TIMEOUT,
        )?;
        let pane = pane_id_from(&split)?;
        if pane.trim().is_empty()
            || pane == current_pane
            || split_pane.as_deref() == Some(pane.as_str())
            || layout
                .as_ref()
                .is_some_and(|panes| panes.iter().any(|p| p.pane == pane))
        {
            return Err(RunnerError::Refused(
                "split did not return a new pane id".into(),
            ));
        }
        owned_pane = Some(pane.clone());
        owned_agent = Some(launch.agent_name.clone());
        equalize(herdr, &pane);
        checked(
            herdr,
            argv::pane_rename(&pane, &pane_label(&launch)),
            COMMAND_TIMEOUT,
        )?;
        ensure_codex_trust(&launch)?;
        apply_codex_worktree_permissions(&mut launch);
        let cd = format!("cd {}", quote(&launch.cwd.to_string_lossy()));
        if let Some(command) = &launch.command {
            let command = quote_command_words(command);
            let args = launch
                .native_args
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(" ");
            checked(
                herdr,
                argv::pane_run(&pane, &format!("{cd} && {command} {args}")),
                COMMAND_TIMEOUT,
            )?;
            // Herdr has to *notice* the agent before it can be renamed; that
            // is not immediate for a command run in a pane's own shell
            // (finding 2).
            wait_detected(herdr, &pane, timing)?;
            checked(
                herdr,
                argv::agent_rename(&pane, &launch.agent_name),
                COMMAND_TIMEOUT,
            )?;
        } else {
            let vars: Vec<_> = launch.clear_auth.iter().map(String::as_str).collect();
            let script = if vars.is_empty() {
                cd
            } else {
                format!("{cd} && {}", argv::clear_auth_script(&vars))
            };
            checked(herdr, argv::pane_run(&pane, &script), COMMAND_TIMEOUT)?;
            let result = herdr.call(
                &argv::agent_start(
                    &launch.agent_name,
                    &launch.kind,
                    &pane,
                    Some(60_000),
                    &launch.native_args,
                ),
                Duration::from_secs(65),
            )?;
            let result = if result.ok() {
                result
            } else {
                // `agent start` can race a shell that has not finished
                // coming up yet; retry it for a few seconds before giving up
                // (finding 9).
                retry_agent_start(
                    herdr,
                    &launch,
                    &pane,
                    timing,
                    store.shell_ready_retry(),
                    result,
                )?
            };
            if !result.ok() {
                // Release before any screen/readiness wait, including startup
                // errors that may be recoverable trust dialogs.
                drop(fleet);
                let screen = visible(herdr, &launch.agent_name, &pane);
                if let Err(error @ RunnerError::NeedsAttention { .. }) = screen {
                    return Err(error);
                }
                if matches!(
                    result.error_code(),
                    Some(HerdrErrorCode::AgentNotReady | HerdrErrorCode::AgentBlocked)
                ) {
                    return Err(attention(&launch.agent_name, &pane, result.message()));
                }
                return Err(RunnerError::Refused(result.message()));
            }
        }
        drop(fleet);
        Ok(StartedDispatch {
            launch,
            pane,
            brief,
            started_at,
        })
    })();
    finalize_dispatch_attempt(store, herdr, owned_pane, owned_agent, dispatched, attempt)
}

/// Readiness wait, lane setup, and prompt — Python's post-`_fleet_lock`
/// span (`runner.py:2735-2777`). The fleet lock must already have been
/// dropped by the caller (or by [`start_dispatch`] when it acquired it).
pub fn prompt_dispatch(
    store: &mut impl DispatchStore,
    herdr: &mut impl Herdr,
    started: StartedDispatch,
    timing: &Timing,
) -> Result<Dispatched, RunnerError> {
    let owned_pane = Some(started.pane.clone());
    let owned_agent = Some(started.launch.agent_name.clone());
    let dispatched = Some(started.launch.clone());
    let attempt = (|| {
        store.setup_lane(&started.launch)?;
        let baseline_seq = wait_ready(herdr, &started.launch.agent_name, &started.pane, timing)?;
        refuse_if_parked_on_dialog(
            herdr,
            &started.launch.agent_name,
            &started.pane,
            &started.launch.kind,
        )?;
        let mut task = Dispatched {
            launch: started.launch,
            pane: started.pane,
            baseline_seq,
            warnings: Vec::new(),
            started_at: started.started_at,
        };
        deliver_prompt(herdr, &task, &started.brief, timing)?;
        // The prompt landing is what "started" means (finding 4; Python's
        // `_append_started_record`, written only after `_prompt_once`
        // returns without raising).
        if let Err(error) = store.record_started(&task.launch, &task.pane, task.started_at) {
            task.warnings.push(error.to_string());
        }
        Ok(task)
    })();
    finalize_dispatch_attempt(store, herdr, owned_pane, owned_agent, dispatched, attempt)
}

fn finalize_dispatch_attempt<T>(
    store: &mut impl DispatchStore,
    herdr: &mut impl Herdr,
    owned_pane: Option<String>,
    owned_agent: Option<String>,
    dispatched: Option<PreparedLaunch>,
    attempt: Result<T, RunnerError>,
) -> Result<T, RunnerError> {
    let attempt = match attempt {
        Err(error) if !matches!(error, RunnerError::NeedsAttention { .. }) => {
            if let (Some(agent), Some(pane)) = (&owned_agent, &owned_pane) {
                if let Err(dialog @ RunnerError::NeedsAttention { .. }) =
                    visible(herdr, agent, pane)
                {
                    Err(dialog)
                } else {
                    Err(error)
                }
            } else {
                Err(error)
            }
        }
        other => other,
    };
    if let Err(error) = &attempt {
        // Classify the newly-created pane for vendor exhaustion on every
        // failure shape — Python's `_maybe_trip_breaker` runs from the same
        // `except Exception` branch regardless of which check inside the
        // `try` raised (`runner.py:2908-2920`), and `limits::classify`
        // recognises only vendor-exhaustion wording, so running it
        // unconditionally here is harmless for every other failure (a trust
        // dialog, a halt, a lock timeout): no verdict, no trip.
        let mut pane_closed_by_trip = false;
        if let (Some(launch), Some(pane)) = (&dispatched, &owned_pane) {
            if let Some(text) = read_for_classification(herdr, pane) {
                let now = OffsetDateTime::now_utc();
                if let Some(verdict) = limits::classify(&text, &launch.kind, Some(now)) {
                    if let Err(cleanup) = store.trip_breaker(launch, &verdict, &text, now) {
                        store.warn(&format!(
                            "could not trip breaker for task {:?}: {cleanup}",
                            launch.task_id
                        ));
                    }
                    // Card `20260911-fixer-d23119`'s dead-seat half
                    // (`runner.py:3166-3175`): a plain exhaustion banner
                    // leaves nothing more to happen in this pane, so it is
                    // torn down the same way a `done` dispatch's pane
                    // already is — unconditionally, not gated by
                    // `keep_pane_on_failure` (Python's own call there is a
                    // second, separate teardown). An *interactive* verdict
                    // (an unanswered Yes/No) is left exactly as the vendor
                    // left it instead, the same as a trust/approval dialog
                    // already is.
                    if !verdict.interactive {
                        if let Err(cleanup) =
                            checked(herdr, argv::pane_close(pane), COMMAND_TIMEOUT)
                        {
                            store.warn(&format!("could not close pane {pane}: {cleanup}"));
                        } else {
                            pane_closed_by_trip = true;
                        }
                    }
                }
            }
        }
        // A start or prompt that never lands must not leave the task
        // AWAITING forever against `tasks.resolve`, nor hold its fleet-cap
        // slot open — recorded even for a needs-attention pane, since
        // Python records `failed-to-start` the same way for one (finding 4).
        if let Some(launch) = &dispatched {
            let pane = owned_pane.as_deref().unwrap_or("");
            if let Err(cleanup) = store.record_failed_to_start(launch, pane) {
                store.warn(&format!(
                    "could not record failed-to-start for task {:?}: {cleanup}",
                    launch.task_id
                ));
            }
        }
        if !pane_closed_by_trip
            && !matches!(error, RunnerError::NeedsAttention { .. })
            && !store.keep_pane_on_failure()
        {
            if let Some(pane) = owned_pane {
                if let Err(cleanup) = checked(herdr, argv::pane_close(&pane), COMMAND_TIMEOUT) {
                    store.warn(&format!("could not close pane {pane}: {cleanup}"));
                }
            }
        }
        if let Err(cleanup) = store.release_trial() {
            store.warn(&format!("could not release trial: {cleanup}"));
        }
    }
    attempt
}

/// Wait for a settled turn. A usable status file plus nonempty result is
/// the success pair; a settled turn missing either still returns the
/// Herdr `agent_status` so `--wait` can print `settled as {state}`
/// (`cli.py:3792`) before the missing-status line. A timeout with no
/// settled snapshot leaves the session open and is not completion.
///
/// The pair is Python's `live.status` plus `live.settled.state`
/// (`runner.py:2828-2856`, `cli.py:3792`): the status file the agent wrote,
/// and the raw Herdr `agent_status` observed when the turn settled. They
/// can disagree; neither field is dropped.
pub fn wait_for_result(
    herdr: &mut impl Herdr,
    task: &Dispatched,
    timeout: Duration,
    poll: Duration,
) -> Result<(Option<crate::gateway::TaskStatus>, AgentState), RunnerError> {
    let start = Instant::now();
    let mut gate = TurnGate::default();
    let mut settled: Option<AgentState> = None;
    loop {
        if let Ok(result) = checked(
            herdr,
            argv::agent_get(&task.launch.agent_name),
            COMMAND_TIMEOUT,
        ) {
            if let Some((state, seq)) = result
                .payload
                .as_ref()
                .and_then(|p| snapshot(&p["result"]["agent"], &task.launch.agent_name, &task.pane))
            {
                if gate.observe(task.baseline_seq, state, seq) {
                    settled = Some(state);
                    let status = wait_status(task);
                    if let Some(status) = usable_wait_status(task, status) {
                        return Ok((Some(status), state));
                    }
                    // Python `wait_for_turn` returns `blocked` without a
                    // status file; a DONE whose result.md is empty is then
                    // nulled (`runner.py:2835-2841`). Idle/done with no
                    // status file yet keep polling, matching wait_for_turn's
                    // requirement that those labels mean READY FOR INPUT.
                    if state == AgentState::Blocked || status.is_some() {
                        return Ok((None, state));
                    }
                }
            }
        }
        if !pause(start, timeout, poll) {
            return match settled {
                Some(state) => Ok((usable_wait_status(task, wait_status(task)), state)),
                None => Err(RunnerError::Refused(
                    "no settled turn with usable result before timeout".into(),
                )),
            };
        }
    }
}

fn wait_status(task: &Dispatched) -> Option<crate::gateway::TaskStatus> {
    mv_state::tasks::read_status_since(&task_dir(&task.launch).join("status"), task.started_at)
}

/// Python `_usable_result` (`tasks.py:744-747`) is applied only to DONE:
/// a blocked/failed status file stands on its own. DONE also needs a
/// nonempty result.md, otherwise `live.status` is cleared to None.
fn usable_wait_status(
    task: &Dispatched,
    status: Option<crate::gateway::TaskStatus>,
) -> Option<crate::gateway::TaskStatus> {
    let status = status?;
    if status == crate::gateway::TaskStatus::Done {
        let text = fs::read_to_string(task_dir(&task.launch).join("result.md")).ok()?;
        if text.trim().is_empty() {
            return None;
        }
    }
    Some(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    use std::sync::{Arc, OnceLock};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "mv-runner-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Guard {
        locks: Arc<AtomicU8>,
        bit: u8,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            assert_ne!(
                self.locks.fetch_and(!self.bit, Ordering::SeqCst) & self.bit,
                0
            );
        }
    }
    struct Store {
        launch: PreparedLaunch,
        locks: Arc<AtomicU8>,
        released: usize,
        setup_failed: bool,
        release_failed: bool,
        routing_failed: bool,
        taken: HashSet<String>,
        warnings: Vec<String>,
        admission_failed: bool,
        halt_failed: bool,
        keep_on_failure: bool,
        shell_retry: Duration,
        dispatched: Vec<String>,
        started: Vec<String>,
        failed_to_start: Vec<(String, String)>,
        failed_to_start_fails: bool,
        /// `(agent_name, verdict.rule, verdict.interactive)` for every
        /// [`DispatchStore::trip_breaker`] call this store received.
        trips: Vec<(String, String, bool)>,
        trip_fails: bool,
    }
    impl DispatchStore for Store {
        type Guard = Guard;
        fn lock_fleet(&mut self) -> Result<Guard, RunnerError> {
            assert_eq!(self.locks.swap(1, Ordering::SeqCst), 0);
            Ok(Guard {
                locks: self.locks.clone(),
                bit: 1,
            })
        }
        fn lock_project(&mut self) -> Result<Guard, RunnerError> {
            assert_eq!(self.locks.fetch_or(2, Ordering::SeqCst), 1);
            Ok(Guard {
                locks: self.locks.clone(),
                bit: 2,
            })
        }
        fn recheck_admission(&mut self) -> Result<(), RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 1);
            if self.admission_failed {
                return Err(RunnerError::Refused("fleet admission exceeded".into()));
            }
            Ok(())
        }
        fn prepare(&mut self) -> Result<PreparedLaunch, RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 3);
            // Simulate the missing state adapter's worktree creation step.
            fs::create_dir_all(&self.launch.cwd)?;
            Ok(self.launch.clone())
        }
        fn unfinished_agent_names(&mut self) -> Result<HashSet<String>, RunnerError> {
            Ok(self.taken.clone())
        }
        fn record_dispatch(&mut self, launch: &PreparedLaunch) -> Result<(), RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 1);
            self.dispatched.push(launch.agent_name.clone());
            Ok(())
        }
        fn check_halt_before_pane_creation(&mut self) -> Result<(), RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 1);
            if self.halt_failed {
                return Err(RunnerError::Refused("fleet halt active".into()));
            }
            Ok(())
        }
        fn record_started(
            &mut self,
            launch: &PreparedLaunch,
            _: &str,
            _: SystemTime,
        ) -> Result<(), RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 0);
            assert!(task_dir(launch).join("brief.md").is_file());
            self.started.push(launch.agent_name.clone());
            if self.routing_failed {
                return Err(RunnerError::Refused("routing unavailable".into()));
            }
            Ok(())
        }
        fn record_failed_to_start(
            &mut self,
            launch: &PreparedLaunch,
            pane: &str,
        ) -> Result<(), RunnerError> {
            self.failed_to_start
                .push((launch.agent_name.clone(), pane.to_owned()));
            if self.failed_to_start_fails {
                return Err(RunnerError::Refused("routing unavailable".into()));
            }
            Ok(())
        }
        fn trip_breaker(
            &mut self,
            launch: &PreparedLaunch,
            verdict: &limits::Verdict,
            _pane_text: &str,
            _now: OffsetDateTime,
        ) -> Result<bool, RunnerError> {
            self.trips.push((
                launch.agent_name.clone(),
                verdict.rule.clone(),
                verdict.interactive,
            ));
            if self.trip_fails {
                return Err(RunnerError::Refused("breaker store unavailable".into()));
            }
            Ok(true)
        }
        fn setup_lane(&mut self, _: &PreparedLaunch) -> Result<(), RunnerError> {
            assert_eq!(self.locks.load(Ordering::SeqCst), 0);
            if self.setup_failed {
                return Err(RunnerError::Refused("lane-setup-failed".into()));
            }
            Ok(())
        }
        fn release_trial(&mut self) -> Result<(), RunnerError> {
            self.released += 1;
            if self.release_failed {
                return Err(RunnerError::Refused("trial storage unavailable".into()));
            }
            Ok(())
        }
        fn warn(&mut self, message: &str) {
            self.warnings.push(message.into());
        }
        fn keep_pane_on_failure(&self) -> bool {
            self.keep_on_failure
        }
        fn shell_ready_retry(&self) -> Duration {
            self.shell_retry
        }
    }

    fn fake_binary() -> &'static Path {
        static BINARY: OnceLock<PathBuf> = OnceLock::new();
        BINARY.get_or_init(|| {
            // Another package's binary is not refreshed by `cargo test`.
            // Let Cargo check freshness, then use its artifact path so custom
            // target directories and profiles do not require path guesses.
            let output = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "-p",
                    "mv-herdr",
                    "--bin",
                    "fake-herdr",
                    "--locked",
                    "--message-format=json",
                ])
                .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
                .output()
                .expect("build fake-herdr with Cargo");
            assert!(
                output.status.success(),
                "fake-herdr build failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let binary = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .filter_map(|line| serde_json::from_str::<Value>(line).ok())
                .find_map(|artifact| {
                    (artifact["reason"] == "compiler-artifact"
                        && artifact["target"]["name"] == "fake-herdr")
                        .then(|| artifact["executable"].as_str().map(PathBuf::from))
                        .flatten()
                })
                .expect("Cargo must report the fake-herdr executable");
            assert!(
                binary.is_file(),
                "fake-herdr must exist; never fall back to Herdr"
            );
            binary
        })
    }

    /// One-shot override for the *second* `agent get` read following a
    /// non-empty `agent prompt` send — the fresh idle check `deliver_prompt`
    /// takes immediately before its own bare-Enter nudge (Second Critic HOLD
    /// `20261106-reviewer-f7aaf4`). The first such read is confirm_submitted's
    /// own initial poll; the second is this fresh check, the one this fixture
    /// targets.
    #[derive(Clone, Copy)]
    enum GetOverride {
        Fail,
        Working,
        Blocked,
    }

    struct Fake {
        script: PathBuf,
        state: PathBuf,
        locks: Arc<AtomicU8>,
        calls: Vec<HerdrCommand>,
        fail: Option<&'static str>,
        trust: bool,
        unreadable: bool,
        fail_reads_after_prompt: bool,
        prompted: bool,
        stalled_prompts: usize,
        start_not_ready: bool,
        /// How many leading `agent start` calls fail with a generic,
        /// uncoded "shell not ready" error before succeeding (finding 9).
        start_failures: usize,
        /// Command-route (Claude) fixtures only: once a pane has been
        /// split and before it has been detected once, synthesize `agent
        /// list` responses instead of asking the real fake-herdr backend,
        /// which (unlike real Herdr) never classifies a pane on its own —
        /// only `agent start`/`rename` ever add an entry for it.
        /// `claude_detect_delay` polls show the pane as not yet detected
        /// before the next one shows it (finding 2). Interception stops
        /// for good once detection succeeds once, so the later readiness
        /// wait sees the real (by-then-renamed) agent instead of the
        /// synthetic, permanently-`unknown` stand-in.
        command_route: bool,
        claude_detect_delay: usize,
        split_done: bool,
        detected_once: bool,
        /// Pane text to show once the agent starts/renames, overriding the
        /// default "Ready >", for a vendor-exhaustion/dialog fixture test —
        /// same injection point `trust`/`unreadable` already use below.
        banner: Option<&'static str>,
        /// How many leading non-empty `agent prompt` sends land as a
        /// composer that accepted the keystrokes (`state_change_seq`
        /// advances) without ever submitting (`agent_status` stays `idle`,
        /// the pane still shows the brief verbatim) — card
        /// `20261106-b-19efc6`'s "pasted but never submitted" codex pane.
        stuck_pastes: usize,
        /// Whether a follow-up empty-text `agent prompt` (the bare-Enter
        /// nudge `deliver_prompt` sends when a stuck paste never clears)
        /// actually submits it. `false` simulates an agent whose composer
        /// does not respond to Enter either, so the dispatch must fail
        /// loudly instead of declaring victory.
        enter_submits: bool,
        /// One-shot: the next non-empty `agent prompt` lands for real, but
        /// the brief stays visible in scrollback while a *fresh* composer
        /// (unrelated ghost text) opens underneath it — reviewer HOLD
        /// `20261106-reviewer-731eab` regression (a). A whole-pane
        /// `contains` would misread the scrolled-up brief as still
        /// unsubmitted and press Enter into the ghost text; composer-scoped
        /// evidence must not.
        leave_scrollback_ghost: bool,
        /// One-shot: the next non-empty `agent prompt` lands for real (same
        /// as `leave_scrollback_ghost`), but the fresh composer underneath
        /// the scrolled-up brief echoes the *same* text back as a ghost
        /// suggestion, instead of unrelated tail text — card
        /// `20261106-b-e08cef`: a Claude Code ghost suggestion that repeats
        /// a just-run short nudge verbatim reads, in the composer's own
        /// region, exactly like that nudge still sitting there unsubmitted.
        /// `prompt_still_in_composer` is (correctly) fooled by this one;
        /// `nudge_stuck_composer`'s own length gate (`GHOST_ECHO_RISK_CHARS`)
        /// is what must refuse the bare Enter here, not composer-text
        /// matching.
        leave_scrollback_ghost_echo: bool,
        /// One-shot: the next non-empty `agent prompt` is accepted, then a
        /// dialog opens before any Enter nudge would be considered —
        /// reviewer HOLD `20261106-reviewer-731eab` regression (b). Holds
        /// the dialog's own pane text.
        dialog_after_paste: Option<&'static str>,
        /// Second Critic HOLD `20261106-reviewer-f7aaf4` regressions: applied
        /// to the second `agent get` read after the next non-empty `agent
        /// prompt` send — the fresh idle check immediately before
        /// `deliver_prompt`'s own bare-Enter nudge — then cleared.
        second_get_override: Option<GetOverride>,
        /// `agent get` reads observed since the most recent `agent prompt`
        /// send of either kind, used to find the "second" read
        /// `second_get_override` targets.
        gets_since_prompt: usize,
    }
    impl Fake {
        fn edit(&self, change: impl FnOnce(&mut Value)) {
            let mut state: Value = serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap();
            change(&mut state);
            fs::write(&self.state, serde_json::to_vec(&state).unwrap()).unwrap();
        }
        fn panes(&self) -> usize {
            serde_json::from_slice::<Value>(&fs::read(&self.state).unwrap()).unwrap()["panes"]
                .as_array()
                .unwrap()
                .len()
        }
        fn called(&self, group: &str, action: &str) -> Vec<&HerdrCommand> {
            self.calls
                .iter()
                .filter(|c| c.argv[1] == group && c.argv[2] == action)
                .collect()
        }
        fn agent_pane(&self, name: &str) -> String {
            let state: Value = serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap();
            state["agents"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"].as_str() == Some(name))
                .expect("agent must exist")["pane_id"]
                .as_str()
                .unwrap()
                .to_owned()
        }
        /// Advance `state_change_seq` from the keystrokes alone, leaving
        /// `agent_status` at `idle` and the pane showing `text` still
        /// sitting behind the fixture's own codex composer marker (`›`,
        /// `composer_marker`'s own codex case — this fixture's `kind` is
        /// always `"codex"`) — a stuck paste that never reached its own
        /// turn, in a shape `prompt_still_in_composer` can actually read as
        /// a composer.
        fn leave_pasted_and_unsubmitted(&self, name: &str, text: &str) {
            let pane = self.agent_pane(name);
            self.edit(|state| {
                let agent = state["agents"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|a| a["pane_id"] == pane)
                    .expect("agent must exist");
                let seq = agent["state_change_seq"].as_u64().unwrap_or(0) + 1;
                agent["state_change_seq"] = json!(seq);
                agent["agent_status"] = json!("idle");
                state["text"][&pane] = json!(format!("› {text}"));
            });
        }
        /// Leave `name`'s pane showing `brief` only in scrollback — above a
        /// *fresh*, empty composer prompt showing `tail` (a ghost-text
        /// suggestion, say) — simulating a turn that already submitted and
        /// moved on. `agent_status` is left `idle`/seq-advanced exactly
        /// like a landed turn, because that is what this models: regression
        /// (a) for reviewer HOLD `20261106-reviewer-731eab`, where a
        /// whole-pane `contains` would wrongly read the scrolled-up brief
        /// as still unsubmitted and press Enter into `tail`.
        fn leave_submitted_with_scrollback_and_fresh_composer(
            &self,
            name: &str,
            brief: &str,
            tail: &str,
        ) {
            let pane = self.agent_pane(name);
            self.edit(|state| {
                let agent = state["agents"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|a| a["pane_id"] == pane)
                    .expect("agent must exist");
                let seq = agent["state_change_seq"].as_u64().unwrap_or(0) + 1;
                agent["state_change_seq"] = json!(seq);
                agent["agent_status"] = json!("idle");
                state["text"][&pane] = json!(format!("{brief}\n\nTask completed.\n\n› {tail}"));
            });
        }
        /// Leave `name`'s pane `blocked` on `dialog` right after an
        /// accepted paste — a trust/approval dialog opening before any
        /// Enter nudge would be considered. Regression (b) for reviewer
        /// HOLD `20261106-reviewer-731eab`: `deliver_prompt` must recognise
        /// it and never answer it with a bare Enter.
        fn leave_dialog_after_paste(&self, name: &str, dialog: &str) {
            let pane = self.agent_pane(name);
            self.edit(|state| {
                let agent = state["agents"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|a| a["pane_id"] == pane)
                    .expect("agent must exist");
                let seq = agent["state_change_seq"].as_u64().unwrap_or(0) + 1;
                agent["state_change_seq"] = json!(seq);
                agent["agent_status"] = json!("blocked");
                state["text"][&pane] = json!(dialog);
            });
        }
        /// Submit for real: the agent leaves idle, the pane stops showing
        /// the brief. Mirrors real fake-herdr's own immediate completion.
        fn complete_prompt(&self, name: &str) {
            let pane = self.agent_pane(name);
            self.edit(|state| {
                let agent = state["agents"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|a| a["pane_id"] == pane)
                    .expect("agent must exist");
                let seq = agent["state_change_seq"].as_u64().unwrap_or(0) + 1;
                agent["state_change_seq"] = json!(seq);
                agent["completion_seq"] = json!(seq);
                agent["agent_status"] = json!("done");
                state["text"][&pane] = json!("Task completed.\n");
            });
        }
    }
    impl Herdr for Fake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let action = command.argv[2].as_str();
            if matches!(
                (command.argv[1].as_str(), action),
                ("pane", "split") | ("agent", "start")
            ) {
                assert_eq!(
                    self.locks.load(Ordering::SeqCst),
                    1,
                    "fleet lock must cover launch"
                );
            }
            // The Codex self-update check inside the shell-retry loop
            // (finding 9) reads the pane while the fleet lock Python holds
            // through the whole retry is still held; `get`/`prompt` stay
            // strictly post-unlock, unchanged from before.
            if matches!(action, "get" | "prompt") {
                assert_eq!(
                    self.locks.load(Ordering::SeqCst),
                    0,
                    "no fleet lock during readiness or prompt"
                );
            }
            self.calls.push(command.clone());
            if command.argv[1] == "agent"
                && action == "list"
                && self.command_route
                && self.split_done
                && !self.detected_once
            {
                let state: Value = serde_json::from_slice(&fs::read(&self.state).unwrap()).unwrap();
                let pane = state["panes"].as_array().unwrap().last().unwrap()["pane_id"]
                    .as_str()
                    .unwrap()
                    .to_owned();
                let agents = if self.claude_detect_delay > 0 {
                    self.claude_detect_delay -= 1;
                    Vec::new()
                } else {
                    self.detected_once = true;
                    vec![json!({"pane_id": pane, "agent_status": "unknown"})]
                };
                return Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(json!({"result": {"agents": agents}})),
                    protocol_error: None,
                });
            }
            if action == "prompt" && self.stalled_prompts > 0 {
                self.stalled_prompts -= 1;
                return Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "injected stalled prompt".into(),
                    payload: None,
                    protocol_error: None,
                });
            }
            if action == "get" {
                self.gets_since_prompt += 1;
                if self.gets_since_prompt == 2 {
                    if let Some(override_) = self.second_get_override.take() {
                        let name = command.argv[3].clone();
                        return Ok(match override_ {
                            GetOverride::Fail => CommandResult {
                                argv: command.argv.clone(),
                                returncode: 1,
                                stdout: String::new(),
                                stderr: "injected get failure".into(),
                                payload: None,
                                protocol_error: None,
                            },
                            GetOverride::Working | GetOverride::Blocked => {
                                let pane = self.agent_pane(&name);
                                let status = if matches!(override_, GetOverride::Working) {
                                    "working"
                                } else {
                                    "blocked"
                                };
                                CommandResult {
                                    argv: command.argv.clone(),
                                    returncode: 0,
                                    stdout: String::new(),
                                    stderr: String::new(),
                                    payload: Some(json!({"result": {"agent": {
                                        "name": name,
                                        "pane_id": pane,
                                        "agent_status": status,
                                        "state_change_seq": 0,
                                    }}})),
                                    protocol_error: None,
                                }
                            }
                        });
                    }
                }
            }
            // Submission-verification fixtures (card `20261106-b-19efc6`):
            // intercept both the original paste and any bare-Enter nudge
            // `deliver_prompt` sends, rather than delegating to real
            // fake-herdr, which would complete the brief immediately and
            // never let the pane sit stuck.
            if action == "prompt" {
                self.prompted = true;
                // Second Critic HOLD `20261106-reviewer-f7aaf4` fixtures key
                // `second_get_override` off "the second `get` read after a
                // prompt send"; start counting fresh from every send.
                self.gets_since_prompt = 0;
                let name = &command.argv[3];
                let text = &command.argv[4];
                if text.is_empty() {
                    if self.enter_submits {
                        self.complete_prompt(name);
                    }
                    // `enter_submits == false`: Herdr accepts the keystroke
                    // but nothing on the pane changes — the composer does
                    // not respond to Enter either.
                    return Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    });
                }
                if self.stuck_pastes > 0 {
                    self.stuck_pastes -= 1;
                    self.leave_pasted_and_unsubmitted(name, text);
                    return Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    });
                }
                if self.leave_scrollback_ghost {
                    self.leave_scrollback_ghost = false;
                    self.leave_submitted_with_scrollback_and_fresh_composer(
                        name,
                        text,
                        "finish the remaining regression tests",
                    );
                    return Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    });
                }
                if self.leave_scrollback_ghost_echo {
                    self.leave_scrollback_ghost_echo = false;
                    self.leave_submitted_with_scrollback_and_fresh_composer(name, text, text);
                    return Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    });
                }
                if let Some(dialog) = self.dialog_after_paste.take() {
                    self.leave_dialog_after_paste(name, dialog);
                    return Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    });
                }
            }
            if action == "start" && self.start_failures > 0 {
                self.start_failures -= 1;
                return Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "pane is not an available shell".into(),
                    payload: None,
                    protocol_error: None,
                });
            }
            if self.fail == Some(action)
                || (action == "read" && self.prompted && self.fail_reads_after_prompt)
            {
                return Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr: "injected failure".into(),
                    payload: None,
                    protocol_error: None,
                });
            }
            if action == "prompt" {
                // `self.prompted` is already set by the interception above;
                // only the real-delegate assertions remain here. A
                // `deliver_prompt_to` nudge/reprompt caller sends arbitrary
                // text with no brief path to check; only `dispatch`'s own
                // formatted brief-reading prompt names one in backticks.
                let prompt = &command.argv[4];
                if let Some(brief) = prompt.split('`').nth(1) {
                    assert_eq!(fs::read_to_string(brief).unwrap(), "scoped task\n");
                }
                assert!(!command.argv.iter().any(|a| a == "--wait"));
            }
            let mut redirected = command.clone();
            redirected.argv[0] = self.script.to_string_lossy().into_owned();
            let result = mv_herdr::run::run(&redirected, Some(timeout))?;
            if action == "split" {
                self.split_done = true;
            }
            if action == "start" || (command.argv[1] == "agent" && action == "rename") {
                let trust = self.trust;
                let unreadable = self.unreadable;
                let banner = self.banner;
                self.edit(|state| {
                    let pane = state["agents"].as_array().unwrap().last().unwrap()["pane_id"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    state["text"][pane] = json!(if trust {
                        "Do you trust the files in this folder?"
                    } else if unreadable {
                        ""
                    } else {
                        banner.unwrap_or("Ready >")
                    });
                });
            }
            if action == "start" && self.start_not_ready {
                return Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 1,
                    stdout: String::new(),
                    stderr:
                        json!({"error": {"code": "agent_not_ready", "message": "startup dialog"}})
                            .to_string(),
                    payload: None,
                    protocol_error: None,
                });
            }
            Ok(result)
        }
    }

    fn fixture() -> (Sandbox, Store, Fake) {
        let sandbox = Sandbox::new();
        let locks = Arc::new(AtomicU8::new(0));
        let state = sandbox.0.join("herdr.json");
        let script = sandbox.0.join("isolated-herdr");
        let config = sandbox.0.join("config");
        let runtime = sandbox.0.join("runtime");
        fs::create_dir(&config).unwrap();
        fs::create_dir(&runtime).unwrap();
        // The wrapper exports isolated state/HOME for the real fake executable.
        // Process-global environment is never changed by parallel unit tests.
        fs::write(&script, format!("#!/bin/sh\nexport HERDR_BIN_PATH={}\nexport MV_FAKE_HERDR_STATE={}\nexport HOME={}\nexport HERDR_PLUGIN_CONFIG_DIR={}\nexport HERDR_PLUGIN_STATE_DIR={}\nexec {} \"$@\"\n",
            quote(&fake_binary().to_string_lossy()), quote(&state.to_string_lossy()), quote(&sandbox.0.to_string_lossy()),
            quote(&config.to_string_lossy()), quote(&runtime.to_string_lossy()), quote(&fake_binary().to_string_lossy()))).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let launch = PreparedLaunch {
            project_root: sandbox.0.clone(),
            cwd: sandbox.0.join("writer's lane"),
            task_id: "20261102-builder-abcdef".into(),
            agent_name: "example-builder".into(),
            role: "builder".into(),
            kind: "codex".into(),
            model: "test-model".into(),
            seat: "test-seat".into(),
            brief: "scoped task\n".into(),
            native_args: argv::native_args("test-model", "codex", Some("high")),
            environment: vec![("CODEX_HOME".into(), config.to_string_lossy().into_owned())],
            clear_auth: vec!["OPENAI_API_KEY".into()],
            command: None,
        };
        let store = Store {
            launch,
            locks: locks.clone(),
            released: 0,
            setup_failed: false,
            release_failed: false,
            routing_failed: false,
            taken: HashSet::new(),
            warnings: Vec::new(),
            admission_failed: false,
            halt_failed: false,
            keep_on_failure: true,
            // Zero by default so a persistently-failing `agent start` fails
            // fast instead of spinning for Python's real 5-second budget;
            // a test exercising the retry itself overrides this upward.
            shell_retry: Duration::ZERO,
            dispatched: Vec::new(),
            started: Vec::new(),
            failed_to_start: Vec::new(),
            failed_to_start_fails: false,
            trips: Vec::new(),
            trip_fails: false,
        };
        let fake = Fake {
            script,
            state,
            locks,
            calls: Vec::new(),
            fail: None,
            trust: false,
            unreadable: false,
            fail_reads_after_prompt: false,
            prompted: false,
            stalled_prompts: 0,
            start_not_ready: false,
            start_failures: 0,
            command_route: false,
            claude_detect_delay: 0,
            split_done: false,
            detected_once: false,
            banner: None,
            stuck_pastes: 0,
            enter_submits: true,
            leave_scrollback_ghost: false,
            leave_scrollback_ghost_echo: false,
            dialog_after_paste: None,
            second_get_override: None,
            gets_since_prompt: 0,
        };
        (sandbox, store, fake)
    }
    fn timing() -> Timing {
        Timing {
            readiness: Duration::ZERO,
            prompt: Duration::ZERO,
            poll: Duration::ZERO,
        }
    }

    #[test]
    fn dispatch_passes_its_own_task_id_to_the_pane_instead_of_inheriting_one() {
        let (_sandbox, mut store, mut fake) = fixture();
        store
            .launch
            .environment
            .push(("MULTIVERSE_TASK_ID".into(), "parent-task".into()));
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let split = &fake.called("pane", "split")[0].argv;
        let task_env: Vec<_> = split
            .windows(2)
            .filter(|pair| pair[0] == "--env" && pair[1].starts_with("MULTIVERSE_TASK_ID="))
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(
            task_env,
            vec![format!("MULTIVERSE_TASK_ID={}", task.launch.task_id)]
        );
    }

    #[test]
    fn fake_dispatch_writes_brief_before_prompt_and_releases_lock_before_wait() {
        let (_sandbox, mut store, mut fake) = fixture();
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert_eq!(store.released, 0);
        assert!(task.warnings.is_empty());
        assert_eq!(fake.called("agent", "prompt").len(), 1);
        assert_eq!(
            wait_for_result(&mut fake, &task, Duration::ZERO, Duration::ZERO).unwrap(),
            (Some(crate::gateway::TaskStatus::Done), AgentState::Done)
        );
        let split = &fake.called("pane", "split")[0].argv;
        assert!(split.iter().any(|s| s.starts_with("CODEX_HOME=")));
        assert!(split.iter().any(|s| s == "--no-focus"));
        let start = &fake.called("agent", "start")[0].argv;
        let boundary = start.iter().position(|s| s == "--").unwrap();
        assert_eq!(&start[boundary + 1..], task.launch.native_args.as_slice());
        assert!(!start.iter().any(|s| s == "--env"));
        assert_eq!(
            fs::metadata(task_dir(&task.launch))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(fake.called("pane", "close").len(), 0);
    }

    /// Critic finding 11: a pre-existing `.multiverse`/`tasks` (the shared
    /// checkout a reader dispatch runs in, or any lane dispatched into
    /// before this fix landed) must still end up `0o700`, not just a
    /// freshly created one — Python always `chmod`s both, whether or not
    /// it created them.
    #[test]
    fn a_pre_existing_state_dir_is_tightened_to_0700() {
        let (_sandbox, mut store, mut fake) = fixture();
        let state = store.launch.cwd.join(".multiverse");
        let tasks = state.join("tasks");
        fs::create_dir_all(&tasks).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&tasks, fs::Permissions::from_mode(0o755)).unwrap();

        dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();

        for path in [&state, &tasks] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{path:?} must be tightened to 0o700 even though it already existed"
            );
        }
    }

    /// Card `20261102-b-07fc14`: a writer lane's own gitdir is outside
    /// Codex's automatic workspace allow-list, so `codex_worktree_
    /// permissions`'s `-c permissions.fleet.filesystem=...` override must
    /// reach the real `agent start` call, computed only once the worktree
    /// `prepare` created actually exists.
    #[test]
    fn codex_worktree_permissions_native_arg_is_appended_for_a_real_writer_lane() {
        let (_sandbox, mut store, mut fake) = fixture();
        let project_root = store.launch.project_root.clone();
        let lane = store.launch.cwd.clone();
        let git = |dir: &Path, args: &[&str]| {
            let output = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&project_root, &["init", "-q"]);
        git(
            &project_root,
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "init",
            ],
        );
        git(
            &project_root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "multiverse/example-builder",
                lane.to_str().unwrap(),
            ],
        );

        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();

        let start = &fake.called("agent", "start")[0].argv;
        let boundary = start.iter().position(|s| s == "--").unwrap();
        let tail = &start[boundary + 1..];
        assert!(
            tail.windows(2)
                .any(|w| w[0] == "-c" && w[1].starts_with("permissions.fleet.filesystem=")),
            "{tail:?}"
        );
        // The returned `Dispatched::launch` is the same mutated `launch`
        // `agent start` was actually called with, so it carries the
        // override too — unlike the earlier `record_dispatch` snapshot,
        // taken before this override is computed and so never sees it.
        assert_eq!(tail, task.launch.native_args.as_slice());
    }

    #[test]
    fn startup_failure_keeps_the_pane_open_by_default_and_releases_trial_once() {
        // Finding 3: Python's `keep_pane_on_failure` defaults to `True` —
        // a failed dispatch's pane is the only evidence of what went
        // wrong, and closing it by default is destructive. This test used
        // to assert the pane was closed; that pinned the wrong behavior.
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail = Some("start");
        store.release_failed = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert!(!error.to_string().contains("trial storage"));
        assert_eq!(store.released, 1);
        assert_eq!(store.warnings.len(), 1);
        assert!(store.warnings[0].contains("trial storage unavailable"));
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(
            fake.panes(),
            mv_herdr::shapes::Shape::Pane.records().len() + 1,
            "the new pane must still be in the layout"
        );
        // Finding 4: a start failure must leave a terminal outcome behind
        // rather than an AWAITING task holding its cap slot forever.
        assert_eq!(store.dispatched, ["example-builder"]);
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
        assert_eq!(store.failed_to_start[0].0, "example-builder");
        assert_ne!(store.failed_to_start[0].1, "");
    }

    #[test]
    fn keep_pane_on_failure_false_still_closes_the_returned_pane() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail = Some("start");
        store.keep_on_failure = false;
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
        let closed = &fake.called("pane", "close")[0].argv[3];
        assert_ne!(closed, "w52:p1");
        assert_eq!(fake.called("pane", "close").len(), 1);
        assert_eq!(fake.panes(), mv_herdr::shapes::Shape::Pane.records().len());
    }

    #[test]
    fn split_failure_has_no_pane_to_clean_up() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail = Some("split");
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
        assert_eq!(store.released, 1);
        assert!(fake.called("pane", "close").is_empty());
        assert!(fake.called("agent", "start").is_empty());
    }

    #[test]
    fn trust_dialog_retains_the_named_pane_and_never_prompts() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.trust = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        match error {
            RunnerError::NeedsAttention {
                agent,
                pane,
                reason,
            } => {
                assert_eq!(agent, "example-builder");
                assert_ne!(pane, "w52:p1");
                assert_eq!(reason, "trust dialog");
            }
            other => panic!("{other}"),
        }
        assert_eq!(store.released, 1);
        assert!(fake.called("pane", "close").is_empty());
        assert!(fake.called("agent", "prompt").is_empty());
        assert_eq!(fake.called("pane", "rename").len(), 1);
    }

    #[test]
    fn unreadable_pane_is_never_prompted_and_is_kept_open() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.unreadable = true;
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
        assert!(fake.called("agent", "prompt").is_empty());
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn failed_pane_read_cannot_prove_prompt_landed() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail_reads_after_prompt = true;
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn setup_failure_does_not_prompt_and_routing_failure_is_only_a_warning() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.setup_failed = true;
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing())
            .unwrap_err()
            .to_string()
            .contains("lane-setup-failed"));
        assert!(fake.called("agent", "prompt").is_empty());
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
        let (_sandbox2, mut store, mut fake) = fixture();
        store.routing_failed = true;
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert_eq!(task.warnings, ["routing unavailable"]);
        assert!(store.failed_to_start.is_empty());
    }

    #[test]
    fn command_route_uses_pane_run_and_rename_with_native_args() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.launch.kind = "claude".into();
        store.launch.command = Some("CACHE=1 test-wrapper".into());
        store.launch.native_args = argv::native_args("test-model", "claude", Some("high"));
        fake.command_route = true;
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert!(fake.called("agent", "start").is_empty());
        assert_eq!(fake.called("agent", "rename")[0].argv[3], task.pane);
        let script = &fake.called("pane", "run")[0].argv[4];
        assert!(
            script.contains("CACHE='1' 'test-wrapper' '--model' 'test-model' '--effort' 'high'")
        );
        assert!(script.contains("'\"'\"'"));
        assert!(pane_label(&task.launch).contains(" · test-wrapper · "));
    }

    #[test]
    fn command_route_waits_for_detection_before_renaming() {
        // Finding 2: Herdr has to notice a seat-command pane before
        // `agent rename` can target it. A fake-herdr backend never
        // classifies a pane on its own, so this fixture's `Fake` wrapper
        // synthesizes the detection poll itself; `claude_detect_delay`
        // makes the first poll miss, forcing a real retry.
        let (_sandbox, mut store, mut fake) = fixture();
        store.launch.kind = "claude".into();
        store.launch.command = Some("CACHE=1 test-wrapper".into());
        store.launch.native_args = argv::native_args("test-model", "claude", Some("high"));
        fake.command_route = true;
        fake.claude_detect_delay = 1;
        let patient = Timing {
            readiness: Duration::from_millis(200),
            prompt: Duration::ZERO,
            poll: Duration::from_millis(1),
        };
        let task = dispatch(&mut store, &mut fake, "w52:p1", &patient).unwrap();
        let split_index = fake
            .calls
            .iter()
            .position(|c| c.argv[1] == "pane" && c.argv[2] == "split")
            .unwrap();
        let rename_index = fake
            .calls
            .iter()
            .position(|c| c.argv[1] == "agent" && c.argv[2] == "rename")
            .unwrap();
        let list_calls_between = fake.calls[split_index..rename_index]
            .iter()
            .filter(|c| c.argv[1] == "agent" && c.argv[2] == "list")
            .count();
        assert_eq!(
            list_calls_between, 2,
            "must poll twice (miss, then detect) before renaming"
        );
        assert_eq!(fake.called("agent", "rename")[0].argv[3], task.pane);
    }

    #[test]
    fn command_route_never_detected_refuses_without_renaming() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.launch.kind = "claude".into();
        store.launch.command = Some("CACHE=1 test-wrapper".into());
        store.launch.native_args = argv::native_args("test-model", "claude", Some("high"));
        fake.command_route = true;
        fake.claude_detect_delay = usize::MAX;
        let impatient = Timing {
            readiness: Duration::from_millis(5),
            prompt: Duration::ZERO,
            poll: Duration::from_millis(1),
        };
        let error = dispatch(&mut store, &mut fake, "w52:p1", &impatient).unwrap_err();
        assert!(error.to_string().contains("never detected"));
        assert!(fake.called("agent", "rename").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn quote_command_words_keeps_assignment_prefixes_unquoted_but_quotes_their_values() {
        assert_eq!(
            quote_command_words("CACHE=1 claude-interactive"),
            "CACHE='1' 'claude-interactive'"
        );
        assert_eq!(
            quote_command_words("A=1 B=two claude-interactive"),
            "A='1' B='two' 'claude-interactive'"
        );
    }

    #[test]
    fn quote_command_words_neutralizes_shell_metacharacters_after_the_prefix() {
        // A config value is "trusted", but every word after the leading
        // `NAME=value` assignments is still quoted rather than spliced
        // raw into the pane's shell string — an injected `;`, `$(...)`,
        // or backtick becomes an inert literal argument, not executable
        // shell syntax, once it is inside single quotes.
        let script = quote_command_words("CACHE=1 wrapper; rm -rf /tmp/pwned");
        assert_eq!(script, "CACHE='1' 'wrapper;' 'rm' '-rf' '/tmp/pwned'");

        let script = quote_command_words("$(touch /tmp/pwned)");
        assert_eq!(script, "'$(touch' '/tmp/pwned)'");
    }

    #[test]
    fn quote_command_words_treats_a_non_identifier_equals_word_as_the_program() {
        // "2=weird" cannot be a shell assignment (names cannot start with
        // a digit), so it is quoted whole like any other word, and every
        // following word is treated as past the prefix too.
        assert_eq!(
            quote_command_words("2=weird CACHE=1 wrapper"),
            "'2=weird' 'CACHE=1' 'wrapper'"
        );
    }

    #[test]
    fn re_dispatch_never_overwrites_a_brief_and_respects_reserved_names() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.taken.insert("example-builder".into());
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert_eq!(task.launch.agent_name, "example-builder-2");
        let brief = task_dir(&task.launch).join("brief.md");
        fs::write(&brief, "first attempt").unwrap();
        let before = fake.called("pane", "split").len();
        assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
        assert_eq!(fs::read_to_string(brief).unwrap(), "first attempt");
        assert_eq!(fake.called("pane", "split").len(), before);
    }

    #[test]
    fn a_status_file_older_than_this_run_s_start_is_not_mistaken_for_this_run_s_result() {
        let (_sandbox, mut store, mut fake) = fixture();
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let status_path = task_dir(&task.launch).join("status");
        let result_path = task_dir(&task.launch).join("result.md");
        // The fake agent's own prompt handler already wrote a fresh "done" +
        // result.md during dispatch (see fake-herdr's prompt handling); this
        // simulates a stale pair left over from a previous occupant of the
        // same task id by backdating the status file's mtime to before the
        // run even started — same-id redispatch must not inherit it.
        fs::write(&status_path, "done\n").unwrap();
        fs::write(&result_path, "stale result from a previous run\n").unwrap();
        let before_start = task.started_at - Duration::from_secs(10);
        File::options()
            .write(true)
            .open(&status_path)
            .unwrap()
            .set_modified(before_start)
            .unwrap();
        assert_eq!(
            wait_for_result(&mut fake, &task, Duration::ZERO, Duration::ZERO).unwrap(),
            (None, AgentState::Done),
            "a status file older than this run's start is not this run's Done; Herdr's settled state is still kept"
        );

        // A status file written at or after the run's own start is this
        // run's own result and must be honored.
        File::options()
            .write(true)
            .open(&status_path)
            .unwrap()
            .set_modified(task.started_at)
            .unwrap();
        assert_eq!(
            wait_for_result(&mut fake, &task, Duration::ZERO, Duration::ZERO).unwrap(),
            (Some(crate::gateway::TaskStatus::Done), AgentState::Done)
        );
    }

    #[test]
    fn a_done_agent_without_status_or_nonempty_result_is_not_success() {
        let (_sandbox, mut store, mut fake) = fixture();
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        for status in ["running", "done"] {
            fs::write(task_dir(&task.launch).join("status"), status).unwrap();
            fs::write(task_dir(&task.launch).join("result.md"), " \n").unwrap();
            assert_eq!(
                wait_for_result(&mut fake, &task, Duration::ZERO, Duration::ZERO).unwrap(),
                (None, AgentState::Done),
                "Python's --wait report keeps live.settled.state when the status/result pair is unusable"
            );
        }
        fs::remove_file(task_dir(&task.launch).join("status")).unwrap();
        assert_eq!(
            wait_for_result(&mut fake, &task, Duration::ZERO, Duration::ZERO).unwrap(),
            (None, AgentState::Done),
            "a settled turn with no status file must still return Herdr agent_status"
        );
        assert!(fake.called("pane", "close").is_empty());
    }

    #[test]
    fn turn_gate_requires_movement_and_stable_blocked_samples() {
        let mut gate = TurnGate::default();
        assert!(!gate.observe(4, AgentState::Done, 4));
        assert!(!gate.observe(4, AgentState::Working, 5));
        assert!(!gate.observe(4, AgentState::Unknown, 6));
        assert!(gate.observe(4, AgentState::Idle, 7));
        assert!(gate.observe(4, AgentState::Done, 7));
        assert!(!gate.observe(4, AgentState::Blocked, 8));
        assert!(!gate.observe(4, AgentState::Blocked, 9));
        assert!(gate.observe(4, AgentState::Blocked, 9));
        assert!(!gate.observe(4, AgentState::Working, 10));
        assert!(!gate.observe(4, AgentState::Blocked, 9));
    }

    #[test]
    fn missing_wrapper_and_unsafe_ids_or_roles_refuse_before_pane_creation() {
        for (field, value) in [
            ("kind", "claude"),
            ("task", "../escape"),
            ("role", "fixer"),
            ("auth", "TOKEN;exit"),
        ] {
            let (_sandbox, mut store, mut fake) = fixture();
            match field {
                "kind" => store.launch.kind = value.into(),
                "task" => store.launch.task_id = value.into(),
                "role" => store.launch.role = value.into(),
                "auth" => store.launch.clear_auth = vec![value.into()],
                _ => unreachable!(),
            }
            assert!(dispatch(&mut store, &mut fake, "w52:p1", &timing()).is_err());
            assert!(fake.called("pane", "split").is_empty());
        }
    }

    #[test]
    fn admission_recheck_refuses_under_the_fleet_lock_before_any_pane_work() {
        // Finding 5: concurrency/project/seat-cap admission is re-verified
        // against *current* fleet state right after the fleet lock is
        // taken, not trusted from plan time — before `prepare` (worktree
        // creation, brief writing) ever runs.
        let (_sandbox, mut store, mut fake) = fixture();
        store.admission_failed = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("admission"));
        assert!(fake.called("pane", "split").is_empty());
        assert!(!store.launch.cwd.exists(), "prepare must never have run");
        assert!(store.dispatched.is_empty());
        assert!(store.failed_to_start.is_empty());
        assert_eq!(store.released, 1);
    }

    #[test]
    fn halt_recheck_refuses_immediately_before_pane_creation() {
        // Finding 5: the fleet could have been halted after planning
        // finished; this is the last check before a pane actually exists,
        // run after naming (so the dispatch is still on record) but before
        // `pane split`.
        let (_sandbox, mut store, mut fake) = fixture();
        store.halt_failed = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("halt"));
        assert!(fake.called("pane", "split").is_empty());
        assert_eq!(store.dispatched, ["example-builder"]);
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn agent_start_retries_a_not_yet_ready_shell_then_succeeds() {
        // Finding 9: `agent target pane ... is not an available shell` can
        // race a shell that has not finished coming up; retry rather than
        // fail the dispatch on the first miss.
        let (_sandbox, mut store, mut fake) = fixture();
        fake.start_failures = 1;
        store.shell_retry = Duration::from_millis(200);
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert_eq!(fake.called("agent", "start").len(), 2);
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.started, ["example-builder"]);
        assert!(store.failed_to_start.is_empty());
        assert_eq!(task.launch.agent_name, "example-builder");
    }

    #[test]
    fn failed_to_start_logging_failure_does_not_mask_the_dispatch_error() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail = Some("start");
        store.failed_to_start_fails = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("injected failure"));
        assert!(store.warnings.iter().any(|w| w.contains("failed-to-start")));
    }

    #[test]
    fn agent_start_retry_exhausted_still_fails_the_dispatch() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.start_failures = usize::MAX;
        store.shell_retry = Duration::from_millis(5);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("not an available shell"));
        assert!(fake.called("agent", "start").len() >= 2);
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn blocked_startup_keeps_the_named_pane_open() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.trust = true;
        fake.start_not_ready = true;
        assert!(matches!(
            dispatch(&mut store, &mut fake, "w52:p1", &timing()),
            Err(RunnerError::NeedsAttention { .. })
        ));
        assert!(fake.called("pane", "close").is_empty());
        assert!(fake.called("agent", "prompt").is_empty());
        assert_eq!(store.released, 1);
    }

    #[test]
    fn stalled_prompt_is_retried_at_most_twice() {
        for stalled in [1, 2] {
            let (_sandbox, mut store, mut fake) = fixture();
            fake.stalled_prompts = stalled;
            let result = dispatch(&mut store, &mut fake, "w52:p1", &timing());
            assert_eq!(result.is_ok(), stalled == 1);
            assert_eq!(fake.called("agent", "prompt").len(), 2);
            assert_eq!(store.released, usize::from(stalled == 2));
        }
    }

    // --- Submission verification (card `20261106-b-19efc6`): a codex pane
    // dispatched by the Python fleet binary sat idle ~70 minutes with the
    // brief still sitting, unsubmitted, in its own input box — `agent
    // prompt` had returned success and `state_change_seq` had already
    // moved from the keystrokes alone. `deliver_prompt` must not accept
    // that as landed; it must require the composer to actually clear, try
    // one bare Enter if it does not, and fail the dispatch loudly rather
    // than leave a silent idle lane. ---

    #[test]
    fn prompt_lands_without_a_stuck_paste_and_never_needs_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let prompts = fake.called("agent", "prompt");
        assert_eq!(prompts.len(), 1, "no stuck paste, no Enter ever sent");
        assert!(!prompts[0].argv[4].is_empty());
        assert_eq!(store.started, [task.launch.agent_name]);
    }

    #[test]
    fn a_stuck_paste_is_submitted_with_one_bare_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.stuck_pastes = 1;
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let prompts = fake.called("agent", "prompt");
        assert_eq!(
            prompts.len(),
            2,
            "the original paste, then exactly one Enter — never a second full retype"
        );
        assert!(
            prompts[0].argv[4].contains("brief.md"),
            "first send carries the real brief: {:?}",
            prompts[0].argv[4]
        );
        assert_eq!(
            prompts[1].argv[4], "",
            "the nudge must be a bare Enter, not the brief retyped"
        );
        assert_eq!(store.started, [task.launch.agent_name]);
    }

    #[test]
    fn a_paste_that_not_even_enter_submits_fails_the_dispatch_loudly() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.stuck_pastes = usize::MAX;
        fake.enter_submits = false;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        let prompts = fake.called("agent", "prompt");
        assert_eq!(
            prompts.len(),
            2,
            "the original paste, then exactly one Enter — no silent idle lane, \
             and no third attempt that would retype the brief into a composer \
             that has already shown it will not submit"
        );
        assert_eq!(prompts[1].argv[4], "");
        assert!(
            error.to_string().contains("still shows the prompt"),
            "{error}"
        );
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    // --- Reviewer HOLD `20261106-reviewer-731eab`: a whole-pane
    // `text.contains(&prompt)` reads a brief retained in scrollback after
    // real submission as "still unsubmitted", so the one bare Enter above
    // can land on whatever the agent's fresh prompt shows next — including
    // editor ghost text. `deliver_prompt`/`confirm_submitted` must look
    // only at the pane's own composer region (`prompt_still_in_composer`),
    // never the whole pane. ---

    #[test]
    fn a_brief_retained_in_scrollback_never_draws_a_phantom_enter_into_ghost_text() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.leave_scrollback_ghost = true;
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let prompts = fake.called("agent", "prompt");
        assert_eq!(
            prompts.len(),
            1,
            "the brief landed and scrolled into history; its own composer \
             now holds unrelated ghost text, which a whole-pane match would \
             wrongly read as the brief still sitting there and answer with \
             an Enter — composer-scoped evidence must not"
        );
        assert_eq!(store.started, [task.launch.agent_name]);
    }

    #[test]
    fn a_dialog_between_paste_and_enter_is_never_answered_with_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.dialog_after_paste = Some("Do you trust the files in this folder?");
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(
            matches!(error, RunnerError::NeedsAttention { .. }),
            "{error}"
        );
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the original paste only — a dialog opening before any landing \
             confirmation must never be answered with a bare Enter"
        );
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    // --- Second Critic HOLD `20261106-reviewer-f7aaf4`: a composer still
    // holding the brief is not, on its own, licence for `deliver_prompt` to
    // press its own bare Enter. A *fresh* `agent get` read, taken right
    // before that key, must show the agent genuinely idle — a failed,
    // unknown, or non-idle (`Working`/`Blocked`) read must refuse the Enter
    // and fail the dispatch loudly instead. Separately, `confirm_submitted`
    // must not let a polling budget whose every `agent get` failed pass as
    // "not submitted" — that is zero evidence, not negative evidence. ---

    #[test]
    fn a_failed_fresh_read_before_the_key_refuses_the_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Fail);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)), "{error}");
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the original paste only — a fresh read that failed right before \
             the key must never be treated as licence to press Enter anyway"
        );
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn a_working_agent_on_the_fresh_read_before_the_key_lands_without_the_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Working);
        dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the composer still shows the brief, but the fresh read says the \
             agent is already Working — the brief landed late, same as \
             confirm_submitted's own Working check, so no Enter is sent"
        );
        assert_eq!(store.started, ["example-builder"]);
        assert!(store.failed_to_start.is_empty());
    }

    #[test]
    fn a_blocked_agent_on_the_fresh_read_before_the_key_refuses_the_enter() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Blocked);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(
            matches!(error, RunnerError::NeedsAttention { .. }),
            "{error}"
        );
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the composer still shows the brief, but the fresh read says the \
             agent is Blocked on a dialog this text scan did not recognise — \
             never press Enter into that"
        );
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn a_polling_budget_with_zero_successful_reads_is_not_evidence_of_anything() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.fail = Some("get");
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)), "{error}");
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "every `agent get` failed outright; `confirm_submitted` must \
             refuse to treat that as 'not submitted, go on and consider \
             Enter' — it must fail loudly instead, never reaching a bare \
             Enter: {:?}",
            fake.called("agent", "prompt")
        );
        assert!(store.started.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    // --- `deliver_prompt_to` extension (card `20261106-b-f174b0`): the same
    // stuck-paste submission check above — composer-specific evidence only,
    // one bare Enter gated on a fresh idle/done read, never a key into a
    // dialog or ghost text — now lives in `deliver_prompt_to` itself, which
    // `deliver_prompt` merely formats a prompt for. These regressions call
    // `deliver_prompt_to` directly, against the pane an ordinary dispatch
    // already landed into, the same way `commands::activity::nudge_one`
    // (nudge) and `commands::start`'s `--reseed` re-point do: a second
    // prompt sent to an already-running agent, not a first dispatch. ---

    /// Land an ordinary first prompt via `dispatch`, then hand back the
    /// agent/pane a nudge or reseed call site would already have found live,
    /// plus `fake`'s own current `state_change_seq` as the baseline the next
    /// `deliver_prompt_to` call should use — mirroring how a real caller
    /// reads `LiveAgent::state_change_seq` from a fresh listing rather than
    /// reusing the baseline `dispatch` itself used.
    fn nudge_fixture() -> (Sandbox, Store, Fake, Dispatched, u64) {
        let (sandbox, mut store, mut fake) = fixture();
        let task = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap();
        let baseline = checked(
            &mut fake,
            argv::agent_get(&task.launch.agent_name),
            COMMAND_TIMEOUT,
        )
        .ok()
        .and_then(|result| {
            result
                .payload
                .as_ref()
                .and_then(|p| snapshot(&p["result"]["agent"], &task.launch.agent_name, &task.pane))
        })
        .expect("dispatch must leave a readable agent behind")
        .1;
        fake.calls.clear();
        (sandbox, store, fake, task, baseline)
    }

    /// Long enough to clear `GHOST_ECHO_RISK_CHARS`, so the regressions
    /// below that actually press the bare Enter exercise that path itself
    /// rather than being short-circuited by the ghost-echo length gate —
    /// which gets its own dedicated regression further down.
    const LONG_NUDGE: &str = "please resume the dispatched task and write its result file";

    #[test]
    fn deliver_prompt_to_lands_without_a_stuck_paste_and_never_needs_enter() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "no stuck paste, no Enter ever sent"
        );
    }

    #[test]
    fn deliver_prompt_to_submits_a_stuck_paste_with_one_bare_enter() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.stuck_pastes = 1;
        deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            LONG_NUDGE,
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap();
        let prompts = fake.called("agent", "prompt");
        assert_eq!(
            prompts.len(),
            2,
            "the original nudge, then exactly one Enter — never a second full retype"
        );
        assert_eq!(prompts[0].argv[4], LONG_NUDGE);
        assert_eq!(prompts[1].argv[4], "");
    }

    #[test]
    fn deliver_prompt_to_fails_loudly_when_not_even_enter_submits() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.stuck_pastes = usize::MAX;
        fake.enter_submits = false;
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            LONG_NUDGE,
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            2,
            "the original nudge, then exactly one Enter — no silent unconfirmed \
             nudge, and no third attempt that would retype it into a composer \
             that has already shown it will not submit"
        );
        assert!(
            error.to_string().contains("still shows the prompt"),
            "{error}"
        );
    }

    #[test]
    fn deliver_prompt_to_never_answers_a_dialog_opened_after_the_nudge_with_enter() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.dialog_after_paste = Some("Do you trust the files in this folder?");
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        assert!(
            matches!(error, RunnerError::NeedsAttention { .. }),
            "{error}"
        );
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the original nudge only — a dialog opening before any landing \
             confirmation must never be answered with a bare Enter"
        );
    }

    #[test]
    fn deliver_prompt_to_never_draws_a_phantom_enter_into_scrollback_ghost_text() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.leave_scrollback_ghost = true;
        deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the nudge landed and scrolled into history; composer-scoped \
             evidence must not mistake its own fresh ghost text for the \
             nudge still sitting unsubmitted"
        );
    }

    #[test]
    fn deliver_prompt_to_never_presses_enter_into_a_ghost_echo_of_a_completed_short_nudge() {
        // Card `20261106-b-e08cef`: unlike the scrollback-ghost regression
        // above, the fresh composer here echoes back the *same* short text
        // that was just run to completion — so `prompt_still_in_composer`
        // is (correctly, on text alone) fooled into reading it as still
        // unsubmitted. Only the length gate can save this one.
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.leave_scrollback_ghost_echo = true;
        deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the nudge already ran to completion; a ghost suggestion that \
             happens to repeat it verbatim must never draw a second, \
             phantom Enter"
        );
    }

    #[test]
    fn deliver_prompt_to_refuses_the_enter_on_a_failed_fresh_read() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Fail);
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            LONG_NUDGE,
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)), "{error}");
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "a fresh read that failed right before the key must never be \
             treated as licence to press Enter anyway"
        );
    }

    #[test]
    fn deliver_prompt_to_treats_a_working_fresh_read_as_already_landed() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Working);
        deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            LONG_NUDGE,
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap();
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the composer still shows the nudge, but the fresh read says the \
             agent is already Working — it reached its own turn late, so no \
             Enter is sent"
        );
    }

    #[test]
    fn deliver_prompt_to_refuses_the_enter_on_a_blocked_fresh_read() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.stuck_pastes = 1;
        fake.second_get_override = Some(GetOverride::Blocked);
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            LONG_NUDGE,
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        assert!(
            matches!(error, RunnerError::NeedsAttention { .. }),
            "{error}"
        );
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "the composer still shows the nudge, but the fresh read says the \
             agent is Blocked on a dialog this text scan did not recognise — \
             never press Enter into that"
        );
    }

    #[test]
    fn deliver_prompt_to_refuses_when_no_agent_get_ever_succeeds() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.fail = Some("get");
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)), "{error}");
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "every `agent get` failed outright; this must fail loudly \
             instead of reaching a bare Enter"
        );
    }

    #[test]
    fn deliver_prompt_to_refuses_a_vendor_dialog_that_opens_after_the_nudge() {
        let (_sandbox, _store, mut fake, task, baseline) = nudge_fixture();
        fake.dialog_after_paste = Some(CODEX_MODEL_SWITCH_FOR_NUDGE);
        let error = deliver_prompt_to(
            &mut fake,
            &task.launch.agent_name,
            &task.pane,
            "nudge text",
            &task.launch.kind,
            baseline,
            &timing(),
        )
        .unwrap_err();
        match error {
            RunnerError::NeedsAttention { reason, .. } => {
                assert_eq!(reason, "dialog (codex_model_switch_offer)");
            }
            other => panic!("{other}"),
        }
        assert_eq!(
            fake.called("agent", "prompt").len(),
            1,
            "a vendor-specific (billing/model-switch) dialog that opens after \
             the nudge must be recognised and refused the same way the \
             generic trust/approval scan already is, not mistaken for an \
             ordinary stuck composer"
        );
    }

    const CODEX_MODEL_SWITCH_FOR_NUDGE: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt");

    // --- Vendor exhaustion/dialog wiring (card `20261103-b-566e90`'s
    // runner half): `limits::classify`/`blocking_prompt::classify_prompt`
    // reused from the readiness/first-prompt path
    // (`refuse_if_parked_on_dialog`) and the dispatch-failure path (the
    // `trip_breaker` call in `dispatch`'s own `if let Err(error) = &attempt`
    // block), against the same captured fixtures `health.rs` and
    // `blocking_prompt.rs`/`limits.rs` already pin. One fixture per vendor. ---

    const CODEX_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-out-of-credits.txt");
    const CODEX_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-rolling-429.txt");
    const CODEX_MODEL_SWITCH: &str =
        include_str!("../../../tests/fixtures/rate-limit/codex-model-switch-offer.txt");
    const CLAUDE_ROLLING: &str =
        include_str!("../../../tests/fixtures/rate-limit/claude-rolling-429.txt");
    const GROK_EXHAUSTED: &str =
        include_str!("../../../tests/fixtures/rate-limit/grok-weekly-limit-reached.txt");

    #[test]
    fn codex_model_switch_offer_refuses_the_first_prompt_without_tripping() {
        // A model-switch offer is `PromptAudience::DoNotAnswer`, not a
        // `limits::classify` exhaustion verdict (Python's own
        // `_classify_and_record_limit` never recognises it either) — it
        // blocks the blind prompt but never trips a breaker.
        let (_sandbox, mut store, mut fake) = fixture();
        fake.banner = Some(CODEX_MODEL_SWITCH);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        match error {
            RunnerError::NeedsAttention {
                agent,
                pane,
                reason,
            } => {
                assert_eq!(agent, "example-builder");
                assert_ne!(pane, "w52:p1");
                assert_eq!(reason, "dialog (codex_model_switch_offer)");
            }
            other => panic!("{other}"),
        }
        assert!(fake.called("agent", "prompt").is_empty());
        assert!(fake.called("pane", "close").is_empty());
        assert!(store.trips.is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn codex_out_of_credits_dialog_trips_the_breaker_and_leaves_the_pane_open() {
        // An *interactive* billing dialog: trip (so the next chain walk
        // skips this pool), but never type into it or close it — a human
        // still has to answer "Request increase?".
        let (_sandbox, mut store, mut fake) = fixture();
        fake.banner = Some(CODEX_EXHAUSTED);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        match error {
            RunnerError::NeedsAttention { reason, .. } => {
                assert_eq!(reason, "dialog (codex_usage_limit_reached)");
            }
            other => panic!("{other}"),
        }
        assert_eq!(
            store.trips,
            vec![(
                "example-builder".to_string(),
                "codex_usage_limit_reached".to_string(),
                true
            )]
        );
        assert!(fake.called("agent", "prompt").is_empty());
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn grok_weekly_limit_dialog_trips_the_breaker_and_leaves_the_pane_open() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.launch.kind = "grok".into();
        fake.banner = Some(GROK_EXHAUSTED);
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        match error {
            RunnerError::NeedsAttention { reason, .. } => {
                assert_eq!(reason, "dialog (grok_weekly_limit_reached)");
            }
            other => panic!("{other}"),
        }
        assert_eq!(
            store.trips,
            vec![(
                "example-builder".to_string(),
                "grok_weekly_limit_reached".to_string(),
                true
            )]
        );
        assert!(fake.called("pane", "close").is_empty());
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn codex_rolling_429_banner_trips_the_breaker_and_closes_the_dead_pane() {
        // A plain, non-interactive 429 banner never blocks the first prompt
        // (`classify_prompt` has no shape for it — `codex_dialog_re` never
        // matches, same as Python's `_wait_for_prompt_pane`), so the prompt
        // is attempted and this test forces its own, unrelated failure
        // (stalled prompts) to reach the dispatch-failure classification —
        // which then trips on the banner that was visible the whole time and
        // tears the dead pane down, exactly like a `done` dispatch's pane
        // (card `20260911-fixer-d23119`), unconditionally of
        // `keep_pane_on_failure`.
        let (_sandbox, mut store, mut fake) = fixture();
        fake.banner = Some(CODEX_ROLLING);
        fake.stalled_prompts = 2;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)));
        assert_eq!(
            store.trips,
            vec![(
                "example-builder".to_string(),
                "codex_retry_limit_exceeded".to_string(),
                false
            )]
        );
        assert_eq!(fake.called("pane", "close").len(), 1);
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn claude_rolling_429_banner_trips_the_breaker_and_closes_the_dead_pane() {
        let (_sandbox, mut store, mut fake) = fixture();
        store.launch.kind = "claude".into();
        store.launch.command = Some("CACHE=1 test-wrapper".into());
        store.launch.native_args = argv::native_args("test-model", "claude", Some("high"));
        fake.command_route = true;
        fake.banner = Some(CLAUDE_ROLLING);
        fake.stalled_prompts = 2;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::Refused(_)));
        assert_eq!(
            store.trips,
            vec![(
                "example-builder".to_string(),
                "claude_rate_limit_error".to_string(),
                false
            )]
        );
        assert_eq!(fake.called("pane", "close").len(), 1);
        assert_eq!(store.failed_to_start.len(), 1);
    }

    #[test]
    fn trip_breaker_failure_is_only_a_warning_and_never_masks_the_dispatch_error() {
        let (_sandbox, mut store, mut fake) = fixture();
        fake.banner = Some(CODEX_ROLLING);
        fake.stalled_prompts = 2;
        store.trip_fails = true;
        let error = dispatch(&mut store, &mut fake, "w52:p1", &timing()).unwrap_err();
        assert!(error.to_string().contains("injected stalled prompt"));
        assert!(store.warnings.iter().any(|w| w.contains("breaker")));
        assert_eq!(store.trips.len(), 1);
        // The trip call failed, but the pane close that follows a
        // non-interactive verdict does not depend on it succeeding.
        assert_eq!(fake.called("pane", "close").len(), 1);
    }

    #[test]
    fn split_placement_stacks_in_the_nearest_right_column() {
        let rectangle = |pane: &str, x, y, height| Rectangle {
            pane: pane.into(),
            x,
            y,
            width: 100.0,
            height,
        };
        let layout = [
            rectangle("caller", 0.0, 0.0, 100.0),
            rectangle("top", 100.0, 0.0, 50.0),
            rectangle("bottom", 100.0, 50.0, 50.0),
            rectangle("far", 200.0, 0.0, 100.0),
        ];
        assert_eq!(
            placement("caller", &layout),
            (Some("bottom".into()), "down")
        );
        assert_eq!(placement("far", &layout), (Some("far".into()), "right"));
        // Origin pane missing from the layout: `None` means "split
        // `--current`" (the fallback matching Python's own `_where_to_split`
        // `mine not in rects` case), not an explicit (wrong) pane id.
        assert_eq!(placement("missing", &layout), (None, "right"));
    }

    /// Card `20261102-b-01b66f`: `rectangles()` must read each pane's
    /// geometry from a nested `rect` object, the shape Python's own
    /// `herdr.pane_layout` caller (`runner.py`'s `_where_to_split`/
    /// `_row_panes`, both reading `pane.get("rect")`) has always assumed.
    /// A flat `x`/`y`/`width`/`height` on the pane entry itself parses as
    /// present but always `None` here, so this is the one schema shape a
    /// synthetic `Vec<Rectangle>` built by hand (as every other test in
    /// this file does) can never catch — only a real `pane layout`-shaped
    /// envelope exercises the field lookup at all.
    #[test]
    fn rectangles_reads_the_nested_rect_object() {
        let result = CommandResult {
            argv: vec![],
            returncode: 0,
            stdout: String::new(),
            stderr: String::new(),
            payload: Some(json!({"result": {"layout": {"panes": [
                {"pane_id": "caller", "rect": {"x": 0.0, "y": 0.0, "width": 100.0, "height": 100.0}},
                {"pane_id": "right", "rect": {"x": 100.0, "y": 0.0, "width": 50.0, "height": 100.0}},
            ]}}})),
            protocol_error: None,
        };
        let layout = rectangles(&result).expect("a well-formed layout must parse");
        assert_eq!(layout.len(), 2);
        assert_eq!(
            placement("caller", &layout),
            (Some("right".into()), "down"),
            "a right-hand column already exists and must be reused, not grown rightward again"
        );
    }

    // --- teardown's equalize_row (card 20261106-b-724e9c) -----------------

    /// A minimal in-process [`Herdr`] for [`teardown`]'s row-equalize path
    /// alone: `row` is the surviving row `anchor` sits in — by the time
    /// `teardown` reads it, the just-closed pane is already gone from
    /// Herdr's own layout, so these fixtures start from the row as it
    /// looks post-close, never modelling the close itself removing an
    /// entry. `other` sits on a different row entirely, to prove a resize
    /// never reaches outside `anchor`'s own row. `pane resize` rescales
    /// the "remainder" (every row pane right of the one being grown or
    /// shrunk) in proportion to its members' own widths — the nested,
    /// rightward-split topology `equalize_row`'s own doc comment assumes
    /// every dispatch-built row has.
    struct RowFake {
        row: Vec<(String, f64)>,
        other: (String, f64),
        closed: Vec<String>,
        resizes: Vec<(String, String, f64)>,
    }

    impl RowFake {
        fn layout(&self) -> Value {
            let mut x = 0.0;
            let mut panes = Vec::new();
            for (pane, width) in &self.row {
                panes.push(json!({
                    "pane_id": pane,
                    "rect": {"x": x, "y": 0.0, "width": width, "height": 50.0},
                }));
                x += width;
            }
            panes.push(json!({
                "pane_id": self.other.0,
                "rect": {"x": 0.0, "y": 100.0, "width": self.other.1, "height": 50.0},
            }));
            json!({"panes": panes})
        }

        fn resize(&mut self, pane: &str, direction: &str, amount: f64) -> bool {
            let Some(idx) = self.row.iter().position(|(p, _)| p == pane) else {
                return false;
            };
            let i = match (direction, idx.checked_sub(1)) {
                ("right", _) => idx,
                ("left", Some(i)) => i,
                _ => return false,
            };
            if i + 1 >= self.row.len() {
                return false;
            }
            let remainder_before: f64 = self.row[i + 1..].iter().map(|(_, w)| *w).sum();
            let span = self.row[i].1 + remainder_before;
            let delta = amount * span;
            let signed = if direction == "right" { delta } else { -delta };
            let remainder_after = remainder_before - signed;
            if remainder_after <= 0.0 || self.row[i].1 + signed <= 0.0 {
                return false;
            }
            let scale = remainder_after / remainder_before;
            self.row[i].1 += signed;
            for (_, w) in self.row[i + 1..].iter_mut() {
                *w *= scale;
            }
            true
        }
    }

    impl Herdr for RowFake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            let ok = |payload: Option<Value>| CommandResult {
                argv: command.argv.clone(),
                returncode: 0,
                stdout: String::new(),
                stderr: String::new(),
                payload,
                protocol_error: None,
            };
            let flag = |name: &str| -> String {
                command
                    .argv
                    .iter()
                    .position(|a| a == name)
                    .and_then(|i| command.argv.get(i + 1))
                    .cloned()
                    .unwrap_or_default()
            };
            match (command.argv[1].as_str(), command.argv[2].as_str()) {
                ("pane", "close") => {
                    self.closed.push(command.argv[3].clone());
                    Ok(ok(None))
                }
                ("pane", "layout") => Ok(ok(Some(json!({"result": {"layout": self.layout()}})))),
                ("pane", "resize") => {
                    let pane = flag("--pane");
                    let direction = flag("--direction");
                    let amount: f64 = flag("--amount").parse().unwrap_or(0.0);
                    self.resizes.push((pane.clone(), direction.clone(), amount));
                    let changed = self.resize(&pane, &direction, amount);
                    Ok(ok(Some(json!({"result": {"resize": {
                        "changed": changed,
                        "layout": self.layout(),
                    }}}))))
                }
                _ => panic!("unexpected herdr call in RowFake: {:?}", command.argv),
            }
        }
    }

    /// A row already within `equalize_row`'s own 1-column tolerance must
    /// come back untouched: the closed pane's row gets exactly one
    /// `pane layout` read and no `pane resize` at all.
    #[test]
    fn teardown_leaves_an_already_even_row_unresized() {
        let mut fake = RowFake {
            row: vec![
                ("left".into(), 100.0),
                ("mid".into(), 100.0),
                ("right".into(), 100.0),
            ],
            other: ("elsewhere".into(), 80.0),
            closed: Vec::new(),
            resizes: Vec::new(),
        };
        teardown(&mut fake, "closed-pane", "left");
        assert_eq!(fake.closed, vec!["closed-pane".to_string()]);
        assert!(
            fake.resizes.is_empty(),
            "a row already within tolerance must not be resized: {:?}",
            fake.resizes
        );
    }

    /// A three-pane row left uneven by a close converges toward an equal
    /// share, and a pane sharing no row with the anchor is never touched —
    /// the gap `teardown`'s own doc comment used to name (card
    /// `20261106-b-724e9c`).
    #[test]
    fn teardown_equalizes_an_uneven_three_pane_row() {
        let mut fake = RowFake {
            row: vec![
                ("left".into(), 60.0),
                ("mid".into(), 100.0),
                ("right".into(), 140.0),
            ],
            other: ("elsewhere".into(), 777.0),
            closed: Vec::new(),
            resizes: Vec::new(),
        };
        teardown(&mut fake, "closed-pane", "left");
        assert_eq!(fake.closed, vec!["closed-pane".to_string()]);
        assert!(
            !fake.resizes.is_empty(),
            "an uneven row must be resized toward an equal share"
        );
        assert!(
            fake.resizes.iter().all(|(pane, _, _)| pane != "elsewhere"),
            "never resize a pane outside the closed pane's own row: {:?}",
            fake.resizes
        );
        assert_eq!(
            fake.other.1, 777.0,
            "a pane on another row must be untouched"
        );
        let widths: Vec<f64> = fake.row.iter().map(|(_, w)| *w).collect();
        let total: f64 = widths.iter().sum();
        assert!(
            (total - 300.0).abs() <= 1e-6,
            "resizing must not change the row's total width: {widths:?}"
        );
        let min = widths.iter().cloned().fold(f64::INFINITY, f64::min);
        let max = widths.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        assert!(
            max - min <= 2.0,
            "every pane in the row should end up within an equal share: {widths:?}"
        );
    }

    // --- deliver_heartbeat (card 20261102-b-4f86db, finding 1) ------------

    /// A minimal in-process [`Herdr`] for [`deliver_heartbeat`] alone — no
    /// dispatch, no lock, no subprocess: just the three calls a heartbeat
    /// ever makes (`pane read`, `agent prompt`, `agent get`).
    struct HeartbeatFake {
        text: &'static str,
        prompt_error_code: Option<&'static str>,
        prompt_ok: bool,
        state: AgentState,
        seq: u64,
        prompt_calls: u32,
    }

    impl Herdr for HeartbeatFake {
        fn call(
            &mut self,
            command: &HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, RunnerError> {
            match (command.argv[1].as_str(), command.argv[2].as_str()) {
                ("pane", "read") => Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: self.text.to_string(),
                    stderr: String::new(),
                    payload: None,
                    protocol_error: None,
                }),
                ("agent", "prompt") => {
                    self.prompt_calls += 1;
                    if let Some(code) = self.prompt_error_code {
                        return Ok(CommandResult {
                            argv: command.argv.clone(),
                            returncode: 1,
                            stdout: String::new(),
                            stderr: json!({"error": {"code": code, "message": "blocked"}})
                                .to_string(),
                            payload: None,
                            protocol_error: None,
                        });
                    }
                    Ok(CommandResult {
                        argv: command.argv.clone(),
                        returncode: i32::from(!self.prompt_ok),
                        stdout: String::new(),
                        stderr: if self.prompt_ok {
                            String::new()
                        } else {
                            "injected failure".into()
                        },
                        payload: None,
                        protocol_error: None,
                    })
                }
                ("agent", "get") => Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: Some(json!({"result": {"agent": {
                        "name": "demo-nexus",
                        "pane_id": "w1:p1",
                        "agent_status": self.state.as_str(),
                        "state_change_seq": self.seq,
                    }}})),
                    protocol_error: None,
                }),
                other => panic!("unexpected call {other:?}"),
            }
        }
    }

    #[test]
    fn deliver_heartbeat_sends_once_and_confirms_landing() {
        let mut fake = HeartbeatFake {
            text: "Ready >",
            prompt_error_code: None,
            prompt_ok: true,
            state: AgentState::Working,
            seq: 5,
            prompt_calls: 0,
        };
        let outcome =
            deliver_heartbeat(&mut fake, "demo-nexus", "w1:p1", "tick", 1, &timing()).unwrap();
        assert_eq!(outcome, Heartbeat::Landed);
        assert_eq!(fake.prompt_calls, 1);
    }

    #[test]
    fn deliver_heartbeat_never_resends_when_landing_is_unconfirmed() {
        // Still idle at the same sequence number the whole poll budget
        // (`timing()`'s own zero prompt/poll durations end the wait after
        // one observation): a landing that never confirms must still count
        // as sent, and must never trigger a second `agent prompt`.
        let mut fake = HeartbeatFake {
            text: "Ready >",
            prompt_error_code: None,
            prompt_ok: true,
            state: AgentState::Idle,
            seq: 1,
            prompt_calls: 0,
        };
        let outcome =
            deliver_heartbeat(&mut fake, "demo-nexus", "w1:p1", "tick", 1, &timing()).unwrap();
        assert_eq!(
            outcome,
            Heartbeat::Unconfirmed {
                last_status: "idle".to_string()
            }
        );
        assert_eq!(fake.prompt_calls, 1);
    }

    #[test]
    fn deliver_heartbeat_refuses_to_type_into_a_dialog() {
        let mut fake = HeartbeatFake {
            text: "Do you trust the files in this folder?",
            prompt_error_code: None,
            prompt_ok: true,
            state: AgentState::Idle,
            seq: 1,
            prompt_calls: 0,
        };
        let error =
            deliver_heartbeat(&mut fake, "demo-nexus", "w1:p1", "tick", 0, &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::NeedsAttention { .. }));
        assert_eq!(
            fake.prompt_calls, 0,
            "a dialog must never receive the tick text"
        );
    }

    #[test]
    fn deliver_heartbeat_reports_a_blocked_pane_without_resending() {
        let mut fake = HeartbeatFake {
            text: "Ready >",
            prompt_error_code: Some("agent_blocked"),
            prompt_ok: true,
            state: AgentState::Idle,
            seq: 1,
            prompt_calls: 0,
        };
        let error =
            deliver_heartbeat(&mut fake, "demo-nexus", "w1:p1", "tick", 0, &timing()).unwrap_err();
        assert!(matches!(error, RunnerError::NeedsAttention { .. }));
        assert_eq!(fake.prompt_calls, 1);
    }

    #[test]
    fn menu_reason_only_matches_an_open_menu_in_the_pane_tail() {
        let menu = "earlier scrollback\n\n? Pick one\n> option a\n  option b\n\n\
                    Enter to select · Esc to cancel";
        assert!(menu_reason(menu));
        assert_eq!(attention_reason(menu), Some("menu"));
        assert!(!menu_reason("just some ordinary working output"));
        assert_eq!(attention_reason("just some ordinary working output"), None);
    }

    #[test]
    fn dialog_or_menu_reason_names_a_trust_dialog_before_any_prompt() {
        let text = "Trust this folder?\n› 1. Trust and continue\n2. Cancel\n";
        let reason = dialog_or_menu_reason(text, "codex").expect("trust dialog");
        assert!(
            reason.starts_with("dialog ("),
            "python prints 'parked on a dialog (…); nothing was sent', got {reason:?}"
        );
        assert!(dialog_or_menu_reason("Ready >", "codex").is_none());
    }

    #[test]
    fn menu_reason_ignores_markers_scrolled_out_of_the_tail() {
        let scrolled = "Enter to select · Esc to cancel\n\
                        line 1\nline 2\nline 3\nline 4\nline 5\nline 6";
        assert!(!menu_reason(scrolled));
        assert_eq!(attention_reason(scrolled), None);
    }

    // --- `prompt_still_in_composer` (reviewer HOLD `20261106-reviewer-
    // 731eab`): composer-region-only evidence, never a whole-pane scan. ---

    #[test]
    fn prompt_still_in_composer_requires_the_marker_after_the_prompt() {
        let prompt = "Read `brief.md` and execute it.";
        let in_composer = format!("some earlier output\n\n› {prompt}");
        assert!(prompt_still_in_composer(&in_composer, prompt, "codex"));
    }

    #[test]
    fn composer_marker_is_vendor_specific_for_codex_and_shared_for_claude_and_grok() {
        assert_eq!(composer_marker("codex"), "›");
        assert_eq!(composer_marker("claude"), "❯");
        assert_eq!(composer_marker("grok"), "❯");
    }

    #[test]
    fn prompt_still_in_composer_ignores_a_copy_left_in_scrollback() {
        let prompt = "Read `brief.md` and execute it.";
        // The brief sits above the pane's last marker, in scrollback; the
        // composer itself has moved on to unrelated ghost text.
        let scrolled_past = format!("› {prompt}\n\nTask completed.\n\n› some ghost suggestion");
        assert!(!prompt_still_in_composer(&scrolled_past, prompt, "codex"));
    }

    #[test]
    fn prompt_still_in_composer_is_false_with_no_marker_at_all() {
        let prompt = "Read `brief.md` and execute it.";
        // No `›` anywhere: an unrecognisable screen is never "still
        // unsubmitted" — `deliver_prompt` must not press Enter into it.
        assert!(!prompt_still_in_composer(prompt, prompt, "codex"));
    }

    #[test]
    fn prompt_still_in_composer_accepts_a_wrapped_composer_via_its_tail() {
        let prompt = "Read `/very/long/path/to/brief.md` and execute it. Follow its write \
                       scope exactly. Write your findings to `/very/long/path/to/result.md` \
                       and then write `done`, `blocked`, or `failed` to `/very/long/path/to/status`.";
        // The composer's finite height has scrolled the opening line out of
        // view; only the tail (still ending in a real file path) remains.
        let tail = distinctive_tail(prompt);
        let wrapped = format!("› …{tail}");
        assert!(prompt_still_in_composer(&wrapped, prompt, "codex"));
    }

    #[test]
    fn deliver_heartbeat_refuses_to_type_into_an_ask_user_question_menu() {
        let mut fake = HeartbeatFake {
            text: "? Which should I do\n> a\n  b\nEnter to select · Esc to cancel",
            prompt_error_code: None,
            prompt_ok: true,
            state: AgentState::Idle,
            seq: 1,
            prompt_calls: 0,
        };
        let error =
            deliver_heartbeat(&mut fake, "demo-nexus", "w1:p1", "tick", 0, &timing()).unwrap_err();
        match error {
            RunnerError::NeedsAttention { reason, .. } => assert_eq!(reason, "menu"),
            other => panic!("{other}"),
        }
        assert_eq!(
            fake.prompt_calls, 0,
            "an open AskUserQuestion picker must never receive the tick text"
        );
    }
}

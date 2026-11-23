//! `worktrees <path> [--prune|--rescue] [--force] [--yes] [--dry-run]
//! [--deep] [--restore TASK_ID]`: inspect, prune, rescue, or restore lanes.
//! `--deep` requires `--prune`. During prune, `--yes` or `--force` confirms
//! disposal of ignored files; all other removal guards still apply.
//! After pruning, `--deep` sweeps old finished-task leftovers and project
//! scratch, and named scratch caches in finished, clean surviving lanes, and
//! `.multiverse/worktree-state/`'s own quarantine copies (card
//! `20261106-b-3d2774`): one already byte-identical to what rescue copied
//! into the project's own `.multiverse/` is dropped regardless of age, and
//! the rest once past a 14-day retention window. Like the rest of `--deep`,
//! never part of a plain `--prune`.
//! Audit records, logs and small text survive the task sweep. A plain
//! `--prune` (no `--deep`) also frees a *kept* lane's own lane-root
//! `target/` once that lane's occupants are all finished and nothing is
//! still live in it, reporting the bytes freed. A lane that is actually
//! removed or archived, or kept under that same condition, also frees its
//! own per-worktree cargo build-dir under the shared build cache
//! `.cargo/config.toml` configures (card `20261115-b-47f043`), found
//! read-only with no `cargo`/`rustup` subprocess, reported the same way —
//! never a build-dir outside that configured root, or the main checkout's
//! own. `--prune` also reclaims every *other* shared-build hash dir under
//! that same root whose own evidence names only workspace roots gone from
//! disk (card `20261115-b-673528`) — independent of which lanes this
//! project's own state still lists — reporting the same bytes-freed line,
//! but never for a hash dir a lane in that same run already reported
//! (card `20261119-b-c3f9f5`).
//! Card `20261119-b-73759c`: every one of those Rust-only cache frees — a
//! lane's `target/` and build-dir, the orphaned shared-build sweep, and
//! `--deep`'s `.multiverse/worktree-state/` quarantine removals — needs
//! the same `--yes` (or `--force`) confirmation ignored-file disposal does;
//! without it each is measured and reported exactly as `--dry-run`
//! reports it ("would free ..."). The deletions plain `--prune` has always
//! made with Python parity — lane removal/archive, branch reclaim, and
//! `--deep`'s own task/scratch sweep (pinned by the
//! `worktrees-deep-prune-keeps-audit-and-sweeps-finished-lane-scratch`
//! conformance scenario) — are unchanged.
//! `--dry-run`
//! previews cleanup paths and byte totals without deleting them; with
//! `--prune` its removable set matches what `--yes` would remove, and with
//! `--rescue` it names what each lane's rescue would copy without copying
//! anything, and prints `skipped (regenerable)` for a venv/`*.egg-info`/
//! similar cache inside `.multiverse/tasks/<task>/` rather than refusing
//! the lane on the symlinks those caches contain (card `20261122-b-4961e4`).
//!
//! Card `20261115-b-9815fe`: `--prune --reap` is an opt-in reaper, report by
//! default. A kept lane whose only refusal is "lane has a live process cwd"
//! is still only ever kept and named, never signaled, unless `--reap` is
//! also given — and never when `--force` was used, since `--force` may have
//! bypassed a live pane the process check can no longer trust. `--reap
//! --dry-run` previews with "would reap" and kills nothing; a real `--reap`
//! signals, waits, and re-checks pid identity before escalating, then
//! prunes once more so a now-clean lane is actually removed/archived in the
//! same run.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_state::worktrees::{
    BranchOutcome, HerdrLaneObserver, MergeError, OrphanedBuildDir, PruneOutcome,
    QuarantineOutcome, QuarantineReason, RescueReport, Worktrees,
};

use super::doctor::resolve_config_path;
use crate::exit::Failure;

fn fail(e: impl std::fmt::Display) -> Failure {
    Failure::Operation(e.to_string())
}

fn outcome_task(outcome: &PruneOutcome) -> &str {
    match outcome {
        PruneOutcome::Kept { task, .. }
        | PruneOutcome::WouldRemove { task, .. }
        | PruneOutcome::WouldArchive { task, .. }
        | PruneOutcome::Removed { task, .. }
        | PruneOutcome::Archived { task, .. } => task,
    }
}

fn action_line(outcome: &PruneOutcome) -> String {
    match outcome {
        PruneOutcome::Kept { reason, .. } => format!("kept — {reason}"),
        PruneOutcome::WouldRemove { .. } => "would remove".to_string(),
        PruneOutcome::WouldArchive { .. } => "would archive".to_string(),
        // Matches Python's own real-run wording (`cli.py`'s
        // `print("    removed (held nothing)", file=out)`).
        PruneOutcome::Removed { .. } => "removed (held nothing)".to_string(),
        PruneOutcome::Archived { tip, .. } => format!("archived at {tip}"),
    }
}

/// The lines `--prune` prints for the orphaned shared-build sweep: each
/// hash dir it returned, then whether it was freed (or, under `preview`,
/// would be) or kept and why. Card `20261119-b-a9f95f`: a delete that
/// failed leaves the dir on disk, so it is never reported as freed. Card
/// `20261122-b-a5e62d`: every dir the sweep returns gets its own pair of
/// lines, including one its scan budget ran out on, which it now returns
/// as kept rather than dropping.
fn orphaned_build_dir_lines(build_dirs: &[OrphanedBuildDir], preview: bool) -> Vec<String> {
    let verb = if preview { "would free" } else { "freed" };
    let mut lines = Vec::new();
    for build_dir in build_dirs {
        lines.push(format!("  cargo shared-build {}", build_dir.path.display()));
        lines.push(match &build_dir.kept {
            Some(reason) => format!("    kept build-dir — {reason}"),
            None => format!("    {verb} build-dir ({} bytes)", build_dir.bytes),
        });
    }
    lines
}

/// `lane_path`'s own lingering host processes, under `--reap` on
/// `worktrees --prune` (card `20261115-b-9815fe`). Pulled out of `prune`
/// as a pure function of one lane's own process snapshot so it is directly
/// unit-testable against a real spawned child, without capturing stdout or
/// driving the full CLI. Returns the notes to print under that lane's own
/// "kept" line, and whether anything was actually signaled (the caller's
/// cue to re-prune once more).
///
/// `reap` false previews "rerun with --reap"; `reap` true and `dry_run`
/// true previews "would reap" and signals nothing — both are the same
/// "report, never kill" shape `--reap --dry-run`/plain `--prune` already
/// promise. `reap` true and `dry_run` false is the one path that calls
/// [`end_leftover`].
///
/// `snapshot` is one [`mv_engine::dockerreport::process_cwds`] read the
/// caller shares across every lane in its pass; `None` means the probe
/// failed. `dead` (card `20261122-b-78bcf9`, [`PruneOutcome::Kept`]'s
/// `lane_dead`) picks which processes are never reaped: in a dead lane
/// only [`leftover_protected`]'s own short list (pid 1, Herdr, this
/// process and its ancestors), since a shell left behind there (`zsh -c`)
/// is exactly the leftover that blocks `--merge`; otherwise the wider
/// [`mv_engine::dockerreport::protected`] list, which also spares shells.
fn reap_lane_notes(
    lane_path: &Path,
    snapshot: Option<&[mv_engine::dockerreport::ProcessCwd]>,
    dead: bool,
    reap: bool,
    dry_run: bool,
    ancestors: &[u32],
) -> (Vec<String>, bool) {
    let Some(snapshot) = snapshot else {
        return (
            vec!["    warning: process-cwd probe unknown".to_string()],
            false,
        );
    };
    let mut notes = Vec::new();
    let mut any_reaped = false;
    for process in mv_engine::dockerreport::processes_under(lane_path, snapshot) {
        let label = mv_engine::dockerreport::process_label(&process);
        let protected = if dead {
            leftover_protected(&process, ancestors)
        } else {
            mv_engine::dockerreport::protected(&process, ancestors)
        };
        if protected {
            notes.push(format!(
                "    lane process {label} (protected, never reaped)"
            ));
        } else if !reap {
            notes.push(format!(
                "    lane process {label} (rerun with --reap to end it)"
            ));
        } else if dry_run {
            notes.push(format!("    would reap {label}"));
        } else {
            notes.push(format!("    reaping {label}"));
            notes.push(format!(
                "      {}",
                reap_outcome_text(end_leftover(&process))
            ));
            any_reaped = true;
        }
    }
    (notes, any_reaped)
}

/// Card `20261122-b-78bcf9`: never a reap target in a *dead* lane — one
/// whose own pane is gone (`worktrees --prune`'s `lane_dead`) or was just
/// closed by `tasks --reclaimable --yes`. Narrower than
/// [`mv_engine::dockerreport::protected`] on purpose: a shell whose cwd is
/// still inside a lane nobody occupies any more is a leftover too, and
/// sparing it left `--merge` refused with nothing able to end it. Pid 1,
/// Herdr itself, this process, and every ancestor of it are still never
/// signaled, whatever cwd they report.
pub(crate) fn leftover_protected(
    process: &mv_engine::dockerreport::ProcessCwd,
    ancestors: &[u32],
) -> bool {
    let first = process.command.split_whitespace().next().unwrap_or("");
    let name = first.rsplit('/').next().unwrap_or(first);
    process.pid <= 1
        || process.pid == std::process::id()
        || ancestors.contains(&process.pid)
        || matches!(name, "herdr" | "herdr-server")
}

/// Ends one reap candidate: for an ssh ControlMaster
/// ([`ssh_mux_master_socket`]) first asks it to exit cleanly with `ssh -O
/// exit -S <socket>`, then always [`mv_engine::dockerreport::reap`] —
/// SIGTERM, a short wait, pid identity (command and cwd) re-confirmed,
/// SIGKILL only if it is still the same process. A master the `-O exit`
/// already ended makes that SIGTERM a no-op.
///
/// Card `20261123-b-ccc2cc`: a snapshot's `command` is `ps`'s `comm=`
/// column, which on macOS is only the executable (`ssh`), never the
/// master's own `[mux]` title or its `-S` socket — so an `ssh` whose
/// snapshot command alone does not show a master has its full argument
/// list read ([`mv_engine::dockerreport::process_parent`]) before the
/// `-O exit` decision.
pub(crate) fn end_leftover(
    process: &mv_engine::dockerreport::ProcessCwd,
) -> mv_engine::dockerreport::ReapOutcome {
    let socket = ssh_mux_master_socket(&process.command).or_else(|| {
        (leftover_command_name(&process.command) == "ssh")
            .then(|| mv_engine::dockerreport::process_parent(process.pid))
            .flatten()
            .and_then(|parent| ssh_mux_master_socket(&parent.args))
    });
    if let Some(socket) = socket {
        ssh_mux_exit(&socket);
    }
    mv_engine::dockerreport::reap(process, mv_engine::dockerreport::DEFAULT_REAP_GRACE)
}

/// One line saying what [`end_leftover`] found, printed under its own
/// "reaping" line so the operator sees what actually ended.
pub(crate) fn reap_outcome_text(outcome: mv_engine::dockerreport::ReapOutcome) -> &'static str {
    use mv_engine::dockerreport::ReapOutcome;
    match outcome {
        ReapOutcome::Reaped => "ended",
        ReapOutcome::Killed => "ended (SIGKILL after the grace window)",
        ReapOutcome::StillAlive => "NOT ended: still alive after SIGKILL",
        ReapOutcome::IdentityChanged => "ended (its pid now names a different process, left alone)",
    }
}

/// The control socket of an ssh ControlMaster `command` describes, when
/// it is visibly one: OpenSSH's own `ssh: <socket> [mux]` process title for
/// a backgrounded master, or an `ssh` command line that both asks to be a
/// master (`-M`, or `-o ControlMaster=yes`) and names its socket (`-S
/// <socket>`, or `-o ControlPath=<socket>`). A plain client using some
/// other master's socket is deliberately never matched: `-O exit` on it
/// would close a master that may be shared outside this lane, while a
/// SIGTERM ends only the client.
fn ssh_mux_master_socket(command: &str) -> Option<String> {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let first = *tokens.first()?;
    if first == "ssh:" {
        return (tokens.len() >= 3 && tokens.last() == Some(&"[mux]"))
            .then(|| tokens[1..tokens.len() - 1].join(" "));
    }
    if first.rsplit('/').next() != Some("ssh") {
        return None;
    }
    let mut master = false;
    let mut socket = None;
    // ssh's own option grammar: a cluster of flags, the first of these
    // that takes an argument consuming the rest of its token (or, when
    // that is empty, the next token). The first non-option token is the
    // destination; everything after it is the remote command, never read.
    const TAKES_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";
    let mut rest = tokens[1..].iter();
    while let Some(token) = rest.next() {
        let Some(cluster) = token.strip_prefix('-') else {
            break;
        };
        if cluster.is_empty() || cluster == "-" {
            break;
        }
        for (at, flag) in cluster.char_indices() {
            if !TAKES_ARG.contains(flag) {
                master |= flag == 'M';
                continue;
            }
            let inline = &cluster[at + flag.len_utf8()..];
            let value = if inline.is_empty() {
                rest.next().copied().unwrap_or("").to_string()
            } else {
                inline.to_string()
            };
            match flag {
                'S' => socket = Some(value),
                'o' => {
                    if let Some((key, val)) = value.split_once('=') {
                        if key.eq_ignore_ascii_case("ControlMaster") {
                            master |= val.eq_ignore_ascii_case("yes");
                        } else if key.eq_ignore_ascii_case("ControlPath") {
                            socket = Some(val.to_string());
                        }
                    }
                }
                _ => {}
            }
            break;
        }
    }
    socket.filter(|s| master && !s.is_empty() && !s.eq_ignore_ascii_case("none"))
}

/// `ssh -O exit -S <socket>` with no config file read (`-F /dev/null`), so
/// nothing in the operator's own `~/.ssh/config` runs; the destination is
/// a placeholder `-O` never connects to. Bounded to a few seconds and its
/// result ignored: [`end_leftover`]'s SIGTERM follows regardless.
fn ssh_mux_exit(socket: &str) {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    let Ok(mut child) = Command::new("ssh")
        .args(["-F", "/dev/null", "-S", socket, "-O", "exit", "mux"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// The executable name of a snapshot `command`: its first word, path
/// stripped, and a login shell's leading `-` (`-zsh`) dropped.
fn leftover_command_name(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("");
    let name = first.rsplit('/').next().unwrap_or(first);
    name.strip_prefix('-').unwrap_or(name)
}

/// The refusal prefix `mv_state`'s own `clean` returns for a lane some
/// process still uses as its cwd, and the one `--merge` refusal
/// [`merge_through_leftovers`] acts on.
const LIVE_CWD_REFUSAL: &str = "lane has a live process cwd";

/// Card `20261123-b-ccc2cc`: the known-benign leftover class `process`
/// belongs to, or `None` when it is anything else. Matched by command
/// name *and* by being orphaned (`parent.ppid` 1) or its lane's pane
/// being gone — never by cwd alone, which is all that made it a holder:
///
/// - `caffeinate` (an agent's own `caffeinate -t 300`), orphaned or with
///   its pane gone;
/// - an ssh ControlMaster ([`ssh_mux_master_socket`] on its full argument
///   list), orphaned or with its pane gone — a plain ssh client never;
/// - a Monitor's `tail`/`ugrep`/`grep`, only once orphaned;
/// - a shell, only once its pane is gone.
///
/// `parent` `None` (the pid is gone, or `ps` could not read it) is never
/// benign; neither is anything [`leftover_protected`] spares. `pane_gone`
/// is only asked when a class needs it — it is one Herdr round trip.
fn benign_leftover(
    process: &mv_engine::dockerreport::ProcessCwd,
    parent: Option<&mv_engine::dockerreport::ProcessParent>,
    ancestors: &[u32],
    pane_gone: &mut dyn FnMut() -> bool,
) -> Option<&'static str> {
    let parent = parent?;
    if leftover_protected(process, ancestors) {
        return None;
    }
    let orphaned = parent.ppid == 1;
    match leftover_command_name(&process.command) {
        "caffeinate" if orphaned || pane_gone() => Some("caffeinate"),
        "ssh" if ssh_mux_master_socket(&parent.args).is_some() && (orphaned || pane_gone()) => {
            Some("ssh mux master")
        }
        "tail" | "ugrep" | "grep" if orphaned => Some("orphaned monitor"),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh" | "nu" if pane_gone() => {
            Some("shell of a gone pane")
        }
        _ => None,
    }
}

/// What [`merge_through_leftovers`] reads and signals, so a test can drive
/// it with an injected process table instead of the real host's `lsof`/
/// `ps` (card `20261123-b-c2b3f1`'s flake) or a real Herdr.
pub(crate) trait MergeLeftovers {
    /// The lane the merging task works in, or `None` when it has none.
    fn lane_path(&mut self) -> Option<PathBuf>;
    /// One [`mv_engine::dockerreport::process_cwds`] snapshot.
    fn snapshot(&mut self) -> Option<Vec<mv_engine::dockerreport::ProcessCwd>>;
    /// [`mv_engine::dockerreport::process_parent`] for one holder.
    fn parent(&mut self, pid: u32) -> Option<mv_engine::dockerreport::ProcessParent>;
    /// Whether every pane the lane's occupants recorded is gone.
    fn pane_gone(&mut self) -> bool;
    /// This process's own ancestors, never benign
    /// ([`mv_engine::dockerreport::own_ancestor_pids`]).
    fn ancestors(&mut self) -> Vec<u32>;
    /// Ends one benign holder — [`end_leftover`].
    fn end(
        &mut self,
        process: &mv_engine::dockerreport::ProcessCwd,
    ) -> mv_engine::dockerreport::ReapOutcome;
}

/// The real host: `lsof`/`ps` snapshots, [`end_leftover`], and the same
/// pane check `--prune` uses for a dead lane (one Herdr `agent list`,
/// asked at most once, and only when a holder's class depends on it).
struct HostMergeLeftovers<'a> {
    worktrees: &'a Worktrees,
    task: &'a str,
    lane: Option<Option<mv_state::worktrees::Lane>>,
    pane_gone: Option<bool>,
}

impl HostMergeLeftovers<'_> {
    /// The lane `task` merges from — its own, or the one a `dispatch
    /// --into` round adopted — resolved once, on first use.
    fn lane(&mut self) -> Option<&mv_state::worktrees::Lane> {
        let (worktrees, task) = (self.worktrees, self.task);
        self.lane
            .get_or_insert_with(|| {
                let owner = mv_state::worktrees::lane_task_for(worktrees.project(), task)
                    .ok()
                    .flatten()?;
                worktrees
                    .lanes()
                    .ok()?
                    .into_iter()
                    .find(|lane| lane.task == owner)
            })
            .as_ref()
    }
}

impl MergeLeftovers for HostMergeLeftovers<'_> {
    fn lane_path(&mut self) -> Option<PathBuf> {
        self.lane().map(|lane| lane.path.clone())
    }

    fn snapshot(&mut self) -> Option<Vec<mv_engine::dockerreport::ProcessCwd>> {
        mv_engine::dockerreport::process_cwds()
    }

    fn parent(&mut self, pid: u32) -> Option<mv_engine::dockerreport::ProcessParent> {
        mv_engine::dockerreport::process_parent(pid)
    }

    fn ancestors(&mut self) -> Vec<u32> {
        mv_engine::dockerreport::own_ancestor_pids()
    }

    fn pane_gone(&mut self) -> bool {
        use mv_state::worktrees::LaneObserver;
        if let Some(gone) = self.pane_gone {
            return gone;
        }
        let project = self.worktrees.project().to_path_buf();
        let gone = self
            .lane()
            .is_some_and(|lane| HerdrLaneObserver::new(project).check(lane, false).is_ok());
        self.pane_gone = Some(gone);
        gone
    }

    fn end(
        &mut self,
        process: &mv_engine::dockerreport::ProcessCwd,
    ) -> mv_engine::dockerreport::ReapOutcome {
        end_leftover(process)
    }
}

/// One holder named in a `--merge` refusal: pid, full command line when
/// `ps` could read it (else the snapshot's own), cwd, and its leftover
/// class or why it is not one.
fn holder_text(
    process: &mv_engine::dockerreport::ProcessCwd,
    parent: Option<&mv_engine::dockerreport::ProcessParent>,
    class: Option<&str>,
) -> String {
    let command = parent
        .map(|parent| parent.args.as_str())
        .filter(|args| !args.trim().is_empty())
        .unwrap_or(&process.command);
    let command: String = command
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(160)
        .collect();
    let class = class.unwrap_or("not a known leftover");
    format!(
        "pid {} `{command}` (cwd {}; {class})",
        process.pid,
        process.cwd.display()
    )
}

/// The "live process cwd" refusal naming every holder `holders` lists.
fn holders_refusal(project: &Path, holders: &[String], suffix: &str) -> MergeError {
    MergeError::Refused(format!(
        "{LIVE_CWD_REFUSAL}: {}{suffix}; end them with `multiverse worktrees {} --prune \
         --reap --yes` (preview first with --dry-run), then retry the merge",
        holders.join("; "),
        project.display()
    ))
}

/// Card `20261123-b-ccc2cc`: `--merge` past a lane's known-benign
/// leftovers. Runs `attempt` (the real [`Worktrees::merge`]); a refusal
/// other than "live process cwd" — or a success — returns unchanged.
/// Otherwise reads the holders under `lane_path` from one snapshot and:
///
/// - if any holder is not a [`benign_leftover`], refuses naming every
///   holder's pid, command and class, signaling nothing;
/// - else ends each one with [`MergeLeftovers::end`] — SIGTERM, the reap
///   grace, identity re-checked, SIGKILL — and re-checks once by running
///   `attempt` again. A second live-cwd refusal names whatever still
///   holds the lane; it is never retried further.
///
/// A failed snapshot, or one that no longer sees any holder, returns the
/// original refusal (which already names what `mv_state` saw). Returns
/// the lines to print ahead of the result, one pair per holder ended.
fn merge_through_leftovers<T>(
    project: &Path,
    leftovers: &mut impl MergeLeftovers,
    attempt: &mut impl FnMut() -> Result<T, MergeError>,
) -> (Vec<String>, Result<T, MergeError>) {
    let first = attempt();
    if !matches!(&first, Err(MergeError::Refused(message)) if message.starts_with(LIVE_CWD_REFUSAL))
    {
        return (Vec::new(), first);
    }
    let Some(lane_path) = leftovers.lane_path() else {
        return (Vec::new(), first);
    };
    let lane_path = lane_path.as_path();
    let Some(snapshot) = leftovers.snapshot() else {
        return (Vec::new(), first);
    };
    let holders = mv_engine::dockerreport::processes_under(lane_path, &snapshot);
    if holders.is_empty() {
        return (Vec::new(), first);
    }
    let ancestors = leftovers.ancestors();
    let mut classified = Vec::new();
    for holder in holders {
        let parent = leftovers.parent(holder.pid);
        let class = benign_leftover(&holder, parent.as_ref(), &ancestors, &mut || {
            leftovers.pane_gone()
        });
        classified.push((holder, parent, class));
    }
    if classified.iter().any(|(_, _, class)| class.is_none()) {
        let named: Vec<String> = classified
            .iter()
            .map(|(holder, parent, class)| holder_text(holder, parent.as_ref(), *class))
            .collect();
        return (Vec::new(), Err(holders_refusal(project, &named, "")));
    }

    let mut notes = Vec::new();
    for (holder, parent, class) in &classified {
        notes.push(format!(
            "ending leftover {}",
            holder_text(holder, parent.as_ref(), *class)
        ));
        notes.push(format!("  {}", reap_outcome_text(leftovers.end(holder))));
    }
    let second = attempt();
    if !matches!(&second, Err(MergeError::Refused(message)) if message.starts_with(LIVE_CWD_REFUSAL))
    {
        return (notes, second);
    }
    let still: Vec<String> = leftovers
        .snapshot()
        .map(|snapshot| mv_engine::dockerreport::processes_under(lane_path, &snapshot))
        .unwrap_or_default()
        .into_iter()
        .map(|holder| {
            let parent = leftovers.parent(holder.pid);
            let class = benign_leftover(&holder, parent.as_ref(), &ancestors, &mut || {
                leftovers.pane_gone()
            });
            holder_text(&holder, parent.as_ref(), class)
        })
        .collect();
    if still.is_empty() {
        return (notes, second);
    }
    let refusal = holders_refusal(project, &still, " (still there after one reap pass)");
    (notes, Err(refusal))
}

/// `worktrees`'s entry point: dispatches to `--restore`, `--merge`,
/// `--prune`, `--rescue`, or the plain inspect listing, in that order of
/// precedence. See the module doc for what each mode does.
pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    if args.get_flag("deep") && !args.get_flag("prune") {
        return Err(fail("--deep is only meaningful together with --prune"));
    }
    let path = args.get_one::<String>("path").expect("required");
    let worktrees = Worktrees::new(path).map_err(fail)?;

    if let Some(task) = args.get_one::<String>("restore") {
        let tip = worktrees.restore(task).map_err(fail)?;
        println!("restored multiverse/{task} at {tip}");
        return Ok(());
    }

    if let Some(task) = args.get_one::<String>("merge") {
        return merge(&worktrees, args, task);
    }

    if args.get_flag("prune") {
        return prune(&worktrees, args);
    }

    if args.get_flag("rescue") {
        return rescue(&worktrees, args.get_flag("dry-run"));
    }

    inspect(&worktrees, path)
}

fn prune(worktrees: &Worktrees, args: &ArgMatches) -> Result<(), Failure> {
    let force = args.get_flag("force");
    let dry_run = args.get_flag("dry-run");
    let reap = args.get_flag("reap");
    let confirm_ignored = force || args.get_flag("yes");
    // Card `20261119-b-73759c`: every Rust-only cache deletion below is
    // gated on that same confirmation; without it a real `--prune` reports
    // each one exactly as `--dry-run` would, deleting none.
    let preview_caches = dry_run || !confirm_ignored;

    for ignored in worktrees.ignored_entries().map_err(fail)? {
        println!("  {}: ignored: not a lane", ignored.display());
    }

    // Snapshotted before `prune()` can remove anything: a removed or
    // archived lane's own directory is gone by the time its outcome comes
    // back, and this label is purely descriptive, not safety-critical.
    let found = worktrees.lanes().map_err(fail)?;
    let summaries: HashMap<String, String> = found
        .iter()
        .map(|lane| {
            (
                lane.task.clone(),
                worktrees.lane_summary(lane, confirm_ignored),
            )
        })
        .collect();
    let lane_paths: HashMap<String, PathBuf> = found
        .iter()
        .map(|lane| (lane.task.clone(), lane.path.clone()))
        .collect();

    let mut observer = HerdrLaneObserver::new(worktrees.project());
    let mut outcomes = worktrees
        .prune_with_ignored(
            &mut observer,
            force,
            dry_run,
            confirm_ignored,
            confirm_ignored,
        )
        .map_err(fail)?;

    // Card `20261115-b-9815fe`: the lane-exit reaper. Report-only by
    // default — the extra lines below are purely descriptive until
    // `--reap` is also given — and never when `--force` was used: a
    // live-pane keep-reason ordinarily implies `observer.check` already
    // passed, but `--force` can have bypassed exactly that check, so the
    // process this lane still shows may belong to a pane this run never
    // actually confirmed gone.
    //
    // Card `20261122-b-78bcf9`: a *dead* lane (`lane_dead`: its pane is
    // gone) is checked whatever its keep-reason — a done, unmerged lane is
    // kept for "not merged" long before `clean`'s own live-cwd check runs,
    // so its leftovers (`tail -f`, a `zsh -c`, an ssh mux master) were
    // never even listed. One process snapshot serves the whole pass, taken
    // only once some lane actually needs it.
    let mut reap_notes: HashMap<String, Vec<String>> = HashMap::new();
    let mut reaped_tasks: Vec<String> = Vec::new();
    if !force {
        let ancestors = mv_engine::dockerreport::own_ancestor_pids();
        let mut snapshot: Option<Option<Vec<mv_engine::dockerreport::ProcessCwd>>> = None;
        for outcome in &outcomes {
            let PruneOutcome::Kept {
                task,
                reason,
                lane_dead,
                ..
            } = outcome
            else {
                continue;
            };
            let live_cwd = reason.contains("live process cwd");
            if !live_cwd && !*lane_dead {
                continue;
            }
            let Some(lane_path) = lane_paths.get(task) else {
                continue;
            };
            let processes = snapshot
                .get_or_insert_with(mv_engine::dockerreport::process_cwds)
                .as_deref();
            if processes.is_none() && !live_cwd {
                // Only a lane already refused for a live cwd warns that the
                // probe failed; a dead lane kept for another reason stays
                // as quiet as it always was.
                continue;
            }
            let (notes, any_reaped) =
                reap_lane_notes(lane_path, processes, *lane_dead, reap, dry_run, &ancestors);
            if notes.is_empty() {
                continue;
            }
            reap_notes.insert(task.clone(), notes);
            if any_reaped {
                reaped_tasks.push(task.clone());
            }
        }
    }
    if !reaped_tasks.is_empty() {
        // The follow-through: re-prune now that the reaped lane(s) no
        // longer have a live process cwd, so a lane this run just reaped
        // is actually removed/archived in the same invocation rather than
        // needing a second `--prune` run. Lanes this pass already finished
        // (`Removed`/`Archived`/…) are gone from `lanes()` by now and are
        // simply absent from this second outcome set; only a reaped
        // task's own entry is spliced back into `outcomes` below.
        let mut follow_up_observer = HerdrLaneObserver::new(worktrees.project());
        if let Ok(follow_up) = worktrees.prune_with_ignored(
            &mut follow_up_observer,
            force,
            dry_run,
            confirm_ignored,
            confirm_ignored,
        ) {
            for outcome in follow_up {
                let task = outcome_task(&outcome).to_string();
                if reaped_tasks.contains(&task) {
                    if let Some(slot) = outcomes
                        .iter_mut()
                        .find(|existing| outcome_task(existing) == task)
                    {
                        *slot = outcome;
                    }
                }
            }
        }
    }

    if outcomes.is_empty() {
        // Matches Python's `cmd_worktrees`: not a return — the orphan
        // sweep below still runs even when there are no lanes at all —
        // and the same wording `inspect`'s own empty case prints, not
        // the unrelated "no lanes to prune" this used to say.
        println!("no worktrees under {}", project_name(worktrees));
    }

    let (mut removed, mut reclaimed, mut archived, mut kept) = (0u32, 0u32, 0u32, 0u32);
    for outcome in &outcomes {
        let task = outcome_task(outcome);
        if let Some(summary) = summaries.get(task) {
            println!("  {task}  {summary}");
        }
        // Matches Python's own `rescue(lane)` report: one line per artifact
        // actually copied, printed right after the lane's summary line and
        // before whatever `--prune` decided to do with the lane itself.
        if let PruneOutcome::Removed { rescued, .. } | PruneOutcome::Archived { rescued, .. } =
            outcome
        {
            for path in rescued {
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let parent = path.parent().unwrap_or(path).display();
                println!("    rescued {name} -> {parent}");
            }
        }
        println!("    {}", action_line(outcome));
        for note in reap_notes.get(task).into_iter().flatten() {
            println!("{note}");
        }
        if let PruneOutcome::Kept {
            target_freed: Some(bytes),
            ..
        } = outcome
        {
            let verb = if preview_caches {
                "would free"
            } else {
                "freed"
            };
            println!("    {verb} target/ ({bytes} bytes)");
        }
        if let PruneOutcome::Kept {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::WouldRemove {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::WouldArchive {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::Removed {
            build_dir_freed: Some(bytes),
            ..
        }
        | PruneOutcome::Archived {
            build_dir_freed: Some(bytes),
            ..
        } = outcome
        {
            let verb = if preview_caches {
                "would free"
            } else {
                "freed"
            };
            println!("    {verb} build-dir ({bytes} bytes)");
        }
        // Card `20261116-b-891676`: on 0.1.9 a lane's build-dir that could
        // not be found or proven safe to delete stayed without a word. Card
        // `20261119-b-087c6c`: only a hash dir this lane actually resolved
        // is "kept" — the sweep below passes that one over. A miss names
        // no hash dir, so the sweep may still free one it proves orphaned
        // on its own, and that is not a dir this line kept.
        if let PruneOutcome::Kept {
            build_dir_kept: Some(reason),
            build_dir,
            ..
        }
        | PruneOutcome::WouldRemove {
            build_dir_kept: Some(reason),
            build_dir,
            ..
        }
        | PruneOutcome::WouldArchive {
            build_dir_kept: Some(reason),
            build_dir,
            ..
        }
        | PruneOutcome::Removed {
            build_dir_kept: Some(reason),
            build_dir,
            ..
        }
        | PruneOutcome::Archived {
            build_dir_kept: Some(reason),
            build_dir,
            ..
        } = outcome
        {
            if build_dir.is_some() {
                println!("    kept build-dir — {reason}");
            } else {
                println!("    build-dir unresolved — {reason}");
            }
        }
        match outcome {
            PruneOutcome::Kept { .. } => kept += 1,
            PruneOutcome::WouldRemove { branch_refusal, .. }
            | PruneOutcome::Removed { branch_refusal, .. } => {
                removed += 1;
                if let Some(reason) = branch_refusal {
                    println!("    kept branch — {reason}");
                } else {
                    reclaimed += 1;
                    println!("    reclaimed branch multiverse/{task}");
                }
            }
            PruneOutcome::WouldArchive { .. } | PruneOutcome::Archived { .. } => archived += 1,
        }
    }
    let orphans = worktrees.prune_orphans(dry_run).map_err(fail)?;
    let orphaned = orphans.len();
    for outcome in &orphans {
        match outcome {
            BranchOutcome::Kept { task, reason } => {
                kept += 1;
                println!("  kept multiverse/{task} — {reason}");
            }
            BranchOutcome::Reclaimed { task } => {
                reclaimed += 1;
                println!("  reclaimed multiverse/{task}");
            }
            BranchOutcome::Archived { task, .. } => {
                archived += 1;
                println!("  archived multiverse/{task} -> refs/multiverse/archive/{task}");
            }
        }
    }
    // Card `20261115-b-673528`: every shared-build hash dir whose own
    // evidence names only vanished workspace roots, independent of which
    // lanes this project's own `lanes()` still lists — reported the same
    // way a lane's own `build_dir_freed` already is. Card
    // `20261119-b-c3f9f5`: never a hash dir a lane above already reported
    // (freed, would free, or kept) — plain `--prune` removes a lane but
    // only measures its build-dir, which then reads as orphaned here.
    let reported_build_dirs: Vec<PathBuf> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            PruneOutcome::Kept { build_dir, .. }
            | PruneOutcome::WouldRemove { build_dir, .. }
            | PruneOutcome::WouldArchive { build_dir, .. }
            | PruneOutcome::Removed { build_dir, .. }
            | PruneOutcome::Archived { build_dir, .. } => build_dir.clone(),
        })
        .collect();
    let orphaned_build_dirs = worktrees
        .prune_orphaned_build_dirs_skipping(preview_caches, &reported_build_dirs)
        .map_err(fail)?;
    for line in orphaned_build_dir_lines(&orphaned_build_dirs, preview_caches) {
        println!("{line}");
    }
    let lanes = outcomes.len();

    println!();
    println!("removed {removed} of {lanes}");
    // Python's real (non-dry-run) path never prints this combined
    // breakdown, or the blank line ahead of it — only `removed N of M`
    // and, when there were orphans, `reclaimed N of M`. The blank line
    // belongs to `--dry-run`'s own summary string (`cli.py`'s leading
    // `f"\ndry run: ..."`), not a separator printed unconditionally; an
    // unconditional one here left a trailing blank line real-run output
    // never has. Found via `conformance/diff_fuzz.py` seed 50001.
    if dry_run {
        let breakdown = format!(
            "{removed} removed, {reclaimed} reclaimed, {archived} archived, {kept} kept \
             ({lanes} lane(s), {orphaned} orphaned branch(es))"
        );
        println!();
        println!("dry run: {breakdown} — nothing was changed");
    }
    if args.get_flag("deep") {
        worktrees
            .deep_prune(&found, dry_run, &mut std::io::stdout().lock())
            .map_err(fail)?;
        print_quarantine_outcomes(worktrees, preview_caches)?;
    }
    Ok(())
}

/// `QuarantineReason`'s own report text, printed next to each
/// `.multiverse/worktree-state/` copy `--deep` removes or would remove.
fn quarantine_reason_text(reason: QuarantineReason) -> &'static str {
    match reason {
        QuarantineReason::Redundant => "already rescued; nothing here isn't saved elsewhere",
        QuarantineReason::RetentionExpired => "past the quarantine retention window",
    }
}

/// `--deep`'s own report for `Worktrees::prune_quarantine` (card
/// `20261106-b-3d2774`), in the same verbose "deep ..." per-path style
/// `deep_prune` already uses for its own accumulated housekeeping — never
/// part of a plain `--prune`, matching that convention, and so never
/// exercised by `conformance/diff_fuzz.py`'s own `worktrees` fuzz action,
/// which only ever drives plain `--prune`.
fn print_quarantine_outcomes(worktrees: &Worktrees, dry_run: bool) -> Result<(), Failure> {
    let outcomes = worktrees.prune_quarantine(dry_run).map_err(fail)?;
    let mut bytes = 0u64;
    let mut removed = 0u64;
    for outcome in &outcomes {
        match outcome {
            QuarantineOutcome::Removed {
                name,
                reason,
                bytes: size,
            } => {
                println!(
                    "  deep removed .multiverse/worktree-state/{name} ({size} bytes) — {}",
                    quarantine_reason_text(*reason)
                );
                bytes += size;
                removed += 1;
            }
            QuarantineOutcome::WouldRemove {
                name,
                reason,
                bytes: size,
            } => {
                println!(
                    "  deep would remove .multiverse/worktree-state/{name} ({size} bytes) — {}",
                    quarantine_reason_text(*reason)
                );
                bytes += size;
                removed += 1;
            }
            QuarantineOutcome::Kept { name, bytes } => {
                println!(
                    "  deep kept .multiverse/worktree-state/{name} ({bytes} bytes) — \
                     within quarantine retention"
                );
            }
        }
    }
    let action = if dry_run { "would free" } else { "freed" };
    let suffix = if dry_run {
        " — nothing was changed"
    } else {
        ""
    };
    println!("\nquarantine cleanup: {action} {bytes} bytes in {removed} path(s){suffix}");
    Ok(())
}

/// `--merge <task>`: the one enforced choke point card `20261114-b-069836`
/// (option (a)) asks for — scans `task`'s own commits for secrets before
/// landing them with `git merge --no-ff --no-edit`, so a lane cannot reach
/// this checkout's `HEAD` the way a raw `git merge` always could. Reads
/// `[secrets_scan]` from the resolved fleet config; every other `worktrees`
/// mode needs no config at all, so this is the one branch that loads one.
///
/// [`Worktrees::merge`] itself now also refuses outright (same exit 1 as
/// every other refusal) when this checkout is already mid-merge —
/// `MERGE_HEAD` present or unmerged paths left over from an earlier
/// crashed or conflicted merge — naming the state and a `git` command to
/// inspect it, and cleanly `git merge --abort`s a lane merge that itself
/// conflicts rather than leaving the checkout mid-merge for the next
/// caller to trip over.
///
/// Also reads the project's own `.multiverse/roster.toml` for
/// `[lane_setup] regenerable` — gitignored `lane_setup` outputs the merge
/// gate treats like `target/` (a malformed roster is a config error, exit
/// 3) — and prints the branch that actually landed, which for an adopted
/// (`dispatch --into`) task id is the adopted lane's own.
fn merge(worktrees: &Worktrees, args: &ArgMatches, task: &str) -> Result<(), Failure> {
    let (config_path, _source) =
        resolve_config_path(args.get_one::<String>("config").map(String::as_str))?;
    let config = mv_config::load::load_config(&config_path)
        .map_err(|error| Failure::Config(error.to_string()))?;

    let regenerable = mv_config::roster::lane_setup_regenerable(worktrees.project())
        .map_err(|error| Failure::Config(error.to_string()))?;

    // Card `20261123-b-ccc2cc`: a refusal for a live process cwd is
    // checked for known-benign leftovers, ended, and retried once.
    let mut leftovers = HostMergeLeftovers {
        worktrees,
        task,
        lane: None,
        pane_gone: None,
    };
    let (notes, outcome) =
        merge_through_leftovers(worktrees.project(), &mut leftovers, &mut || {
            worktrees.merge(task, &config.secrets_scan, &regenerable)
        });
    for note in notes {
        println!("{note}");
    }

    match outcome {
        Ok(result) => {
            for warning in &result.warnings {
                println!("{warning}");
            }
            println!("merged {} at {}", result.branch, result.sha);
            Ok(())
        }
        Err(MergeError::ScannerMissing(message)) => Err(Failure::Config(message)),
        Err(MergeError::Refused(message)) => Err(Failure::Operation(message)),
    }
}

/// `--rescue` takes no task argument, so it sweeps every discovered lane
/// — the bulk-maintenance reading, not a single-lane operation (the only
/// one consistent with `worktrees`'s own argument list, which has no
/// `--task` alongside `--rescue`). `dry_run` previews what each lane's
/// rescue would copy without copying anything, including caches it would
/// skip as regenerable.
fn rescue(worktrees: &Worktrees, dry_run: bool) -> Result<(), Failure> {
    let lanes = worktrees.lanes().map_err(fail)?;
    if lanes.is_empty() {
        println!("no lanes to rescue");
        return Ok(());
    }
    let verb = if dry_run { "would rescue" } else { "rescued" };
    let mut failures = 0;
    for lane in &lanes {
        match worktrees.rescue(&lane.task, dry_run) {
            Ok(report) => {
                for line in rescue_item_lines(&report, dry_run) {
                    println!("{line}");
                }
                println!("{verb} {}", lane.task);
            }
            Err(e) => {
                failures += 1;
                println!("rescue failed for {}: {e}", lane.task);
            }
        }
    }
    if dry_run {
        println!();
        println!("dry run: nothing was changed");
    }
    if failures == 0 {
        Ok(())
    } else {
        Err(Failure::Operation(format!(
            "{failures} of {} lane rescues failed",
            lanes.len()
        )))
    }
}

/// One line per skipped regenerable cache and one per copied artifact —
/// pulled out of [`rescue`] so the `skipped (regenerable)` wording is
/// unit-testable without capturing stdout (card `20261122-b-4961e4`).
fn rescue_item_lines(report: &RescueReport, dry_run: bool) -> Vec<String> {
    let verb = if dry_run { "would rescue" } else { "rescued" };
    let mut lines = Vec::new();
    for path in &report.skipped {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        lines.push(format!("  skipped (regenerable) {name}"));
    }
    for path in &report.copied {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let parent = path.parent().unwrap_or(path).display();
        lines.push(format!("  {verb} {name} -> {parent}"));
    }
    lines
}

fn inspect(worktrees: &Worktrees, path: &str) -> Result<(), Failure> {
    let lanes = worktrees.lanes().map_err(fail)?;
    let mut printed_any = false;
    for lane in &lanes {
        println!("{}\t{}\t{}", lane.task, lane.branch, lane.path.display());
        printed_any = true;
    }
    for (task, sha) in worktrees.archived().map_err(fail)? {
        let short = &sha[..sha.len().min(12)];
        println!("archived {task} {short} (restore: multiverse worktrees {path} --restore {task})");
        printed_any = true;
    }
    if !printed_any {
        println!("no worktrees under {}", project_name(worktrees));
    }
    Ok(())
}

/// The last path component of the (canonicalized) project root, matching
/// Python's `project.name` (a `pathlib.Path.name`) for the "no worktrees
/// under {project.name}" message (card `20261102-b-dd1507`). Falls back
/// to the full path for the degenerate case `Path.name` also returns
/// empty for (root, or a path ending in `..`).
fn project_name(worktrees: &Worktrees) -> String {
    let project = worktrees.project();
    match project.file_name() {
        Some(name) => name.to_string_lossy().into_owned(),
        None => project.display().to_string(),
    }
}

/// Process-global env lock shared by every `mv-cli` unit test that must
/// still mutate process env (card `20261102-b-f781e6`, extended so
/// `HERDR_BIN_PATH` / `MULTIVERSE_PMSET_BIN` / `MULTIVERSE_CODEX_BIN`
/// writers in this binary serialize together). Integration tests under
/// `tests/` pass the fake path per spawned `Command::env` and never touch
/// process env. One crate-wide lock: the previous per-module mutexes raced
/// under `cargo test`'s default thread pool, because
/// `accounts`/`dispatch`/`review_policy`/`worktrees`/`sweep` unit tests
/// share one binary. These tests call library code (`mv_herdr::run::run`,
/// `HerdrLaneObserver`, `live_open_count`, `pmset_binary`, `codex_binary`)
/// that reads the process environment, so `Command::env` on a test-owned
/// spawn cannot reach the child.
#[cfg(test)]
pub(crate) mod herdr_env {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    static LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the crate-wide `HERDR_BIN_PATH` env lock directly, for a
    /// test that needs to hold it without going through [`HerdrEnvGuard`].
    pub(crate) fn lock() -> MutexGuard<'static, ()> {
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Holds the crate-wide env lock for its lifetime and restores
    /// `HERDR_BIN_PATH` to whatever it was when dropped. See the module
    /// doc above for why this is one lock shared by every test module.
    pub(crate) struct HerdrEnvGuard {
        previous: Option<String>,
        _lock: MutexGuard<'static, ()>,
    }

    impl HerdrEnvGuard {
        /// Points `HERDR_BIN_PATH` at `path` for the guard's lifetime.
        pub(crate) fn set(path: &Path) -> Self {
            let held = lock();
            let previous = std::env::var("HERDR_BIN_PATH").ok();
            std::env::set_var("HERDR_BIN_PATH", path);
            Self {
                previous,
                _lock: held,
            }
        }

        /// A guaranteed-nonexistent binary: `mv_herdr::run::run` fails
        /// with a missing-binary error on the first spawn attempt, never
        /// reaching any real executable.
        pub(crate) fn unreachable() -> Self {
            Self::set(Path::new("/nonexistent/mv-cli-test-herdr-binary"))
        }
    }

    impl Drop for HerdrEnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("HERDR_BIN_PATH", value),
                None => std::env::remove_var("HERDR_BIN_PATH"),
            }
        }
    }
}

/// A test-spawned stand-in for a lane's lingering process, shared by the
/// `worktrees` and `tasks` reap tests. Card `20261123-b-491c58`: a
/// `sleep 60` exited on its own while a loaded host's `--prune` ran past
/// a minute, and `try_wait().is_none()` read that normal exit as the
/// prune having signalled it. An hour outlives any prune, and the checks
/// below tell a signal from a timer, so load can no longer fake either.
#[cfg(test)]
pub(crate) mod lane_child {
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// Owns the child: dropping it — a failed assertion included — kills
    /// and reaps it, so a test can never leak an hour-long `sleep`.
    pub(crate) struct LaneChild(Child);

    impl LaneChild {
        /// `sleep 3600` with its cwd in `dir`.
        pub(crate) fn sleep_in(dir: &Path) -> Self {
            Self::spawn(Command::new("sleep").arg("3600").current_dir(dir))
        }

        /// Spawns an already-configured `command` under the guard.
        pub(crate) fn spawn(command: &mut Command) -> Self {
            Self(command.spawn().unwrap())
        }

        pub(crate) fn id(&self) -> u32 {
            self.0.id()
        }

        /// Nothing has signalled the child: it is still running, or it
        /// exited on its own. Only a signal fails this, never a timer
        /// that a slow host let run out.
        pub(crate) fn not_signalled(&mut self) -> bool {
            self.0
                .try_wait()
                .unwrap()
                .is_none_or(|status| status.signal().is_none())
        }

        /// The signal that ended the child within `secs`, or `None` when
        /// it is still running by then or exited on its own.
        pub(crate) fn signalled_within(&mut self, secs: u64) -> Option<i32> {
            let deadline = Instant::now() + Duration::from_secs(secs);
            loop {
                if let Some(status) = self.0.try_wait().unwrap() {
                    return status.signal();
                }
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    impl Drop for LaneChild {
        fn drop(&mut self) {
            // A resident shell's own `sleep` would otherwise outlive it by
            // the hour. Only while the child is unreaped, so its pid
            // cannot yet belong to anyone else.
            if matches!(self.0.try_wait(), Ok(None)) {
                let _ = Command::new("pkill")
                    .args(["-KILL", "-P", &self.0.id().to_string()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::herdr_env::HerdrEnvGuard;
    use super::lane_child::LaneChild;
    use super::*;
    use mv_state::worktrees::{Lane, LaneObserver};
    use std::process::Command;

    /// Nanoseconds alone are not enough: this crate's test binary runs
    /// every `#[test]` fn as a thread of one process, and under heavy
    /// parallel load two threads have landed on the same tick before —
    /// observed as a flake in `herdr_lane_observer_keeps_an_unsettled_lane_with_a_live_pane_even_with_force`
    /// (card `20261102-b-9235c6`), where a path collision let one test's
    /// fake-herdr script or project dir get clobbered by another's
    /// concurrently-running write. An atomic counter alongside the
    /// timestamp — the same pairing `mv_state::worktrees`'s private
    /// `nonce()` already uses — makes every call unique regardless of
    /// clock resolution.
    fn unique_suffix() -> String {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            "{nanos}-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        )
    }

    /// A throwaway `#!/bin/sh` script replying to `agent list` with one
    /// agent occupying `pane` at `agent_status`. The literal values are
    /// test-controlled, never interpolated from untrusted input, so
    /// building the JSON by hand (rather than via `serde_json`) is safe.
    fn fake_herdr_agent_list(pane: &str, agent_status: &str) -> TempFile {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "mv-cli-fake-herdr-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s' '{{\"result\": {{\"agents\": [{{\"pane_id\": \"{pane}\", \"agent_status\": \"{agent_status}\", \"name\": \"fixture\"}}]}}}}'\n"
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        TempFile(path)
    }

    /// A throwaway file under `TMPDIR`, removed on drop including on
    /// panic (card `20261115-b-c8065f`: the plain `PathBuf` this and
    /// `fake_scanner` returned only cleaned up on each test's own
    /// trailing `remove_file` call). `Deref`s to `Path` so every existing
    /// call site keeps reading `&script`/`script.to_string_lossy()`
    /// unchanged.
    struct TempFile(std::path::PathBuf);
    impl std::ops::Deref for TempFile {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempFile {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A throwaway project directory under `TMPDIR`, removed on drop
    /// including on panic (card `20261115-b-c8065f`: the plain `PathBuf`
    /// the `herdr_lane_observer_*` tests built here only cleaned up on
    /// their own trailing `remove_dir_all` call). `Deref`s to `Path` so
    /// every existing call site keeps reading `&project`/
    /// `project.join(...)` unchanged.
    struct TempDir(std::path::PathBuf);
    impl std::ops::Deref for TempDir {
        type Target = std::path::Path;
        fn deref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl AsRef<std::path::Path> for TempDir {
        fn as_ref(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct TempRepo(std::path::PathBuf);

    impl TempRepo {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "mv-cli-worktrees-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let git = |args: &[&str]| {
                let status = Command::new("git")
                    .current_dir(&dir)
                    .args(args)
                    .status()
                    .unwrap();
                assert!(status.success(), "git {args:?} failed");
            };
            git(&["init", "-q", "-b", "main"]);
            git(&["config", "user.email", "test@example.com"]);
            git(&["config", "user.name", "test"]);
            std::fs::write(dir.join("README.md"), "hi\n").unwrap();
            git(&["add", "README.md"]);
            git(&["commit", "-q", "-m", "init"]);
            Self(dir)
        }
    }

    impl Drop for TempRepo {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Card `20261122-b-a5e62d`: on 0.1.16 `--prune --yes` printed a line
    /// for only the first of two orphan shared-build dirs its dry run had
    /// listed. Every dir the sweep returns gets its own line — both freed
    /// ones, and one its scan budget ran out on, kept with the reason.
    #[test]
    fn orphaned_build_dir_lines_print_every_swept_dir_freed_or_kept_with_its_reason() {
        let first = OrphanedBuildDir {
            path: PathBuf::from("/shared-build/29/5a434937c16426"),
            bytes: 3_681_883_433,
            kept: None,
        };
        let second = OrphanedBuildDir {
            path: PathBuf::from("/shared-build/e7/bb1edf3e1ddaa3"),
            bytes: 2_050_041_399,
            kept: None,
        };
        let both = [first.clone(), second.clone()];
        assert_eq!(
            orphaned_build_dir_lines(&both, false),
            [
                "  cargo shared-build /shared-build/29/5a434937c16426",
                "    freed build-dir (3681883433 bytes)",
                "  cargo shared-build /shared-build/e7/bb1edf3e1ddaa3",
                "    freed build-dir (2050041399 bytes)",
            ]
        );
        assert_eq!(
            orphaned_build_dir_lines(&both, true)[3],
            "    would free build-dir (2050041399 bytes)"
        );

        let out_of_time = OrphanedBuildDir {
            bytes: 0,
            kept: Some("scan budget ran out before its build evidence was read in full".into()),
            ..second
        };
        assert_eq!(
            orphaned_build_dir_lines(&[first, out_of_time], false)[2..],
            [
                "  cargo shared-build /shared-build/e7/bb1edf3e1ddaa3",
                "    kept build-dir — scan budget ran out before its build evidence was read in full",
            ]
        );
    }

    #[test]
    fn inspect_lists_a_created_lane_and_archived_is_empty() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(inspect(&worktrees, repo.0.to_str().unwrap()).is_ok());
    }

    /// Card 20261102-b-dd1507: a project with zero lanes (none created,
    /// none archived, no orphan branches) used to fall straight through
    /// `inspect`'s three loops to `Ok(())` with no output at all. Python's
    /// `cmd_worktrees` always prints a "no worktrees under {project.name}"
    /// line for this case — nothing here captures stdout to check the
    /// exact text, but this at least pins that the empty path is reached
    /// and doesn't panic. `--deep` is validated before reaching this listing.
    #[test]
    fn inspect_on_a_project_with_no_lanes_does_not_panic() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        assert!(inspect(&worktrees, repo.0.to_str().unwrap()).is_ok());
    }

    #[test]
    fn project_name_is_the_last_path_component() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let expected = repo
            .0
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert_eq!(project_name(&worktrees), expected);
    }

    #[test]
    fn prune_keeps_a_lane_with_no_recorded_pane_on_missing_status_alone() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let _herdr = HerdrEnvGuard::unreachable();
        let mut observer = HerdrLaneObserver::new(&repo.0);
        // No routing.log at all: HerdrLaneObserver finds nothing to
        // confirm live and steps aside; `terminal_statuses` is what
        // actually keeps this lane.
        let outcomes = worktrees.prune(&mut observer, false, true).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(
            matches!(&outcomes[0], PruneOutcome::Kept { reason, .. } if reason.contains("missing terminal"))
        );
    }

    #[test]
    fn rescue_sweeps_every_lane() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        assert!(rescue(&worktrees, false).is_ok());
    }

    /// Card `20261110-b-d3ba4f` defect 1: `worktrees <proj> --rescue
    /// --dry-run` used to perform the real rescue, because `run`'s dispatch
    /// never read the `--dry-run` flag for the `--rescue` branch at all.
    /// Drives the full CLI parse-and-dispatch path (`crate::args::command()`
    /// through `run`) rather than calling the internal `rescue` fn directly,
    /// so a regression in that wiring — not just in `Worktrees::rescue`
    /// itself — would be caught here.
    #[test]
    fn cli_rescue_dry_run_does_not_copy_task_artifacts() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let lane = worktrees.create("20261101-fixer-abcdef", "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks/20261101-fixer-abcdef");
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(task_dir.join("result.md"), "findings\n").unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "worktrees", path, "--rescue", "--dry-run"])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert!(run(sub).is_ok());

        assert!(
            !repo
                .0
                .join(".multiverse/tasks/20261101-fixer-abcdef/result.md")
                .exists(),
            "--rescue --dry-run must not actually copy anything into the project"
        );
    }

    /// Card `20261122-b-4961e4`: `--rescue` copies real task artifacts and
    /// skips a task-dir venv rather than refusing the lane on its python
    /// symlink. The printed `skipped (regenerable)` line is pinned on
    /// [`rescue_item_lines`].
    #[test]
    fn cli_rescue_skips_a_task_dir_venv_and_copies_artifacts() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-builder-4961e4";
        let lane = worktrees.create(task, "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks").join(task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        let venv_bin = task_dir.join("venv/bin");
        std::fs::create_dir_all(&venv_bin).unwrap();
        std::os::unix::fs::symlink("python3.15", venv_bin.join("python")).unwrap();
        let egg = task_dir.join("pkg/foo.egg-info");
        std::fs::create_dir_all(&egg).unwrap();
        std::fs::write(egg.join("PKG-INFO"), "Name: foo\n").unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from(["multiverse", "worktrees", path, "--rescue", "--yes"])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert!(run(sub).is_ok());

        let rescued = repo.0.join(".multiverse/tasks").join(task);
        assert_eq!(
            std::fs::read_to_string(rescued.join("result.md")).unwrap(),
            "findings\n"
        );
        assert!(
            !rescued.join("venv").exists(),
            "CLI rescue must not copy the venv"
        );
        assert!(
            !rescued.join("pkg/foo.egg-info").exists(),
            "CLI rescue must not copy egg-info"
        );

        let _herdr = HerdrEnvGuard::unreachable();
        let mut observer = HerdrLaneObserver::new(&repo.0);
        let outcomes = worktrees.prune(&mut observer, true, true).unwrap();
        assert!(
            matches!(&outcomes[0], PruneOutcome::WouldRemove { .. }),
            "expected prune --dry-run to list the lane removable after skipping the venv, got {:?}",
            outcomes[0]
        );

        let report = worktrees.rescue(task, true).unwrap();
        let lines = rescue_item_lines(&report, true);
        assert!(
            lines.iter().any(|l| l == "  skipped (regenerable) venv"),
            "{lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l == "  skipped (regenerable) foo.egg-info"),
            "{lines:?}"
        );
    }

    #[test]
    fn rescue_item_lines_name_skipped_regenerable_caches() {
        let report = RescueReport {
            copied: vec![PathBuf::from("/proj/.multiverse/tasks/t/result.md")],
            skipped: vec![
                PathBuf::from("/lane/.multiverse/tasks/t/venv"),
                PathBuf::from("/lane/.multiverse/tasks/t/pkg/foo.egg-info"),
            ],
        };
        assert_eq!(
            rescue_item_lines(&report, false),
            [
                "  skipped (regenerable) venv",
                "  skipped (regenerable) foo.egg-info",
                "  rescued result.md -> /proj/.multiverse/tasks/t",
            ]
        );
    }

    /// A throwaway `#!/bin/sh` scanner script — these `--merge` CLI tests
    /// never touch the network or a real installed gitleaks/trufflehog.
    fn fake_scanner(body: &str) -> TempFile {
        let path = std::env::temp_dir().join(format!(
            "mv-cli-fake-scanner-{}-{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        TempFile(path)
    }

    /// A `--merge`-ready lane: created, recorded `done` with a non-empty
    /// `result.md`, and one committed change for the scanner to look at.
    fn done_lane(worktrees: &Worktrees, task: &str) -> Lane {
        let lane = worktrees.create(task, "HEAD").unwrap();
        let task_dir = lane.path.join(".multiverse/tasks").join(task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(task_dir.join("result.md"), "findings\n").unwrap();
        std::fs::write(lane.path.join("change.txt"), "lane work\n").unwrap();
        for args in [["add", "change.txt"].as_slice(), &["commit", "-m", "work"]] {
            assert!(Command::new("git")
                .current_dir(&lane.path)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        lane
    }

    /// Proof item: `--merge` refuses on a planted-secret finding (exit
    /// code 1), names the evidence file in its own error, and leaves
    /// `HEAD` exactly where it was.
    #[test]
    fn cli_merge_refuses_on_a_gitleaks_finding_and_leaves_main_unchanged() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);
        let before = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();

        let script = fake_scanner("echo 'AKIA-REDACTED'\nexit 1");
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", script.display()),
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        assert!(error.to_string().contains("evidence"), "{error}");
        assert!(
            repo.0
                .join(format!(".multiverse/evidence/{task}-secrets.txt"))
                .is_file(),
            "evidence file must be written on a finding"
        );

        let after = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(before, after, "a refused merge must leave HEAD unchanged");
    }

    /// Proof item: a clean, done lane merges and `--merge` prints the
    /// merge sha, matching what `HEAD` now points at.
    #[test]
    fn cli_merge_merges_a_clean_lane_and_prints_the_merge_sha() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);

        let script = fake_scanner("exit 0");
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", script.display()),
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        assert!(run(sub).is_ok());

        let sha = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["rev-parse", "HEAD"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        let message = String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["log", "-1", "--format=%B"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        assert_eq!(message.trim(), format!("Merge branch 'multiverse/{task}'"));
        let _ = sha;
    }

    /// Runs `worktrees <repo> --merge <task>` through the real CLI entry
    /// point against a config whose gitleaks is the fake `scanner`.
    fn cli_merge(repo: &TempRepo, task: &str, scanner: &Path) -> Result<(), Failure> {
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", scanner.display()),
        )
        .unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                repo.0.to_str().unwrap(),
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        run(sub)
    }

    fn head_message(repo: &TempRepo) -> String {
        String::from_utf8(
            Command::new("git")
                .current_dir(&repo.0)
                .args(["log", "-1", "--format=%B"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string()
    }

    /// Card 20261122-b-40cb46 proof item: `--merge <adopted id>` — a
    /// `dispatch --into` round whose own id has no worktree directory —
    /// resolves to the lane it adopted and lands that lane's branch, even
    /// though round 1's own status file still reads `blocked`.
    #[test]
    fn cli_merge_of_an_adopted_task_id_lands_the_adopted_lanes_branch() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let owner = "20261101-builder-aaaaaa";
        let adopted = "20261101-builder-bbbbbb";
        let lane = done_lane(&worktrees, owner);
        let tasks = lane.path.join(".multiverse/tasks");
        std::fs::write(tasks.join(owner).join("status"), "blocked\n").unwrap();
        std::fs::create_dir_all(tasks.join(adopted)).unwrap();
        std::fs::write(tasks.join(adopted).join("status"), "done\n").unwrap();
        std::fs::write(tasks.join(adopted).join("result.md"), "redone\n").unwrap();
        std::fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={owner}\tbranch=multiverse/{owner}\n\
                 2026-11-01T00:10:00+00:00\tevent=result\ttask={owner}\tstatus=blocked\tstate=reconciled\trevisions=0\tpane=p1\n\
                 2026-11-01T01:00:00+00:00\tevent=dispatch\ttask={adopted}\tbranch=multiverse/{owner}\n\
                 2026-11-01T01:10:00+00:00\tevent=result\ttask={adopted}\tstatus=done\tstate=reconciled\trevisions=0\tpane=p2\n"
            ),
        )
        .unwrap();

        let script = fake_scanner("exit 0");
        cli_merge(&repo, adopted, &script).unwrap();
        assert_eq!(
            head_message(&repo),
            format!("Merge branch 'multiverse/{owner}'")
        );
    }

    /// Card `20261123-b-970200` proof item: a lane abandoned, resumed by
    /// a `dispatch --into` round that never recorded an outcome, then
    /// corrected with `tasks --supersede <owner> --supersedes 2 --status
    /// done` merges through the CLI with no status file or `result.md`
    /// written by hand.
    #[test]
    fn cli_merge_honors_a_supersede_of_an_abandoned_lane() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let owner = "20261101-builder-dddddd";
        let round = "20261101-builder-eeeeee";
        let lane = done_lane(&worktrees, owner);
        std::fs::remove_dir_all(lane.path.join(".multiverse/tasks").join(owner)).unwrap();
        std::fs::write(
            repo.0.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={owner}\tbranch=multiverse/{owner}\n\
                 2026-11-01T01:10:00+00:00\tevent=result\ttask={owner}\tstatus=failed\tstate=abandoned\trevisions=0\tpane=p1\n\
                 2026-11-01T02:00:00+00:00\tevent=dispatch\ttask={round}\tbranch=multiverse/{owner}\n"
            ),
        )
        .unwrap();
        let script = fake_scanner("exit 0");

        let error = cli_merge(&repo, owner, &script).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        assert!(
            error.to_string().contains("no recorded terminal status"),
            "{error}"
        );

        mv_state::tasks::append_supersede_if_absent(
            &repo.0,
            owner,
            2,
            "done",
            "verified",
            "work checked by hand",
            0,
            None,
        )
        .unwrap();
        cli_merge(&repo, owner, &script).unwrap();
        assert_eq!(
            head_message(&repo),
            format!("Merge branch 'multiverse/{owner}'")
        );
    }

    /// Card 20261122-b-40cb46: `--merge` reads the project's own
    /// `.multiverse/roster.toml` `[lane_setup] regenerable` list, so a
    /// declared gitignored `lane_setup` output no longer refuses the merge.
    #[test]
    fn cli_merge_honors_the_projects_declared_regenerable_paths() {
        let repo = TempRepo::new();
        std::fs::write(repo.0.join(".gitignore"), "gen/\n").unwrap();
        for args in [
            ["add", ".gitignore"].as_slice(),
            &["commit", "-q", "-m", "ignore gen"],
        ] {
            assert!(Command::new("git")
                .current_dir(&repo.0)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-builder-cccccc";
        let lane = done_lane(&worktrees, task);
        std::fs::create_dir_all(lane.path.join("gen")).unwrap();
        std::fs::write(lane.path.join("gen/client.ts"), "x\n").unwrap();
        let script = fake_scanner("exit 0");

        let error = cli_merge(&repo, task, &script).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        assert!(error.to_string().contains("gen/client.ts"), "{error}");

        std::fs::write(
            repo.0.join(".multiverse/roster.toml"),
            "[lane_setup]\nregenerable = [\"gen/\"]\n",
        )
        .unwrap();
        cli_merge(&repo, task, &script).unwrap();
        assert_eq!(
            head_message(&repo),
            format!("Merge branch 'multiverse/{task}'")
        );
    }

    /// Card `20261122-b-4961e4`: `--merge` treats a gitignored
    /// `*.egg-info/` directory as regenerable (built-in list from
    /// `44d217a`) instead of refusing it as an unrescued ignored file.
    #[test]
    fn cli_merge_does_not_refuse_an_ignored_egg_info_directory() {
        let repo = TempRepo::new();
        std::fs::write(repo.0.join(".gitignore"), "*.egg-info/\n").unwrap();
        for args in [
            ["add", ".gitignore"].as_slice(),
            &["commit", "-q", "-m", "ignore egg-info"],
        ] {
            assert!(Command::new("git")
                .current_dir(&repo.0)
                .args(args)
                .status()
                .unwrap()
                .success());
        }
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-builder-4961e4";
        let lane = done_lane(&worktrees, task);
        let egg = lane.path.join("pkg/foo.egg-info");
        std::fs::create_dir_all(&egg).unwrap();
        std::fs::write(egg.join("PKG-INFO"), "Name: foo\n").unwrap();
        let script = fake_scanner("exit 0");
        cli_merge(&repo, task, &script).unwrap();
        assert_eq!(
            head_message(&repo),
            format!("Merge branch 'multiverse/{task}'")
        );
    }

    /// Proof item: a missing required scanner exits 3 (needs config) with
    /// an install hint, and merges nothing.
    #[test]
    fn cli_merge_exits_needs_config_when_gitleaks_is_missing_and_required() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);

        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            "[secrets_scan]\ngitleaks = \"/nonexistent/mv-cli-test-gitleaks\"\n",
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::NEEDS_CONFIG);
        assert!(error.to_string().contains("install"), "{error}");
    }

    /// Proof item: `--merge` refuses outright through the CLI boundary
    /// when the project checkout is already mid-merge (`MERGE_HEAD`
    /// present, left over here by a raw conflicting `git merge`,
    /// independent of `--merge`'s own code), naming the stuck state and
    /// exiting 1 like any other refusal — and leaves that state, and
    /// `HEAD`, exactly as it found them.
    #[test]
    fn cli_merge_refuses_when_trunk_is_mid_merge_and_leaves_it_unchanged() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        done_lane(&worktrees, task);

        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(&repo.0)
                .args(args)
                .status()
                .unwrap()
        };
        assert!(git(&["checkout", "-b", "other"]).success());
        std::fs::write(repo.0.join("README.md"), "other\n").unwrap();
        assert!(git(&["commit", "-am", "other change"]).success());
        assert!(git(&["checkout", "main"]).success());
        std::fs::write(repo.0.join("README.md"), "main\n").unwrap();
        assert!(git(&["commit", "-am", "main change"]).success());
        assert!(!git(&["merge", "--no-ff", "--no-edit", "other"]).success());
        assert!(
            repo.0.join(".git/MERGE_HEAD").is_file(),
            "test setup must leave a real MERGE_HEAD"
        );

        let script = fake_scanner("exit 0");
        let config_path = repo.0.join("config.toml");
        std::fs::write(
            &config_path,
            format!("[secrets_scan]\ngitleaks = \"{}\"\n", script.display()),
        )
        .unwrap();

        let path = repo.0.to_str().unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        assert!(error.to_string().contains("MERGE_HEAD"), "{error}");
        assert!(
            repo.0.join(".git/MERGE_HEAD").is_file(),
            "a refused merge must leave the pre-existing merge-in-progress state untouched"
        );

        // Clean up by hand so `TempRepo`'s own teardown doesn't trip on it.
        git(&["merge", "--abort"]);
    }

    /// One real process snapshot taken after `pid` was spawned with its cwd
    /// in `dir`, or `None` — after printing why — when this host's probe
    /// cannot see it (e.g. a sandbox that hides `lsof`/`/proc`), so a test
    /// needing real process cwd visibility skips rather than fails.
    ///
    /// Polls briefly: a snapshot taken the instant after `spawn` can still
    /// show the child under this test binary's own command line, before
    /// its `exec`, which would make every by-name check meaningless.
    fn lane_snapshot(dir: &Path, pid: u32) -> Option<Vec<mv_engine::dockerreport::ProcessCwd>> {
        let own = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = mv_engine::dockerreport::process_cwds();
            let visible = snapshot.as_deref().is_some_and(|processes| {
                mv_engine::dockerreport::processes_under(dir, processes)
                    .iter()
                    .any(|process| {
                        process.pid == pid && (own.is_empty() || !process.command.contains(&own))
                    })
            });
            if visible {
                return snapshot;
            }
            if std::time::Instant::now() >= deadline {
                eprintln!("skipped: this host's process cwd probe cannot see spawned pid {pid}");
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Proof item 1: a kept lane names the lingering pid without `--reap`,
    /// and kills nothing.
    #[test]
    fn reap_lane_notes_without_reap_names_the_pid_and_signals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = LaneChild::sleep_in(dir.path());
        let Some(snapshot) = lane_snapshot(dir.path(), child.id()) else {
            return;
        };
        let (notes, any_reaped) =
            reap_lane_notes(dir.path(), Some(&snapshot), false, false, false, &[]);
        assert!(!any_reaped);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(
            notes[0].contains(&format!("pid {}", child.id())),
            "{notes:?}"
        );
        assert!(notes[0].contains("rerun with --reap"), "{notes:?}");
        assert!(
            child.not_signalled(),
            "no --reap must never signal the process"
        );
    }

    /// Proof item 3: `--reap --dry-run` previews "would reap" and kills
    /// nothing.
    #[test]
    fn reap_lane_notes_dry_run_previews_and_signals_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = LaneChild::sleep_in(dir.path());
        let Some(snapshot) = lane_snapshot(dir.path(), child.id()) else {
            return;
        };
        let (notes, any_reaped) =
            reap_lane_notes(dir.path(), Some(&snapshot), false, true, true, &[]);
        assert!(!any_reaped);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("would reap"), "{notes:?}");
        assert!(
            child.not_signalled(),
            "--reap --dry-run must never signal the process"
        );
    }

    /// Proof item 2: `--reap` without `--dry-run` actually ends the
    /// process.
    #[test]
    fn reap_lane_notes_reaps_a_real_spawned_child() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = LaneChild::sleep_in(dir.path());
        let Some(snapshot) = lane_snapshot(dir.path(), child.id()) else {
            return;
        };
        let (notes, any_reaped) =
            reap_lane_notes(dir.path(), Some(&snapshot), false, true, false, &[]);
        assert!(any_reaped);
        assert!(notes.iter().any(|n| n.contains("reaping")), "{notes:?}");
        assert!(
            child.signalled_within(30).is_some(),
            "--reap must signal the process"
        );
    }

    /// Proof item 4: a sibling directory's process is never matched, even
    /// when it shares the lane path as a string prefix.
    #[test]
    fn reap_lane_notes_never_matches_a_sibling_prefix_directory() {
        let dir = tempfile::tempdir().unwrap();
        let lane = dir.path().join("lane");
        let sibling = dir.path().join("lane-other");
        std::fs::create_dir_all(&lane).unwrap();
        std::fs::create_dir_all(&sibling).unwrap();
        let mut child = LaneChild::sleep_in(&sibling);
        let Some(snapshot) = lane_snapshot(&sibling, child.id()) else {
            return;
        };
        let (notes, any_reaped) = reap_lane_notes(&lane, Some(&snapshot), true, true, false, &[]);
        assert!(!any_reaped);
        assert!(notes.is_empty(), "{notes:?}");
        assert!(
            child.not_signalled(),
            "a sibling directory's process must never be touched"
        );
    }

    // Decision 6's by-name denylist (shell/herdr/pid 1) is exercised
    // directly, without a live process, by `dockerreport`'s own
    // `protected_refuses_a_shell_or_herdr_by_name_even_with_a_matching_cwd`
    // (`crates/mv-engine/src/dockerreport.rs`) — `sh -c "sleep N"` execs
    // straight into `sleep` on both bash and dash (the tail-call
    // optimization), so there is no reliable way to spawn a real,
    // short-lived process whose `ps` comm is still a shell; forcing one to
    // stay resident (backgrounding and `wait`-ing) risks leaking the
    // grandchild if this test is ever interrupted before cleanup runs. The
    // ancestor-pid branch below exercises the same `protected()` call from
    // this file's own wiring, with a real spawned process.

    /// Decision 6: a candidate pid in the caller's own ancestor list is
    /// never reaped, even though its cwd and comm would otherwise qualify.
    #[test]
    fn reap_lane_notes_never_reaps_a_pid_in_the_ancestor_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = LaneChild::sleep_in(dir.path());
        let Some(snapshot) = lane_snapshot(dir.path(), child.id()) else {
            return;
        };
        let (notes, any_reaped) = reap_lane_notes(
            dir.path(),
            Some(&snapshot),
            true,
            true,
            false,
            &[child.id()],
        );
        assert!(!any_reaped);
        assert!(notes.iter().any(|n| n.contains("protected")), "{notes:?}");
        assert!(
            child.not_signalled(),
            "a pid the caller named as an ancestor must never be reaped"
        );
    }

    /// Card `20261122-b-78bcf9`: a shell left behind in a *dead* lane (the
    /// `zsh -c` the card saw) is a reap target there, while the same shell
    /// in a lane not known dead stays protected by name. `; :` keeps the
    /// shell resident instead of exec-ing straight into `sleep`.
    #[test]
    fn reap_lane_notes_reaps_a_leftover_shell_only_in_a_dead_lane() {
        let dir = tempfile::tempdir().unwrap();
        let mut child = LaneChild::spawn(
            Command::new("sh")
                .args(["-c", "sleep 3600; :"])
                .current_dir(dir.path()),
        );
        let Some(snapshot) = lane_snapshot(dir.path(), child.id()) else {
            return;
        };
        let shell = format!("pid {} ", child.id());

        // A preview: its `sleep` child shares the cwd and is no shell, so a
        // real reap here would end it, and the shell with it.
        let (notes, _) = reap_lane_notes(dir.path(), Some(&snapshot), false, true, true, &[]);
        assert!(
            notes
                .iter()
                .any(|n| n.contains(&shell) && n.contains("protected")),
            "{notes:?}"
        );
        assert!(
            child.not_signalled(),
            "a shell in a lane not known dead must never be reaped"
        );

        let (notes, any_reaped) =
            reap_lane_notes(dir.path(), Some(&snapshot), true, true, false, &[]);
        assert!(any_reaped, "{notes:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains(&format!("reaping {shell}"))),
            "{notes:?}"
        );
        assert!(
            child.signalled_within(30).is_some(),
            "a dead lane's leftover shell must be ended"
        );
    }

    /// The ssh ControlMaster parse `end_leftover` uses before its SIGTERM:
    /// OpenSSH's own `[mux]` title and an explicit master with a visible
    /// socket match; a plain client on some other master's socket, a
    /// master with no socket, and anything after the destination never do.
    #[test]
    fn ssh_mux_master_socket_matches_only_a_visible_master() {
        assert_eq!(
            ssh_mux_master_socket("ssh: /home/u/.ssh/cm-u@host:22 [mux]").as_deref(),
            Some("/home/u/.ssh/cm-u@host:22")
        );
        assert_eq!(
            ssh_mux_master_socket("/usr/bin/ssh -fNM -S /tmp/cm.sock host").as_deref(),
            Some("/tmp/cm.sock")
        );
        assert_eq!(
            ssh_mux_master_socket(
                "ssh -o ControlMaster=yes -oControlPath=/tmp/cm.sock -l u host docker"
            )
            .as_deref(),
            Some("/tmp/cm.sock")
        );
        assert_eq!(ssh_mux_master_socket("ssh -S /tmp/cm.sock host"), None);
        assert_eq!(
            ssh_mux_master_socket("ssh -o ControlMaster=auto -o ControlPath=/tmp/c host"),
            None
        );
        assert_eq!(ssh_mux_master_socket("ssh -M host"), None);
        assert_eq!(
            ssh_mux_master_socket("ssh -S /tmp/cm.sock host run -M"),
            None
        );
        assert_eq!(ssh_mux_master_socket("ssh -M -S none host"), None);
        assert_eq!(ssh_mux_master_socket("sshd: u [priv]"), None);
    }

    /// Card `20261122-b-78bcf9`, through the real CLI: a done lane whose
    /// branch is *not* merged is kept for that, not for its live cwd — the
    /// exact shape that used to print no "would reap" at all. Its pane is
    /// gone (no live Herdr), so `--prune --reap --dry-run` and a plain
    /// `--prune` leave the leftover alone, and `--prune --reap --yes` ends
    /// it while still keeping the unmerged lane for `--merge`.
    #[test]
    fn cli_prune_reap_ends_a_dead_unmerged_lane_leftover_and_keeps_the_lane() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-bcdefa";
        let lane = done_lane(&worktrees, task);
        let _herdr = HerdrEnvGuard::unreachable();

        let mut child = LaneChild::sleep_in(&lane.path);
        if lane_snapshot(&lane.path, child.id()).is_none() {
            return;
        }

        let path = repo.0.to_str().unwrap();
        for preview in [
            vec!["multiverse", "worktrees", path, "--prune"],
            vec![
                "multiverse",
                "worktrees",
                path,
                "--prune",
                "--reap",
                "--dry-run",
            ],
        ] {
            let matches = crate::args::command()
                .try_get_matches_from(&preview)
                .unwrap();
            let (_, sub) = matches.subcommand().unwrap();
            assert!(run(sub).is_ok());
            assert!(
                child.not_signalled(),
                "{preview:?} must never signal the process"
            );
        }

        let reaping = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--prune",
                "--reap",
                "--yes",
            ])
            .unwrap();
        let (_, sub) = reaping.subcommand().unwrap();
        assert!(run(sub).is_ok());
        assert!(
            child.signalled_within(30).is_some(),
            "--prune --reap --yes must end a dead lane's leftover"
        );
        assert!(
            lane.path.exists(),
            "an unmerged lane is still kept after its leftover is reaped"
        );
    }

    /// Card `20261122-b-78bcf9`, through the real CLI: `--merge`'s
    /// live-cwd refusal names the holder's pid and the reap verb.
    #[test]
    fn cli_merge_refusal_names_the_live_cwd_holder_pid() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-cdefab";
        let lane = done_lane(&worktrees, task);
        let child = LaneChild::sleep_in(&lane.path);
        if lane_snapshot(&lane.path, child.id()).is_none() {
            return;
        }
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[secrets_scan]\ngitleaks = \"true\"\nrequired = false\n",
        )
        .unwrap();
        let matches = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                repo.0.to_str().unwrap(),
                "--merge",
                task,
                "--config",
                config_path.to_str().unwrap(),
            ])
            .unwrap();
        let (_, sub) = matches.subcommand().unwrap();
        let error = run(sub).unwrap_err();
        assert_eq!(error.code(), crate::exit::ERROR);
        let message = error.to_string();
        assert!(
            message.contains(&format!("pid {}", child.id())),
            "{message}"
        );
        assert!(message.contains("--prune --reap --yes"), "{message}");
    }

    /// Full `--prune --reap --yes` CLI path: a done, pane-clear lane with a
    /// spawned `sleep` is reported as kept on a plain `--prune`, then
    /// actually pruned (branch handled, lane directory gone) once `--reap`
    /// ends that process in the same invocation — proof items 1 and 2
    /// together, through the real CLI dispatch rather than the pure helper
    /// above.
    #[test]
    fn cli_prune_reap_ends_the_process_and_the_lane_is_no_longer_kept_for_it() {
        let repo = TempRepo::new();
        let worktrees = Worktrees::new(&repo.0).unwrap();
        let task = "20261101-fixer-abcdef";
        let lane = done_lane(&worktrees, task);
        // `prune_with_ignored`'s own archiving check runs ahead of
        // `clean`'s own `has_process_cwd` refusal — an unmerged branch
        // would keep this lane for "not merged into HEAD" instead, never
        // reaching the live-process check this test is about. Merging the
        // lane's branch first makes the live process cwd the only
        // remaining reason to keep it.
        assert!(Command::new("git")
            .current_dir(&repo.0)
            .args(["merge", "--ff-only", &format!("multiverse/{task}")])
            .status()
            .unwrap()
            .success());
        let _herdr = HerdrEnvGuard::unreachable();

        let mut child = LaneChild::sleep_in(&lane.path);

        let path = repo.0.to_str().unwrap();
        let plain = crate::args::command()
            .try_get_matches_from(["multiverse", "worktrees", path, "--prune"])
            .unwrap();
        let (_, sub) = plain.subcommand().unwrap();
        assert!(run(sub).is_ok());
        assert!(
            lane.path.exists(),
            "a plain --prune must keep a lane with a live process cwd"
        );
        assert!(
            child.not_signalled(),
            "a plain --prune must never signal the process"
        );

        let reaping = crate::args::command()
            .try_get_matches_from([
                "multiverse",
                "worktrees",
                path,
                "--prune",
                "--reap",
                "--yes",
            ])
            .unwrap();
        let (_, sub) = reaping.subcommand().unwrap();
        assert!(run(sub).is_ok());

        assert!(
            child.signalled_within(30).is_some(),
            "--prune --reap must signal the process"
        );
        assert!(
            !lane.path.exists(),
            "--prune --reap must remove the lane once its process is gone"
        );
    }

    #[test]
    fn herdr_lane_observer_keeps_an_unsettled_lane_with_a_live_pane_even_with_force() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        std::fs::create_dir_all(project.join(".multiverse")).unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        let script = fake_herdr_agent_list("w1:p1", "working");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        for force in [false, true] {
            let error = observer.check(&lane, force).unwrap_err().to_string();
            assert!(
                error.contains("has no terminal status and still has a live agent pane"),
                "{error}"
            );
        }
    }

    #[test]
    fn herdr_lane_observer_keeps_a_settled_lane_whose_pane_is_still_live() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        let task_dir = project.join(".multiverse/tasks").join(&lane.task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        let script = fake_herdr_agent_list("w1:p1", "idle");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        let error = observer.check(&lane, false).unwrap_err().to_string();
        assert_eq!(
            error,
            format!(
                "{} still has a live agent pane (idle); rerun with --force to remove it anyway.",
                lane.task
            )
        );
    }

    #[test]
    fn herdr_lane_observer_allows_a_settled_lane_once_its_pane_is_gone() {
        let lane = Lane {
            task: "20261101-fixer-abcdef".into(),
            branch: "multiverse/20261101-fixer-abcdef".into(),
            path: std::path::PathBuf::from("/tmp/mv-cli-test-lane-does-not-need-to-exist"),
        };
        let project = TempDir(std::env::temp_dir().join(format!(
            "mv-cli-herdr-observer-test-{}-{}",
            std::process::id(),
            unique_suffix()
        )));
        let task_dir = project.join(".multiverse/tasks").join(&lane.task);
        std::fs::create_dir_all(&task_dir).unwrap();
        std::fs::write(task_dir.join("status"), "done\n").unwrap();
        std::fs::write(
            project.join(".multiverse/routing.log"),
            format!(
                "2026-11-01T00:00:00+00:00\tevent=dispatch\ttask={0}\tbranch={1}\n\
                 2026-11-01T00:05:00+00:00\tevent=started\ttask={0}\tpane=w1:p1\n",
                lane.task, lane.branch
            ),
        )
        .unwrap();

        // Herdr's own list names a different pane entirely: this lane's
        // pane has closed.
        let script = fake_herdr_agent_list("w2:p9", "idle");
        let mut observer =
            HerdrLaneObserver::with_herdr_binary(&*project, script.to_string_lossy());
        assert!(observer.check(&lane, false).is_ok());
    }

    /// Card `20261123-b-ccc2cc`: an injected process table and pane
    /// answer for [`merge_through_leftovers`] — `snapshots` is read in
    /// order (the last one repeating), and every [`MergeLeftovers::end`]
    /// only records the pid it was asked to end. Nothing here reads the
    /// real host's `lsof`/`ps` or asks a real Herdr.
    struct FakeLeftovers {
        lane: PathBuf,
        snapshots: Vec<Vec<mv_engine::dockerreport::ProcessCwd>>,
        parents: HashMap<u32, mv_engine::dockerreport::ProcessParent>,
        ancestors: Vec<u32>,
        pane_gone: bool,
        pane_asked: u32,
        ended: Vec<u32>,
    }

    impl FakeLeftovers {
        /// `rows` are `(pid, comm, ppid, args)`, all with their cwd in `lane`.
        fn new(lane: &Path, rows: &[(u32, &str, u32, &str)]) -> Self {
            let mut fake = Self {
                lane: lane.to_path_buf(),
                snapshots: Vec::new(),
                parents: HashMap::new(),
                ancestors: Vec::new(),
                pane_gone: false,
                pane_asked: 0,
                ended: Vec::new(),
            };
            let first = fake.rows(rows);
            fake.snapshots.push(first);
            fake
        }

        fn rows(
            &mut self,
            rows: &[(u32, &str, u32, &str)],
        ) -> Vec<mv_engine::dockerreport::ProcessCwd> {
            rows.iter()
                .map(|(pid, comm, ppid, args)| {
                    self.parents.insert(
                        *pid,
                        mv_engine::dockerreport::ProcessParent {
                            ppid: *ppid,
                            args: args.to_string(),
                        },
                    );
                    mv_engine::dockerreport::ProcessCwd {
                        pid: *pid,
                        cwd: self.lane.clone(),
                        command: comm.to_string(),
                        cpu_percent: "0.0".into(),
                    }
                })
                .collect()
        }
    }

    impl MergeLeftovers for FakeLeftovers {
        fn lane_path(&mut self) -> Option<PathBuf> {
            Some(self.lane.clone())
        }
        fn snapshot(&mut self) -> Option<Vec<mv_engine::dockerreport::ProcessCwd>> {
            let next = if self.snapshots.len() > 1 {
                self.snapshots.remove(0)
            } else {
                self.snapshots.first().cloned().unwrap_or_default()
            };
            Some(next)
        }
        fn parent(&mut self, pid: u32) -> Option<mv_engine::dockerreport::ProcessParent> {
            self.parents.get(&pid).cloned()
        }
        fn ancestors(&mut self) -> Vec<u32> {
            self.ancestors.clone()
        }
        fn pane_gone(&mut self) -> bool {
            self.pane_asked += 1;
            self.pane_gone
        }
        fn end(
            &mut self,
            process: &mv_engine::dockerreport::ProcessCwd,
        ) -> mv_engine::dockerreport::ReapOutcome {
            self.ended.push(process.pid);
            mv_engine::dockerreport::ReapOutcome::Reaped
        }
    }

    /// The real refusal `mv_state`'s `merge` returns for a held lane.
    fn live_cwd_refusal() -> MergeError {
        MergeError::Refused(format!("{LIVE_CWD_REFUSAL}: pid 1 `x` (cwd /x); end them"))
    }

    /// `attempt` refusing for a live cwd `refusals` times, then merging;
    /// returns the result and how many attempts ran.
    fn merge_with(
        fake: &mut FakeLeftovers,
        refusals: u32,
    ) -> (Vec<String>, Result<&'static str, MergeError>, u32) {
        let mut attempts = 0;
        let (notes, outcome) = merge_through_leftovers(Path::new("/proj"), fake, &mut || {
            attempts += 1;
            if attempts <= refusals {
                Err(live_cwd_refusal())
            } else {
                Ok("merged")
            }
        });
        (notes, outcome, attempts)
    }

    /// Card `20261123-b-ccc2cc` proof: an orphaned `caffeinate -t 300`, ssh
    /// mux master, Monitor `tail -F` and `ugrep` in the lane cwd are ended
    /// and the merge goes through on its one retry — with the pane still
    /// open, since being orphaned is enough for these classes.
    #[test]
    fn merge_ends_orphaned_benign_leftovers_and_merges() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeLeftovers::new(
            dir.path(),
            &[
                (101, "/usr/bin/caffeinate", 1, "caffeinate -t 300"),
                (102, "ssh", 1, "ssh: /tmp/cm-u@host:22 [mux]"),
                (103, "tail", 1, "tail -F /tmp/task/log"),
                (104, "ugrep", 1, "ugrep --line-buffered done"),
            ],
        );
        let (notes, outcome, attempts) = merge_with(&mut fake, 1);
        assert_eq!(outcome.unwrap(), "merged");
        assert_eq!(attempts, 2, "exactly one retry after the reap pass");
        assert_eq!(fake.ended, vec![101, 102, 103, 104]);
        assert_eq!(fake.pane_asked, 0, "orphans never need the pane check");
        let printed = notes.join("\n");
        for expected in [
            "pid 101 `caffeinate -t 300`",
            "(cwd ",
            "; caffeinate)",
            "ssh mux master",
            "orphaned monitor",
            "ended",
        ] {
            assert!(printed.contains(expected), "{expected}: {printed}");
        }
    }

    /// Card `20261123-b-ccc2cc` proof: an unknown live process among the
    /// holders refuses the merge naming every holder's pid and command —
    /// and ends nothing, not even the benign `caffeinate` beside it.
    #[test]
    fn merge_refuses_an_unknown_holder_naming_pid_and_command_and_ends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeLeftovers::new(
            dir.path(),
            &[
                (201, "caffeinate", 1, "caffeinate -t 300"),
                (202, "cargo", 1, "cargo test --locked"),
            ],
        );
        fake.pane_gone = true;
        let (notes, outcome, attempts) = merge_with(&mut fake, 1);
        let Err(MergeError::Refused(message)) = outcome else {
            panic!("expected a refusal, got {outcome:?}");
        };
        assert!(message.starts_with(LIVE_CWD_REFUSAL), "{message}");
        assert!(
            message.contains("pid 202 `cargo test --locked`")
                && message.contains("not a known leftover"),
            "{message}"
        );
        assert!(message.contains("pid 201 `caffeinate -t 300`"), "{message}");
        assert!(message.contains("--prune --reap --yes"), "{message}");
        assert!(
            fake.ended.is_empty(),
            "nothing is signaled: {:?}",
            fake.ended
        );
        assert!(notes.is_empty());
        assert_eq!(attempts, 1, "a refusal is never retried");
    }

    /// Never by cwd alone: a shell is benign only once its pane is gone; a
    /// `tail` or `caffeinate` still parented by a live shell is not; a
    /// plain ssh client is never a mux master; and this process's own
    /// ancestor is never a leftover, whatever its name.
    #[test]
    fn benign_leftover_needs_command_name_plus_orphaned_or_pane_gone() {
        let process = |pid: u32, comm: &str| mv_engine::dockerreport::ProcessCwd {
            pid,
            cwd: PathBuf::from("/lane"),
            command: comm.into(),
            cpu_percent: "0.0".into(),
        };
        let parent = |ppid: u32, args: &str| mv_engine::dockerreport::ProcessParent {
            ppid,
            args: args.into(),
        };
        let class = |pid: u32, comm: &str, ppid: u32, args: &str, gone: bool, ancestors: &[u32]| {
            benign_leftover(
                &process(pid, comm),
                Some(&parent(ppid, args)),
                ancestors,
                &mut || gone,
            )
        };
        assert_eq!(
            class(9, "-zsh", 500, "-zsh", true, &[]),
            Some("shell of a gone pane")
        );
        assert_eq!(class(9, "zsh", 1, "zsh -c x", false, &[]), None);
        assert_eq!(class(9, "zsh", 500, "zsh", true, &[9]), None, "an ancestor");
        assert_eq!(class(9, "tail", 500, "tail -F log", true, &[]), None);
        assert_eq!(
            class(9, "caffeinate", 500, "caffeinate -t 300", false, &[]),
            None
        );
        assert_eq!(
            class(9, "caffeinate", 500, "caffeinate -t 300", true, &[]),
            Some("caffeinate")
        );
        assert_eq!(
            class(9, "ssh", 1, "ssh -tt build-host make", true, &[]),
            None
        );
        assert_eq!(
            class(9, "ssh", 500, "ssh -fNM -S /tmp/cm.sock host", true, &[]),
            Some("ssh mux master")
        );
        assert_eq!(class(9, "sleep", 1, "sleep 300", true, &[]), None);
        assert_eq!(
            benign_leftover(&process(9, "caffeinate"), None, &[], &mut || true),
            None,
            "an unreadable parent is never benign"
        );
    }

    /// A stopped pane's own shell (pane still open) refuses with its pid
    /// named; once the pane is gone the same shell is ended and merged.
    #[test]
    fn merge_ends_a_shell_only_once_its_pane_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let rows = [(301, "zsh", 900, "zsh -c sleep 1")];
        let mut open = FakeLeftovers::new(dir.path(), &rows);
        let (_, outcome, _) = merge_with(&mut open, 1);
        assert!(
            matches!(&outcome, Err(MergeError::Refused(m)) if m.contains("pid 301 `zsh -c sleep 1`")),
            "{outcome:?}"
        );
        assert!(open.ended.is_empty());

        let mut gone = FakeLeftovers::new(dir.path(), &rows);
        gone.pane_gone = true;
        let (_, outcome, _) = merge_with(&mut gone, 1);
        assert_eq!(outcome.unwrap(), "merged");
        assert_eq!(gone.ended, vec![301]);
    }

    /// Bounded: one reap pass and one retry. A holder still there after it
    /// is named in the refusal, and nothing is ended a second time.
    #[test]
    fn merge_reaps_once_and_names_what_is_still_there() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake =
            FakeLeftovers::new(dir.path(), &[(401, "caffeinate", 1, "caffeinate -t 300")]);
        let (_, outcome, attempts) = merge_with(&mut fake, 99);
        assert_eq!(attempts, 2);
        assert_eq!(fake.ended, vec![401]);
        let Err(MergeError::Refused(message)) = outcome else {
            panic!("expected a refusal, got {outcome:?}");
        };
        assert!(message.contains("pid 401 `caffeinate -t 300`"), "{message}");
        assert!(
            message.contains("still there after one reap pass"),
            "{message}"
        );
    }

    /// Any other refusal, or a merge that succeeds outright, never takes
    /// a snapshot or ends anything.
    #[test]
    fn merge_leaves_every_other_outcome_alone() {
        let dir = tempfile::tempdir().unwrap();
        let mut fake = FakeLeftovers::new(dir.path(), &[(501, "caffeinate", 1, "caffeinate")]);
        let (_, outcome) = merge_through_leftovers(Path::new("/proj"), &mut fake, &mut || {
            Err::<(), _>(MergeError::Refused("lane has uncommitted changes".into()))
        });
        assert!(
            matches!(outcome, Err(MergeError::Refused(m)) if m == "lane has uncommitted changes")
        );
        let (_, outcome, attempts) = merge_with(&mut fake, 0);
        assert_eq!(outcome.unwrap(), "merged");
        assert_eq!(attempts, 1);
        assert!(fake.ended.is_empty());
    }
}

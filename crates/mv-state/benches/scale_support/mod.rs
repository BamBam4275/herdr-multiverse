//! Synthetic state workloads shared by the opt-in integration measurements.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use mv_state::activity;
use mv_state::index::{self, Selector};
use mv_state::log::{encode_versioned, ProjectLog};
use mv_state::tasks::{read_status, TaskStatus};
use mv_state::worktrees::{
    HerdrLaneObserver, Lane, LaneObserver, PruneOutcome, WorktreeError, Worktrees,
};

const WHEN: &str = "2026-11-02T00:00:00+00:00";
const ROLES: &[&str] = &[
    "orchestrator",
    "nexus",
    "explorer",
    "scout",
    "fixer",
    "builder",
    "librarian",
    "archivist",
    "reviewer",
    "critic",
    "oracle",
    "seer",
    "council",
    "designer",
    "artisan",
    "observer",
    "watcher",
];

pub struct History {
    _temp: tempfile::TempDir,
    pub root: PathBuf,
    pub log: PathBuf,
    pub lines: usize,
    pub tasks: usize,
    messages: Vec<u64>,
    damaged: usize,
}

impl History {
    pub fn new(lines: usize, tasks: usize) -> Self {
        assert!(lines >= tasks * 10 && tasks >= ROLES.len());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let state = root.join(".multiverse");
        fs::create_dir_all(state.join("tasks")).unwrap();
        for task in 0..tasks {
            let dir = state.join("tasks").join(task_id(task));
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join("brief.md"), "# Synthetic scale task\n").unwrap();
            match task % 5 {
                0 => {
                    fs::write(dir.join("status"), " Done\n").unwrap();
                    fs::write(dir.join("result.md"), "Synthetic result\n").unwrap();
                }
                1 => fs::write(dir.join("status"), "blocked\n").unwrap(),
                2 => fs::write(dir.join("status"), "failed\n").unwrap(),
                3 => fs::write(dir.join("status"), "donedone\n").unwrap(),
                _ => (),
            }
            fs::write(dir.join("unrelated.txt"), "ignored task artifact\n").unwrap();
        }
        let log = state.join("routing.log");
        let mut writer = BufWriter::new(File::create(&log).unwrap());
        let mut messages = Vec::new();
        let mut damaged = 0;
        for i in 0..lines {
            let seq = (i + 1) as u64;
            let task = task_id((i / 10) % tasks);
            let role = ROLES[(i / 10) % ROLES.len()];
            let line = match i % 10 {
                0..=2 => {
                    let event = ["dispatch", "started", "result"][i % 10];
                    format!("{WHEN}\tevent={event}\ttask={task}\trole={role}\taccount=synthetic\tstatus=failed")
                }
                3 | 4 | 7 => {
                    let id = format!("synthetic-{seq}");
                    let to = if i % 10 == 4 { "elsewhere" } else { "scale" };
                    let line = encode_versioned(
                        WHEN,
                        seq,
                        "message",
                        &[
                            ("id", &id),
                            ("from", "source"),
                            ("to", to),
                            ("ask", "0"),
                            ("body", "\"synthetic caf\\u00e9 = history\""),
                        ],
                    )
                    .unwrap();
                    if i % 10 == 7 {
                        damaged += 1;
                        // Change CRC-covered bytes without updating the trailer.
                        line.replace("history", "damaged")
                    } else {
                        if to == "scale" {
                            messages.push(seq);
                        }
                        line
                    }
                }
                5 => encode_versioned(
                    WHEN,
                    seq,
                    "update",
                    &[("task", &task), ("phase", "working"), ("note", "\"scale\"")],
                )
                .unwrap(),
                6 => encode_versioned(
                    WHEN,
                    seq,
                    "backlog",
                    &[("item", "synthetic-card"), ("role", role)],
                )
                .unwrap(),
                8 => {
                    damaged += 1;
                    format!("{WHEN}\tv=1\tseq={seq}\tevent=message")
                }
                _ => format!("{WHEN}\tevent=annotation-legacy\tnote=ignored"),
            };
            writeln!(writer, "{line}").unwrap();
        }
        writer.flush().unwrap();
        writer.get_ref().sync_all().unwrap();
        Self {
            _temp: temp,
            root,
            log,
            lines,
            tasks,
            messages,
            damaged,
        }
    }
}

fn task_id(task: usize) -> String {
    format!("20261102-{}-{task:06x}", ROLES[task % ROLES.len()])
}

#[derive(Debug)]
pub struct Measurement {
    pub operation: &'static str,
    pub elapsed: Duration,
    pub budget: Duration,
}

fn measure<T>(
    out: &mut Vec<Measurement>,
    operation: &'static str,
    budget: f64,
    f: impl FnOnce() -> T,
) -> T {
    let start = Instant::now();
    let value = f();
    let elapsed = start.elapsed();
    eprintln!(
        "scale operation={operation} elapsed_ms={:.3} budget_ms={:.0}",
        elapsed.as_secs_f64() * 1000.0,
        budget * 1000.0
    );
    out.push(Measurement {
        operation,
        elapsed,
        budget: Duration::from_secs_f64(budget),
    });
    value
}

/// State-side inputs for task reconciliation. There is no reconcile/list API
/// in mv-state yet: read selected history once, then one status per unique task.
fn task_listing(history: &History) -> BTreeMap<String, Option<TaskStatus>> {
    let mut ids = BTreeSet::new();
    let mut roles = BTreeSet::new();
    for record in index::records(&history.log, Selector::Tasks).unwrap() {
        let record = record.parse().unwrap();
        roles.insert(record.fields["role"].clone());
        ids.insert(record.fields["task"].clone());
    }
    assert_eq!(roles, ROLES.iter().map(|role| (*role).to_owned()).collect());
    ids.into_iter()
        .map(|task| {
            let status = read_status(
                &history
                    .root
                    .join(".multiverse/tasks")
                    .join(&task)
                    .join("status"),
            );
            (task, status)
        })
        .collect()
}

pub fn history_workloads(history: &History) -> Vec<Measurement> {
    let mut timings = Vec::new();
    eprintln!(
        "scale lines={} tasks={} log_bytes={}",
        history.lines,
        history.tasks,
        fs::metadata(&history.log).unwrap().len()
    );
    // There is no bounded tail API yet. Measure the current public read path
    // including materialization, rather than timing only Vec::truncate.
    let tail = |selector| {
        let mut records = index::records(&history.log, selector).unwrap();
        records.drain(..records.len().saturating_sub(20));
        records
    };
    let uncached = measure(&mut timings, "tail_20_uncached", 2.0, || {
        tail(Selector::All)
    });
    let log = ProjectLog::new(&history.root);
    let guard = log.lock().unwrap();
    let entries = measure(&mut timings, "index_rebuild", 2.0, || {
        index::rebuild(guard.path()).unwrap()
    });
    drop(guard);
    assert_eq!(entries.len(), history.lines);
    let mut offset = 0;
    for (ordinal, entry) in entries.iter().enumerate() {
        assert_eq!(entry.ordinal, ordinal as u64);
        assert_eq!(entry.offset, offset);
        offset += entry.length;
    }
    assert_eq!(offset, fs::metadata(&history.log).unwrap().len());
    assert_eq!(
        entries.last().unwrap().high_water_seq,
        history.lines as u64 - 1
    );
    drop(entries);
    let cached = measure(&mut timings, "tail_20_cached", 2.0, || tail(Selector::All));
    assert_eq!(cached, uncached);
    assert_eq!(cached.len(), 20);
    assert_eq!(
        cached.first().unwrap().entry.ordinal,
        history.lines as u64 - 20
    );
    assert_eq!(cached.iter().filter(|r| r.parse().is_err()).count(), 4);

    let state = history.root.join(".multiverse");
    let cursor = history.lines as u64 - 100;
    fs::write(state.join("activity.cursor"), format!("{cursor}\n")).unwrap();
    let log = activity::ProjectLog::open("scale", &history.root);
    let page = measure(&mut timings, "activity_inbox_peek_20", 2.0, || {
        activity::read_inbox(&state, &log, 20, true).unwrap()
    });
    let expected: Vec<_> = history
        .messages
        .iter()
        .copied()
        .filter(|seq| *seq > cursor)
        .take(20)
        .collect();
    assert_eq!(
        page.messages.iter().map(|m| m.seq).collect::<Vec<_>>(),
        expected
    );
    assert_eq!(page.through_seq, expected.last().copied());
    assert!(page
        .messages
        .iter()
        .all(|m| m.body == "synthetic café = history"));
    assert_eq!(
        fs::read_to_string(state.join("activity.cursor")).unwrap(),
        format!("{cursor}\n")
    );
    // `read_inbox` writes `activity.lastseen` on every call, peek or not
    // (`activity.rs`'s own doc comment on `read_inbox`, matching the Python
    // reference) — assert the marker it wrote, not its absence.
    assert_eq!(
        fs::read_to_string(state.join("activity.lastseen")).unwrap(),
        format!("{}\t{}\n", page.marker_seq, page.marker_at)
    );

    let listing = measure(&mut timings, "task_reconcile_read_inputs", 3.0, || {
        task_listing(history)
    });
    assert_eq!(listing.len(), history.tasks);
    for task in 0..history.tasks {
        let expected = match task % 5 {
            0 => Some(TaskStatus::Done),
            1 => Some(TaskStatus::Blocked),
            2 => Some(TaskStatus::Failed),
            _ => None,
        };
        assert_eq!(listing[&task_id(task)], expected);
    }
    // Keep ledger damage visible through the index even when semantic readers
    // skip broken envelopes. A stale cache must preserve the same last page.
    fs::write(
        index::fingerprint_path(&history.log),
        "damaged fingerprint\n",
    )
    .unwrap();
    let fallback = measure(&mut timings, "tail_20_corrupt_cache", 2.0, || {
        tail(Selector::All)
    });
    assert_eq!(fallback, cached);
    let records = index::records(&history.log, Selector::All).unwrap();
    assert_eq!(
        records.iter().filter(|r| r.parse().is_err()).count(),
        history.damaged
    );
    timings
}

struct IdleObserver;
impl LaneObserver for IdleObserver {
    fn check(&mut self, _lane: &Lane, _force: bool) -> Result<(), WorktreeError> {
        Ok(())
    }
    fn record_prune(
        &mut self,
        _lane: &Lane,
        _task_records: &[index::IndexedRecord],
    ) -> Result<(), WorktreeError> {
        panic!("dry-run must not record or remove a lane")
    }
}

pub fn prune_workload(history: &History, lanes: usize) -> Vec<Measurement> {
    prune_workload_expect(history, lanes, "missing terminal status")
}

pub fn oversized_log_workload() {
    let history = History::new(200, 20);
    let mut file = File::options().append(true).open(&history.log).unwrap();
    writeln!(
        file,
        "{WHEN}\tevent=annotation-legacy\tnote={}",
        "x".repeat(4 * 1024 * 1024)
    )
    .unwrap();
    drop(file);
    assert!(fs::metadata(&history.log).unwrap().len() > 4 * 1024 * 1024);
    prune_workload(&history, 1);
}

pub fn oversized_status_workload() {
    let history = History::new(200, 20);
    let task = history
        .root
        .join(".multiverse/tasks/20261102-builder-scan000000");
    fs::create_dir(&task).unwrap();
    fs::write(task.join("status"), vec![b'x'; 4 * 1024 * 1024 + 1]).unwrap();
    prune_workload_expect(&history, 1, "expected a regular, unlinked file");
}

fn prune_workload_expect(history: &History, lanes: usize, refusal: &str) -> Vec<Measurement> {
    // Only an isolated empty git repository is needed: these candidates must
    // be retained for missing status before branch/registration checks.
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .arg(&history.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let root = history.root.join(".multiverse/worktrees");
    fs::create_dir(&root).unwrap();
    for i in 0..lanes {
        fs::create_dir(root.join(format!("20261102-builder-scan{i:06}"))).unwrap();
    }
    fs::create_dir(root.join(".helper")).unwrap();
    fs::write(root.join("foreign-file"), "leave alone").unwrap();
    let store = Worktrees::new(&history.root).unwrap();
    let mut timings = Vec::new();
    let found = measure(&mut timings, "worktree_lane_discovery", 1.0, || {
        store.lanes().unwrap()
    });
    assert_eq!(found.len(), lanes);
    eprintln!("scale prune_lanes={lanes} lines={}", history.lines);
    let outcomes = measure(
        &mut timings,
        "worktree_prune_dry_run_status_scan",
        10.0,
        || store.prune(&mut IdleObserver, false, true).unwrap(),
    );
    assert_eq!(outcomes.len(), lanes);
    for (outcome, lane) in outcomes.iter().zip(&found) {
        assert!(
            matches!(outcome, PruneOutcome::Kept { task, reason, .. } if task == &lane.task && reason.contains(refusal)),
            "{outcome:?}"
        );
        assert!(lane.path.is_dir());
    }
    assert!(!history.root.join(".multiverse/worktrees.lock").exists());
    assert_eq!(
        fs::read_to_string(root.join("foreign-file")).unwrap(),
        "leave alone"
    );
    timings
}

/// Candidate lanes shaped to reach `Worktrees::retirement_authorized`, the
/// nested path the README's own "further unmeasured nested path" note
/// named but `prune_workload_expect`'s missing-status candidates never
/// reach (`terminal_statuses` refuses them first). Each lane carries a
/// terminal status recorded directly in the project — so `terminal_
/// statuses` passes without even touching `has_terminal_outcome`'s own
/// ledger scan — and a real branch with one commit never merged into
/// `HEAD`, so `prune`'s ancestor and patch-id checks both say "not
/// landed" and the per-lane loop reaches `retirement_authorized`. No lane
/// here ever carries a `retire` sidecar, so every run keeps every lane
/// with the same "no archive authority" reason — the point is the scan
/// cost, not the outcome.
pub fn prune_archive_candidate_workload(history: &History, lanes: usize) -> Vec<Measurement> {
    let git = |args: &[&str], dir: &std::path::Path| {
        let output = Command::new("git")
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet", "-b", "main"], &history.root);
    git(&["config", "user.name", "Scale Test"], &history.root);
    git(
        &["config", "user.email", "scale@example.invalid"],
        &history.root,
    );
    git(&["config", "commit.gpgsign", "false"], &history.root);
    fs::write(history.root.join("README.md"), "scale\n").unwrap();
    git(&["add", "README.md"], &history.root);
    git(&["commit", "-m", "initial"], &history.root);

    let store = Worktrees::new(&history.root).unwrap();
    let mut created = Vec::new();
    for i in 0..lanes {
        let task = format!("20261102-builder-archive{i:06}");
        let lane = store.create(&task, "HEAD").unwrap();
        fs::write(lane.path.join("unique.txt"), format!("{i}\n")).unwrap();
        git(&["add", "unique.txt"], &lane.path);
        git(&["commit", "-m", "unique"], &lane.path);
        let task_dir = history.root.join(".multiverse/tasks").join(&task);
        fs::create_dir_all(&task_dir).unwrap();
        fs::write(task_dir.join("status"), "done\n").unwrap();
        fs::write(task_dir.join("result.md"), "synthetic archive candidate\n").unwrap();
        created.push(lane);
    }

    let mut timings = Vec::new();
    eprintln!(
        "scale prune_archive_candidate_lanes={lanes} lines={}",
        history.lines
    );
    // Twice the sibling "status scan" row's own budget: this path does
    // strictly more work than that one (the same terminal-status gate,
    // plus ancestor/patch-id/retirement-authority checks on top), and both
    // rows are measured on the same shared, noisy host — see the README's
    // own note on budgets being regression ceilings, not service
    // guarantees.
    let outcomes = measure(
        &mut timings,
        "worktree_prune_dry_run_archive_candidate_scan",
        20.0,
        || store.prune(&mut IdleObserver, false, true).unwrap(),
    );
    assert_eq!(outcomes.len(), lanes);
    for (outcome, lane) in outcomes.iter().zip(&created) {
        assert!(
            matches!(outcome, PruneOutcome::Kept { task, reason, .. } if task == &lane.task && reason.contains("no archive authority")),
            "{outcome:?}"
        );
        assert!(lane.path.is_dir());
    }
    timings
}

/// Lanes shaped to reach the real (non-dry-run) removal path — card
/// `20261117-b-c9349a` — not merely a dry-run preview: each one's branch
/// tip is `HEAD` itself (trivially merged, so `prune` reclaims rather than
/// archives it) and its own task already carries a terminal `event=result`
/// in the ledger, so nothing is refused and nothing new is appended. The
/// point is the per-lane scan cost `Worktrees::prune_with_ignored`'s real
/// removal pays to confirm that via `observer.record_prune`, which the two
/// dry-run workloads above never exercise at all — a dry-run prune never
/// reaches `record_prune`.
pub fn prune_real_removal_workload(history: &History, lanes: usize) -> Vec<Measurement> {
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir(&history.root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet", "-b", "main"]);
    git(&["config", "user.name", "Scale Test"]);
    git(&["config", "user.email", "scale@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    fs::write(history.root.join("README.md"), "scale\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-m", "initial"]);

    let mut file = File::options().append(true).open(&history.log).unwrap();
    let mut created = Vec::new();
    let store = Worktrees::new(&history.root).unwrap();
    for i in 0..lanes {
        let task = format!("20261102-builder-removal{i:06}");
        let lane = store.create(&task, "HEAD").unwrap();
        writeln!(
            file,
            "{WHEN}\tevent=result\ttask={task}\tstatus=done\tstate=reconciled\trevisions=0\tpane="
        )
        .unwrap();
        created.push(lane);
    }
    drop(file);

    let mut timings = Vec::new();
    eprintln!(
        "scale prune_real_removal_lanes={lanes} lines={}",
        history.lines
    );
    let mut observer =
        HerdrLaneObserver::with_herdr_binary(&history.root, "/nonexistent/fake-herdr-unused");
    // A real removal does strictly more work than either dry-run sibling
    // above (the same terminal-status gate, plus the actual rescue/branch/
    // directory removal and retirement-candidate re-listing their preview
    // never reaches), so its own ceiling is set well above both — see the
    // README's own note on budgets being regression ceilings tuned for a
    // quieter host than this shared one, not service guarantees.
    let outcomes = measure(&mut timings, "worktree_prune_real_removal", 60.0, || {
        store.prune(&mut observer, false, false).unwrap()
    });
    assert_eq!(outcomes.len(), lanes);
    for (outcome, lane) in outcomes.iter().zip(&created) {
        assert!(
            matches!(outcome, PruneOutcome::Removed { task, .. } if task == &lane.task),
            "{outcome:?}"
        );
        assert!(!lane.path.exists());
        assert!(!store.branch_exists(&lane.task).unwrap());
    }
    timings
}

pub fn assert_budgets(timings: &[Measurement]) {
    let failures: Vec<_> = timings.iter().filter(|m| m.elapsed > m.budget).collect();
    assert!(failures.is_empty(), "scale budgets exceeded: {failures:?}");
}

pub fn damage_workload() {
    let history = History::new(200, 20);
    let original = fs::read(&history.log).unwrap();
    let mut file = File::options().append(true).open(&history.log).unwrap();
    file.write_all(b"legacy\tevent=dispatch\ttask=\xff\n2026-11-02T00:00:00+00:00\tv=1\tseq=999\tevent=message").unwrap();
    drop(file);
    let damaged_bytes = fs::read(&history.log).unwrap();
    let records = index::records(&history.log, Selector::All).unwrap();
    assert_eq!(records.len(), history.lines + 2);
    let invalid = &records[history.lines];
    assert!(invalid.entry.invalid_utf8);
    assert!(invalid.parse().is_err());
    assert!(records.last().unwrap().parse().is_err());
    assert_eq!(records.last().unwrap().entry.high_water_seq, 999);
    assert!(activity::read_inbox(
        &history.root.join(".multiverse"),
        &activity::ProjectLog::open("scale", &history.root),
        20,
        true
    )
    .is_err());
    assert!(ProjectLog::new(&history.root)
        .append(&format!("{WHEN}\tevent=dispatch\ttask=next"))
        .is_err());
    assert!(damaged_bytes.starts_with(&original));
    assert_eq!(fs::read(&history.log).unwrap(), damaged_bytes);
    assert_eq!(
        fs::metadata(&history.log).unwrap().len(),
        records.iter().map(|r| r.entry.length).sum::<u64>()
    );
}

/// Routing-log appends happen one at a time, continuously — the busiest
/// write path in the whole state layer. Measure `count` sequential single-
/// record appends through `ProjectLog::append`, the entry point the
/// highest-volume task-lifecycle events (dispatch/started/result) use,
/// starting from a freshly rebuilt (trusted) cache the way a long-running
/// process would have after its previous append. This deliberately bypasses
/// `append_versioned`'s own seq auto-assignment so this workload measures
/// `index::append`'s fast path in isolation; see `append_versioned_workload`
/// below for the assignment step itself.
pub fn append_workload(history: &History, count: usize) -> Vec<Measurement> {
    let mut timings = Vec::new();
    let log = ProjectLog::new(&history.root);
    let guard = log.lock().unwrap();
    let entries = index::rebuild(guard.path()).unwrap();
    let mut seq = entries.last().map_or(0, |entry| entry.high_water_seq);
    let task = task_id(0);
    // Dominated by two fsyncs per append (index file + atomic fingerprint
    // write), not by log size: budget generously per append rather than
    // asserting a near-zero bound this shared host's disk can't promise.
    measure(
        &mut timings,
        "index_append_sequential",
        count as f64 * 0.1,
        || {
            for i in 0..count {
                seq += 1;
                let line = encode_versioned(
                    WHEN,
                    seq,
                    "update",
                    &[("task", &task), ("note", &format!("\"scale-append-{i}\""))],
                )
                .unwrap();
                guard.append(&line).unwrap();
            }
        },
    );
    drop(guard);
    // Compare what the incremental fast path actually left on disk against
    // an independent full rebuild, not the rebuild's own output against itself.
    let cached = index::read_index(&history.log).unwrap().unwrap();
    let rebuilt = index::rebuild(&history.log).unwrap();
    assert_eq!(
        cached, rebuilt,
        "incremental append index must stay byte-identical to a full rebuild"
    );
    assert_eq!(cached.len(), history.lines + count);
    timings
}

/// `append_versioned`'s own seq auto-assignment — the cost `append_workload`
/// above deliberately excludes by calling `ProjectLog::append` directly.
/// Measure `count` sequential `ProjectLog::append_versioned` calls, starting
/// from a freshly rebuilt (trusted) cache, so a regression in that
/// assignment (e.g. falling back to a full `index::records` scan on every
/// call again) shows up here even though `append_workload` can't see it.
pub fn append_versioned_workload(history: &History, count: usize) -> Vec<Measurement> {
    let mut timings = Vec::new();
    let log = ProjectLog::new(&history.root);
    let guard = log.lock().unwrap();
    // Baseline taken fresh (not `history.lines`) so this workload gives the
    // right answer whether it runs against a pristine history or one an
    // earlier workload (e.g. `append_workload`) already appended to.
    let baseline = index::rebuild(guard.path()).unwrap().len();
    let task = task_id(0);
    // Same per-op budget as `append_workload`'s `index_append_sequential`:
    // dominated by the same two fsyncs per append, plus a handful of small,
    // bounded reads to validate the cheap high-water read — not by log size.
    measure(
        &mut timings,
        "append_versioned_sequential",
        count as f64 * 0.1,
        || {
            for i in 0..count {
                guard
                    .append_versioned(
                        WHEN,
                        "update",
                        &[
                            ("task", &task),
                            ("note", &format!("\"scale-append-versioned-{i}\"")),
                        ],
                    )
                    .unwrap();
            }
        },
    );
    drop(guard);
    // Compare what the incremental fast path actually left on disk against
    // an independent full rebuild, not the rebuild's own output against itself.
    let cached = index::read_index(&history.log).unwrap().unwrap();
    let rebuilt = index::rebuild(&history.log).unwrap();
    assert_eq!(
        cached, rebuilt,
        "incremental append index must stay byte-identical to a full rebuild"
    );
    assert_eq!(cached.len(), baseline + count);
    timings
}

pub fn print_ratios(small: &[Measurement], large: &[Measurement]) {
    for (a, b) in small.iter().zip(large) {
        assert_eq!(a.operation, b.operation);
        eprintln!(
            "scale operation={} doubling_ratio={:.2}",
            a.operation,
            b.elapsed.as_secs_f64() / a.elapsed.as_secs_f64()
        );
    }
}

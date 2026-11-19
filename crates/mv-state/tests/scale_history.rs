//! Run the expensive budgets explicitly; ordinary CI exercises only small data.
#[path = "../benches/scale_support/mod.rs"]
mod scale_support;

use scale_support::{
    append_versioned_workload, append_workload, history_workloads,
    prune_archive_candidate_workload, prune_real_removal_workload, prune_workload, History,
};

#[test]
fn synthetic_history_preserves_state_read_contracts() {
    let history = History::new(1_000, 50);
    history_workloads(&history);
    prune_workload(&history, 2);
    append_workload(&history, 5);
    append_versioned_workload(&history, 5);
    scale_support::damage_workload();
    scale_support::oversized_log_workload();
    scale_support::oversized_status_workload();
    // A fresh history: `prune_workload` above already turned this one's
    // root into a git repo with its own (missing-status) lane directories,
    // and the archive-candidate workload needs its own branches/commits on
    // top of a clean initial commit.
    prune_archive_candidate_workload(&History::new(1_000, 50), 2);
    // Its own fresh history/root too: a real (non-dry-run) prune actually
    // removes the lanes and branches it creates, so it can't share a root
    // with either prune workload above.
    prune_real_removal_workload(&History::new(1_000, 50), 2);
}

#[test]
fn release_scale_budgets() {
    if std::env::var("MV_STATE_SCALE").as_deref() != Ok("1") {
        eprintln!("set MV_STATE_SCALE=1 and use --release to run scale budgets");
        return;
    }
    if cfg!(debug_assertions) {
        panic!("scale budgets require cargo test --release");
    }
    let mut runs = Vec::new();
    for (lines, tasks, lanes) in [(250_000, 2_000, 8), (500_000, 4_000, 16)] {
        let history = History::new(lines, tasks);
        let mut timings = history_workloads(&history);
        timings.extend(prune_workload(&history, lanes));
        timings.extend(append_workload(&history, 200));
        timings.extend(append_versioned_workload(&history, 200));
        // Its own fresh history/root: see the comment in the small test
        // above for why this can't reuse `history`.
        timings.extend(prune_archive_candidate_workload(
            &History::new(lines, tasks),
            lanes,
        ));
        timings.extend(prune_real_removal_workload(
            &History::new(lines, tasks),
            lanes,
        ));
        runs.push(timings);
    }
    scale_support::print_ratios(&runs[0], &runs[1]);
    // Report every workload before failing any budget.
    for timings in runs {
        scale_support::assert_budgets(&timings);
    }
}

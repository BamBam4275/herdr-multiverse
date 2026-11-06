//! Run the expensive budgets explicitly; ordinary CI exercises only small data.
#[path = "../benches/scale_support/mod.rs"]
mod scale_support;

use scale_support::{history_workloads, prune_workload, History};

#[test]
fn synthetic_history_preserves_state_read_contracts() {
    let history = History::new(1_000, 50);
    history_workloads(&history);
    prune_workload(&history, 2);
    scale_support::damage_workload();
    scale_support::oversized_log_workload();
    scale_support::oversized_status_workload();
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
        runs.push(timings);
    }
    scale_support::print_ratios(&runs[0], &runs[1]);
    // Report every workload before failing any budget.
    for timings in runs {
        scale_support::assert_budgets(&timings);
    }
}

//! Walks one or more history-corpus roots (`collect.sh`'s `--out`, or this
//! lane's committed `fixtures/`) and runs every check this crate owns
//! against them, reporting every record that fails to parse by file and
//! line, plus a count of every documented format variant it recognized.
//!
//! See `README.md` for what a root is expected to look like and why this
//! is its own Cargo workspace rather than a member of the repo root's.

mod activity;
mod finding;
mod fleet_stores;
mod routing_log;
mod tasks;
mod worktrees;

use std::path::Path;
use std::process::ExitCode;

use finding::Report;

fn check_project(root: &Path, report: &mut Report) {
    let high_water = routing_log::check_routing_log(&root.join("routing.log"), report).unwrap_or(0);

    let cursor = root.join("activity.cursor");
    if cursor.is_file() {
        activity::check_cursor(&cursor, high_water, report);
    }
    let lastseen = root.join("activity.lastseen");
    if lastseen.is_file() {
        activity::check_lastseen(&lastseen, report);
    }
    let lastack = root.join("activity.lastack");
    if lastack.is_file() {
        activity::check_lastack(&lastack, report);
    }

    let tasks_root = root.join("tasks");
    if tasks_root.is_dir() {
        tasks::check_tasks_root(&tasks_root, report);
    }

    let worktrees_root = root.join("worktrees");
    if worktrees_root.is_dir() {
        worktrees::check_worktrees_root(&worktrees_root, report);
    }
}

fn check_fleet_state(dir: &Path, report: &mut Report) {
    let breaker = dir.join("breaker-state.json");
    if breaker.is_file() {
        fleet_stores::check_breaker_state(&breaker, report);
    }
    let seat = dir.join("seat-state.json");
    if seat.is_file() {
        fleet_stores::check_seat_state(&seat, report);
    }
    let gauge = dir.join("gauge-state.json");
    if gauge.is_file() {
        fleet_stores::check_gauge_state(&gauge, report);
    }
}

fn check_root(root: &Path, report: &mut Report) {
    let Ok(entries) = std::fs::read_dir(root) else {
        eprintln!("history-runner: cannot read corpus root {}", root.display());
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name == "fleet-state" {
            check_fleet_state(&path, report);
            continue;
        }
        // Corpus roots name this `multiverse-state/`, not `.multiverse/` —
        // see `collect.sh`'s header for why (a machine-local
        // `.git/info/exclude` rule would otherwise swallow every
        // committed fixture using the real name).
        let project_root = path.join("multiverse-state");
        if project_root.is_dir() {
            check_project(&project_root, report);
        }
    }
}

fn main() -> ExitCode {
    let roots: Vec<String> = std::env::args().skip(1).collect();
    if roots.is_empty() {
        eprintln!("usage: mv-history-runner CORPUS_ROOT [CORPUS_ROOT ...]");
        return ExitCode::from(2);
    }

    let mut report = Report::default();
    for root in &roots {
        check_root(Path::new(root), &mut report);
    }

    println!("== variant counts ==");
    for (variant, count) in &report.variants {
        println!("{count:>6}  {variant}");
    }

    println!();
    println!("== findings ({}) ==", report.findings.len());
    for finding in &report.findings {
        println!("{finding}");
    }

    if report.findings.is_empty() {
        println!();
        println!(
            "mv-history-runner: 0 findings across {} root(s)",
            roots.len()
        );
        ExitCode::SUCCESS
    } else {
        println!();
        println!(
            "mv-history-runner: {} finding(s) across {} root(s)",
            report.findings.len(),
            roots.len()
        );
        ExitCode::FAILURE
    }
}

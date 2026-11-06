//! Runs the built binary against this lane's committed `fixtures/` and
//! pins the exact finding count and each damaged fleet entry (see
//! `fixtures/generate.py`'s comments), so the total is
//! a real regression check, not a smoke test that only proves the binary
//! starts. A change that silently stops detecting one of them, or starts
//! flagging a tolerated variant (old role names, either routing.log
//! envelope era, unsanctioned task files, a non-terminal status), fails
//! this test.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn fixtures_produce_exactly_their_planted_findings() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures");
    let output = Command::new(env!("CARGO_BIN_EXE_mv-history-runner"))
        .arg(&fixtures)
        .arg(fixtures.join("fleet-tolerant"))
        .arg(fixtures.join("fleet-invalid-json"))
        .arg(fixtures.join("fleet-invalid-top-level"))
        .output()
        .expect("failed to run mv-history-runner");

    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(
        output.status.code(),
        Some(1),
        "expected the planted-defect exit code; stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("28 finding(s) across 4 root(s)"),
        "expected 6 history defects, 16 damaged fleet entries, and 6 invalid fleet files; stdout:\n{stdout}"
    );

    for (filename, damaged) in [
        (
            "breaker-state.json",
            &["open", "entry", "timestamp", "trip-count", "manual"][..],
        ),
        (
            "seat-state.json",
            &["cap", "cap-overflow", "lend", "prefer", "entry"][..],
        ),
        (
            "gauge-state.json",
            &[
                "percent",
                "window",
                "label",
                "entry",
                "secondary-percent",
                "secondary-window",
            ][..],
        ),
    ] {
        for shape in damaged {
            let needle = format!("fleet-state/{filename}: ");
            let entry = format!("\"damaged-{shape}\"");
            assert_eq!(
                stdout
                    .lines()
                    .filter(|line| line.contains(&needle) && line.contains(&entry))
                    .count(),
                1,
                "expected one finding for {filename} {entry}: {stdout}"
            );
        }
        for root in ["fleet-invalid-json", "fleet-invalid-top-level"] {
            let needle = format!("{root}/fleet-state/{filename}:");
            assert_eq!(
                stdout.lines().filter(|line| line.contains(&needle)).count(),
                1,
                "expected one finding for {needle}: {stdout}"
            );
        }
    }

    // The scenarios that must NOT produce a finding: both routing.log
    // envelope eras, an old role name, unsanctioned-file tolerance, and
    // worktree adoption (a nested task dir whose id differs from its
    // worktree — confirmed as the normal real shape, not corruption).
    for file in [
        "legacy-and-v1-envelope",
        "old-role-names",
        "task-dir-noise",
        "status-variants",
        "worktrees-nested-task",
        "worktrees-adopted-followup",
        "activity-markers-tolerant",
        "fleet-tolerant",
    ] {
        assert!(
            !stdout.contains(&format!("fixtures/{file}/")),
            "{file} is a tolerant-parsing scenario and must not appear in the findings list; stdout:\n{stdout}"
        );
    }
}

#[test]
fn optional_fleet_fields_produce_zero_findings() {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fixtures/fleet-tolerant");
    let output = Command::new(env!("CARGO_BIN_EXE_mv-history-runner"))
        .arg(&fixtures)
        .output()
        .expect("failed to run mv-history-runner");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "tolerated fleet variants: {stdout}"
    );
    assert!(stdout.contains("0 findings across 1 root(s)"), "{stdout}");
    for variant in [
        "     4  fleet_stores.breaker_pool_ok",
        "     5  fleet_stores.gauge_pool_ok",
        "     4  fleet_stores.seat_entry_ok",
    ] {
        assert!(stdout.contains(variant), "missing {variant}: {stdout}");
    }
}

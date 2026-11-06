//! `multiverse health`: fleet pane-health classification
//! (LIMIT/RATE/CONTEXT/DEAD/DIALOG problems, per spec-seats-quotas.md's
//! `cmd_health`).
//!
//! Config resolution and output live here; the read-only scan and pane
//! classification live in `mv-engine::health`.

use clap::ArgMatches;

use crate::exit::{Failure, NEEDS_CONFIG};
use mv_engine::health::{problems_json, scan_live_panes, PaneProblem};

/// One fleet health report: empty `problems` means a clean scan.
pub struct HealthReport {
    pub problems: Vec<PaneProblem>,
}

/// Source of one completed health scan.
pub trait FleetHealth {
    /// `Err` only for a failure to even perform the scan (e.g. `herdr agent
    /// list` itself failing) — exit `ERROR`. A completed scan that found
    /// problems is `Ok` with a nonempty `problems` list — exit
    /// `NEEDS_CONFIG`, matching `cmd_health`'s documented 0/3/`ERROR` split.
    fn check(&self) -> Result<HealthReport, Failure>;
}

struct LiveFleet(mv_config::load::Config);

impl FleetHealth for LiveFleet {
    fn check(&self) -> Result<HealthReport, Failure> {
        let problems = scan_live_panes(&self.0).map_err(Failure::Operation)?;
        Ok(HealthReport { problems })
    }
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    let path = mv_config::load::resolve_config_path_from_env(
        args.get_one::<String>("config").map(std::path::Path::new),
    )
    .map_err(|error| Failure::Config(error.to_string()))?;
    let config =
        mv_config::load::load_config(&path).map_err(|error| Failure::Config(error.to_string()))?;
    run_with(args, &LiveFleet(config))
}

/// `problem.as_line()` interpolates project/task/seat straight from
/// Herdr's own pane text — sanitize the whole line before it reaches the
/// operator's terminal.
fn display_line(problem: &PaneProblem) -> String {
    mv_board::layout::sanitize_title(&problem.as_line())
}

fn run_with(args: &ArgMatches, source: &dyn FleetHealth) -> Result<(), Failure> {
    let report = source.check()?;
    if args.get_flag("json") {
        println!("{}", problems_json(&report.problems));
    } else {
        for problem in &report.problems {
            println!("{}", display_line(problem));
        }
    }
    if report.problems.is_empty() {
        Ok(())
    } else {
        Err(Failure::Reported(NEEDS_CONFIG))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;

    struct Clean;
    impl FleetHealth for Clean {
        fn check(&self) -> Result<HealthReport, Failure> {
            Ok(HealthReport {
                problems: Vec::new(),
            })
        }
    }

    struct ScanFailed;
    impl FleetHealth for ScanFailed {
        fn check(&self) -> Result<HealthReport, Failure> {
            Err(Failure::Operation("herdr agent list failed".into()))
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("health").unwrap().clone()
    }

    #[test]
    fn found_problems_return_needs_config_without_a_second_error_report() {
        struct Unhealthy;
        impl FleetHealth for Unhealthy {
            fn check(&self) -> Result<HealthReport, Failure> {
                Ok(HealthReport {
                    problems: vec![PaneProblem {
                        class: mv_engine::health::PaneClass::Rate,
                        pane: "w1:p1".into(),
                        project: "sandbox".into(),
                        task: "—".into(),
                        seat: "—".into(),
                    }],
                })
            }
        }
        let error = run_with(&matches(&["health"]), &Unhealthy).unwrap_err();
        assert_eq!(error.code(), NEEDS_CONFIG);
        assert!(error.to_string().is_empty());
    }

    #[test]
    fn missing_config_needs_config_before_scanning() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.toml");
        let err = run(&matches(&["health", "--config", missing.to_str().unwrap()])).unwrap_err();
        assert_eq!(err.code(), 3);
        assert!(!err.to_string().contains("not implemented"));
    }

    #[test]
    fn a_clean_scan_is_ok() {
        assert!(run_with(&matches(&["health"]), &Clean).is_ok());
    }

    #[test]
    fn a_failed_scan_is_an_error_not_needs_config() {
        let err = run_with(&matches(&["health"]), &ScanFailed).unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn json_flag_is_accepted() {
        assert!(run_with(&matches(&["health", "--json"]), &Clean).is_ok());
    }

    #[test]
    fn display_line_strips_escape_sequences_from_herdr_supplied_fields() {
        let problem = PaneProblem {
            class: mv_engine::health::PaneClass::Dead,
            pane: "w1:p1".into(),
            project: "sand\u{1b}]0;pwned\u{7}box".into(),
            task: "\u{1b}[2Ktask-1".into(),
            seat: "—".into(),
        };
        let line = display_line(&problem);
        assert!(!line.contains('\u{1b}'));
        assert_eq!(line, "DEAD w1:p1 sandbox task-1 —");
    }
}

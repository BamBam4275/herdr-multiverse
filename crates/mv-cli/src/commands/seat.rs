//! `multiverse seat show <account> [--json]`: metadata and stored cap.
//! Config errors are operation errors (exit 1). This inspection
//! treats an unreadable Herdr listing as an unknown open-agent count.

use std::path::Path;

use clap::ArgMatches;
use mv_config::load::{
    config_state_dir_from_env, load_config, resolve_config_path_from_env, seat_provider,
    SeatProvider,
};
use mv_engine::seatstate;

use super::{is_retired_role_key_error, MIGRATE_FIX_LINE};
use crate::exit::Failure;

/// Append the migrate fix line (card `20261102-b-5c41bb`) to a
/// retired-role-key refusal without changing this command's exit code:
/// this module's own doc comment above keeps config errors as `Operation`
/// (exit 1) on purpose, so only the message gains the fix line here.
fn config_error_message(message: String) -> String {
    if is_retired_role_key_error(&message) {
        format!("{message}\n{MIGRATE_FIX_LINE}")
    } else {
        message
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatState {
    pub seat: String,
    pub cap: Option<u32>,
    pub kind: String,
    pub model: Option<String>,
    pub provider: Option<SeatProvider>,
    pub open: Option<u32>,
}

pub trait SeatSource {
    fn show(&self, seat: &str, config_override: Option<&Path>) -> Result<SeatState, Failure>;
}

struct LiveConfig;

impl SeatSource for LiveConfig {
    fn show(&self, seat: &str, config_override: Option<&Path>) -> Result<SeatState, Failure> {
        self.show_with_count(seat, config_override, seatstate::live_open_count)
    }
}

impl LiveConfig {
    fn show_with_count(
        &self,
        seat: &str,
        config_override: Option<&Path>,
        count: impl FnOnce(&str) -> Option<u32>,
    ) -> Result<SeatState, Failure> {
        let path = resolve_config_path_from_env(config_override).map_err(|error| {
            Failure::Operation(format!("cannot resolve the config path: {error}"))
        })?;
        let config = load_config(&path)
            .map_err(|error| Failure::Operation(config_error_message(error.to_string())))?;
        let account = config
            .accounts
            .get(seat)
            .or_else(|| {
                config
                    .accounts
                    .values()
                    .find(|account| account.short == seat)
            })
            .ok_or_else(|| Failure::Operation(format!("unknown seat {seat:?}")))?;
        let state_dir = config_state_dir_from_env(&path);
        Ok(SeatState {
            seat: account.name.clone(),
            cap: seatstate::get_cap(&state_dir, &account.name),
            kind: account.kind.clone(),
            model: account.model.clone(),
            provider: seat_provider(account),
            open: count(&account.short),
        })
    }
}

pub fn run(args: &ArgMatches) -> Result<(), Failure> {
    run_with(args, &LiveConfig)
}

fn run_with(args: &ArgMatches, source: &dyn SeatSource) -> Result<(), Failure> {
    let Some((name, show_args)) = args.subcommand() else {
        println!("usage: multiverse seat show <account> [--json]");
        return Err(Failure::Reported(crate::exit::ERROR));
    };
    debug_assert_eq!(name, "show");
    let seat = show_args
        .get_one::<String>("seat")
        .expect("seat is required");
    let config_override = args
        .get_one::<String>("config")
        .map(|s| Path::new(s.as_str()));
    let state = source.show(seat, config_override)?;
    if show_args.get_flag("json") {
        println!(
            "{}",
            seatstate::show_json(
                &state.seat,
                &state.kind,
                state.model.as_deref(),
                state.provider.as_ref(),
                state.cap,
                state.open
            )
        );
    } else {
        println!(
            "{}",
            seatstate::show_human(
                &state.seat,
                &state.kind,
                state.model.as_deref(),
                state.provider.as_ref(),
                state.cap,
                state.open,
            )
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;

    struct Fixed(SeatState);
    impl SeatSource for Fixed {
        fn show(&self, _seat: &str, _config: Option<&Path>) -> Result<SeatState, Failure> {
            Ok(self.0.clone())
        }
    }

    fn matches(argv: &[&str]) -> clap::ArgMatches {
        let m = args::command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap();
        m.subcommand_matches("seat").unwrap().clone()
    }

    struct TempFile(std::path::PathBuf);
    impl TempFile {
        fn write(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "mv-cli-seat-test-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::write(&path, content).unwrap();
            Self(path)
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn missing_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "mv-cli-seat-test-missing-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn bare_seat_prints_usage_without_reading_configuration() {
        struct Unused;
        impl SeatSource for Unused {
            fn show(&self, _: &str, _: Option<&Path>) -> Result<SeatState, Failure> {
                panic!("bare seat must not inspect configuration");
            }
        }
        let error = run_with(&matches(&["seat"]), &Unused).unwrap_err();
        assert!(matches!(error, Failure::Reported(1)));
    }

    #[test]
    fn a_missing_config_file_is_an_error_not_needs_config() {
        let err = LiveConfig.show("t1", Some(&missing_path("a"))).unwrap_err();
        assert_eq!(err.code(), 1);
    }

    #[test]
    fn an_unconfigured_seat_is_an_error_naming_the_seat() {
        let file = TempFile::write("empty", "");
        let err = LiveConfig.show("t1", Some(&file.0)).unwrap_err();
        assert_eq!(err.code(), 1);
        assert!(err.to_string().contains("t1"));
    }

    #[test]
    fn a_configured_seat_reads_metadata_and_cap_defaults() {
        let file = TempFile::write(
            "configured",
            r#"
            [accounts.testacct]
            kind = "codex"
            config_dir_var = "CODEX_HOME"
            config_dir = "/tmp/mv-cli-seat-test-fake-codex-home"
            short = "t1"
            "#,
        );
        let state = LiveConfig
            .show_with_count("t1", Some(&file.0), |short| {
                assert_eq!(short, "t1");
                Some(1)
            })
            .unwrap();
        assert_eq!(
            state,
            SeatState {
                seat: "testacct".into(),
                cap: None,
                kind: "codex".into(),
                model: None,
                provider: None,
                open: Some(1),
            }
        );
    }

    #[test]
    fn a_known_seat_prints_its_state() {
        let state = SeatState {
            seat: "scout-1".into(),
            cap: Some(2),
            kind: "codex".into(),
            model: None,
            provider: None,
            open: Some(1),
        };
        assert!(run_with(&matches(&["seat", "show", "scout-1"]), &Fixed(state)).is_ok());
    }

    #[test]
    fn json_flag_is_accepted_and_reaches_the_source() {
        let state = SeatState {
            seat: "scout-1".into(),
            cap: None,
            kind: "codex".into(),
            model: None,
            provider: None,
            open: None,
        };
        assert!(run_with(
            &matches(&["seat", "show", "scout-1", "--json"]),
            &Fixed(state)
        )
        .is_ok());
    }
}

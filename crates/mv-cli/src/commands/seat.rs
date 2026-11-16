//! `multiverse seat show <account> [--json]`: metadata and stored cap.
//! Config errors are operation errors (exit 1). This inspection
//! treats an unreadable Herdr listing as an unknown open-agent count.
//! Open count is worker panes only — a busy Nexus pane on the same seat
//! is excluded, matching `accounts`/`check` (card `20261115-b-4ded07`).
//! A seat whose own `nexus_slots > 0` additionally renders the Nexus
//! count alongside it, in the same `workers N/<cap>, nexus M/<slots>`
//! wording `accounts` uses — byte-identical to the plain `open:` line
//! otherwise (card `20261115-b-4cda58`).

use std::path::{Path, PathBuf};

use clap::ArgMatches;
use mv_config::load::{
    load_config, resolve_config_path_from_env, seat_provider, Config, SeatProvider,
};
use mv_engine::seatstate;

use super::doctor::{project_path, state_dir_from_config};
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

/// A seat's resolved display state for `seat show`: config metadata (kind,
/// model, provider), its stored cap, and its live open-pane counts from
/// Herdr (`None` when Herdr is unreachable, not "zero").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatState {
    pub seat: String,
    pub cap: Option<u32>,
    pub kind: String,
    pub model: Option<String>,
    pub provider: Option<SeatProvider>,
    pub open: Option<u32>,
    /// Busy Nexus-pane count for this seat, alongside `open`'s worker
    /// count. Only rendered when `nexus_slots > 0` (card
    /// `20261115-b-4cda58`); `None` tracks `open`'s own Herdr-unreachable
    /// case rather than meaning "zero".
    pub nexus_open: Option<u32>,
    /// This account's configured Nexus-slot cap
    /// (`mv_config::load::Account::nexus_slots`); `0` is the default and
    /// keeps this seat's output exactly as it read before the split.
    pub nexus_slots: u32,
}

/// The seam `seat show` resolves its state through. `LiveConfig` is the
/// real implementation (config + live Herdr counts); tests substitute a
/// fixed one so `run_with` can be exercised without touching either.
pub trait SeatSource {
    /// Resolve `seat`'s display state, or the operation error naming why
    /// (unknown seat, unreadable config) — never a needs-config exit, per
    /// this module's own doc comment above.
    fn show(&self, seat: &str, config_override: Option<&Path>) -> Result<SeatState, Failure>;
}

struct LiveConfig;

/// Every configured project's resolved path — [`seatstate::
/// nexus_agent_names`]'s own fleet scope, matching `commands::dispatch`'s
/// `is_nexus_role` exclusion (card `20261115-b-234610`): a project still on
/// a `<<PLACEHOLDER: ...>>` path contributes no routing log.
fn project_roots(config: &Config) -> Vec<PathBuf> {
    config
        .projects
        .values()
        .filter_map(|project| project_path(project).map(Path::to_path_buf))
        .collect()
}

impl SeatSource for LiveConfig {
    fn show(&self, seat: &str, config_override: Option<&Path>) -> Result<SeatState, Failure> {
        // Card `20261115-b-4ded07`: a busy Nexus pane must never inflate
        // this seat's worker open count, the same exclusion
        // `commands::dispatch`'s `is_nexus_role` already applies.
        // `live_open_count`'s `&str -> Option<u32>` signature stays
        // unchanged (card `20261115-b-234610` left it as-is because this
        // file was outside that write scope) — `show_with_count` now
        // threads the full `seatstate::SeatOpenCounts` split instead, so
        // this seat's own Nexus count can render alongside it when
        // `nexus_slots > 0` (card `20261115-b-4cda58`).
        self.show_with_count(seat, config_override, |short, config| {
            let nexus_agents = seatstate::nexus_agent_names(&project_roots(config));
            seatstate::live_seat_open_counts(short, &nexus_agents)
        })
    }
}

impl LiveConfig {
    fn show_with_count(
        &self,
        seat: &str,
        config_override: Option<&Path>,
        count: impl FnOnce(&str, &Config) -> Option<seatstate::SeatOpenCounts>,
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
        let state_dir = state_dir_from_config(&path);
        let counts = count(&account.short, &config);
        Ok(SeatState {
            seat: account.name.clone(),
            cap: seatstate::get_cap(&state_dir, &account.name),
            kind: account.kind.clone(),
            model: account.model.clone(),
            provider: seat_provider(account),
            open: counts.map(|c| c.workers),
            nexus_open: counts.map(|c| c.nexus),
            nexus_slots: account.nexus_slots,
        })
    }
}

/// Renders `seat show`'s human block via [`seatstate::show_human`],
/// byte-identical to it when this seat's `nexus_slots == 0` (the default).
/// With `nexus_slots > 0`, the trailing `open:` line is replaced with the
/// same `workers N/<cap>, nexus M/<slots>` wording `accounts` renders for
/// a split seat (card `20261115-b-4cda58`).
fn render_human(state: &SeatState) -> String {
    let base = seatstate::show_human(
        &state.seat,
        &state.kind,
        state.model.as_deref(),
        state.provider.as_ref(),
        state.cap,
        state.open,
    );
    if state.nexus_slots == 0 {
        return base;
    }
    let mut lines: Vec<&str> = base.split('\n').collect();
    let open_line = match (state.open, state.nexus_open, state.cap) {
        (Some(workers), Some(nexus), Some(cap)) => {
            format!(
                "open: workers {workers}/{cap}, nexus {nexus}/{}",
                state.nexus_slots
            )
        }
        (Some(workers), Some(nexus), None) => {
            format!(
                "open: workers {workers}, nexus {nexus}/{}",
                state.nexus_slots
            )
        }
        _ => "open: ? (Herdr unreachable)".to_owned(),
    };
    *lines
        .last_mut()
        .expect("show_human always ends with the open line") = &open_line;
    lines.join("\n")
}

/// Renders `seat show --json` via [`seatstate::show_json`], byte-identical
/// to it when this seat's `nexus_slots == 0` (the default). With
/// `nexus_slots > 0`, splices `nexus_open`/`nexus_slots` fields in just
/// ahead of `"open"` — their alphabetical position among `show_json`'s own
/// sorted keys — on top of the existing ones, never renaming a key
/// already there (card `20261115-b-4cda58`). `mv-cli` has no runtime
/// `serde_json` dependency (only as a dev-dependency for tests), so this
/// splices the formatted string rather than re-parsing it.
fn render_json(state: &SeatState) -> String {
    let base = seatstate::show_json(
        &state.seat,
        &state.kind,
        state.model.as_deref(),
        state.provider.as_ref(),
        state.cap,
        state.open,
    );
    if state.nexus_slots == 0 {
        return base;
    }
    let marker = "\"open\":";
    let insert_at = base
        .find(marker)
        .expect("show_json always includes an `open` field");
    let nexus_open = state
        .nexus_open
        .map_or_else(|| "null".to_owned(), |nexus| nexus.to_string());
    format!(
        "{}\"nexus_open\": {nexus_open}, \"nexus_slots\": {}, {}",
        &base[..insert_at],
        state.nexus_slots,
        &base[insert_at..]
    )
}

/// Entry point for `multiverse seat`, wired to the real `LiveConfig`
/// source.
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
        println!("{}", render_json(&state));
    } else {
        println!("{}", render_human(&state));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args;
    use crate::commands::worktrees::herdr_env::HerdrEnvGuard;

    /// Card `20261114-b-74029b`: `LiveConfig::show_with_count` reaches
    /// `config_state_dir_from_env` at this file's line ~73, the same
    /// failure mode closed in `commands::sweep`/`commands::dispatch`/
    /// `commands::council`/`commands::review_policy` by a guard of this
    /// same name — this module had none, so its one test that reaches
    /// that call relied on whatever `$MULTIVERSE_STATE_DIR` happened to
    /// already be in this process (the `.cargo/config.toml`-forced
    /// sentinel under `cargo test`, or a dispatched pane's live state dir
    /// when the compiled test binary is run directly).
    struct StateDirEnvGuard {
        _herdr: HerdrEnvGuard,
        _dir: tempfile::TempDir,
        previous_state_dir: Option<std::ffi::OsString>,
        previous_plugin_state_dir: Option<std::ffi::OsString>,
        previous_config: Option<std::ffi::OsString>,
    }

    impl StateDirEnvGuard {
        fn new() -> Self {
            Self::with_herdr_guard(HerdrEnvGuard::unreachable())
        }

        fn with_herdr(path: &Path) -> Self {
            Self::with_herdr_guard(HerdrEnvGuard::set(path))
        }

        fn with_herdr_guard(herdr: HerdrEnvGuard) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let previous_state_dir = std::env::var_os("MULTIVERSE_STATE_DIR");
            let previous_plugin_state_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
            let previous_config = std::env::var_os("MULTIVERSE_CONFIG");
            std::env::set_var("MULTIVERSE_STATE_DIR", dir.path());
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", dir.path());
            std::env::remove_var("MULTIVERSE_CONFIG");
            Self {
                _herdr: herdr,
                _dir: dir,
                previous_state_dir,
                previous_plugin_state_dir,
                previous_config,
            }
        }
    }

    impl Drop for StateDirEnvGuard {
        fn drop(&mut self) {
            match self.previous_state_dir.take() {
                Some(value) => std::env::set_var("MULTIVERSE_STATE_DIR", value),
                None => std::env::remove_var("MULTIVERSE_STATE_DIR"),
            }
            match self.previous_plugin_state_dir.take() {
                Some(value) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", value),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            match self.previous_config.take() {
                Some(value) => std::env::set_var("MULTIVERSE_CONFIG", value),
                None => std::env::remove_var("MULTIVERSE_CONFIG"),
            }
        }
    }

    /// Card `20261114-b-a73f4c`: `LiveConfig::show_with_count` now
    /// routes through `doctor::state_dir_from_config`, so a non-temp,
    /// non-sentinel `$MULTIVERSE_STATE_DIR` panics here instead of
    /// resolving a live fleet path. Fails on current main (the direct
    /// `config_state_dir_from_env` call has no sandbox check).
    struct SimulatedAmbientStateDirGuard {
        _herdr: HerdrEnvGuard,
        previous: Option<std::ffi::OsString>,
    }

    impl SimulatedAmbientStateDirGuard {
        fn set(path: &Path) -> Self {
            let herdr = HerdrEnvGuard::unreachable();
            let previous = std::env::var_os("MULTIVERSE_STATE_DIR");
            std::env::set_var("MULTIVERSE_STATE_DIR", path);
            Self {
                _herdr: herdr,
                previous,
            }
        }
    }

    impl Drop for SimulatedAmbientStateDirGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var("MULTIVERSE_STATE_DIR", value),
                None => std::env::remove_var("MULTIVERSE_STATE_DIR"),
            }
        }
    }

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

    fn chmod_script(path: &Path) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(path, perms).unwrap();
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
    #[should_panic(expected = "neither under the test sandbox")]
    fn show_panics_when_the_ambient_state_dir_looks_real() {
        let live_like = Path::new("/MV_TEST_SIMULATED_LIVE_STATE_DIR_NOT_A_SENTINEL");
        let _state = SimulatedAmbientStateDirGuard::set(live_like);
        let file = TempFile::write(
            "live-like-state",
            r#"
            [accounts.testacct]
            kind = "codex"
            config_dir_var = "CODEX_HOME"
            config_dir = "/tmp/mv-cli-seat-test-fake-codex-home"
            short = "t1"
            "#,
        );
        let _ = LiveConfig.show_with_count("t1", Some(&file.0), |_, _| Some(Default::default()));
    }

    #[test]
    fn a_configured_seat_reads_metadata_and_cap_defaults() {
        let _state = StateDirEnvGuard::new();
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
            .show_with_count("t1", Some(&file.0), |short, _| {
                assert_eq!(short, "t1");
                Some(seatstate::SeatOpenCounts {
                    workers: 1,
                    nexus: 0,
                })
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
                nexus_open: Some(0),
                nexus_slots: 0,
            }
        );
    }

    /// Card `20261115-b-4ded07`, proof: a seat hosting one busy Nexus pane
    /// and one busy worker pane must read `open: 1`, never `open: 2` —
    /// the same role exclusion `commands::dispatch`'s `is_nexus_role`
    /// already applies, and that `check`/`accounts`/`review_policy` consume
    /// via `seatstate::live_seat_open_counts`. Revert-checked: `open` was
    /// `Some(2)` while `LiveConfig::show` still wired `live_open_count`.
    #[test]
    fn show_excludes_a_busy_nexus_pane_from_its_open_count() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".multiverse")).unwrap();
        std::fs::write(
            project.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-nexus-aaaaaa\t\
             role=nexus\tagent=mv-nexus-1-t1\tbranch=main\n",
        )
        .unwrap();

        let marker = tempfile::NamedTempFile::new().unwrap();
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap()
            .into_temp_path();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf '%s' '{{\"result\": {{\"agents\": \
                 [{{\"pane_id\": \"w1:p1\", \"agent_status\": \"working\", \"name\": \"mv-nexus-1-t1\"}}, \
                 {{\"pane_id\": \"w1:p2\", \"agent_status\": \"working\", \"name\": \"mv-builder-2-t1\"}}]}}}}'\n",
                marker.path().display()
            ),
        )
        .unwrap();
        chmod_script(&script);
        let _state = StateDirEnvGuard::with_herdr(&script);

        let file = TempFile::write(
            "nexus-open",
            &format!(
                r#"
                [accounts.testacct]
                kind = "codex"
                config_dir_var = "CODEX_HOME"
                config_dir = "/tmp/mv-cli-seat-test-fake-codex-home"
                short = "t1"

                [projects.sandbox]
                path = {:?}
                "#,
                project.path().to_str().unwrap()
            ),
        );
        let state = LiveConfig.show("t1", Some(&file.0)).unwrap();
        assert_eq!(
            state.open,
            Some(1),
            "a busy Nexus pane must not occupy the seat's worker open count"
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
            nexus_open: None,
            nexus_slots: 0,
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
            nexus_open: None,
            nexus_slots: 0,
        };
        assert!(run_with(
            &matches(&["seat", "show", "scout-1", "--json"]),
            &Fixed(state)
        )
        .is_ok());
    }

    fn split_state(cap: Option<u32>) -> SeatState {
        SeatState {
            seat: "testacct".into(),
            cap,
            kind: "codex".into(),
            model: None,
            provider: None,
            open: Some(1),
            nexus_open: Some(1),
            nexus_slots: 1,
        }
    }

    /// Card `20261115-b-4cda58`, proof: `nexus_slots == 0` (the default)
    /// must leave `seat show`'s human block byte-identical to
    /// `seatstate::show_human`, even though `render_human` is now the call
    /// site `run_with` actually uses.
    #[test]
    fn render_human_is_byte_identical_to_show_human_when_nexus_slots_is_zero() {
        let state = SeatState {
            seat: "testacct".into(),
            cap: Some(2),
            kind: "codex".into(),
            model: None,
            provider: None,
            open: Some(1),
            nexus_open: None,
            nexus_slots: 0,
        };
        assert_eq!(
            render_human(&state),
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

    /// Card `20261115-b-4cda58`, proof: a seat with `nexus_slots = 1`, one
    /// busy Nexus pane and one busy worker renders `workers 1/<cap>, nexus
    /// 1/1` in place of the plain `open:` line, matching `accounts`'
    /// wording. Fails on current main (`render_human` doesn't exist yet,
    /// and `show_human` has no split branch).
    #[test]
    fn render_human_renders_the_worker_nexus_split_when_nexus_slots_is_set() {
        let state = split_state(Some(2));
        let rendered = render_human(&state);
        assert_eq!(
            rendered.lines().last(),
            Some("open: workers 1/2, nexus 1/1")
        );
        assert!(rendered.starts_with("seat: testacct\nkind: codex\n"));
    }

    #[test]
    fn render_human_renders_the_split_without_a_cap() {
        let state = split_state(None);
        assert_eq!(
            render_human(&state).lines().last(),
            Some("open: workers 1, nexus 1/1")
        );
    }

    /// Card `20261115-b-4cda58`, proof: `nexus_slots == 0` leaves `seat
    /// show --json` byte-identical to `seatstate::show_json`.
    #[test]
    fn render_json_is_byte_identical_to_show_json_when_nexus_slots_is_zero() {
        let state = SeatState {
            seat: "testacct".into(),
            cap: Some(2),
            kind: "codex".into(),
            model: None,
            provider: None,
            open: Some(1),
            nexus_open: None,
            nexus_slots: 0,
        };
        assert_eq!(
            render_json(&state),
            seatstate::show_json(
                &state.seat,
                &state.kind,
                state.model.as_deref(),
                state.provider.as_ref(),
                state.cap,
                state.open,
            )
        );
    }

    /// Card `20261115-b-4cda58`, proof: `nexus_slots > 0` adds
    /// `nexus_open`/`nexus_slots` fields on top of the existing ones —
    /// never renaming a key already there.
    #[test]
    fn render_json_adds_nexus_fields_only_when_nexus_slots_is_set() {
        let state = split_state(Some(2));
        let rendered = render_json(&state);
        let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(parsed["seat"], "testacct");
        assert_eq!(parsed["cap"], 2);
        assert_eq!(parsed["open"], 1);
        assert_eq!(parsed["nexus_open"], 1);
        assert_eq!(parsed["nexus_slots"], 1);
    }

    /// Card `20261115-b-4cda58`, proof: the same busy-Nexus-pane fixture
    /// `show_excludes_a_busy_nexus_pane_from_its_open_count` uses, but with
    /// this seat's own `nexus_slots = 1` and a stored cap, renders the full
    /// `workers 1/<cap>, nexus 1/1` split end to end through `LiveConfig`.
    #[test]
    fn show_renders_the_worker_nexus_split_for_a_seat_with_nexus_slots() {
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(project.path().join(".multiverse")).unwrap();
        std::fs::write(
            project.path().join(".multiverse/routing.log"),
            "2020-01-01T00:00:00+00:00\tevent=dispatch\ttask=20250101-nexus-aaaaaa\t\
             role=nexus\tagent=mv-nexus-1-t1\tbranch=main\n",
        )
        .unwrap();

        let marker = tempfile::NamedTempFile::new().unwrap();
        let script = tempfile::Builder::new()
            .prefix("mv-cli-fake-herdr-")
            .suffix(".sh")
            .tempfile()
            .unwrap()
            .into_temp_path();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nprintf '%s' '{{\"result\": {{\"agents\": \
                 [{{\"pane_id\": \"w1:p1\", \"agent_status\": \"working\", \"name\": \"mv-nexus-1-t1\"}}, \
                 {{\"pane_id\": \"w1:p2\", \"agent_status\": \"working\", \"name\": \"mv-builder-2-t1\"}}]}}}}'\n",
                marker.path().display()
            ),
        )
        .unwrap();
        chmod_script(&script);
        let _state = StateDirEnvGuard::with_herdr(&script);
        let state_dir = std::env::var("MULTIVERSE_STATE_DIR").unwrap();
        seatstate::set_cap(Path::new(&state_dir), "testacct", Some(2)).unwrap();

        let file = TempFile::write(
            "nexus-slots-open",
            &format!(
                r#"
                [accounts.testacct]
                kind = "codex"
                config_dir_var = "CODEX_HOME"
                config_dir = "/tmp/mv-cli-seat-test-fake-codex-home"
                short = "t1"
                nexus_slots = 1

                [projects.sandbox]
                path = {:?}
                "#,
                project.path().to_str().unwrap()
            ),
        );
        let state = LiveConfig.show("t1", Some(&file.0)).unwrap();
        assert_eq!(state.nexus_slots, 1);
        assert_eq!(
            render_human(&state).lines().last(),
            Some("open: workers 1/2, nexus 1/1")
        );
        let rendered_json = render_json(&state);
        let parsed: serde_json::Value = serde_json::from_str(&rendered_json).unwrap();
        assert_eq!(parsed["nexus_open"], 1);
        assert_eq!(parsed["nexus_slots"], 1);
    }
}

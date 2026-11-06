mod args;
mod commands;
mod exit;

use std::{ffi::OsString, process::ExitCode};

use clap::error::ErrorKind;

struct Output {
    code: u8,
    text: String,
    stderr: bool,
}

fn failure(error: exit::Failure) -> Output {
    // `Reported` means the command already wrote its report to stdout
    // itself; the boundary's only job is to carry the exit code through,
    // never to add a stray blank line on stderr behind it.
    if let exit::Failure::Reported(code) = error {
        return Output {
            code,
            text: String::new(),
            stderr: false,
        };
    }
    let mut text = format!("{error}\n");
    // Every command's config load funnels a retired-role-key refusal
    // through `Failure::Config` to this one boundary (card
    // `20261102-b-4a5e3c`): name the fix once, here, rather than leaving
    // each read-only command to rediscover it from the raw `ConfigError`
    // text, or worse, to crash without ever reaching this mapping.
    if let exit::Failure::Config(message) = &error {
        if commands::is_retired_role_key_error(message) {
            text.push_str(commands::MIGRATE_FIX_LINE);
            text.push('\n');
        }
    }
    Output {
        code: error.code(),
        text,
        stderr: true,
    }
}

fn execute(argv: Vec<OsString>) -> Output {
    let matches = match args::command().try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(error) => {
            // Preserve the architecture's `dispatch foo` authorization diagnostic
            // even when the task/question is missing. Never execute partial matches.
            if error.kind() == ErrorKind::MissingRequiredArgument {
                if let Ok(partial) = args::command()
                    .ignore_errors(true)
                    .try_get_matches_from(&argv)
                {
                    if let Some((name, args)) = partial.subcommand() {
                        if let Err(error) = commands::authorization(name, args) {
                            return failure(error);
                        }
                    }
                }
            }
            let code = if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) {
                exit::OK
            } else {
                exit::UNKNOWN
            };
            return Output {
                code,
                text: error.to_string(),
                stderr: error.use_stderr(),
            };
        }
    };
    match commands::run(&matches) {
        Ok(()) => Output {
            code: exit::OK,
            text: String::new(),
            stderr: false,
        },
        Err(error) => failure(error),
    }
}

fn main() -> ExitCode {
    let output = execute(std::env::args_os().collect());
    if output.stderr {
        eprint!("{}", output.text);
    } else {
        print!("{}", output.text);
    }
    ExitCode::from(output.code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(words: &[&str]) -> Output {
        execute(
            std::iter::once("multiverse")
                .chain(words.iter().copied())
                .map(OsString::from)
                .collect(),
        )
    }

    #[test]
    fn clap_schema_is_consistent() {
        args::command().debug_assert();
    }

    #[test]
    fn help_and_version_succeed_but_usage_errors_are_two() {
        for argv in [
            &["--help"][..],
            &["--version"],
            &["dispatch", "--help"],
            &["backlog", "add", "--help"],
        ] {
            let out = run(argv);
            assert_eq!(out.code, 0, "{}", out.text);
            assert!(!out.stderr);
        }
        for argv in [
            &[][..],
            &["no-such-command"],
            &["board", "--bogus"],
            &["dispatch", "foo", "--yes"],
            &["council", "foo", "--yes"],
            &["worktrees", "foo", "--prune", "--rescue"],
        ] {
            let out = run(argv);
            assert_eq!(out.code, 2, "{argv:?}: {}", out.text);
            assert!(out.stderr);
        }
    }

    #[test]
    fn dispatch_and_council_require_explicit_yes() {
        for argv in [
            &["dispatch", "foo"][..],
            &["dispatch", "foo", "task"],
            &["council", "foo"],
            &["council", "foo", "question"],
        ] {
            let out = run(argv);
            assert_eq!(out.code, 1);
            assert!(out.text.contains("--yes"), "{}", out.text);
            assert!(!out.text.contains("not implemented"));
        }
        // `dispatch` is wired for real (task `20261102-fixer-d5b3b8`):
        // `--yes` and `--dry-run` both plan first, and `foo` is not a real
        // path on disk, so both refuse on that — never the stub's literal
        // wording, and never the same text twice (`--dry-run`'s refusal
        // never touches `config.toml`, `--yes`'s does once path resolution
        // passes it, but neither reaches here since `foo` never resolves).
        //
        // `council` joined `dispatch` here in task `20261102-fixer-62c3b7`:
        // its own `RealEngine` resolves the project path the same way
        // (`commands::council::resolve_user_path`), so `foo` refuses on
        // the same unresolvable-path fact, not the old generic stub text.
        for argv in [
            &["dispatch", "foo", "task", "--yes"][..],
            &["dispatch", "foo", "task", "--dry-run"],
            &["council", "foo", "question", "--yes"],
            &["council", "foo", "question", "--dry-run"],
        ] {
            let out = run(argv);
            assert_eq!(out.code, 1, "{}", out.text);
            assert!(!out.text.contains("not implemented"), "{}", out.text);
            assert!(out.text.contains("foo"), "{}", out.text);
        }
    }

    #[test]
    fn role_inputs_only_accept_new_worker_names() {
        // An isolated, nonexistent config path: `backlog` now does real
        // filework (unlike when this test was first written), and running
        // it without `--config` would fall back to the real default
        // `~/.config/multiverse/config.toml` — a developer's own machine,
        // not this test. `dispatch foo ...` never reaches config loading
        // for an unresolvable project either way, so isolating it here
        // costs nothing and matches the scratch-config pattern `args.rs`'s
        // own coverage test already uses for the same reason.
        let scratch = std::env::temp_dir().join(format!(
            "mv-cli-role-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&scratch).unwrap();
        let config_arg = scratch
            .join("alternate.toml")
            .to_string_lossy()
            .into_owned();

        for role in args::WORKER_ROLES {
            // `dispatch` is wired for real; `foo` is not a real path, so
            // every valid role still refuses on that, never on the role
            // itself and never with the stub's literal wording.
            let out = run(&["dispatch", "foo", "task", "--yes", "--role", role]);
            assert_eq!(out.code, 1, "{}", out.text);
            assert!(!out.text.contains("not implemented"), "{}", out.text);
            assert!(out.text.contains("foo"), "{}", out.text);

            for prefix in [
                &["backlog", "add", "title"][..],
                &[
                    "backlog",
                    "ask",
                    "question",
                    "--option",
                    "first",
                    "--option",
                    "second",
                    "--recommend",
                    "1",
                ],
            ] {
                let mut argv = prefix.to_vec();
                argv.extend(["--role", role, "--config", &config_arg]);
                let out = run(&argv);
                // A valid worker role must never be rejected as unknown —
                // whatever the command does next (dispatch still answers
                // its "not implemented" stub for an unresolvable project;
                // backlog now reaches real code and can legitimately also
                // land on exit 3 itself, via the missing scratch config
                // file rather than role rejection), so the message, not
                // the code, is what distinguishes the two exit-3 causes.
                assert!(!out.text.contains("unknown role"), "{}", out.text);
            }
        }
        for role in [
            "explorer",
            "fixer",
            "oracle",
            "librarian",
            "reviewer",
            "designer",
            "observer",
            "monitor",
            "orchestrator",
            "nexus",
            "unknown",
            "Scout",
            "",
        ] {
            for prefix in [
                &["dispatch", "foo", "task", "--yes"][..],
                &["backlog", "add", "title"],
                &[
                    "backlog",
                    "ask",
                    "question",
                    "--option",
                    "first",
                    "--option",
                    "second",
                    "--recommend",
                    "1",
                ],
            ] {
                let mut argv = prefix.to_vec();
                argv.extend(["--role", role, "--config", &config_arg]);
                let out = run(&argv);
                assert_eq!(out.code, 3, "{argv:?}: {}", out.text);
                assert!(out.text.contains("unknown role"));
            }
        }
        std::fs::remove_dir_all(&scratch).ok();
    }

    // Ported from the regression names in spec-cli.md section 7.
    #[test]
    fn dispatch_bare_wait_for_seat_before_positionals_is_no_longer_ambiguous() {
        let m = args::command()
            .try_get_matches_from([
                "multiverse",
                "dispatch",
                "--wait-for-seat",
                "foo",
                "task",
                "--yes",
            ])
            .unwrap();
        let dispatch = m.subcommand_matches("dispatch").unwrap();
        assert!(dispatch.get_flag("wait-for-seat"));
        assert_eq!(dispatch.get_one::<String>("path").unwrap(), "foo");
        assert_eq!(dispatch.get_one::<String>("task").unwrap(), "task");
    }

    #[test]
    fn dispatch_wait_for_seat_seconds_accepts_an_explicit_value() {
        // A valid value passes `--wait-for-seat-seconds` validation and
        // reaches the (real) engine, which refuses on `foo` not being a
        // real path — not on the flag.
        let out = run(&[
            "dispatch",
            "foo",
            "task",
            "--yes",
            "--wait-for-seat-seconds",
            "0.25",
        ]);
        assert!(
            !out.text.contains("--wait-for-seat-seconds"),
            "{}",
            out.text
        );
        assert!(out.text.contains("foo"), "{}", out.text);
        let out = run(&[
            "dispatch",
            "foo",
            "task",
            "--yes",
            "--wait-for-seat-seconds",
            "0",
        ]);
        assert!(
            !out.text.contains("--wait-for-seat-seconds"),
            "{}",
            out.text
        );
        assert!(out.text.contains("foo"), "{}", out.text);
    }

    #[test]
    fn dispatch_wait_for_seat_seconds_rejects_non_finite_or_negative() {
        for value in ["NaN", "inf", "-inf", "-1", "garbage"] {
            let out = run(&[
                "dispatch",
                "foo",
                "task",
                "--yes",
                &format!("--wait-for-seat-seconds={value}"),
            ]);
            assert_eq!(out.code, 1, "{}", out.text);
            assert!(out.text.contains("--wait-for-seat-seconds"));
            assert!(!out.text.contains("not implemented"));
            let separated = run(&[
                "dispatch",
                "foo",
                "task",
                "--yes",
                "--wait-for-seat-seconds",
                value,
            ]);
            assert_eq!(separated.code, 1, "{}", separated.text);
            assert!(!separated.text.contains("not implemented"));
        }
    }

    #[test]
    fn dispatch_and_council_timeout_reject_non_positive_or_non_finite() {
        for cmd in ["dispatch", "council"] {
            for value in ["0", "-1", "NaN", "inf", "-inf", "garbage"] {
                let out = run(&[
                    cmd,
                    "foo",
                    "description",
                    "--yes",
                    &format!("--timeout={value}"),
                ]);
                assert_eq!(out.code, 1, "{}", out.text);
                assert!(out.text.contains("--timeout"));
                assert!(!out.text.contains("not implemented"));
                let separated = run(&[cmd, "foo", "description", "--yes", "--timeout", value]);
                assert_eq!(separated.code, 1, "{}", separated.text);
                assert!(!separated.text.contains("not implemented"));
            }
            let out = run(&[cmd, "foo", "description", "--yes", "--timeout", "1.5"]);
            // Both commands are wired to a real engine now (`council`
            // joined `dispatch` in task `20261102-fixer-62c3b7`): a valid
            // `--timeout` reaches it, which refuses on `foo` not being a
            // real path — never on `--timeout`, and never the old stub
            // text.
            assert!(!out.text.contains("--timeout"), "{}", out.text);
            assert!(!out.text.contains("not implemented"), "{}", out.text);
            assert!(out.text.contains("foo"), "{}", out.text);
        }
    }

    #[test]
    fn a_retired_role_key_config_names_the_migrator_fix_for_a_read_only_command() {
        // card `20261102-b-4a5e3c`: a read-only command (here `projects`,
        // which never touches Herdr) must still exit cleanly — never
        // crash — on a Python-era config, and must name the fix rather
        // than leaving the operator to parse `ConfigError`'s raw text.
        let scratch = std::env::temp_dir().join(format!(
            "mv-cli-retired-role-key-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&scratch).unwrap();
        let config_path = scratch.join("config.toml");
        std::fs::write(
            &config_path,
            "[roles.fixer]\nwrite_scope = \"declared\"\n[roles.fixer.models]\n\
             claude = \"<<PLACEHOLDER: x>>\"\n",
        )
        .unwrap();

        let out = run(&["projects", "--config", config_path.to_str().unwrap()]);
        assert_eq!(out.code, 3, "{}", out.text);
        assert!(out.stderr);
        assert!(
            out.text.contains(
                "run: multiverse config migrate --dry-run, then multiverse config migrate"
            ),
            "{}",
            out.text
        );
        std::fs::remove_dir_all(&scratch).ok();
    }

    #[test]
    fn stop_and_sweep_help_preserve_busy_and_menu_explanations() {
        let stop = run(&["stop", "--help"]).text;
        assert!(stop.contains("idle or done") && stop.contains("unknown"));
        let sweep = run(&["sweep", "--help"]).text;
        assert!(sweep.contains("menu"));
        let check = run(&["check", "--help"]).text;
        for phrase in ["review policy", "prunable lanes", "stale seeds", "scratch"] {
            assert!(check.contains(phrase));
        }
    }
}

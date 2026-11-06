//! `scratch open|read [pane]|close [pane]`: isolated scratch panes.
//!
use std::path::Path;
use std::time::Duration;

use clap::ArgMatches;
use mv_engine::mv_herdr::argv;
use mv_engine::mv_herdr::envelope::{pane_id_from, CommandResult};
use mv_engine::runner::{Herdr, ProcessHerdr};
use mv_state::scratch::{HerdrPanes, ScratchPane, ScratchPanes, ScratchStore};

use crate::exit::Failure;

/// Matches `dispatch`'s own per-call budget for the same `pane split`/
/// `pane rename` primitives (`mv_engine::runner`'s private `COMMAND_TIMEOUT`).
const PANE_TIMEOUT: Duration = Duration::from_secs(30);

/// One `Herdr::call`, mapped into `Failure` and refused unless Herdr
/// itself reports success — the same two checks `dispatch`'s own private
/// `checked` helper runs before trusting a reply.
fn checked(herdr: &mut impl Herdr, command: argv::HerdrCommand) -> Result<CommandResult, Failure> {
    let result = herdr
        .call(&command, PANE_TIMEOUT)
        .map_err(|error| Failure::Operation(error.to_string()))?;
    if !result.ok() {
        return Err(Failure::Operation(format!(
            "{}: {}",
            command.purpose,
            result.message()
        )));
    }
    Ok(result)
}

/// Reserve private intent before splitting and keep the project lock until
/// completion. Every failure after a split closes the pane and removes intent.
fn do_open(project: &Path, root: &Path, task: &str, herdr: &mut impl Herdr) -> Result<(), Failure> {
    let _lock = mv_state::log::ProjectLog::new(project)
        .lock()
        .map_err(|error| Failure::Operation(error.to_string()))?;
    let store = ScratchStore::new(root).map_err(|error| Failure::Operation(error.to_string()))?;
    store
        .reserve(task)
        .map_err(|error| Failure::Operation(error.to_string()))?;
    let mut pane = String::new();
    let opened = (|| {
        let split = checked(herdr, argv::pane_split(root, &[], None, true, None, false))?;
        pane = pane_id_from(&split).map_err(|error| Failure::Operation(error.to_string()))?;
        let record =
            ScratchPane::new(task, &pane).map_err(|error| Failure::Operation(error.to_string()))?;
        checked(herdr, argv::pane_rename(&pane, &record.name))?;
        store
            .complete(&record)
            .map_err(|error| Failure::Operation(error.to_string()))
    })();
    if let Err(error) = opened {
        if !pane.is_empty() {
            let _ = checked(herdr, argv::pane_close(&pane));
        }
        let _ = store.unlink(task);
        return Err(error);
    }
    println!("opened scratch pane {pane}");
    Ok(())
}

pub fn run(matches: &ArgMatches) -> Result<(), Failure> {
    let pane = std::env::var("HERDR_PANE_ID").ok();
    let cwd = std::env::current_dir().map_err(|error| Failure::Operation(error.to_string()))?;
    let context = mv_state::scratch::resolve_context(pane.as_deref(), &cwd)
        .map_err(|error| Failure::Operation(error.to_string()))?;
    let (name, args) = matches.subcommand().expect("scratch requires a subcommand");
    let root = &context.lane_root;
    let task = &context.task;
    if name != "open" {
        let store =
            ScratchStore::new(root).map_err(|error| Failure::Operation(error.to_string()))?;
        let record = store
            .load(task)
            .map_err(|error| Failure::Operation(error.to_string()))?
            .ok_or_else(|| Failure::Operation("no scratch pane for task".into()))?;
        if let Some(requested) = args.get_one::<String>("pane") {
            if requested != &record.pane {
                return Err(Failure::Operation(format!(
                    "refusing scratch pane {requested}: the lane's recorded scratch pane is {}",
                    record.pane
                )));
            }
        }
    }
    match name {
        "open" => do_open(&context.project_root, root, task, &mut ProcessHerdr),
        "read" => do_read(root, task, &mut HerdrPanes),
        "close" => do_close(root, task, &mut HerdrPanes),
        _ => unreachable!("clap restricts scratch to open/read/close"),
    }
}

fn do_read(root: &Path, task: &str, panes: &mut impl ScratchPanes) -> Result<(), Failure> {
    let store = ScratchStore::new(root).map_err(|e| Failure::Operation(e.to_string()))?;
    let text = store
        .read(task, panes)
        .map_err(|e| Failure::Operation(e.to_string()))?;
    print!("{}", mv_board::layout::sanitize_multiline(&text));
    Ok(())
}

fn do_close(root: &Path, task: &str, panes: &mut impl ScratchPanes) -> Result<(), Failure> {
    let store = ScratchStore::new(root).map_err(|e| Failure::Operation(e.to_string()))?;
    let closed = store
        .close(task, panes)
        .map_err(|e| Failure::Operation(e.to_string()))?;
    if closed {
        println!("closed scratch pane for {task}");
    } else {
        println!("no scratch pane recorded for {task}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakePanes {
        label: String,
    }
    impl ScratchPanes for FakePanes {
        fn label(&mut self, _pane: &str) -> Result<String, mv_state::scratch::ScratchError> {
            Ok(self.label.clone())
        }
        fn read(&mut self, _pane: &str) -> Result<String, mv_state::scratch::ScratchError> {
            Ok("visible text".into())
        }
        fn close(&mut self, _pane: &str) -> Result<(), mv_state::scratch::ScratchError> {
            Ok(())
        }
    }

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("mv-cli-scratch-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const TASK: &str = "20261101-fixer-abcdef";

    /// Stands in for `ProcessHerdr`: a `pane split` reply carrying
    /// `pane_id`, and a `pane rename` that just succeeds, with either
    /// switchable to a failure to exercise `do_open`'s error paths.
    struct FakeHerdr {
        calls: Vec<(String, String)>,
        pane_id: String,
        fail_split: bool,
        fail_rename: bool,
        probe_root: Option<std::path::PathBuf>,
        remove_intent_on_rename: Option<std::path::PathBuf>,
    }

    impl Herdr for FakeHerdr {
        fn call(
            &mut self,
            command: &argv::HerdrCommand,
            _timeout: Duration,
        ) -> Result<CommandResult, mv_engine::runner::RunnerError> {
            self.calls
                .push((command.argv[1].clone(), command.argv[2].clone()));
            let failure = |message: &str| CommandResult {
                argv: command.argv.clone(),
                returncode: 1,
                stdout: String::new(),
                stderr: message.to_string(),
                payload: None,
                protocol_error: None,
            };
            if command.argv[2] == "split" {
                if let Some(root) = &self.probe_root {
                    use std::os::unix::fs::PermissionsExt;
                    let store = ScratchStore::new(root).unwrap();
                    let path = store.record_path(TASK).unwrap();
                    assert!(path.is_file(), "record must exist before pane split");
                    assert_eq!(
                        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                        0o600
                    );
                    assert_eq!(store.load(TASK).unwrap().unwrap().pane, "");
                    let root = root.clone();
                    assert!(
                        std::thread::spawn(move || {
                            mv_state::log::ProjectLog::new(root)
                                .lock_timeout(Duration::from_millis(20))
                                .is_err()
                        })
                        .join()
                        .unwrap(),
                        "project lock must cover pane split"
                    );
                }
            }
            if command.argv[2] == "rename" {
                if let Some(path) = &self.remove_intent_on_rename {
                    std::fs::remove_file(path).unwrap();
                }
            }
            match (command.argv[1].as_str(), command.argv[2].as_str()) {
                ("pane", "split") => Ok(if self.fail_split {
                    failure("split failed")
                } else {
                    CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: format!(
                            r#"{{"result":{{"pane":{{"pane_id":"{}"}}}}}}"#,
                            self.pane_id
                        )
                        .parse()
                        .ok(),
                        protocol_error: None,
                    }
                }),
                ("pane", "rename") => Ok(if self.fail_rename {
                    failure("rename failed")
                } else {
                    CommandResult {
                        argv: command.argv.clone(),
                        returncode: 0,
                        stdout: String::new(),
                        stderr: String::new(),
                        payload: None,
                        protocol_error: None,
                    }
                }),
                ("pane", "close") => Ok(CommandResult {
                    argv: command.argv.clone(),
                    returncode: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    payload: None,
                    protocol_error: None,
                }),
                other => panic!("unexpected herdr call: {other:?}"),
            }
        }
    }

    fn fake_herdr(pane_id: &str) -> FakeHerdr {
        FakeHerdr {
            calls: Vec::new(),
            pane_id: pane_id.into(),
            fail_split: false,
            fail_rename: false,
            probe_root: None,
            remove_intent_on_rename: None,
        }
    }

    #[test]
    fn do_open_reserves_a_private_record_under_the_project_lock_before_split() {
        let root = temp_dir("open-intent");
        let mut herdr = fake_herdr("w1:p9");
        herdr.probe_root = Some(root.clone());
        assert!(do_open(&root, &root, TASK, &mut herdr).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn do_open_creates_and_records_a_new_pane() {
        let root = temp_dir("open");
        let mut herdr = fake_herdr("w1:p9");

        assert!(do_open(&root, &root, TASK, &mut herdr).is_ok());

        assert_eq!(
            herdr.calls,
            vec![
                ("pane".to_string(), "split".to_string()),
                ("pane".to_string(), "rename".to_string()),
            ]
        );
        let store = ScratchStore::new(&root).unwrap();
        let record = store.load(TASK).unwrap().unwrap();
        assert_eq!(record.pane, "w1:p9");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn do_open_refuses_when_a_pane_is_already_recorded() {
        let root = temp_dir("open-dup");
        let store = ScratchStore::new(&root).unwrap();
        store
            .record(&ScratchPane::new(TASK, "w1:p1").unwrap())
            .unwrap();
        let mut herdr = fake_herdr("w1:p9");

        assert!(do_open(&root, &root, TASK, &mut herdr).is_err());
        assert!(herdr.calls.is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn do_open_does_not_record_on_a_split_failure() {
        let root = temp_dir("open-split-fail");
        let mut herdr = fake_herdr("w1:p9");
        herdr.fail_split = true;

        assert!(do_open(&root, &root, TASK, &mut herdr).is_err());
        let store = ScratchStore::new(&root).unwrap();
        assert!(store.load(TASK).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn do_open_does_not_record_on_a_rename_failure() {
        let root = temp_dir("open-rename-fail");
        let mut herdr = fake_herdr("w1:p9");
        herdr.fail_rename = true;

        assert!(do_open(&root, &root, TASK, &mut herdr).is_err());
        let store = ScratchStore::new(&root).unwrap();
        assert!(store.load(TASK).unwrap().is_none());
        assert_eq!(herdr.calls.last(), Some(&("pane".into(), "close".into())));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn completion_failure_closes_the_created_pane_and_unlinks_intent() {
        let root = temp_dir("completion-failure");
        let store = ScratchStore::new(&root).unwrap();
        let mut herdr = fake_herdr("w1:p9");
        herdr.remove_intent_on_rename = Some(store.record_path(TASK).unwrap());
        assert!(do_open(&root, &root, TASK, &mut herdr).is_err());
        assert!(store.load(TASK).unwrap().is_none());
        assert_eq!(herdr.calls.last(), Some(&("pane".into(), "close".into())));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reads_and_closes_an_owned_pane() {
        let root = temp_dir("read-close");
        let store = ScratchStore::new(&root).unwrap();
        let record = ScratchPane::new(TASK, "p1").unwrap();
        store.record(&record).unwrap();
        let mut panes = FakePanes {
            label: record.name.clone(),
        };

        assert!(do_read(&root, TASK, &mut panes).is_ok());
        assert!(do_close(&root, TASK, &mut panes).is_ok());
        // A second close finds nothing left to close, which is not an error.
        assert!(do_close(&root, TASK, &mut panes).is_ok());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reading_without_a_recorded_pane_refuses() {
        let root = temp_dir("no-record");
        let mut panes = FakePanes {
            label: "irrelevant".into(),
        };
        assert!(do_read(&root, TASK, &mut panes).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scratch_refuses_an_unregistered_lookalike_lane() {
        let root = temp_dir("lookalike");
        let lane = root.join(".multiverse/worktrees/20261101-fixer-abcdef/src");
        std::fs::create_dir_all(&lane).unwrap();
        assert_eq!(
            mv_state::scratch::resolve_context(None, &lane)
                .unwrap_err()
                .to_string(),
            "refusing scratch outside a dispatched lane: HERDR_PANE_ID is unset"
        );
        assert!(mv_state::scratch::resolve_context(Some("w1:p1"), &lane).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pane_text_printed_by_read_strips_osc_52_and_keeps_newlines() {
        let dirty = "visible\n\u{1b}]52;c;QUJDRA==\u{07}still here\n";
        let clean = mv_board::layout::sanitize_multiline(dirty);
        assert!(!clean.contains('\u{1b}'));
        assert!(!clean.contains('\u{07}'));
        assert_eq!(clean, "visible\nstill here\n");
    }
}

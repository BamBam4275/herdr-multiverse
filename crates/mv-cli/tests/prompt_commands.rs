//! Every `multiverse ...` invocation an agent is actually shown in
//! `prompts/**` must parse under the current CLI — a prompt teaching an
//! agent a flag the parser rejects is a cutover bug exactly like a CLI
//! regression would be, just discovered from the other direction.
//! `templates/**` names none (`templates/claude-account.sh.template` and
//! `templates/install-wrappers.sh` never mention `multiverse`), so this
//! file's coverage is `prompts/**` only.
//!
//! Extracted by hand (`grep -rnoE '`multiverse[^`]*`' prompts/`, plus a
//! second pass for spans a markdown line-wrap split across lines) from
//! every role prompt, column prompt, and skill file that mentions a
//! `multiverse` command, as of task `20261102-fixer-4e4057`. Most of
//! these appear inline in prose, not as copy-pasteable commands, and
//! several are genuinely incomplete there on purpose — `` `multiverse
//! tasks --supersede` `` names a flag mid-sentence ("correct the record
//! with ... naming the duplicate"), not a full command. Testing that
//! fragment literally would only prove a sentence is a sentence; each
//! placeholder (`<title>`, `SEQ`, `"..."`, `<path>`, a dangling flag with
//! its value elided by the prose around it) is filled with a concrete
//! stand-in so what is actually tested is the complete command the prose
//! means an agent to run.
//!
//! Run with `cargo test -p mv-cli --test prompt_commands --locked`.

// This test only needs `Sandbox::new`/`cli`/`stderr`; `support`'s other
// helpers (`herdr`, `dispatch`, `snapshot`, `task_dirs`, `seed_task`) exist
// for `cli_workflows.rs`'s own coverage and would otherwise warn as unused
// in this binary's own compilation unit (each integration test compiles
// `mod support` fresh).
#[allow(dead_code)]
mod support;

use support::Sandbox;

/// Clap's own fixed suffix on a genuine parse/usage error — never
/// written by this crate's own error formatting (`src/exit.rs`'s
/// `Failure::Display` has no code path that produces it). Checking for
/// this exact phrase, rather than the exit code alone, separates "the
/// CLI rejected this argv shape" (this test's only concern) from a
/// business-level refusal this sandbox's minimal fixture is expected to
/// hit for some commands (no such project, no reviewer configured, and
/// so on) — exit code 2 alone is ambiguous, since `presence`'s honest
/// "status: unknown" answer is also a `Reported(2)`, not a parse
/// failure (`src/exit.rs`'s own doc comment on `Failure::Reported`).
const USAGE_ERROR_MARKER: &str = "For more information, try '--help'.";

struct Case {
    /// `file:line` of the prompt text this shape was read from.
    source: &'static str,
    args: Vec<String>,
}

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn cases(project: &str) -> Vec<Case> {
    vec![
        Case {
            source: "prompts/role-scout.md:35, prompts/role-builder.md:35, prompts/README.md:46",
            args: s(&["start"]),
        },
        Case {
            // Reworded with the same shape across prompts/role-{scout,seer,
            // watcher,archivist,artisan,critic,builder}.md (file-a-finding
            // boilerplate); `<title>`/the `--why` text differ only in
            // prose, never in flag shape.
            source: "prompts/role-scout.md:53 (and 6 sibling role prompts)",
            args: s(&[
                "backlog",
                "add",
                "Placeholder finding title",
                "--why",
                "Placeholder: what, where, what it costs",
            ]),
        },
        Case {
            // Every sibling mentions `--role` as optional ("--role if you
            // know whose it is"); this case is this test's own addition
            // covering that combination, not a second literal quote.
            source: "prompts/role-scout.md:53 (and siblings) + their `--role` addendum",
            args: s(&[
                "backlog",
                "add",
                "Placeholder finding title",
                "--why",
                "Placeholder: what, where, what it costs",
                "--role",
                "scout",
            ]),
        },
        Case {
            source: "prompts/role-scout.md:138, prompts/role-builder.md:200",
            args: s(&["notify"]),
        },
        Case {
            source: "prompts/role-critic.md:41,106; prompts/role-nexus.md:238,404",
            args: s(&["sweep"]),
        },
        Case {
            source: "prompts/role-nexus.md:125,237",
            args: s(&["board"]),
        },
        Case {
            source: "prompts/role-nexus.md:194",
            args: s(&["board", "--limit", "200"]),
        },
        Case {
            source: "prompts/role-nexus.md:139",
            args: s(&["workers"]),
        },
        Case {
            // Prose names the flag mid-sentence ("correct the record with
            // `multiverse tasks --supersede`, naming the duplicate"); the
            // value it elides is filled in here.
            source: "prompts/role-nexus.md:171",
            args: s(&["tasks", "--supersede", "20261101-fixer-aaaaaa"]),
        },
        Case {
            source: "prompts/role-nexus.md:177",
            args: s(&["tasks", "--acknowledge", "20261101-fixer-bbbbbb"]),
        },
        Case {
            // `--help` is clap's own flag, not one this crate defines;
            // the expected outcome is `DisplayHelp` (exit 0), asserted
            // specially below rather than via `USAGE_ERROR_MARKER`.
            source: "prompts/role-nexus.md:231",
            args: s(&["activity", "inbox", "--help"]),
        },
        Case {
            source: "prompts/role-nexus.md:242",
            args: s(&["activity", "inbox"]),
        },
        Case {
            // "Ask it with `multiverse backlog ask`, not by hand-writing
            // the shape above" — references the subcommand, not a full
            // invocation; its required `question`, and the `--option`
            // pair plus `--recommend` index the surrounding prose (same
            // file, lines 359-373) teaches every menu needs, are filled
            // in here.
            source: "prompts/role-nexus.md:370",
            args: s(&[
                "backlog",
                "ask",
                "Placeholder question?",
                "--option",
                "Placeholder option A",
                "--option",
                "Placeholder option B",
                "--recommend",
                "1",
            ]),
        },
        Case {
            source: "prompts/role-nexus.md:397",
            args: s(&["activity", "reply", "42", "Placeholder reply text"]),
        },
        Case {
            source: "prompts/role-nexus.md:453",
            args: s(&["presence"]),
        },
        Case {
            source: "prompts/role-nexus.md:459",
            args: s(&["activity", "send", "multiverse", "Placeholder message text"]),
        },
        Case {
            source: "prompts/role-nexus.md:649,680",
            args: s(&["review-policy"]),
        },
        Case {
            source: "prompts/role-nexus.md:685",
            args: s(&["review-policy", "--lane", "20261101-fixer-cccccc"]),
        },
        Case {
            source: "prompts/role-nexus.md:783",
            args: s(&["stop"]),
        },
        Case {
            // "Reclaim a finished lane with `multiverse worktrees
            // --prune`" names the flag, not the path argument every real
            // invocation needs; filled in with this sandbox's own project.
            source: "prompts/role-nexus.md:788, prompts/skills/worktrees.md:37",
            args: s(&["worktrees", project, "--prune"]),
        },
        Case {
            source: "prompts/role-nexus.md:824, prompts/skills/worktrees.md:74",
            args: s(&["worktrees", project]),
        },
        Case {
            source: "prompts/role-nexus.md:826",
            args: s(&["worktrees", project, "--restore", "20261101-fixer-dddddd"]),
        },
        Case {
            source: "prompts/role-nexus.md:834",
            args: s(&["worktrees", project, "--prune", "--dry-run"]),
        },
        Case {
            // "`multiverse scratch open|read|close`" names three
            // subcommands with alternation notation; tested separately.
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (open)",
            args: s(&["scratch", "open"]),
        },
        Case {
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (read)",
            args: s(&["scratch", "read"]),
        },
        Case {
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (close)",
            args: s(&["scratch", "close"]),
        },
        Case {
            source: "prompts/skills/multiverse.md:15",
            args: s(&["check"]),
        },
        Case {
            source: "prompts/skills/multiverse.md:16",
            args: s(&["accounts"]),
        },
    ]
}

#[test]
fn every_prompt_documented_command_parses() {
    let sandbox = Sandbox::new();
    let project = sandbox.project.to_str().unwrap().to_owned();

    for case in cases(&project) {
        if case.args.first().map(String::as_str) == Some("activity")
            && case.args.get(1).map(String::as_str) == Some("inbox")
            && case.args.contains(&"--help".to_string())
        {
            let argv: Vec<&str> = case.args.iter().map(String::as_str).collect();
            let output = sandbox.cli(&argv);
            assert_eq!(
                output.status.code(),
                Some(0),
                "{}: {:?} must be accepted as --help, got:\n{}",
                case.source,
                case.args,
                support::stderr(&output)
            );
            continue;
        }

        let argv: Vec<&str> = case.args.iter().map(String::as_str).collect();
        let output = sandbox.cli(&argv);
        let err = support::stderr(&output);
        assert!(
            !err.contains(USAGE_ERROR_MARKER),
            "{}: {:?} was rejected by argument parsing:\n{}",
            case.source,
            case.args,
            err
        );
    }
}

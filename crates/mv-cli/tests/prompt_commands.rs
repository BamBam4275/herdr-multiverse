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

use std::fs;
use std::path::Path;

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
    /// The literal substring (or distinctive first-token run) `source`'s
    /// cited line(s) must contain — checked by
    /// `prompt_command_labels_point_at_their_command` below, so a prompt
    /// edit that shifts lines out from under a stale `source` fails loudly
    /// instead of silently.
    expect: &'static str,
    args: Vec<String>,
}

fn s(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| a.to_string()).collect()
}

fn cases(project: &str) -> Vec<Case> {
    vec![
        Case {
            source: "prompts/role-scout.md:35, prompts/role-builder.md:35, prompts/README.md:46",
            expect: "multiverse start",
            args: s(&["start"]),
        },
        Case {
            // Reworded with the same shape across prompts/role-{scout,seer,
            // watcher,archivist,artisan,critic,builder}.md (file-a-finding
            // boilerplate); `<title>`/the `--why` text differ only in
            // prose, never in flag shape.
            source: "prompts/role-scout.md:53 (and 6 sibling role prompts)",
            expect: "multiverse backlog add",
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
            expect: "multiverse backlog add",
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
            expect: "multiverse notify",
            args: s(&["notify"]),
        },
        Case {
            source: "prompts/role-critic.md:41,106; prompts/role-nexus.md:253,448",
            expect: "multiverse sweep",
            args: s(&["sweep"]),
        },
        Case {
            source: "prompts/role-nexus.md:126,252",
            expect: "multiverse board",
            args: s(&["board"]),
        },
        Case {
            source: "prompts/role-nexus.md:209",
            expect: "multiverse board --limit 200",
            args: s(&["board", "--limit", "200"]),
        },
        Case {
            source: "prompts/role-nexus.md:140",
            expect: "multiverse workers",
            args: s(&["workers"]),
        },
        Case {
            // Prose names the flag mid-sentence ("correct the record with
            // `multiverse tasks --supersede`, naming the duplicate"); the
            // value it elides is filled in here.
            source: "prompts/role-nexus.md:186",
            expect: "multiverse tasks --supersede",
            args: s(&["tasks", "--supersede", "20261101-fixer-aaaaaa"]),
        },
        Case {
            source: "prompts/role-nexus.md:192",
            expect: "multiverse tasks --acknowledge",
            args: s(&["tasks", "--acknowledge", "20261101-fixer-bbbbbb"]),
        },
        Case {
            // `--help` is clap's own flag, not one this crate defines;
            // the expected outcome is `DisplayHelp` (exit 0), asserted
            // specially below rather than via `USAGE_ERROR_MARKER`.
            source: "prompts/role-nexus.md:246",
            expect: "multiverse activity inbox --help",
            args: s(&["activity", "inbox", "--help"]),
        },
        Case {
            source: "prompts/role-nexus.md:234-235,257",
            expect: "multiverse activity inbox",
            args: s(&["activity", "inbox"]),
        },
        Case {
            // "Ask it with `multiverse backlog ask`, not by hand-writing
            // the shape above" — references the subcommand, not a full
            // invocation; its required `question`, and the `--option`
            // pair plus `--recommend` index the surrounding prose (same
            // file, lines 359-373) teaches every menu needs, are filled
            // in here.
            source: "prompts/role-nexus.md:407",
            expect: "multiverse backlog ask",
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
            source: "prompts/role-nexus.md:441",
            expect: "multiverse activity reply SEQ",
            args: s(&["activity", "reply", "42", "Placeholder reply text"]),
        },
        Case {
            source: "prompts/role-nexus.md:452",
            expect: "PROJECT#SEQ",
            args: s(&[
                "activity",
                "reply",
                "multiverse#42",
                "Placeholder reply text",
            ]),
        },
        Case {
            source: "prompts/role-nexus.md:461",
            expect: "multiverse activity show SEQ",
            args: s(&["activity", "show", "42"]),
        },
        Case {
            source: "prompts/role-nexus.md:461",
            expect: "multiverse activity show SEQ",
            args: s(&["activity", "show", "multiverse#42"]),
        },
        Case {
            source: "prompts/role-nexus.md:520",
            expect: "multiverse presence",
            args: s(&["presence"]),
        },
        Case {
            source: "prompts/role-nexus.md:421-422,526",
            expect: "multiverse activity send multiverse",
            args: s(&["activity", "send", "multiverse", "Placeholder message text"]),
        },
        Case {
            // "use `multiverse activity send PROJECT "..." --ask`" names
            // the flag this task's notify-hook warning now hangs off of
            // (card `20261115-b-51cb75`); `PROJECT`/`"..."` are filled in
            // the same way the plain-send case above fills them.
            source: "prompts/role-nexus.md:428-429",
            expect: "multiverse activity send PROJECT",
            args: s(&[
                "activity",
                "send",
                "multiverse",
                "Placeholder message text",
                "--ask",
            ]),
        },
        Case {
            source: "prompts/role-nexus.md:716,747",
            expect: "multiverse review-policy",
            args: s(&["review-policy"]),
        },
        Case {
            source: "prompts/role-nexus.md:752",
            expect: "multiverse review-policy --lane",
            args: s(&["review-policy", "--lane", "20261101-fixer-cccccc"]),
        },
        Case {
            source: "prompts/role-nexus.md:850",
            expect: "multiverse stop",
            args: s(&["stop"]),
        },
        Case {
            // "Reclaim a finished lane with `multiverse worktrees
            // --prune`" names the flag, not the path argument every real
            // invocation needs; filled in with this sandbox's own project.
            source: "prompts/role-nexus.md:855, prompts/skills/worktrees.md:37",
            expect: "multiverse worktrees <path> --prune",
            args: s(&["worktrees", project, "--prune"]),
        },
        Case {
            source: "prompts/role-nexus.md:892, prompts/skills/worktrees.md:74",
            expect: "multiverse worktrees <path>",
            args: s(&["worktrees", project]),
        },
        Case {
            source: "prompts/role-nexus.md:894",
            expect: "multiverse worktrees <path> --restore",
            args: s(&["worktrees", project, "--restore", "20261101-fixer-dddddd"]),
        },
        Case {
            source: "prompts/role-nexus.md:902",
            expect: "multiverse worktrees <path> --prune --dry-run",
            args: s(&["worktrees", project, "--prune", "--dry-run"]),
        },
        Case {
            source: "prompts/role-nexus.md:917, prompts/skills/worktrees.md:96",
            expect: "--prune --reap --dry-run",
            args: s(&["worktrees", project, "--prune", "--reap", "--dry-run"]),
        },
        Case {
            source: "prompts/role-nexus.md:335",
            expect: "tasks <project> --reclaimable --reap",
            args: s(&["tasks", "--reclaimable", "--reap"]),
        },
        Case {
            // "Land a finished lane with `multiverse worktrees <path>
            // --merge <task>`" (and the merge-step parenthetical at 799)
            // names the flag and the task; the path is filled in with
            // this sandbox's own project, matching the --prune case.
            source: "prompts/role-nexus.md:865,922",
            expect: "worktrees <path> --merge <task>",
            args: s(&["worktrees", project, "--merge", "20261101-fixer-eeeeee"]),
        },
        Case {
            // "Add `--ask` (`multiverse activity broadcast "..." --ask`)" —
            // card `20261115-b-d4cf40`; `"..."` filled in the same way the
            // plain-send placeholder above is.
            source: "prompts/role-nexus-home.md:53-56",
            expect: "multiverse activity broadcast",
            args: s(&["activity", "broadcast", "Placeholder message text", "--ask"]),
        },
        Case {
            // "`multiverse scratch open|read|close`" names three
            // subcommands with alternation notation; tested separately.
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (open)",
            expect: "multiverse scratch open|read|close",
            args: s(&["scratch", "open"]),
        },
        Case {
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (read)",
            expect: "multiverse scratch open|read|close",
            args: s(&["scratch", "read"]),
        },
        Case {
            source: "prompts/role-artisan.md:126, prompts/role-builder.md:147 (close)",
            expect: "multiverse scratch open|read|close",
            args: s(&["scratch", "close"]),
        },
        Case {
            source: "prompts/skills/multiverse.md:15",
            expect: "multiverse check",
            args: s(&["check"]),
        },
        Case {
            source: "prompts/skills/multiverse.md:16",
            expect: "multiverse accounts",
            args: s(&["accounts"]),
        },
        Case {
            // Bare mention — card `20261116-b-f0612d` (Lane A, appended at
            // EOF so no other line reference above shifted). Unlike the
            // other bare subcommand cases above, `down`/`up` is not safe
            // to run unadorned here: with no open Nexus to wrap and no
            // sidecar ever written, the real handler's default 1800s wait
            // would make this one case run for 30 minutes before this
            // test could finish. `--dry-run` returns immediately and
            // exercises the identical argument shape; neither prompt
            // mention is itself a dry-run instruction, so this fills in
            // the one flag needed to make the fragment safe to execute,
            // the same way other cases above fill in an elided value.
            source: "prompts/role-nexus.md:1020, prompts/role-nexus-home.md:102",
            expect: "multiverse down",
            args: s(&["down", "--dry-run"]),
        },
        Case {
            source: "prompts/role-nexus.md:1038, prompts/role-nexus-home.md:113",
            expect: "multiverse up",
            args: s(&["up", "--dry-run"]),
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

/// Splits a `source` label into its `(prompts/<file>.md, 1-indexed lines)`
/// groups. A label cites one or more files as `prompts/<path>.md:<spec>`,
/// `<spec>` being digits, `-` (a range) and `,` (more lines in the same
/// file) with no spaces; free-form text after the spec — "(and 6 sibling
/// role prompts)", "(open)", and the like — is exactly what stops the scan,
/// so it is ignored rather than parsed.
fn parse_source(source: &str) -> Vec<(&str, Vec<usize>)> {
    let mut groups = Vec::new();
    let mut rest = source;
    while let Some(start) = rest.find("prompts/") {
        rest = &rest[start..];
        let colon = rest.find(':').unwrap_or_else(|| {
            panic!("source label missing ':line' after a 'prompts/' span: {source}")
        });
        let file = &rest[..colon];
        let after = &rest[colon + 1..];
        let spec_len = after
            .find(|c: char| !(c.is_ascii_digit() || c == '-' || c == ','))
            .unwrap_or(after.len());
        let spec = &after[..spec_len];
        let mut lines = Vec::new();
        for part in spec.split(',') {
            if part.is_empty() {
                continue;
            }
            match part.split_once('-') {
                Some((a, b)) => {
                    let a: usize = a
                        .parse()
                        .unwrap_or_else(|_| panic!("bad range start in {source}"));
                    let b: usize = b
                        .parse()
                        .unwrap_or_else(|_| panic!("bad range end in {source}"));
                    lines.extend(a..=b);
                }
                None => lines.push(
                    part.parse()
                        .unwrap_or_else(|_| panic!("bad line number in {source}")),
                ),
            }
        }
        groups.push((file, lines));
        rest = &after[spec_len..];
    }
    groups
}

/// A `source` label is only useful as long as it still points at the
/// command it was extracted from. Every case's cited line(s), read fresh
/// from the current `prompts/**` file, must contain `expect` — so a prompt
/// edit that shifts lines out from under a stale label fails here, loudly,
/// naming the case and the line numbers checked, instead of quietly
/// documenting the wrong line forever.
#[test]
fn prompt_command_labels_point_at_their_command() {
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    for case in cases("placeholder-project") {
        let groups = parse_source(case.source);
        assert!(
            !groups.is_empty(),
            "{}: no 'prompts/<file>.md:<line>' span found to check",
            case.source
        );
        for (file, lines) in groups {
            let path = repo_root.join(file);
            let text = fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: cannot read {}: {e}", case.source, path.display()));
            let file_lines: Vec<&str> = text.lines().collect();
            let joined = lines
                .iter()
                .map(|&n| {
                    *file_lines.get(n - 1).unwrap_or_else(|| {
                        panic!(
                            "{}: {file} has no line {n} (it has {} lines)",
                            case.source,
                            file_lines.len()
                        )
                    })
                })
                .collect::<Vec<_>>()
                .join(" ");
            assert!(
                joined.contains(case.expect),
                "{}: {file} line(s) {lines:?} no longer contain {:?} — label is stale:\n{joined}",
                case.source,
                case.expect
            );
        }
    }
}

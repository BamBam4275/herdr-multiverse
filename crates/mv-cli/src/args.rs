//! The public CLI surface. Business rules belong in the engine, not this parser.

use clap::{Arg, ArgAction, ArgGroup, Command};

pub const WORKER_ROLES: &[&str] = &[
    "scout",
    "builder",
    "archivist",
    "critic",
    "seer",
    "council",
    "artisan",
    "watcher",
];

fn flag(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .action(ArgAction::SetTrue)
        .help(help)
}

fn value(name: &'static str, label: &'static str, help: &'static str) -> Arg {
    Arg::new(name).long(name).value_name(label).help(help)
}

fn positional(name: &'static str, required: bool, help: &'static str) -> Arg {
    Arg::new(name).required(required).help(help)
}

fn many(name: &'static str, help: &'static str) -> Arg {
    positional(name, false, help).num_args(0..)
}

fn repeat(name: &'static str, label: &'static str, help: &'static str) -> Arg {
    value(name, label, help).action(ArgAction::Append)
}

fn role() -> Arg {
    // Validate at the exit boundary: unknown roles are needs-config (3), not usage (2).
    value(
        "role",
        "ROLE",
        "Role: scout, builder, archivist, critic, seer, council, artisan, watcher",
    )
}

fn yes() -> Arg {
    flag("yes", "Explicitly authorize the operation")
}

fn dry_run() -> Arg {
    flag("dry-run", "Show the plan without performing the operation")
}

fn json() -> Arg {
    flag("json", "Emit machine-readable JSON")
}

fn read_only() -> Arg {
    flag("read-only", "Read state without reconciling or writing it")
}

fn no_reconcile() -> Arg {
    flag("no-reconcile", "Do not reconcile task state")
}

fn timeout() -> Arg {
    value(
        "timeout",
        "SECONDS",
        "Positive, finite operation timeout in seconds",
    )
    .allow_hyphen_values(true)
}

fn projects() -> Arg {
    many(
        "projects",
        "Project names or paths (defaults to the current project)",
    )
}

fn group(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).subcommand_required(true)
}

pub fn command() -> Command {
    Command::new("multiverse")
        .version(env!("CARGO_PKG_VERSION"))
        .about("Coordinates Claude, Codex, and Grok through isolated Herdr worktrees.")
        .subcommand_required(true)
        .arg(value("config", "PATH", "Use this configuration file").global(true))
        .subcommand(
            Command::new("dispatch")
                .about("Dispatch a scoped task to a worker; execution requires --yes")
                .arg(positional("path", true, "Project path"))
                .arg(positional("task", true, "Task description"))
                .arg(dry_run()).arg(yes())
                .arg(flag("force", "Override dispatch safeguards where permitted"))
                .arg(timeout())
                .arg(flag("wait", "Wait for the task to finish"))
                .arg(flag("wait-for-seat", "Wait for an eligible seat (does not take a value)"))
                .arg(value("wait-for-seat-seconds", "SECONDS", "Finite, non-negative seat wait limit").allow_hyphen_values(true))
                .arg(role())
                .arg(value("column", "COLUMN", "Task board column").value_parser(["Explore", "Execute", "Review"]))
                .arg(flag("full", "Include full context in the brief"))
                .arg(repeat("reviewing", "TASK_ID", "Lane to review; repeat for a batch review"))
                .arg(repeat("exclude-vendor", "VENDOR", "Vendor to exclude from selection; repeatable"))
                .arg(repeat(
                    "commits",
                    "SHA",
                    "Exact commit to review, oldest first; repeatable. Only used with exactly one --reviewing id",
                ))
                .arg(value("title", "TITLE", "Task title"))
                .arg(repeat("input", "PATH", "Input file to attach to the brief; repeatable"))
                .arg(repeat("scope", "PATH", "Allowed write scope; repeatable"))
                .arg(value("into", "TASK_ID", "Adopt an existing lane"))
                .arg(no_reconcile()),
        )
        .subcommand(
            group("halt", "Control the fleet halt")
                .subcommand(Command::new("on").about("Halt new work").arg(value("reason", "TEXT", "Reason for halting").default_value("operator requested")))
                .subcommand(Command::new("off").about("Clear the fleet halt"))
                .subcommand(Command::new("status").about("Show fleet halt status")),
        )
        .subcommand(
            group("scratch", "Manage isolated scratch panes")
                .subcommand(Command::new("open").about("Open a scratch pane"))
                .subcommand(Command::new("read").about("Read scratch output").arg(positional("pane", false, "Scratch pane ID")))
                .subcommand(Command::new("close").about("Close a scratch pane").arg(positional("pane", false, "Scratch pane ID"))),
        )
        .subcommand(
            Command::new("start").about("Start Nexus for selected projects")
                .arg(projects()).arg(flag("all", "Start all configured projects"))
                .arg(flag("reseed", "Reseed existing Nexus sessions"))
                .arg(flag("dashboard", "Open a dashboard watch pane")),
        )
        .subcommand(Command::new("projects").about("List configured projects").arg(flag("names", "Print only project names")))
        .subcommand(
            Command::new("stop").about("Stop project sessions (all configured projects by default)")
                .arg(many("projects", "Project names (defaults to all configured projects)"))
                .arg(flag("force", "Stop even when an agent has a status other than idle or done, including unknown")),
        )
        .subcommand(
            Command::new("add").about("Register an existing project")
                .arg(positional("path", true, "Project directory"))
                .arg(value("name", "NAME", "Configured project name")).arg(dry_run()),
        )
        .subcommand(
            Command::new("new").about("Create, register, and start a project")
                .arg(positional("name", true, "New project name"))
                .arg(value("path", "PATH", "New project directory")).arg(yes()).arg(dry_run()),
        )
        .subcommand(tasks())
        .subcommand(board())
        .subcommand(
            Command::new("council").about("Ask Council; execution requires --yes")
                .arg(positional("path", true, "Project path"))
                .arg(positional("question", true, "Question for Council"))
                .arg(dry_run()).arg(yes()).arg(timeout()),
        )
        .subcommand(accounts())
        .subcommand(
            Command::new("seat").about("Inspect an account seat")
                .subcommand(Command::new("show").about("Show seat state").arg(positional("seat", true, "Seat name")).arg(json())),
        )
        .subcommand(
            Command::new("worktrees").about("Inspect, prune, rescue, or restore task worktrees")
                .arg(positional("path", true, "Project path"))
                .arg(flag("prune", "Prune eligible worktrees").conflicts_with("rescue"))
                .arg(flag("rescue", "Rescue worktrees").conflicts_with("prune"))
                .arg(flag("force", "Override permitted prune guards"))
                .arg(yes()).arg(dry_run()).arg(flag("deep", "Inspect deeper worktree history"))
                .arg(value("restore", "TASK_ID", "Restore an archived lane branch")),
        )
        .subcommand(
            Command::new("check").about("Report readiness (exit 0 ready, 3 needs configuration)")
                .arg(flag("dispatch-only", "Check only dispatch readiness"))
                .arg(no_reconcile()).arg(read_only())
                .arg(flag("verbose", "Include review policy, prunable lanes, stale seeds, and scratch advisory scans")),
        )
        .subcommand(Command::new("doctor").about("Check installation prerequisites (exit 0 pass, 1 fail)"))
        .subcommand(
            Command::new("import-python")
                .about("Copy a Python install's config and fleet state into this install's config/state directories (copies only; never moves or deletes the source)")
                .arg(value("from", "DIR", "Source directory to copy from (default: $HOME/.config/multiverse)"))
                .arg(dry_run())
                .arg(flag("force", "Overwrite an existing destination file")),
        )
        .subcommand(Command::new("presence").about("Report operator presence (exit 2 if unknown)").arg(json()))
        .subcommand(Command::new("health").about("Report fleet health").arg(json()))
        .subcommand(Command::new("review-policy").about("Show effective review policy").arg(value("lane", "TASK_ID", "Resolve policy for this lane")))
        .subcommand(
            Command::new("sweep").about("Inspect and maintain fleet state")
                .arg(dry_run()).arg(flag("tick", "Run one maintenance tick; skip agents waiting at a menu"))
                .arg(flag("stale-only", "Process only stale tasks"))
                .arg(flag("verbose", "Include detailed diagnostics")),
        )
        .subcommand(
            Command::new("notify").about("Queue or flush operator notifications")
                .arg(value("project", "PROJECT", "Source project"))
                .arg(value("title", "TITLE", "Notification title"))
                .arg(value("message", "TEXT", "Notification message"))
                .arg(value("priority", "PRIORITY", "Delivery priority").value_parser(["low", "default", "high", "urgent"]).default_value("default"))
                .arg(repeat(
                    "tag",
                    "TAG",
                    "An ntfy tag; repeat for more than one",
                ))
                .arg(flag("flush", "Flush queued notifications"))
                .arg(value("episode-key", "KEY", "Deduplication episode key"))
                .arg(value(
                    "episode-state",
                    "PATH",
                    "JSON file holding --episode-key's {first_seen, alerted} entry; requires --episode-key",
                )),
        )
        .subcommand(
            Command::new("inspect").about("Inspect project context")
                .arg(positional("paths", true, "Paths to inspect (one or two)").num_args(1..))
                .arg(value("reason", "TEXT", "Reason for inspection").required(true))
                .arg(value(
                    "lines",
                    "START:END",
                    "Line range START:END, N, or N-M (exactly one of --lines/--match)",
                ))
                .arg(value("match", "TEXT", "Show lines containing this text (exactly one of --lines/--match)"))
                .group(ArgGroup::new("selector").args(["lines", "match"]).required(true)),
        )
        .subcommand(Command::new("result").about("Read a task result").arg(positional("task_id", true, "Task ID")))
        .subcommand(activity())
        .subcommand(backlog())
        .subcommand(
            group("directives", "Manage standing project directives")
                .subcommand(Command::new("set").about("Set a directive")
                    .arg(projects()).arg(value("body", "TEXT", "Directive body").required(true))
                    .arg(value("until", "TIMESTAMP", "Directive expiration time")))
                .subcommand(Command::new("show").about("Show project directives").arg(projects())),
        )
        .subcommand(Command::new("workers").about("List worker sessions"))
        .subcommand(
            group("config", "Manage the configuration file")
                .subcommand(
                    Command::new("migrate")
                        .about("Rewrite retired role keys to their current names in place")
                        .arg(dry_run())
                        .arg(flag(
                            "backup",
                            "Write a timestamped backup, then migrate in place",
                        )),
                ),
        )
}

fn tasks() -> Command {
    Command::new("tasks")
        .about("Inspect or update project tasks")
        .arg(projects())
        .arg(flag("reconcile", "Reconcile task state").conflicts_with("no-reconcile"))
        .arg(no_reconcile())
        .arg(read_only())
        .arg(value("task", "TASK_ID", "Select one task"))
        .arg(flag("close-stranded", "Close stranded tasks"))
        .arg(flag("reclaimable", "Show reclaimable tasks"))
        .arg(value("supersede", "TASK_ID", "Task to supersede"))
        .arg(
            value(
                "supersedes",
                "ORDINAL",
                "The target outcome record's 1-based routing-log line ordinal",
            )
            .value_parser(clap::value_parser!(i64)),
        )
        .arg(value("status", "STATUS", "--supersede's corrected status"))
        .arg(value(
            "state",
            "STATE",
            "--supersede's corrected outcome state label",
        ))
        .arg(value(
            "reason",
            "TEXT",
            "--supersede's evidence for the correction",
        ))
        .arg(
            value(
                "revisions",
                "N",
                "--supersede's revision count to record on the correction",
            )
            .value_parser(clap::value_parser!(i64))
            .default_value("0"),
        )
        .arg(value("acknowledge", "TASK_ID", "Task to acknowledge"))
        .arg(
            value(
                "acknowledges",
                "ORDINAL",
                "The target outcome record's 1-based routing-log line ordinal",
            )
            .value_parser(clap::value_parser!(i64)),
        )
        .arg(yes())
}

/// `--watch [SECONDS]`: omitted flag is off; bare `--watch` uses `[board].refresh_seconds`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum WatchArg {
    FromConfig,
    Seconds(f64),
}

fn parse_watch_arg(value: &str) -> Result<WatchArg, String> {
    if value == "__from_config__" {
        return Ok(WatchArg::FromConfig);
    }
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("--watch: not a number: {value:?}"))?;
    if !seconds.is_finite() || seconds <= 0.0 {
        return Err(format!(
            "--watch must be a finite, positive number of seconds, got {value:?}"
        ));
    }
    Ok(WatchArg::Seconds(seconds))
}

/// `--limit {auto,N}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BoardLimitArg {
    Auto,
    Count(u32),
}

fn parse_board_limit(value: &str) -> Result<BoardLimitArg, String> {
    if value == "auto" {
        return Ok(BoardLimitArg::Auto);
    }
    value
        .parse::<u32>()
        .map(BoardLimitArg::Count)
        .map_err(|_| format!("--limit must be 'auto' or an integer, got {value:?}"))
}

fn board() -> Command {
    Command::new("board")
        .about("Show the task board")
        .arg(projects())
        .arg(flag("all", "Show all projects"))
        .arg(flag("here", "Show the current project"))
        .arg(flag("active", "Show active work"))
        .arg(flag("by-project", "Group cards by project"))
        .arg(
            repeat(
                "status",
                "{blocked,in-progress,backlog}",
                "Show only this column; repeatable. A single selected column uses the full width",
            )
            .value_parser(["blocked", "in-progress", "backlog"]),
        )
        .arg(
            value("limit", "{auto,N}", "Cards shown per column before '+N more' (default auto)")
                .value_parser(parse_board_limit)
                .default_value("auto"),
        )
        .arg(flag("no-color", "Disable ANSI color"))
        .arg(
            value(
                "cards-per-row",
                "{auto,1,2}",
                "Cards placed side by side within a column (default auto)",
            )
            .value_parser(["auto", "1", "2"])
            .default_value("auto"),
        )
        .arg(no_reconcile())
        .arg(read_only())
        .arg(
            value(
                "watch",
                "SECONDS",
                "Redraw in place until q or Ctrl-C; omitted value uses [board].refresh_seconds (default 300s)",
            )
            .num_args(0..=1)
            .default_missing_value("__from_config__")
            .value_parser(parse_watch_arg),
        )
}

fn accounts() -> Command {
    // A bare `accounts` invocation lists seats, so its subcommand is optional.
    Command::new("accounts")
        .about("List accounts or manage seat availability")
        .subcommand(
            Command::new("trip")
                .about("Trip a pool breaker")
                .arg(positional("pool", true, "Pool name"))
                .arg(
                    value("kind", "KIND", "Breaker kind")
                        .value_parser(["rolling", "weekly", "budget"])
                        .default_value("rolling"),
                )
                .arg(value("until", "TIMESTAMP", "Breaker expiration")),
        )
        .subcommand(
            Command::new("reset")
                .about("Reset a pool breaker")
                .arg(positional("pool", true, "Pool name")),
        )
        .subcommand(
            Command::new("limit")
                .about("Set a seat capacity limit")
                .arg(positional("seat", true, "Seat name"))
                .arg(
                    positional("cap", false, "Capacity limit")
                        .value_parser(clap::value_parser!(u32))
                        .required_unless_present("off"),
                )
                .arg(flag("off", "Clear the capacity limit")),
        )
        .subcommand(Command::new("usage").about("Show account usage").arg(value(
            "detail",
            "ACCOUNT",
            "Per-day and per-project usage tables for one account",
        )))
        .subcommand(
            Command::new("lend")
                .about("Set interactive seat lending")
                .arg(positional("seat", true, "Seat name"))
                .arg(positional("state", true, "Lending state")),
        )
        .subcommand(
            Command::new("prefer")
                .about("Set the preferred seat")
                .arg(positional("seat", false, "Seat name").required_unless_present("clear"))
                .arg(flag("clear", "Clear the preferred seat")),
        )
}

fn activity() -> Command {
    group("activity", "Send messages and inspect task activity")
        .subcommand(
            Command::new("send")
                .about("Send a project message")
                .arg(positional("project", true, "Recipient project"))
                .arg(positional("message", true, "Message body"))
                .arg(value("id", "ID", "Message ID"))
                .arg(flag("ask", "Request a reply"))
                .arg(value("re", "SEQ", "Sequence being replied to"))
                .arg(flag("not-a-reply", "Send without reply semantics")),
        )
        .subcommand(
            Command::new("reply")
                .about("Reply to a message")
                .arg(
                    positional("seq", true, "Message sequence")
                        .value_parser(clap::value_parser!(u64)),
                )
                .arg(positional("message", true, "Reply body"))
                .arg(value("id", "ID", "Reply ID")),
        )
        .subcommand(
            Command::new("close")
                .about("Close a message thread")
                .arg(positional("seq", true, "Message sequence or Project#SEQ"))
                .arg(value("reason", "TEXT", "Reason for closing").required(true)),
        )
        .subcommand(
            Command::new("broadcast")
                .about("Broadcast a message")
                .arg(positional("message", true, "Message body"))
                .arg(value("id", "ID", "Message ID")),
        )
        .subcommand(
            Command::new("nudge")
                .about("Nudge project coordinators")
                .arg(projects()),
        )
        .subcommand(
            Command::new("inbox")
                .about("Read the activity inbox")
                .arg(
                    flag("peek", "Read without advancing the cursor")
                        .conflicts_with("replay-since")
                        .conflicts_with("replay-last"),
                )
                .arg(
                    flag("as-nexus", "Authorize acknowledging as the Nexus")
                        .conflicts_with("replay-since")
                        .conflicts_with("replay-last"),
                )
                .arg(value("limit", "N", "Maximum messages").value_parser(clap::value_parser!(u32)))
                .arg(
                    value(
                        "replay-since",
                        "SEQ",
                        "Re-show every message from this event forward, without acknowledging",
                    )
                    .value_parser(clap::value_parser!(u64))
                    .conflicts_with("replay-last")
                    .conflicts_with("peek")
                    .conflicts_with("as-nexus"),
                )
                .arg(
                    value(
                        "replay-last",
                        "N",
                        "Re-show the last N messages, without acknowledging",
                    )
                    .value_parser(clap::value_parser!(u32))
                    .conflicts_with("replay-since")
                    .conflicts_with("peek")
                    .conflicts_with("as-nexus"),
                ),
        )
        .subcommand(
            Command::new("update")
                .about("Update card progress")
                .arg(positional("card", true, "Card ID"))
                .arg(
                    value("phase", "PHASE", "Current work phase")
                        .required(true)
                        .value_parser(["working", "finding", "wait"]),
                )
                .arg(value("note", "TEXT", "Progress note")),
        )
}

fn backlog() -> Command {
    group("backlog", "Manage durable project backlog cards")
        .subcommand(
            Command::new("add")
                .about("Add a backlog card")
                .arg(positional("title", true, "Card title").num_args(1..))
                .arg(value("why", "TEXT", "Evidence and cost of the issue"))
                .arg(value("due", "DATE", "Due date"))
                .arg(value("source", "SOURCE", "Source reference"))
                .arg(role())
                .arg(value("priority", "PRIORITY", "Card priority")),
        )
        .subcommand(
            Command::new("list")
                .about("List backlog cards")
                .arg(projects())
                .arg(
                    flag(
                        "open",
                        "Show unfinished items (open and asked); this is the default",
                    )
                    .conflicts_with("done")
                    .conflicts_with("all"),
                )
                .arg(
                    flag("done", "Show only items that are closed")
                        .conflicts_with("open")
                        .conflicts_with("all"),
                )
                .arg(
                    flag("all", "Show both open and closed items")
                        .conflicts_with("open")
                        .conflicts_with("done"),
                ),
        )
        .subcommand(
            Command::new("done")
                .about("Complete a backlog card")
                .arg(positional("item", true, "Card ID").num_args(1..))
                .arg(value("evidence", "TEXT", "Completion evidence")),
        )
        .subcommand(
            Command::new("reopen")
                .about("Reopen a backlog card")
                .arg(positional("item", true, "Card ID").num_args(1..)),
        )
        .subcommand(
            Command::new("annotate")
                .about("Annotate a backlog card")
                .arg(positional("item", true, "Card ID").num_args(1..))
                .arg(value("note", "TEXT", "Annotation").required(true)),
        )
        .subcommand(
            Command::new("reprioritize")
                .about("Change card priority")
                .arg(positional("item", true, "Card ID").num_args(1..))
                .arg(value("priority", "PRIORITY", "New priority").required(true)),
        )
        .subcommand(
            Command::new("show")
                .about("Show a backlog card")
                .arg(positional("item", true, "Card ID").num_args(1..)),
        )
        .subcommand(
            Command::new("ask")
                .about("Record a question for the operator")
                .arg(positional("question", true, "Question").num_args(1..))
                .arg(repeat(
                    "option",
                    "TEXT",
                    "Suggested answer; repeat for alternatives",
                ))
                .arg(
                    value(
                        "recommend",
                        "N",
                        "1-based index into --option marking the recommended choice",
                    )
                    .required(true)
                    .value_parser(clap::value_parser!(u32)),
                )
                .arg(value("why", "TEXT", "Why a decision is needed"))
                .arg(value("source", "SOURCE", "Source reference"))
                .arg(role())
                .arg(value("priority", "PRIORITY", "Card priority")),
        )
        .subcommand(
            Command::new("mark-asked")
                .about("Mark a card as asked")
                .arg(positional("item", true, "Card ID").num_args(1..))
                .arg(repeat(
                    "option",
                    "TEXT",
                    "Suggested answer; repeat for alternatives",
                ))
                .arg(
                    value(
                        "recommend",
                        "N",
                        "1-based index into --option marking the recommended choice",
                    )
                    .required(true)
                    .value_parser(clap::value_parser!(i64))
                    .allow_negative_numbers(true),
                ),
        )
        .subcommand(
            Command::new("answer")
                .about("Record an answer")
                .arg(positional("item", true, "Card ID").num_args(1..))
                .arg(value("note", "TEXT", "Answer note").required(true)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every command and documented option from spec-cli.md sections 2 and 3.
    // Each row must parse, route to its own stub, and accept global config at
    // both the root and the leaf. No command is allowed to silently succeed.
    const INVOCATIONS: &[&[&str]] = &[
        &[
            "dispatch",
            "project",
            "task",
            "--dry-run",
            "--yes",
            "--force",
            "--timeout",
            "2",
            "--wait",
            "--wait-for-seat",
            "--wait-for-seat-seconds",
            "1",
            "--role",
            "builder",
            "--column",
            "Execute",
            "--full",
            "--reviewing",
            "lane-a",
            "--reviewing",
            "lane-b",
            "--exclude-vendor",
            "claude",
            "--commits",
            "abc123",
            "--commits",
            "def456",
            "--title",
            "title",
            "--input",
            "input-a",
            "--input",
            "input-b",
            "--scope",
            "src",
            "--scope",
            "tests",
            "--into",
            "lane",
            "--no-reconcile",
        ],
        &["halt", "on", "--reason", "maintenance"],
        &["halt", "off"],
        &["halt", "status"],
        &["scratch", "open"],
        &["scratch", "read"],
        &["scratch", "read", "pane"],
        &["scratch", "close"],
        &["scratch", "close", "pane"],
        &["start", "one", "two", "--all", "--reseed", "--dashboard"],
        &["projects", "--names"],
        &["stop", "one", "two", "--force"],
        &["add", "project", "--name", "name", "--dry-run"],
        &["new", "name", "--path", "project", "--yes", "--dry-run"],
        &[
            "tasks",
            "one",
            "two",
            "--reconcile",
            "--read-only",
            "--task",
            "lane",
            "--close-stranded",
            "--reclaimable",
            "--supersede",
            "old",
            "--supersedes",
            "7",
            "--status",
            "done",
            "--state",
            "settled",
            "--reason",
            "reason",
            "--revisions",
            "2",
            "--acknowledge",
            "lane",
            "--acknowledges",
            "9",
            "--yes",
        ],
        &["tasks", "--no-reconcile"],
        &[
            "board",
            "one",
            "two",
            "--all",
            "--here",
            "--active",
            "--by-project",
            "--status",
            "blocked",
            "--status",
            "backlog",
            "--limit",
            "10",
            "--no-color",
            "--cards-per-row",
            "2",
            "--no-reconcile",
            "--read-only",
            "--watch",
        ],
        &[
            "council",
            "project",
            "question",
            "--dry-run",
            "--yes",
            "--timeout",
            "5",
        ],
        &["accounts"],
        &[
            "accounts",
            "trip",
            "pool",
            "--kind",
            "rolling",
            "--until",
            "2030-01-01T00:00:00Z",
        ],
        &["accounts", "reset", "pool"],
        &["accounts", "limit", "seat", "2"],
        &["accounts", "limit", "seat", "--off"],
        &["accounts", "usage", "--detail", "grok"],
        &["accounts", "lend", "seat", "on"],
        &["accounts", "prefer", "seat"],
        &["accounts", "prefer", "--clear"],
        &["seat", "show", "seat", "--json"],
        &[
            "worktrees",
            "project",
            "--prune",
            "--force",
            "--yes",
            "--dry-run",
            "--deep",
            "--restore",
            "lane",
        ],
        &["worktrees", "project", "--rescue"],
        &[
            "check",
            "--dispatch-only",
            "--no-reconcile",
            "--read-only",
            "--verbose",
        ],
        &["doctor"],
        &[
            "import-python",
            "--from",
            "does-not-exist",
            "--dry-run",
            "--force",
        ],
        &["presence", "--json"],
        &["health", "--json"],
        &["review-policy", "--lane", "lane"],
        &["sweep", "--dry-run", "--tick", "--stale-only", "--verbose"],
        &[
            "notify",
            "--project",
            "one",
            "--title",
            "title",
            "--message",
            "message",
            "--priority",
            "high",
            "--tag",
            "tag",
            "--tag",
            "warning",
            "--flush",
            "--episode-key",
            "episode",
            "--episode-state",
            "state.json",
        ],
        &[
            "inspect", "one", "two", "--reason", "reason", "--lines", "1:2",
        ],
        &["result", "lane"],
        &[
            "activity",
            "send",
            "one",
            "message",
            "--id",
            "id",
            "--ask",
            "--re",
            "1",
            "--not-a-reply",
        ],
        &["activity", "reply", "1", "message", "--id", "id"],
        &["activity", "close", "1", "--reason", "reason"],
        &["activity", "broadcast", "message", "--id", "id"],
        &["activity", "nudge", "one", "two"],
        &["activity", "inbox", "--peek", "--as-nexus", "--limit", "10"],
        &["activity", "inbox", "--replay-since", "1"],
        &["activity", "inbox", "--replay-last", "5"],
        &[
            "activity", "update", "card", "--phase", "working", "--note", "note",
        ],
        &[
            "backlog",
            "add",
            "title",
            "--why",
            "why",
            "--due",
            "2030-01-01",
            "--source",
            "source",
            "--role",
            "scout",
            "--priority",
            "high",
        ],
        &["backlog", "list", "one", "two"],
        &["backlog", "done", "card", "--evidence", "evidence"],
        &["backlog", "reopen", "card"],
        &["backlog", "annotate", "card", "--note", "note"],
        &["backlog", "reprioritize", "card", "--priority", "high"],
        &["backlog", "show", "card"],
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
            "--why",
            "why",
            "--source",
            "source",
            "--role",
            "seer",
            "--priority",
            "high",
        ],
        &[
            "backlog",
            "mark-asked",
            "card",
            "--option",
            "first",
            "--option",
            "second",
            "--recommend",
            "1",
        ],
        &["backlog", "answer", "card", "--note", "note"],
        &[
            "directives",
            "set",
            "one",
            "two",
            "--body",
            "body",
            "--until",
            "2030-01-01T00:00:00Z",
        ],
        &["directives", "show", "one", "two"],
        &["workers"],
    ];

    // Commands wired to a real handler no longer return the generic "not
    // implemented" stub. Commands with one reliable exit code regardless of
    // filesystem/config state go in `WIRED`, each with the code its command
    // family documents in spec-cli.md/architecture.md (see those modules'
    // doc comments for why each code was chosen). `presence` moved to
    // `IMPLEMENTED` (task `20261102-fixer-c0b2dc`): it now probes the real
    // macOS HID idle timer, so its exit code (0/1/2) depends on live
    // operator-idle state, not a fixed stub answer.
    const WIRED: &[(&str, u8)] = &[
        ("board", 1),
        ("accounts", 3),
        ("seat", 1),
        ("health", 3),
        ("review-policy", 3),
        ("sweep", 3),
    ];

    /// Commands whose outcome (and so exit code) genuinely depends on
    /// filesystem/config state under this test's scratch config path —
    /// still routed and parsed here, but no longer expected to answer
    /// "not implemented" literally. `tasks`/`scratch`/`worktrees`/`result`/
    /// `activity`/`backlog` are wired to `mv-state`; `mv_state::backlog`
    /// is a real, fully implemented module (`crates/mv-state/src/backlog.rs`)
    /// backing `commands::backlog`'s add/list/done/reopen/annotate/
    /// reprioritize/show/ask/mark-asked/answer subcommands, not a stub.
    /// Their actual behavior is covered by their own modules' unit tests
    /// and, for `doctor`/`check`, by
    /// `conformance/scenarios/{doctor,check}-missing-accounts.json`.
    ///
    /// `dispatch` joined this list in task `20261102-fixer-d5b3b8`: its
    /// `--dry-run` planner/renderer is real now (`commands::dispatch`'s
    /// `RealEngine`), covered by its own module's tests and by
    /// `conformance/scenarios/dispatch-*.json`. `INVOCATIONS`' `dispatch`
    /// entry names a project path ("project") that does not exist on
    /// disk, so every call here fails on that alone — before `--config`'s
    /// (also nonexistent) file is ever read — with a message naming the
    /// path, never the stub's literal wording.
    ///
    /// `presence` joined this list in task `20261102-fixer-c0b2dc`: it
    /// now probes the real macOS HID idle timer (`commands::presence`),
    /// so its exit code depends on live operator-idle state rather than
    /// a fixed stub answer; covered by that module's own unit tests and
    /// `conformance/scenarios/presence-json-shape.json`.
    ///
    /// `council` joined this list in task `20261102-fixer-62c3b7`: its own
    /// `RealEngine` (`commands::council`, wiring `mv_engine::council`) now
    /// resolves the project path for real, same as `dispatch`'s entry
    /// above. `INVOCATIONS`' `council` entry names a project path
    /// ("project") that does not exist on disk, so — same as `dispatch` —
    /// every call here fails on that alone, with a message naming the
    /// path, never the old `"council: not implemented"` stub text.
    const IMPLEMENTED: &[&str] = &[
        "halt",
        "start",
        "projects",
        "stop",
        "add",
        "new",
        "check",
        "doctor",
        "tasks",
        "scratch",
        "worktrees",
        "result",
        "activity",
        "backlog",
        "workers",
        "notify",
        "inspect",
        "directives",
        "dispatch",
        "presence",
        "council",
        "import-python",
    ];

    #[test]
    fn every_documented_invocation_parses_and_reaches_its_stub() {
        // `["tasks", "--no-reconcile"]` below resolves no project from
        // `--config` (the scratch path is never a real config file), so
        // `commands::tasks::run_with` falls through to
        // `current_project_root()` — a bare cwd walk for an ancestor
        // `.multiverse/`, which this crate's own source tree satisfies
        // (this repo manages itself). That reaches `list()`'s
        // unconditional `agents.list_checked()` through the real
        // `mv_state::tasks::HerdrAgents`, which reads process-wide
        // `HERDR_BIN_PATH` outside any test-only seam (task
        // `20261105-fixer-e1b6cc`: this raced
        // `accounts::tests::listing_a_capped_seat_calls_herdr_agent_list_exactly_once`,
        // which holds `herdr_env`'s lock while pointing `HERDR_BIN_PATH`
        // at a script that counts calls — this test's own unguarded read
        // of that same process-wide var, mid-window, was the second
        // caller inflating that count). Held for the whole loop since
        // several of its other invocations reach `mv-herdr` the same way.
        let _herdr = crate::commands::worktrees::herdr_env::HerdrEnvGuard::unreachable();

        // A config path under a scratch temp dir, not the literal relative
        // "alternate.toml" previously used here:
        // several of the now-implemented commands (`halt` in particular)
        // perform real file I/O relative to wherever `--config` resolves,
        // and a relative path resolves against this test's cwd — which is
        // this crate's own source tree, not a throwaway location. Routing
        // through an isolated temp dir keeps this test's side effects out
        // of the working tree. An earlier version of this change actually
        // wrote a stray state file into a
        // developer's real `~/.config/multiverse` before this existed.
        let scratch = std::env::temp_dir().join(format!(
            "mv-cli-args-test-{}-{}",
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

        let mut covered = std::collections::BTreeSet::new();
        // `config migrate` is real — `mv_config::migrate` already exists —
        // so it cannot share the blanket "not implemented" assertion below.
        // Its own parsing and runtime behavior are covered by
        // `commands::migrate`'s tests; this only confirms the global
        // `--config` flag still reaches its leaf matches.
        covered.insert("config");
        let m = command()
            .try_get_matches_from([
                "multiverse",
                "config",
                "migrate",
                "--dry-run",
                "--backup",
                "--config",
                "alternate.toml",
            ])
            .unwrap();
        assert_eq!(m.get_one::<String>("config").unwrap(), "alternate.toml");
        let migrate = m
            .subcommand_matches("config")
            .unwrap()
            .subcommand_matches("migrate")
            .unwrap();
        assert_eq!(
            migrate.get_one::<String>("config").unwrap(),
            "alternate.toml"
        );

        for words in INVOCATIONS {
            covered.insert(words[0]);
            for config_first in [false, true] {
                let mut argv = vec!["multiverse"];
                if config_first {
                    argv.extend(["--config", &config_arg]);
                }
                argv.extend_from_slice(words);
                if !config_first {
                    argv.extend(["--config", &config_arg]);
                }
                let m = command()
                    .try_get_matches_from(&argv)
                    .unwrap_or_else(|e| panic!("{argv:?}: {e}"));
                assert_eq!(m.get_one::<String>("config").unwrap(), &config_arg);
                let mut leaf = &m;
                while let Some((_, next)) = leaf.subcommand() {
                    leaf = next;
                }
                assert_eq!(leaf.get_one::<String>("config").unwrap(), &config_arg);
                let result = crate::commands::run(&m);
                if let Some((_, code)) = WIRED.iter().find(|(name, _)| *name == words[0]) {
                    let err = result.unwrap_err();
                    assert_eq!(err.code(), *code, "{argv:?}: {err}");
                    assert!(
                        !err.to_string().contains("not implemented"),
                        "{argv:?}: {err}"
                    );
                } else if IMPLEMENTED.contains(&words[0]) {
                    // Routed to real behavior: never the stub's exact
                    // wording, whatever the outcome (most of these fail
                    // against a config path that deliberately doesn't
                    // exist — that is a config error, not "not
                    // implemented").
                    if let Err(err) = result {
                        assert_ne!(err.to_string(), format!("{}: not implemented", words[0]));
                    }
                } else {
                    let err = result.unwrap_err();
                    assert_eq!(err.code(), 1);
                    assert_eq!(err.to_string(), format!("{}: not implemented", words[0]));
                }
            }
        }
        std::fs::remove_dir_all(&scratch).ok();

        let expected: std::collections::BTreeSet<_> = command()
            .get_subcommands()
            .map(Command::get_name)
            .filter(|name| *name != "help")
            .map(str::to_owned)
            .collect();
        assert_eq!(
            covered
                .into_iter()
                .map(str::to_owned)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
        assert_eq!(expected.len(), 29);
    }

    #[test]
    fn every_command_and_option_has_help() {
        fn check(cmd: &Command, path: Vec<String>) {
            assert!(cmd.get_about().is_some(), "{path:?}");
            for arg in cmd.get_arguments() {
                assert!(arg.get_help().is_some(), "{path:?}: {}", arg.get_id());
            }
            let mut argv = vec!["multiverse".to_owned()];
            argv.extend(path.iter().cloned());
            argv.push("--help".to_owned());
            let error = command().try_get_matches_from(argv).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
            let help = error.to_string();
            assert!(help.contains("--config"), "{path:?}: {help}");
            for sub in cmd.get_subcommands() {
                let mut next = path.clone();
                next.push(sub.get_name().to_owned());
                check(sub, next);
            }
        }
        check(&command(), Vec::new());
    }

    /// Release policy: retired role names are read from on-disk history
    /// and translated, never written or shown in new output — `--help` is
    /// new output on every invocation, so it must never name one. Guards
    /// the whole CLI surface the way `dispatch.rs`'s own `legacy_display_name`
    /// removal (task `20261103-fixer-fa9c70`) guards the dry-run card: a
    /// word-boundary check (not a bare substring one), so `--reviewing`'s
    /// help text ("review...") and `review-policy`'s name don't false-positive
    /// against the retired `reviewer`.
    #[test]
    fn help_text_never_shows_a_retired_role_name() {
        const RETIRED_ROLE_NAMES: &[&str] = &[
            "orchestrator",
            "explorer",
            "fixer",
            "librarian",
            "reviewer",
            "oracle",
            "designer",
            "observer",
        ];
        fn retired_word_in(haystack: &str) -> Option<&'static str> {
            let lower = haystack.to_ascii_lowercase();
            let tokens: std::collections::HashSet<&str> =
                lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
            RETIRED_ROLE_NAMES
                .iter()
                .copied()
                .find(|word| tokens.contains(word))
        }
        fn check(cmd: &Command, path: Vec<String>) {
            let mut argv = vec!["multiverse".to_owned()];
            argv.extend(path.iter().cloned());
            argv.push("--help".to_owned());
            let error = command().try_get_matches_from(argv).unwrap_err();
            let help = error.to_string();
            assert_eq!(
                retired_word_in(&help),
                None,
                "{path:?}: retired role name in --help:\n{help}"
            );
            for sub in cmd.get_subcommands() {
                let mut next = path.clone();
                next.push(sub.get_name().to_owned());
                check(sub, next);
            }
        }
        check(&command(), Vec::new());
    }

    #[test]
    fn as_nexus_is_accepted() {
        let m = command()
            .try_get_matches_from(["multiverse", "activity", "inbox", "--as-nexus"])
            .unwrap();
        let inbox = m
            .subcommand_matches("activity")
            .unwrap()
            .subcommand_matches("inbox")
            .unwrap();
        assert!(inbox.get_flag("as-nexus"));
    }

    /// Release policy: no old role names and no aliases for them anywhere in
    /// new input. `--as-orchestrator` was `--as-nexus`'s alias; it must now
    /// be rejected outright, not silently accepted.
    #[test]
    fn as_orchestrator_is_rejected() {
        let error = command()
            .try_get_matches_from(["multiverse", "activity", "inbox", "--as-orchestrator"])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn replay_since_and_replay_last_are_mutually_exclusive() {
        let error = command()
            .try_get_matches_from([
                "multiverse",
                "activity",
                "inbox",
                "--replay-since",
                "1",
                "--replay-last",
                "5",
            ])
            .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn replay_since_conflicts_with_peek_and_as_nexus() {
        for extra in [["--peek"].as_slice(), ["--as-nexus"].as_slice()] {
            let mut argv = vec!["multiverse", "activity", "inbox", "--replay-since", "1"];
            argv.extend_from_slice(extra);
            let error = command().try_get_matches_from(argv).unwrap_err();
            assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        }
    }

    #[test]
    fn repeated_context_options_retain_all_values() {
        let m = command()
            .try_get_matches_from(
                std::iter::once("multiverse").chain(INVOCATIONS[0].iter().copied()),
            )
            .unwrap();
        let d = m.subcommand_matches("dispatch").unwrap();
        for (name, values) in [
            ("reviewing", vec!["lane-a", "lane-b"]),
            ("input", vec!["input-a", "input-b"]),
            ("scope", vec!["src", "tests"]),
            ("commits", vec!["abc123", "def456"]),
        ] {
            assert_eq!(
                d.get_many::<String>(name)
                    .unwrap()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                values
            );
        }
    }

    fn parse(argv: &[&str]) -> clap::ArgMatches {
        command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .unwrap_or_else(|e| panic!("{argv:?}: {e}"))
    }

    fn parse_err(argv: &[&str]) -> clap::Error {
        command()
            .try_get_matches_from(std::iter::once("multiverse").chain(argv.iter().copied()))
            .expect_err("expected a usage error")
    }

    #[test]
    fn activity_close_requires_a_reason_at_parse_time() {
        let error = parse_err(&["activity", "close", "1"]);
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("--reason"));
        parse(&["activity", "close", "1", "--reason", "withdrawn"]);
    }

    #[test]
    fn activity_update_requires_one_of_the_python_phases() {
        let error = parse_err(&["activity", "update", "card"]);
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(error.exit_code(), 2);
        for phase in ["working", "finding", "wait"] {
            parse(&["activity", "update", "card", "--phase", phase]);
        }
        for phase in ["Working", "unknown"] {
            let error = parse_err(&["activity", "update", "card", "--phase", phase]);
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn inspect_requires_paths_reason_and_exactly_one_selector() {
        for words in [
            vec!["inspect", "--reason", "checking", "--lines", "1:2"],
            vec!["inspect", "README.md", "--lines", "1:2"],
            vec!["inspect", "README.md", "--reason", "checking"],
        ] {
            let error = parse_err(&words);
            assert_eq!(
                error.kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
            assert_eq!(error.exit_code(), 2);
        }
        let error = parse_err(&[
            "inspect",
            "README.md",
            "--reason",
            "checking",
            "--lines",
            "1:2",
            "--match",
            "text",
        ]);
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert_eq!(error.exit_code(), 2);
        for selector in ["--lines", "--match"] {
            let matches = parse(&[
                "inspect", "one", "two", "three", "--reason", "checking", selector, "1",
            ]);
            let inspect = matches.subcommand_matches("inspect").unwrap();
            assert_eq!(inspect.get_many::<String>("paths").unwrap().len(), 3);
        }
    }

    #[test]
    fn dispatch_column_choices_are_case_sensitive() {
        for column in ["Explore", "Execute", "Review"] {
            parse(&["dispatch", "project", "task", "--column", column]);
        }
        for column in ["explore", "execute", "review", "Other"] {
            let error = parse_err(&["dispatch", "project", "task", "--column", column]);
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn notify_priority_choices_and_default_match_python() {
        let matches = parse(&["notify"]);
        assert_eq!(
            matches
                .subcommand_matches("notify")
                .unwrap()
                .get_one::<String>("priority")
                .map(String::as_str),
            Some("default")
        );
        for priority in ["low", "default", "high", "urgent"] {
            parse(&["notify", "--priority", priority]);
        }
        for priority in ["normal", "Urgent"] {
            let error = parse_err(&["notify", "--priority", priority]);
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn accounts_trip_kind_choices_and_default_match_python() {
        let matches = parse(&["accounts", "trip", "pool"]);
        let trip = matches
            .subcommand_matches("accounts")
            .unwrap()
            .subcommand_matches("trip")
            .unwrap();
        assert_eq!(
            trip.get_one::<String>("kind").map(String::as_str),
            Some("rolling")
        );
        for kind in ["rolling", "weekly", "budget"] {
            parse(&["accounts", "trip", "pool", "--kind", kind]);
        }
        for kind in ["usage", "Rolling"] {
            let error = parse_err(&["accounts", "trip", "pool", "--kind", kind]);
            assert_eq!(error.kind(), clap::error::ErrorKind::InvalidValue);
            assert_eq!(error.exit_code(), 2);
        }
    }

    #[test]
    fn backlog_cwd_values_capture_overflow_but_remain_required() {
        for (command, field, options) in [
            ("add", "title", vec![]),
            ("done", "item", vec![]),
            ("reopen", "item", vec![]),
            ("annotate", "item", vec!["--note", "note"]),
            ("reprioritize", "item", vec!["--priority", "P2"]),
            ("show", "item", vec![]),
            ("ask", "question", vec!["--recommend", "1"]),
            ("mark-asked", "item", vec!["--recommend", "1"]),
            ("answer", "item", vec!["--note", "note"]),
        ] {
            let mut argv = vec!["backlog", command, "sandbox", "actual value"];
            argv.extend(&options);
            let matches = parse(&argv);
            let args = matches
                .subcommand_matches("backlog")
                .unwrap()
                .subcommand_matches(command)
                .unwrap();
            assert_eq!(
                args.get_many::<String>(field)
                    .unwrap()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                ["sandbox", "actual value"]
            );
            let mut missing = vec!["backlog", command];
            missing.extend(options);
            assert_eq!(
                parse_err(&missing).kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }
    }

    #[test]
    fn backlog_mark_asked_requires_an_integer_recommendation() {
        let error = parse_err(&["backlog", "mark-asked", "card"]);
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
        assert_eq!(error.exit_code(), 2);
        for value in ["first", "1.5"] {
            let error = parse_err(&["backlog", "mark-asked", "card", "--recommend", value]);
            assert_eq!(error.kind(), clap::error::ErrorKind::ValueValidation);
            assert_eq!(error.exit_code(), 2);
        }
        // Like argparse's int, parsing accepts negatives; range validation belongs to the handler.
        for value in ["-1", "0", "1", "2"] {
            let matches = parse(&["backlog", "mark-asked", "card", "--recommend", value]);
            let mark = matches
                .subcommand_matches("backlog")
                .unwrap()
                .subcommand_matches("mark-asked")
                .unwrap();
            assert_eq!(
                *mark.get_one::<i64>("recommend").unwrap(),
                value.parse::<i64>().unwrap()
            );
        }
    }

    #[test]
    fn bare_seat_reaches_the_handler_and_exits_one() {
        let matches = parse(&["seat"]);
        let error = crate::commands::run(&matches).unwrap_err();
        assert!(matches!(error, crate::exit::Failure::Reported(1)));
    }

    #[test]
    fn board_watch_limit_cards_per_row_and_status_match_python_shapes() {
        let board = parse(&["board", "--watch"])
            .subcommand_matches("board")
            .unwrap()
            .clone();
        assert_eq!(
            board.get_one::<WatchArg>("watch"),
            Some(&WatchArg::FromConfig)
        );

        let board = parse(&["board", "--watch", "60"])
            .subcommand_matches("board")
            .unwrap()
            .clone();
        assert_eq!(
            board.get_one::<WatchArg>("watch"),
            Some(&WatchArg::Seconds(60.0))
        );

        assert!(
            parse_err(&["board", "--watch", "0"]).kind() != clap::error::ErrorKind::DisplayHelp
        );
        assert!(parse_err(&["board", "--watch", "nope"])
            .to_string()
            .contains("--watch"));

        let board = parse(&["board"])
            .subcommand_matches("board")
            .unwrap()
            .clone();
        assert_eq!(
            board.get_one::<BoardLimitArg>("limit"),
            Some(&BoardLimitArg::Auto)
        );
        assert_eq!(
            board.get_one::<String>("cards-per-row").map(String::as_str),
            Some("auto")
        );

        let board = parse(&["board", "--limit", "auto", "--cards-per-row", "1"])
            .subcommand_matches("board")
            .unwrap()
            .clone();
        assert_eq!(
            board.get_one::<BoardLimitArg>("limit"),
            Some(&BoardLimitArg::Auto)
        );
        assert_eq!(
            board.get_one::<String>("cards-per-row").map(String::as_str),
            Some("1")
        );

        let board = parse(&[
            "board", "--limit", "7", "--status", "blocked", "--status", "backlog",
        ])
        .subcommand_matches("board")
        .unwrap()
        .clone();
        assert_eq!(
            board.get_one::<BoardLimitArg>("limit"),
            Some(&BoardLimitArg::Count(7))
        );
        assert_eq!(
            board
                .get_many::<String>("status")
                .unwrap()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["blocked", "backlog"]
        );

        assert!(parse_err(&["board", "--limit", "nope"])
            .to_string()
            .contains("--limit"));
        assert!(parse_err(&["board", "--cards-per-row", "3"])
            .to_string()
            .contains("cards-per-row"));
        assert!(parse_err(&["board", "--status", "done"])
            .to_string()
            .contains("status"));
    }

    #[test]
    fn accounts_usage_detail_takes_an_account_value() {
        let usage = parse(&["accounts", "usage", "--detail", "grok"])
            .subcommand_matches("accounts")
            .unwrap()
            .subcommand_matches("usage")
            .unwrap()
            .clone();
        assert_eq!(
            usage.get_one::<String>("detail").map(String::as_str),
            Some("grok")
        );
        assert!(parse_err(&["accounts", "usage", "--detail"])
            .to_string()
            .contains("detail"));
    }

    #[test]
    fn notify_tag_is_repeatable_and_episode_state_is_a_path() {
        let notify = parse(&[
            "notify",
            "--tag",
            "a",
            "--tag",
            "b",
            "--episode-key",
            "k",
            "--episode-state",
            "episodes.json",
        ])
        .subcommand_matches("notify")
        .unwrap()
        .clone();
        assert_eq!(
            notify
                .get_many::<String>("tag")
                .unwrap()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
        assert_eq!(
            notify
                .get_one::<String>("episode-state")
                .map(String::as_str),
            Some("episodes.json")
        );
    }
}

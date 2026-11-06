# Python / Rust CLI parity

Side-by-side of every Python `multiverse` subcommand and flag against `target/debug/multiverse`. Built from the frozen Python argparse tree (`multiverse.cli.build_parser`, `PYTHONPATH` pointed at the reference checkout) and from this binary's clap `--help` (`crates/mv-cli/src/args.rs`).

Role names on Rust **write** surfaces (config keys, `--role`, help text) use the current names. Python still uses the retired names on the same positions. History readers on both sides accept the old on-disk names. This page translates the names in the tables below; it does not claim byte-identical help prose.

## Role-name translation

| Python (retired) | Rust (current) |
|---|---|
| `orchestrator` / Orchestrator | `nexus` / Nexus |
| `explorer` / Explorer | `scout` / Scout |
| `fixer` / Fixer | `builder` / Builder |
| `librarian` / Librarian | `archivist` / Archivist |
| `reviewer` / Reviewer | `critic` / Critic |
| `oracle` / Oracle | `seer` / Seer |
| `designer` / Designer | `artisan` / Artisan |
| `observer` / Observer | `watcher` / Watcher |
| `council` / Council | `council` / Council (unchanged) |

`nexus` is not a valid `--role` on either side (Python: undispatchable with `orchestrator`; Rust: exit 3). Nexus is launched by `start`.

## How to read this page

- **Match** — same flag name and shape (flag vs value vs repeat). Help prose may differ; that is not a gap by itself.
- **Shape gap** — same flag name, different arity, metavar, or requiredness. Called out in the Notes column and in [Gaps](#gaps).
- **Python-only / Rust-only** — the other parser does not accept the flag or subcommand.
- Global `--config PATH` is on every leaf on both sides and is omitted from per-command tables. `-h` / `--help` likewise. Rust also has `-V` / `--version` on the root (clap); Python has no `--version`.

## Command inventory

Python top-level subcommands: 27. Rust top-level subcommands: 28 (`config` is Rust-only). Nested leaves match one-for-one except `config migrate`.

| Command | Python | Rust |
|---|---|---|
| `accounts` | yes | yes |
| `accounts lend` | yes | yes |
| `accounts limit` | yes | yes |
| `accounts prefer` | yes | yes |
| `accounts reset` | yes | yes |
| `accounts trip` | yes | yes |
| `accounts usage` | yes | yes |
| `activity` | yes | yes |
| `activity broadcast` | yes | yes |
| `activity close` | yes | yes |
| `activity inbox` | yes | yes |
| `activity nudge` | yes | yes |
| `activity reply` | yes | yes |
| `activity send` | yes | yes |
| `activity update` | yes | yes |
| `add` | yes | yes |
| `backlog` | yes | yes |
| `backlog add` | yes | yes |
| `backlog annotate` | yes | yes |
| `backlog answer` | yes | yes |
| `backlog ask` | yes | yes |
| `backlog done` | yes | yes |
| `backlog list` | yes | yes |
| `backlog mark-asked` | yes | yes |
| `backlog reopen` | yes | yes |
| `backlog reprioritize` | yes | yes |
| `backlog show` | yes | yes |
| `board` | yes | yes |
| `check` | yes | yes |
| `council` | yes | yes |
| `directives` | yes | yes |
| `directives set` | yes | yes |
| `directives show` | yes | yes |
| `dispatch` | yes | yes |
| `doctor` | yes | yes |
| `halt` | yes | yes |
| `halt off` | yes | yes |
| `halt on` | yes | yes |
| `halt status` | yes | yes |
| `health` | yes | yes |
| `inspect` | yes | yes |
| `new` | yes | yes |
| `notify` | yes | yes |
| `presence` | yes | yes |
| `projects` | yes | yes |
| `result` | yes | yes |
| `review-policy` | yes | yes |
| `scratch` | yes | yes |
| `scratch close` | yes | yes |
| `scratch open` | yes | yes |
| `scratch read` | yes | yes |
| `seat` | yes | yes |
| `seat show` | yes | yes |
| `start` | yes | yes |
| `stop` | yes | yes |
| `sweep` | yes | yes |
| `tasks` | yes | yes |
| `workers` | yes | yes |
| `worktrees` | yes | yes |
| `config` | — | yes (Rust-only) |
| `config migrate` | — | yes (Rust-only) |

## Global flags

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--config PATH` | yes | yes | Same name; default resolution differs (`$MULTIVERSE_CONFIG` / plugin config dir). |
| `-h` / `--help` | yes | yes | |
| `-V` / `--version` | — | yes | Clap default on the Rust root. |

## Per-command surface

### `accounts`

Rust about: List accounts or manage seat availability

No flags besides global `--config` / `--help`.

### `accounts lend`

Rust about: Set interactive seat lending

| Positional | Python | Rust |
|---|---|---|
| `seat` | required | required |
| `state` | required | required |

No flags besides global `--config` / `--help`.

### `accounts limit`

Rust about: Set a seat capacity limit

| Positional | Python | Rust |
|---|---|---|
| `seat` | required | required |
| `cap` | optional, nargs=? | optional |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--off` | flag | flag |  |

### `accounts prefer`

Rust about: Set the preferred seat

| Positional | Python | Rust |
|---|---|---|
| `seat` | optional, nargs=? | optional |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--clear` | flag | flag |  |

### `accounts reset`

Rust about: Reset a pool breaker

| Positional | Python | Rust |
|---|---|---|
| `pool` | required | required |

No flags besides global `--config` / `--help`.

### `accounts trip`

Rust about: Trip a pool breaker

| Positional | Python | Rust |
|---|---|---|
| `pool` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--kind` | VAL {budget, rolling, weekly} | KIND |  |
| `--until` | ISO8601 | TIMESTAMP |  |

### `accounts usage`

Rust about: Show account usage

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--detail` | ACCOUNT | ACCOUNT | Same optional flag taking an account name. |

### `activity`

Rust about: Send messages and inspect task activity

No flags besides global `--config` / `--help`.

### `activity broadcast`

Rust about: Broadcast a message

| Positional | Python | Rust |
|---|---|---|
| `message` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--id` | ID | ID |  |

### `activity close`

Rust about: Close a message thread

| Positional | Python | Rust |
|---|---|---|
| `seq` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--reason` | VAL, required | TEXT | Python required; Rust optional (defaults to `closed`). |

### `activity inbox`

Rust about: Read the activity inbox

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--as-orchestrator` | flag | — | Rejected outright (`UnknownArgument`); no alias. Use `--as-nexus`. |
| `--as-nexus` | — | flag | Rust-only spelling of Python's `--as-orchestrator`; handler reads it to authorize acknowledgment. |
| `--limit` | N | N |  |
| `--peek` | flag | flag | Conflicts with `--replay-since` / `--replay-last`. |
| `--replay-last` | N | N | Re-show the last N messages without acknowledging. Conflicts with `--replay-since`, `--peek`, and `--as-nexus`. |
| `--replay-since` | SEQ | SEQ | Re-show every message from this event forward without acknowledging. Conflicts with `--replay-last`, `--peek`, and `--as-nexus`. |

### `activity nudge`

Rust about: Nudge project coordinators

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

No flags besides global `--config` / `--help`.

### `activity reply`

Rust about: Reply to a message

| Positional | Python | Rust |
|---|---|---|
| `seq` | required | required |
| `message` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--id` | ID | ID |  |

### `activity send`

Rust about: Send a project message

| Positional | Python | Rust |
|---|---|---|
| `project` | required | required |
| `message` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--ask` | flag | flag |  |
| `--id` | ID | ID |  |
| `--not-a-reply` | flag | flag |  |
| `--re` | N | SEQ |  |

### `activity update`

Rust about: Update card progress

| Positional | Python | Rust |
|---|---|---|
| `card` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--note` | VAL | TEXT |  |
| `--phase` | VAL, required {finding, wait, working} | PHASE | Python required with choices working/finding/wait; Rust optional (defaults to working). |

### `add`

Rust about: Register an existing project

| Positional | Python | Rust |
|---|---|---|
| `path` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--dry-run` | flag | flag |  |
| `--name` | VAL | NAME |  |

### `backlog`

Rust about: Manage durable project backlog cards

No flags besides global `--config` / `--help`.

### `backlog add`

Rust about: Add a backlog card

| Positional | Python | Rust |
|---|---|---|
| `title` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--due` | YYYY-MM-DD | DATE |  |
| `--priority` | VAL {P1, P2, P3} | PRIORITY |  |
| `--role` | VAL | ROLE | Python free text; Rust closed worker-role list. |
| `--source` | VAL | SOURCE |  |
| `--why` | VAL | TEXT |  |

### `backlog annotate`

Rust about: Annotate a backlog card

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--note` | VAL, required | TEXT |  |

### `backlog answer`

Rust about: Record an answer

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--note` | VAL, required | TEXT |  |

### `backlog ask`

Rust about: Record a question for the operator

| Positional | Python | Rust |
|---|---|---|
| `question` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--option` | repeat TEXT | repeat TEXT |  |
| `--priority` | VAL {P1, P2, P3} | PRIORITY |  |
| `--recommend` | N, required | N |  |
| `--role` | VAL | ROLE | Python free text; Rust closed worker-role list. |
| `--source` | VAL | SOURCE |  |
| `--why` | VAL | TEXT |  |

### `backlog done`

Rust about: Complete a backlog card

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--evidence` | VAL | TEXT |  |

### `backlog list`

Rust about: List backlog cards

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--all` | flag | flag |  |
| `--done` | flag | flag |  |
| `--open` | flag | flag |  |

### `backlog mark-asked`

Rust about: Mark a card as asked

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--option` | repeat TEXT | repeat TEXT |  |
| `--recommend` | N, required | N | Python required 1-based int; Rust optional string parsed as a 1-based index at runtime. |

### `backlog reopen`

Rust about: Reopen a backlog card

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

No flags besides global `--config` / `--help`.

### `backlog reprioritize`

Rust about: Change card priority

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--priority` | VAL, required {P1, P2, P3} | PRIORITY |  |

### `backlog show`

Rust about: Show a backlog card

| Positional | Python | Rust |
|---|---|---|
| `item` | required, nargs=+ | required |

Python's cwd-scoped backlog writes take `nargs='+'` on the item/title positional so a mistaken project-name token is explained in-handler rather than as argparse surplus. Rust takes a single positional.

No flags besides global `--config` / `--help`.

### `board`

Rust about: Show the task board

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--active` | flag | flag |  |
| `--all` | flag | flag |  |
| `--by-project` | flag | flag |  |
| `--cards-per-row` | VAL {1, 2, auto} | {auto,1,2} | Same `{auto,1,2}` default auto. |
| `--here` | flag | flag |  |
| `--limit` | {auto,N} | {auto,N} | Same `{auto,N}` default auto. Off a tty, `auto` falls back to 10 cards. |
| `--no-color` | flag | flag |  |
| `--no-reconcile` | flag | flag |  |
| `--read-only` | flag | flag |  |
| `--status` | repeat {blocked,in-progress,backlog} {backlog, blocked, in-progress} | repeat {blocked,in-progress,backlog} | Repeatable column filter; a single selected column uses the full width. |
| `--watch` | SECONDS | SECONDS | Optional SECONDS (`nargs='?'` / clap `num_args(0..=1)`). Bare `--watch` uses `[board].refresh_seconds` (default 300s). |

### `check`

Rust about: Report readiness (exit 0 ready, 3 needs configuration)

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--dispatch-only` | flag | flag |  |
| `--no-reconcile` | flag | flag |  |
| `--read-only` | flag | flag |  |
| `--verbose` | flag | flag |  |

### `council`

Rust about: Ask Council; execution requires --yes

| Positional | Python | Rust |
|---|---|---|
| `path` | required | required |
| `question` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--dry-run` | flag | flag |  |
| `--timeout` | SECONDS | SECONDS |  |
| `--yes` | flag | flag |  |

### `directives`

Rust about: Manage standing project directives

No flags besides global `--config` / `--help`.

### `directives set`

Rust about: Set a directive

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--body` | VAL, required | TEXT |  |
| `--until` | ISO8601 | TIMESTAMP |  |

### `directives show`

Rust about: Show project directives

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

No flags besides global `--config` / `--help`.

### `dispatch`

Rust about: Dispatch a scoped task to a worker; execution requires --yes

| Positional | Python | Rust |
|---|---|---|
| `path` | required | required |
| `task` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--column` | COLUMN {Execute, Explore, Review} | COLUMN |  |
| `--commits` | repeat SHA | repeat SHA | Repeatable, oldest first. Warns and ignores unless exactly one `--reviewing` id. |
| `--dry-run` | flag | flag |  |
| `--exclude-vendor` | repeat VENDOR | repeat VENDOR |  |
| `--force` | flag | flag |  |
| `--full` | flag | flag |  |
| `--input` | repeat PATH | repeat PATH |  |
| `--into` | TASK_ID | TASK_ID |  |
| `--no-reconcile` | flag | flag |  |
| `--reviewing` | repeat TASK_ID | repeat TASK_ID |  |
| `--role` | ROLE {council, designer, explorer, fixer, librarian, observer, oracle, orchestrator, reviewer} | ROLE | Python choices are old role keys (default explorer); Rust write surface uses current names only. |
| `--scope` | repeat PATH | repeat PATH |  |
| `--timeout` | SECONDS | SECONDS |  |
| `--title` | TEXT | TITLE |  |
| `--wait` | flag | flag |  |
| `--wait-for-seat` | flag | flag |  |
| `--wait-for-seat-seconds` | SECONDS | SECONDS |  |
| `--yes` | flag | flag |  |

### `doctor`

Rust about: Check installation prerequisites (exit 0 pass, 1 fail)

No flags besides global `--config` / `--help`.

### `halt`

Rust about: Control the fleet halt

No flags besides global `--config` / `--help`.

### `halt off`

Rust about: Clear the fleet halt

No flags besides global `--config` / `--help`.

### `halt on`

Rust about: Halt new work

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--reason` | TEXT | TEXT |  |

### `halt status`

Rust about: Show fleet halt status

No flags besides global `--config` / `--help`.

### `health`

Rust about: Report fleet health

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--json` | flag | flag |  |

### `inspect`

Rust about: Inspect project context

| Positional | Python | Rust |
|---|---|---|
| `paths` | required, nargs=+ | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--lines` | START:END | START:END | Parser accepts START:END, N, and N-M. |
| `--match` | TEXT | TEXT |  |
| `--reason` | VAL, required | TEXT | Python required; Rust optional (empty default). |

### `new`

Rust about: Create, register, and start a project

| Positional | Python | Rust |
|---|---|---|
| `name` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--dry-run` | flag | flag |  |
| `--path` | DIR | PATH |  |
| `--yes` | flag | flag |  |

### `notify`

Rust about: Queue or flush operator notifications

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--episode-key` | KEY | KEY |  |
| `--episode-state` | PATH | PATH | JSON episode file; only used together with `--episode-key`. |
| `--flush` | flag | flag |  |
| `--message` | MESSAGE | TEXT |  |
| `--priority` | VAL {default, high, low, urgent} | PRIORITY |  |
| `--project` | NAME | PROJECT |  |
| `--tag` | repeat TAG | repeat TAG | Repeatable; joined with commas on the ntfy `Tags` header. |
| `--title` | TITLE | TITLE |  |

### `presence`

Rust about: Report operator presence (exit 2 if unknown)

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--json` | flag | flag |  |

### `projects`

Rust about: List configured projects

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--names` | flag | flag |  |

### `result`

Rust about: Read a task result

| Positional | Python | Rust |
|---|---|---|
| `task_id` | required | required |

No flags besides global `--config` / `--help`.

### `review-policy`

Rust about: Show effective review policy

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--lane` | TASK_ID | TASK_ID |  |

### `scratch`

Rust about: Manage isolated scratch panes

No flags besides global `--config` / `--help`.

### `scratch close`

Rust about: Close a scratch pane

| Positional | Python | Rust |
|---|---|---|
| `pane` | optional, nargs=? | optional |

No flags besides global `--config` / `--help`.

### `scratch open`

Rust about: Open a scratch pane

No flags besides global `--config` / `--help`.

### `scratch read`

Rust about: Read scratch output

| Positional | Python | Rust |
|---|---|---|
| `pane` | optional, nargs=? | optional |

No flags besides global `--config` / `--help`.

### `seat`

Rust about: Inspect an account seat

No flags besides global `--config` / `--help`.

### `seat show`

Rust about: Show seat state

| Positional | Python | Rust |
|---|---|---|
| `seat` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--json` | flag | flag |  |

### `start`

Rust about: Start Nexus for selected projects

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--all` | flag | flag |  |
| `--dashboard` | flag | flag |  |
| `--reseed` | flag | flag |  |

### `stop`

Rust about: Stop project sessions

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--force` | flag | flag |  |

### `sweep`

Rust about: Inspect and maintain fleet state

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--dry-run` | flag | flag |  |
| `--stale-only` | flag | flag |  |
| `--tick` | flag | flag |  |
| `--verbose` | flag | flag |  |

### `tasks`

Rust about: Inspect or update project tasks

| Positional | Python | Rust |
|---|---|---|
| `projects` | optional, nargs=* | optional, variadic |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--acknowledge` | TASK | TASK_ID |  |
| `--acknowledges` | ORDINAL | ORDINAL |  |
| `--close-stranded` | flag | flag |  |
| `--no-reconcile` | flag | flag |  |
| `--read-only` | flag | flag |  |
| `--reason` | TEXT | TEXT |  |
| `--reclaimable` | flag | flag |  |
| `--reconcile` | flag | flag |  |
| `--revisions` | VAL | N |  |
| `--state` | STATE | STATE |  |
| `--status` | VAL {blocked, done, failed} | STATUS |  |
| `--supersede` | TASK | TASK_ID |  |
| `--supersedes` | ORDINAL | ORDINAL |  |
| `--task` | TASK | TASK_ID |  |
| `--yes` | flag | flag |  |

### `workers`

Rust about: List worker sessions

No flags besides global `--config` / `--help`.

### `worktrees`

Rust about: Inspect, prune, rescue, or restore task worktrees

| Positional | Python | Rust |
|---|---|---|
| `path` | required | required |

| Flag | Python | Rust | Notes |
|---|---|---|---|
| `--deep` | flag | flag |  |
| `--dry-run` | flag | flag |  |
| `--force` | flag | flag |  |
| `--prune` | flag | flag |  |
| `--rescue` | flag | flag |  |
| `--restore` | TASK_ID | TASK_ID |  |
| `--yes` | flag | flag |  |

### `config`

Rust-only. Python has no matching subcommand.

Rust about: Manage the configuration file

No flags besides global `--config` / `--help`.

### `config migrate`

Rust-only. Python has no matching subcommand.

Rust about: Rewrite retired role keys to their current names in place

| Flag | Shape | Help |
|---|---|---|
| `--dry-run` | flag | Show the plan without performing the operation |
| `--backup` | flag | Write a timestamped backup, then migrate in place |

## Behavioral parity (conformance and diff-fuzz)

Parser coverage above is clap versus argparse. Runtime parity is checked separately:

- 154 committed JSON scenarios under `conformance/scenarios/` (`python3 conformance/run_all.py`). All 154 currently pass against Rust. Seven of them (`history-*`) pin retired on-disk role names that history readers must still accept.
- `conformance/diff_fuzz.py`, a seeded differential fuzzer. It generates random sequences of state-changing commands that need no live agent (`backlog add` / `done` / `annotate` / `reopen` / `reprioritize`, `accounts trip` / `reset`, `halt on` / `off`, `notify` / `--flush`, `activity send`), replays the same sequence against the frozen Python oracle and this binary, and after each write runs a fixed battery of read probes (`backlog list` / `show`, `tasks`, `workers`, `health`, `board --no-reconcile --read-only --no-color`, `review-policy`, `accounts usage`, `activity inbox --peek`, `halt status`, and `dispatch --dry-run` for Scout, Builder, and Critic). There is no pre-recorded expectation: Python's own behavior for that sequence is the oracle. Requires `MV_CONFORMANCE_PYTHON_REPO` or `--python-repo`; without an oracle the harness prints `skipped` and exits 0.

Four `dispatch --dry-run` rendering differences are accepted and folded before comparison. They are not bugs in this binary:

1. Agent short-codes follow the role rename (retired four-letter codes versus current, for example Scout's `scou`).
2. Prompt-file names follow the role rename (`prompts/role-scout.md` versus the retired filename). Appended prompt-file line counts differ with the files and are folded to `(N lines)`.
3. The `WOULD RUN` pane-command section is this binary's own real runner, not Python's.
4. Prompt wrapping is a rendering-width artifact that can split a sandbox path mid-string, which a literal path replace cannot re-stitch, so leftover tmp-dir text would never match across runs.

Any other `--dry-run` divergence still fails the comparison. `normalize_dry_run` in `conformance/diff_fuzz.py` is scoped to `dispatch --dry-run` argv only.

## Gaps

Parser name-and-shape coverage, not behavioral parity of every handler. Several commands that share a flag still implement a subset of Python's runtime (see each handler's module doc and the README Status section). Those cuts are out of this page's scope unless they change the clap/argparse surface. Runtime checks live in [Behavioral parity](#behavioral-parity-conformance-and-diff-fuzz).

### Small (fixed this pass)

| Change | Where |
|---|---|
| `activity inbox --as-orchestrator` is renamed to `--as-nexus`, rejected outright rather than kept as an alias | `crates/mv-cli/src/args.rs` (`activity::inbox`) |
| `inspect --lines` metavar is `START:END`; help lists `START:END`, `N`, and `N-M` (the parser already accepted all three) | `crates/mv-cli/src/args.rs` |
| `backlog mark-asked --recommend` metavar is `N`; help matches `backlog ask` (1-based index into `--option`) | `crates/mv-cli/src/args.rs` |
| `dispatch --exclude-vendor` / `--input` / `--scope` help names them repeatable | `crates/mv-cli/src/args.rs` |
| `activity inbox --replay-since` / `--replay-last` match Python's argparse shapes (card `20261103-b-be6653`) | `crates/mv-cli/src/args.rs` (`activity::inbox`); handler in `commands::activity` |

`--as-orchestrator` no longer parses (`UnknownArgument`) — the role-rename spelling is `--as-nexus`, and the handler now reads it to authorize acknowledgment.

### Larger (backlog)

No remaining clap-surface gaps are carded. Board `--watch`/`--limit`/`--cards-per-row`/`--status` and `accounts usage --detail` / `notify --tag` / `notify --episode-state` / `dispatch --commits` / `activity inbox --replay-since` / `--replay-last` now match Python's argparse shapes (`crates/mv-cli/src/args.rs`, handlers in `commands::{board,accounts,notify,dispatch,activity}`). Both behavioral cuts previously noted here are closed: live-tty `--limit auto` now packs cards against the real terminal height (`crates/mv-cli/src/commands/board.rs`'s `auto_height`, `mv_board::layout::terminal_height`) rather than the fixed off-tty fallback of 10 cards; and `dispatch --commits` (with exactly one `--reviewing` lane) now overrides the Critic diff to only those commits, mirroring Python's `worktrees._commits_diff` (`RealDispatchStore::review_scope` in `crates/mv-cli/src/commands/dispatch.rs`, backed by `mv_engine::lane_diff::commit_set_diff`).

### Smaller surface notes (not carded)

Handlers already reject several of these at runtime; clap is looser than argparse.

- `inspect --reason`, `activity close --reason`, and `activity update --phase` are required on Python and optional on Rust (`activity close` defaults the reason to `closed`; `activity update --phase` defaults to `working`; `inspect --reason` defaults to empty).
- `backlog mark-asked --recommend` is required `int` on Python; Rust parses a string as a 1-based index in `commands::backlog` and errors if it is missing or non-numeric.
- Cwd-scoped `backlog` writes (`add` / `done` / `reopen` / `annotate` / `reprioritize` / `show` / `ask` / `mark-asked` / `answer`) take `nargs='+'` on Python so a mistaken project-name token is explained in-handler. Rust takes a single positional.
- `dispatch --column` is a closed `{Explore, Execute, Review}` choice on Python; Rust takes free text and looks up `column-{name}.md` case-insensitively (`crates/mv-engine/src/prompts.rs`).
- `dispatch --role` / `backlog add --role` / `backlog ask --role`: Python argparse still lists the retired keys (and treats `orchestrator`/`council` as undispatchable on `dispatch`). Rust write surfaces accept current worker names only; unknown and retired names exit 3.
- `notify --priority` is a closed `{low, default, high, urgent}` choice on Python; Rust takes a string and treats `urgent` as the bypass, everything else as normal (`commands::notify`).
- `config migrate` (`--dry-run`, `--backup`) is Rust-only. It rewrites retired role keys so an unmigrated Python `config.toml` can load.
- Root `-V` / `--version` is Rust-only (clap).

## Sources

- Python parser: `multiverse/cli.py` `build_parser()` in the frozen reference checkout.
- Rust parser: `crates/mv-cli/src/args.rs`.
- Role rename: `conformance/role_translate.py` `ROLE_RENAME`.
- Inbox replay: `crates/mv-cli/src/args.rs` (`--replay-since` / `--replay-last`); handler `crates/mv-cli/src/commands/activity.rs`; library `crates/mv-state/src/activity.rs` `replay_inbox`.
- Differential fuzz: `conformance/diff_fuzz.py` (`normalize_dry_run`, read probes); tests in `conformance/test_diff_fuzz.py`.

### Halt marker safety

An existing unreadable or corrupt fleet `halt` marker is treated as halted,
with reason `halt state is unreadable`. Only a missing canonical marker permits
legacy fallback. `halt off` explicitly clears the marker.

Inspect and result open file components with `O_NOFOLLOW | O_NONBLOCK` and
check the opened descriptor. Named pipes and other non-regular files are refused
without waiting for a writer.

`result` resolves a worktree or subdirectory cwd to the longest containing
configured project. Task results are read from that project's history, so a
lane can read a sibling result even if its checkout contains an older copy.

Inspect emits selected file text as-is, including credentials and terminal
control characters; only audit metadata is redacted. An audit append failure
prints a warning on stderr and preserves the inspection's outcome.

Health reports a failed pane read as `UNREADABLE`, scans the remaining panes,
and exits 3 if any problem is found. Reads target the pane ID when available;
failure to list the fleet still exits 1.

Health skips the hub's interactive Nexus pane, recognizing both current `nexu`
and historical `orch` names derived from the configured project and seat shorts.

Scratch requires a git-registered `multiverse/<lane-id>` worktree and the
routing-log occupant of `HERDR_PANE_ID` on that branch. It ignores
`MULTIVERSE_TASK_ID`, keeps the scratch record in the lane's own task directory,
and treats the optional read/close argument as a pane ID that must match.

Health follows Python's LIMIT/RATE/CONTEXT/DEAD/DIALOG order. DEAD requires
connection-loss vocabulary on the last event, skipping input-bar chrome and
recognizing progress as a later event. Context overflow includes `prompt is too
long` and `exceeds the available context`; trust, approval, vendor dialogs, and
question-menu footers use the existing prompt detectors.

Scratch open holds the owning project's routing-log lock, writes a mode-0600
intent record with an empty pane ID, and only then splits the current pane.
Success records the pane ID; split, rename, or completion failure attempts to
close any created pane and unlinks intent. Empty intent is retryable and cannot
be used by read/close. Rollback is best effort if Herdr or the filesystem also
fails during cleanup; a killed process cannot execute rollback.

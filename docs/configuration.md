# Configuration

Herdr Multiverse is pre-1.0. This page describes the configuration model
the example file and plugin wrapper already encode. Field names may still
shift before `1.0.0` — the invariants below (file locations, placeholder
handling, redaction) are the part that is expected to stay stable.

`mv-config` has a real loader, migrator, and placeholder walker, all
wired to the CLI. `config.example.toml` is the schema reference.

## Where configuration lives

Configuration and runtime state are **never** stored inside the managed
plugin checkout. Herdr provides two directories to every plugin action:

- `HERDR_PLUGIN_CONFIG_DIR` — editable configuration: `config.toml`,
  which holds accounts, roles, and projects. There is no fleet-wide
  `roster.toml`; see [Roster files](#roster-files) below for the one
  roster surface that does exist.
- `HERDR_PLUGIN_STATE_DIR` — runtime state: locks, caches, directives, and
  anything the binary needs to persist between invocations that isn't meant
  to be hand-edited.

The plugin wrapper (`scripts/plugin-action.sh`) requires those two
directories to be absolute, points `MULTIVERSE_CONFIG` at
`$HERDR_PLUGIN_CONFIG_DIR/config.toml`, points `MULTIVERSE_STATE_DIR` at
`$HERDR_PLUGIN_STATE_DIR`, and refuses to start if the config file is
missing (exit 3). It does not silently select a standalone global fleet
config.

`dispatch` and `council` also export both variables into every worker
or council pane they launch (`RealDispatchStore::prepare`; council
builds that store with `RealDispatchStore::for_seat`, and the same
values appear on the `--dry-run` card via `render_card`). The values
are the dispatching process's already-resolved absolute paths:
`Config::source_path` from `load_config` /
`mv_config::load::resolve_config_path_from_env`, and the state
directory from `mv_config::load::config_state_dir_from_env`. A worker's
own `multiverse` therefore uses the dispatcher's config and state,
including when the dispatcher resolved those paths via `--config` or
the environment.

When you invoke `bin/multiverse` directly (local development and the
conformance test harness), `MULTIVERSE_CONFIG` and
`MULTIVERSE_STATE_DIR` override the default locations the same way.
In a Herdr-managed install the plugin wrapper and dispatch already
export them.

Each project's own task history — briefs, results, status files, the
routing log, worktrees, backlog, and the Nexus seed
(`.multiverse/nexus.md`; a project still carrying the legacy
`.multiverse/orchestrator.md` keeps reading and writing that file until
its next seed write, which migrates it onto `nexus.md`) — lives under
that project's `.multiverse/` directory, not in the plugin's config or
state directories. That history is read as-is across upgrades; Multiverse
does not rewrite it except through an explicit, versioned migration (see
[Migrations](#migrations) below).

## `config.toml`

The example file, [`config.example.toml`](../config.example.toml), is the
intended fleet-wide schema:

- Top-level knobs such as `max_concurrent`, quota floors, and board
  settings. Board colour is off by default until the terminal is
  confirmed — see [Board colour](#board-colour).
- `[accounts.<name>]` — one seat per vendor login (`kind`,
  `config_dir_var`, `config_dir`, `pool`, `available`, optional `short`,
  `interactive`, `command`, `model`, `nexus_slots`). `kind` must be one of
  `claude`, `codex`, `grok`, `local` — anything else, including a
  mis-cased spelling, is a load-time config error (exit code 3) that
  names the bad value and suggests the closest accepted one. `command` is required
  for every `kind = "claude"` account, not only an `interactive` one — a
  chain that reaches a `claude` seat with no `command` set loads fine but
  refuses at dispatch time ("no command wrapper configured"). Codex and
  Grok don't use it. `nexus_slots` (default `0`) is how many Nexus panes
  that seat hosts, one per started project whose `nexus` chain routes
  there; a seat's open-agent cap (set by `multiverse accounts limit`) now
  counts worker panes only, so a Nexus pane never eats a shared seat's
  worker capacity, and the fleet-wide `max_concurrent` check excludes
  Nexus panes entirely. `multiverse accounts` reports both counts on a
  capped seat: `workers N/cap, nexus M/nexus_slots`. An explicitly written
  `vendor` must be one of `claude`, `codex`, `grok`, `chatgpt`, `local` —
  anything else, including a mis-cased spelling, is a load-time config
  error (exit code 3) that names the bad value and suggests the closest
  accepted one, the same check `kind` gets above. `vendor` defaults to the
  part of `pool` before its first `:` when left unset; that derived
  default is not checked against the list, since `pool` is free-form
  grouping and need not resemble a vendor name.
- `[roles.<key>]` — write scope, effort, `on_exhausted`, skills, and a
  `[roles.<key>.models]` table keyed by agent kind. `write_scope` must be
  one of `none` or `declared` — anything else, including a mis-cased
  spelling, is a load-time config error (exit code 3) that names the bad
  value and suggests the closest accepted one. An explicitly written
  `effort` outside `minimal`, `low`, `medium`, `high`, `xhigh`, `max` — the
  values multiverse has seen accepted; the vendor CLI is the authority —
  is a load warning, not a load-time error: it names the bad value, the
  `models` kind(s) it would be forwarded to, and the known values, and the
  config still loads with the value passed through unchanged. An empty
  string is still a load-time config error. This one `effort` string
  applies to every `kind` the role's `models` table names, not per vendor,
  since Multiverse forwards it verbatim to whichever vendor CLI that seat
  runs, and the vendors accept different sets this repo cannot state as
  one closed list (Claude Code and codex accept `xhigh`; codex also
  accepts `minimal` but not `max`) — a hard error here would break a
  working config on a vendor-CLI upgrade. The stable keys are
  `scout`, `builder`, `archivist`, `critic`, `seer`, `artisan`, `watcher`,
  plus `nexus` / `council` chains. Seven worker roles dispatch with
  `--role`; Council is its own subcommand; Nexus is seeded by `start`.
- `[projects.<name>]` with `path` and `[projects.<name>.chains]` — ordered
  account lists per role. `path` is required for every project except the
  reserved `multiverse` hub project, whose `path` is optional and defaults
  to `<state dir>/home` when omitted — `multiverse start multiverse` creates
  that directory if it does not exist yet. If no chain is configured for a
  role, loading that dispatch is a configuration error (exit code 3) naming
  the role and project, not a silent default. `autostart` (boolean, default
  `true`) is read-only — nothing writes it — and only changes what
  `multiverse start --all` does: a project with `autostart = false` is
  skipped, with one line naming it, while `multiverse start <name>` still
  starts that project when named explicitly. A non-boolean value is a
  configuration error (exit code 3).
- Optional `[review_policy]`, `[secrets_scan]`, `[activity.pairs]`, and
  `[mail.pairs]`. Like every other section, `[activity]` and `[mail]` warn
  on any key other than `pairs` and suggest the closest match (a typo'd
  `piars`, say) rather than silently ignoring it — this is a warning, not
  a load-time error, since an unknown key here does not block loading the
  way a bad `kind` or `vendor` value does above. An explicitly written
  `review_policy.review_effort` outside the same known values as
  `roles.<key>.effort` is the same kind of load warning described above —
  the config still loads and the value is still passed through. An empty
  string is still a load-time config error.

**Sweep stall wake.** A Claude lane can end its turn to wait on its own
background shell, and Herdr reports it idle exactly like a finished lane.
If the job's completion notification then fails to wake it, `multiverse
sweep` sends one stall wake (printing `woke stalled: <id>`, or `would wake
stalled: <id>` under `--dry-run` or a fleet halt), but only after that
lane's one-shot `finish: write result.md and status` nudge is spent and
only while it still has no result. The pane must show
`⏺ Background command "…" completed (exit code N)` below its last turn-end
line, with only an empty composer after it, and that pane text must have
stayed unchanged for 15 minutes, not counting host sleep. The wait is a
fixed constant, not a config key. The wake is claimed once per task in
`.multiverse/stall-wake-memory.json`. The pane-unchanged clock lives in
`.multiverse/idle-pane-memory.json` (`idle_pane_since` in
`crates/mv-state/src/nudge_memory.rs`): an applying sweep pass seeds and
rewrites it; `--dry-run`, a fleet halt, and `MULTIVERSE_READ_ONLY=1`
read an existing clock and never write the file. Codex lanes are not
woken this way yet. A lane whose footer still shows a running background
shell or monitor (below) is never stall-woken: the job it waits on has
not finished, even when an earlier background command's completion line
is on screen.

**Stuck resume.** A lane past the stuck threshold with a matching pane
banner is not resumed on the first observation. `sweep` records that
read in `.multiverse/stuck-signal-memory.json` and only confirms stuck
on a later pass that sees the same fingerprint
(`stuck_signal_confirmed` in `crates/mv-state/src/nudge_memory.rs`).

**Waiting on a shell.** A Claude lane back at its prompt while a
background shell or monitor it started is still running shows a footer
segment such as `· 1 shell ·`, `2 shells still running`, or `1 monitor
still running` below its composer, and Herdr reports it idle/done exactly
like a finished lane. `multiverse workers` prints such a lane's STATUS as
`waiting-on-shell`, and the board card reads `waiting-on-shell: a
background shell is still running — not finished, do not reclaim` instead
of `stalled`. Both read the pane text they already read (`workers` for its
`stalled` check, the board for its blocking-prompt probe), so neither
makes an extra Herdr call. The label is display only and takes precedence
over `stalled`, since a long, quiet test run is the lane waiting rather
than frozen. Only lines below the pane's last `❯` composer line are read,
so the same words in the transcript never count. The `workers N/CAP` line
and the dispatch gate are one number, computed by one function from the
same Herdr listing. It counts every unrecorded worker lane whose agent
Herdr still lists, whatever its STATUS reads — working,
`waiting-on-shell`, any other idle/done lane whose OUTCOME is `running`,
and `done-unrecorded` rows until they are reconciled — and any live pane
carrying a dispatched worker name that no task history accounts for. A
lane dispatched within its seat's startup grace (default 600 seconds)
counts even before Herdr lists it, so `N` can be one more than the table
has rows for a minute after a dispatch. Nexus sessions are excluded, as
the `nexus sessions` line says, and so are recorded lanes whose pane
Herdr has not reaped yet (they still print a row). Several tasks under
one reissued agent name count once. At admission, under the fleet lock,
dispatch always asks Herdr for a fresh listing and refuses (or, with
`--wait-for-seat`, waits) whenever this count is at or above
`max_concurrent`. Planning, including `--dry-run`, makes that call only
when its cheaper count from task history alone is already full, so a dry
run can report room that admission then refuses. If Herdr cannot be asked, the gate counts
every unrecorded worker lane instead. A refusal names the count the way
the panel does:
`workers N/CAP: N agent(s) already running and max_concurrent is CAP;
nothing was started`. `tasks --reclaimable` and the reclaim pass every
dispatch runs first keep a recorded Claude lane's pane open while its
footer still shows a running shell (`kept … waiting-on-shell` in
`--reclaimable`'s report, `kept N waiting-on-shell pane(s)` before a
dispatch), and also keep a Claude pane whose text cannot be read.

A finished lane can leave processes behind whose cwd is still inside its
worktree: a `tail -f` of its suite log, a `zsh -c`, a `ugrep`, an ssh
ControlMaster, each reparented to pid 1. Such a process makes
`worktrees --merge` refuse with "lane has a live process cwd". The refusal
lists each holder's pid, command and cwd, and names the command that ends
them, `worktrees <path> --prune --reap --yes`. `--prune` checks every
*dead* lane for leftovers, whatever its reason for being kept. A dead lane
is one whose pane Herdr no longer lists and that either has a recorded
pane or a terminal outcome for every task in it. A plain `--prune` names
each leftover. `--reap --dry-run` prints `would reap` for each one.
`--reap` with `--yes`, or without `--dry-run`, ends them: an ssh master
gets `ssh -O exit -S <socket>` first, then each process gets SIGTERM, a
short wait, a re-check of its command and cwd, and SIGKILL only if it is
still the same process. `tasks --reclaimable --yes` does the same for a
lane right after it closes that lane's pane, `--reap` or not, printing
what it ended. Without `--yes` it prints only `would reap`.
`--reclaimable --reap` also covers a stale entry whose pane was already
gone. In a dead lane, a shell is reaped like any other leftover. Pid 1,
Herdr, the running command and every ancestor of it are never reaped,
and nothing is reaped under `--force`.

`worktrees --merge` ends known-benign leftovers itself, whether or not
the lane's pane is gone. A holder is benign only when its command name
and its parent agree, never because of its cwd alone: a `caffeinate` or
an ssh ControlMaster that is orphaned (parent pid 1) or whose lane's pane
is gone; a `tail`, `ugrep` or `grep` that is orphaned; a shell whose
lane's pane is gone. When every holder is benign, `--merge` prints
`ending leftover pid …` for each, ends it the way `--reap` does, and
retries the merge once. A holder that is still there after that one pass
refuses the merge again, named. Any other holder, such as a stopped
pane's own shell or a running build, refuses the merge without ending
anything. The refusal lists each holder's pid, full command, cwd and why
it is not a known leftover. To keep ssh masters out of lanes, give lane
ssh sessions a short `ControlPersist` or a `ControlPath` outside the lane.

A `<<PLACEHOLDER: ...>>` marker is carried through, never guessed at, by
any tool that loads the file (`mv_config::placeholder::MaybePlaceholder`)
— a run against an unfilled placeholder fails with a configuration error
(exit code 3) rather than silently falling back to a default.

Comments you add to `config.toml` are preserved across tool-driven edits:
`mv-config`'s `edit` module appends a new `[accounts.<name>]` or
`[projects.<name>]` block via `toml_edit`'s `DocumentMut`, which keeps
every comment and the original formatting of everything it does not
touch; edits are always appends, never a full-file rewrite.
`multiverse add` itself currently appends the new project block as
plain text rather than going through `mv-config::edit` (the module is
implemented; this one caller isn't wired to it yet), which has the same
append-only effect on an unchanged file.

## `[secrets_scan]`: the merge gate

`multiverse worktrees <path> --merge <task>` refuses to merge a lane whose
commits trip a secrets scan. `[secrets_scan]` configures it (see
[`config.example.toml`](../config.example.toml) for every key and its
default, spelled out):

- `gitleaks` (default `"gitleaks"`) — a bare command resolved against
  `PATH`, or an explicit path.
- `trufflehog` (optional, unset by default) — run as a second pass, after
  gitleaks passes, only when set *and* actually found.
- `required` (default `true`) — a missing `gitleaks` blocks the merge
  (exit code 3, with an install hint) rather than letting an unscanned
  lane land silently; `false` only warns and lets the merge proceed.
- `allowlist` (default `".gitleaks.toml"`) — a path relative to the
  project being merged (not to `config.toml`), passed to gitleaks as
  `--config` only when that file exists there.

A finding refuses the merge (exit code 1) and writes the triggering
tool's output to `.multiverse/evidence/<task>-secrets.txt` in the
project; nothing is merged. `gitleaks` is run with its own `--redact`
flag, so its output is already redacted before Multiverse ever sees it.
TruffleHog has no verified-offline equivalent of `--redact`, so Multiverse
instead runs it with `--json` and strips the raw-secret fields
(`Raw`/`RawV2`/`Redacted`) from its own output before writing it, then
passes the result through `mv-core::redact` (see [Redaction](#redaction)
below) as a second pass. `multiverse check` reports when `[secrets_scan]`
is required and `gitleaks` cannot be found.

## Merge gate: outcome and ignored files

Apart from the secrets scan, `worktrees --merge <task>` checks two more
things before it lands a lane:

- **Outcome.** `<task>` can be the lane's own task id or the id of any
  `dispatch --into` round that adopted the lane. In both cases the lane's
  own branch, `multiverse/<lane>`, is what merges. The gate looks at every
  task that has owned the lane and takes the newest outcome the
  project's `routing.log` records for them. That includes
  `tasks --supersede` corrections, but only when the correction targets
  that newest outcome line. The merge goes ahead only if that outcome is
  `done` and the task has a non-empty `result.md`. The status file is a
  fallback, used only when the newest round has no recorded outcome yet
  or the log names no owner. `tasks --supersede --status done --yes` also
  rewrites the task's status file inside its worktree (when the worktree
  still exists), so the file matches the corrected record.
- **Ignored files.** The merge refuses on a gitignored file in the lane
  unless that file is regenerable. The built-in list is `.multiverse/`,
  `*.pyc`, any `target/`, `__pycache__/`, `node_modules/`,
  `prisma/generated/` or `*.egg-info/` directory, and root-level `bin/`,
  `.venv/` and `.pytest_cache/`. If a project's `lane_setup` produces
  other ignored output, declare it in the project's own
  `.multiverse/roster.toml`:

  ```toml
  [lane_setup]
  regenerable = ["src/generated/", "*.tsbuildinfo"]
  ```

  Entries follow gitignore rules: a trailing `/` matches only a directory;
  a pattern with a leading or inner `/` is anchored at the repository root;
  a single-component pattern matches at any depth; `*` and `?` match
  within one path component. An entry that is empty, contains a `..`
  component or contains a control character, an unknown key in
  `[lane_setup]`, or a non-list value is a load error (exit code 3). For
  any other ignored file, the refusal names the path and shows the
  `regenerable` entry that would declare it. This list applies only to
  `--merge`; `--prune` still uses just the built-in list.

## Board colour

`multiverse board` emits ANSI SGR color only when the terminal is
confirmed. A first Linux install inside a Herdr board tab otherwise
prints fragments like `[92m` / `[0m` as text: the renderer emits a
real ESC byte (`0x1b`) in front of every SGR sequence, `visible_width`
skips those sequences, and if the pane then drops the ESC the leftover
brackets become visible characters the width math did not count, so
cards overlap.

Color stays **off** unless every one of these is true:

- stdout is a tty
- `TERM` is unset or is not `dumb`
- the process is not inside a Herdr pane (`HERDR_ENV=1` or a non-empty
  `HERDR_PANE_ID` — plugin-directory variables and `HERDR_BIN_PATH` do
  not count)
- the terminal names color support: a non-empty `COLORTERM`, or `TERM`
  containing `color`, `256`, or `xterm` (matched case-insensitively)

`--no-color` and a non-empty `NO_COLOR` always suppress color, including
when the terminal is confirmed and when `--color always` is passed.
`--color {always,auto,never}` (default `auto`) is the board flag:
`always` forces color on even inside a Herdr pane and even when stdout
is not a tty; `never` is the same as `--no-color`. `--no-color` wins if
both are passed. `[board].color` is a boolean (default `false`); `true`
is the config equivalent of `--color always`, for a fleet that wants
color in the dashboard / home board tabs Herdr starts. A non-boolean
value such as `"always"` is a load-time type error.

The home `board` tab and per-project dashboard tab that `multiverse
start` opens do **not** pass `--color` or `--no-color` on the pane-run
script; the board process applies the rules above itself, so an
operator can opt in with `--color always` or `[board].color = true`
without the launch argv fighting them.

## Roster files

Accounts and roles both live in `config.toml` — there is no separate
fleet-wide roster file. The one `roster.toml` the loader reads is
per-project: **`<project>/.multiverse/roster.toml`**, inside the
project's own repository, naming chain overrides (`[chains]`) and,
optionally, the merge gate's `[lane_setup] regenerable` list (see
[Merge gate: outcome and ignored files](#merge-gate-outcome-and-ignored-files)).
An override
replaces the named role's chain entirely; roles the file does not
mention keep the chain from `config.toml`. An unknown top-level table
there is a hard error, not a warning or a silently ignored key. A
worker chain may not name an `interactive = true` account unless
`lend_interactive_to_workers` is true; the Nexus chain may.

`config.toml`'s own `[accounts.<name>]` / `[roles.<key>]` split (above)
is what lets two roles safely share or avoid sharing the same
underlying vendor account; a project's `roster.toml` only overrides
which accounts a role's chain draws from, per project.

## Environment variables

| Variable | Purpose | Used by |
|---|---|---|
| `HERDR_PLUGIN_CONFIG_DIR` | Herdr-provided directory for `config.toml` | `scripts/plugin-action.sh` |
| `HERDR_PLUGIN_STATE_DIR` | Herdr-provided directory for runtime state | `scripts/plugin-action.sh` |
| `HERDR_PLUGIN_ROOT` | Root of the installed plugin checkout (binary, scripts) | prompt/template resolution |
| `MULTIVERSE_CONFIG` | Absolute config-file path. Plugin wrapper sets it from `$HERDR_PLUGIN_CONFIG_DIR/config.toml`; `dispatch`/`council` set it on every worker or council pane to the dispatcher's already-resolved path, so a worker's own `multiverse` uses the dispatcher's config. Direct `bin/multiverse` invocations also use it as a development/testing override. | plugin wrapper; `RealDispatchStore::prepare` / `render_card`; `mv_config::load::resolve_config_path_from_env` |
| `MULTIVERSE_STATE_DIR` | Absolute fleet state directory. Plugin wrapper sets it from `$HERDR_PLUGIN_STATE_DIR`; `dispatch`/`council` set it on every worker or council pane to the dispatcher's already-resolved state dir, so a worker's own `multiverse` uses the dispatcher's state. Direct `bin/multiverse` invocations also use it as a development/testing override. | plugin wrapper; `RealDispatchStore::prepare` / `render_card`; `mv_config::load::config_state_dir_from_env` |
| `MULTIVERSE_INVOCATION` | The command a pane uses to re-invoke Multiverse; set by the start action | plugin wrapper, Nexus seed renderer |
| `MULTIVERSE_READ_ONLY` | Set on every plugin action except `start` or `import-python` | plugin wrapper |
| `MULTIVERSE_PROMPTS_DIR` | Override embedded `prompts/` | `mv-engine::prompts` |
| `MULTIVERSE_TEMPLATES_DIR` | Override embedded `templates/` | `mv-engine::prompts` |
| `MULTIVERSE_DIRECTIVES` | Override the fleet directives directory | `mv-engine::directives` |
| `MULTIVERSE_TASK_ID` | Full task id a dispatched lane's own session is expected to export; read by `scratch`/`backlog` to find that task's pane/record | skill playbooks (the docker skill body, `mv-engine::skills`); `mv-engine::runner`'s `PreparedLaunch` does not set it in the launched process's environment — `dispatch --yes` and `start` do not export it today, so a lane must `export MULTIVERSE_TASK_ID` itself before relying on it |
| `HERDR_BIN_PATH` | Override the `herdr` binary Multiverse shells out to | `mv-herdr`; tests and conformance against `fake-herdr` |
| `MV_FAKE_HERDR_STATE` | Disposable JSON path for `fake-herdr` mutations | `fake-herdr` |

## Shared cargo build-dir

This repository's `.cargo/config.toml` sets
`[build] build-dir = "{cargo-cache-home}/shared-build/{workspace-path-hash}"`.
Cargo expands `{workspace-path-hash}` from the workspace's manifest path,
so each lane worktree builds into its own hash dir under one shared root
instead of reusing another lane's artifacts. `CARGO_BUILD_BUILD_DIR`
overrides it with the same precedence as any other `[build]` key.

`worktrees --prune` (Rust-only) reclaims these hash dirs, but only when the
active template contains `{workspace-path-hash}`; without it nothing under
the root counts as a per-lane cache.

- **Per lane:** a finished lane's hash dir is computed from its own path,
  then freed only when that dir's dep-info names the lane. The shared root
  and the main checkout's own hash dir are never freed. A lane in the
  routing log whose worktree is gone has its computed hash dir freed only
  when the dir's evidence names nothing outside that lane.
- **Orphan sweep:** any other hash dir under the shared root is freed only
  when every workspace its evidence names is gone. It is kept if it names
  any live workspace or was not read in full.
- **Authorization:** caches are freed only with `--yes` (or `--force`).
  Plain `--prune` and `--dry-run` only report `would free`.
- **Output:** `freed`/`would free build-dir (N bytes)` under a lane, or
  `kept build-dir — <reason>` / `build-dir unresolved — <reason>`. The
  orphan sweep prints `cargo shared-build <dir>` followed by the same
  freed / would-free line, or `kept build-dir — <reason>` when a delete
  failed (`crates/mv-cli/src/commands/worktrees.rs`).

The full rules are in the `--prune` row of
[`docs/parity.md`](parity.md#worktrees).

## Codex lane isolation

Every dispatched or council Codex seat launches with
`-c default_permissions="fleet"` and defines that `fleet` permission
profile on the same command line (`apply_codex_worktree_permissions` in
`crates/mv-engine/src/runner.rs` always emits
`permissions.fleet.extends=":workspace"` even when the filesystem table
cannot be built), so no `~/.codex/config.toml` setting is needed or
consulted for it:

- `permissions.fleet.extends=":workspace"` — reads anywhere, writes to the
  workspace root (`-C <worktree>`) and temp directories.
- `permissions.fleet.filesystem={...}` — `write` on the worktree, the
  project's common `.git` and the lane's own gitdir, `~/.docker/buildx`,
  and Go's hardcoded caches `$HOME/Library/Caches/go-build` and
  `$HOME/go/pkg/mod` (`crates/mv-engine/src/codex_worktree_permissions.rs`);
  `deny` on every entry *beside* the path from the project's parent
  directory down to the worktree — sibling projects, the main checkout's
  own files and `.multiverse` state, and other lanes' worktrees. Other
  lanes' gitdirs under `<common>/.git/worktrees/` are denied the same
  way, and `.git/hooks` is always denied, so a seat cannot retarget
  another lane's HEAD/index or plant a hook that later runs unsandboxed.
- For a writer lane, `write` on the project's `.multiverse` directory
  itself, so the lane-side commands its brief names keep working:
  `multiverse activity update` and `activity inbox --peek` append to
  `routing.log` under `routing.log.lock`, maintain the
  `routing.log.index`/`routing.log.index.fp` sidecars and rewrite
  `activity.lastseen`, and `multiverse scratch open|read|close` takes the
  same lock. Those entries are left undenied; `worktrees` and
  `activity.cursor` are narrowed back to `read` (a lane may read the
  Nexus's inbox cursor but never advance it), and every other entry
  there — `tasks` (other tasks), `briefs`, `inputs`, `evidence`,
  `rescued`, `worktree-state`, the Nexus seed, `PASSDOWN.md` — stays
  `deny`, which also refuses renaming or deleting it. The lane's own
  `result.md` and `status` live in `<worktree>/.multiverse/tasks/<task>`,
  inside the worktree's own grant. Directory-level `write` is needed
  because the lock path sets the directory's mode and the sidecars are
  replaced through a temp file beside them.
- For a reader dispatch (no worktree), `write` on `.multiverse` (the
  lock `chmod` and sidecar temp files), `.multiverse/tasks/<task-id>`,
  and the lane-state files (`routing.log` and its lock/index,
  `activity.lastseen`); `read` on every other existing child of
  `.multiverse` and of the project. `:workspace` already writes the
  project cwd, and a same-path `read` loses to that `write`, so the
  project root is not listed; the child `read` entries are the
  carve-out that stops a rewrite of `PASSDOWN.md`, other tasks, briefs,
  or the checkout. The task directory stays writable as a child `write`
  under `tasks` `read`. That seat can still write its own
  `result.md`/`status` and append to the routing log.

A profile with a `deny` entry is the only Codex setting that restricts
reads: Codex's macOS Seatbelt policy then drops its blanket read allow, so
a denied path is unreadable to every command the agent runs. The parent
directories themselves stay readable (git needs them to resolve the
worktree's real path), so a lane can list sibling names but not open
them. The deny list is computed when the lane launches; anything created
after that — a later lane's worktree — stays readable, and a new entry
directly under the project's `.multiverse` is also writable. Ancestors of
`$HOME` are never listed. A project sitting directly in `$HOME` does
deny its HOME-level siblings (`~/other-project`, `~/.ssh`) except the
toolchain directories the builder already names (`~/.cargo`,
`~/.rustup`, `~/.codex`, and the ancestors of the HOME-relative write
grants). If `$HOME` does not resolve, those HOME-relative grants are
omitted rather than rewritten under `/`. Sibling names that contain C0
control characters are escaped in the table so a planted newline cannot
drop the filesystem map. The profile is never combined with
`--sandbox`/`sandbox_mode`, which cannot deny reads. The builder is
`crates/mv-engine/src/codex_worktree_permissions.rs`.

## Vendor folder trust

Codex and Grok both ask, the first time they open a folder, whether to
trust it, and park there until someone answers. A dispatch answers that
question ahead of time in the seat's own config directory, for the
project root and the lane worktree (one entry when they are the same
path), immediately before the seat starts:

- Codex: `[projects."<path>"] trust_level = "trusted"` in
  `$CODEX_HOME/config.toml` (`crates/mv-engine/src/codex_trust.rs`).
- Grok: `[folders."<path>"] trusted = true` (plus `decided_at`, the Unix
  time) in `$GROK_HOME/trusted_folders.toml`, under the
  `trusted_folders.toml.lock` lock Grok itself uses
  (`crates/mv-engine/src/grok_trust.rs`). This is Grok's own folder-trust
  store, the one `grok --trust` and an interactive `y` write. A nested
  git checkout counts as a separate folder to Grok, so the lane gets its
  own entry.

Paths are canonicalized (`/private/var/...`, not `/var/...`, on macOS).
Entries already marked trusted are left alone and everything else in
the file is kept as it was. An explicit "not trusted" entry
(`trust_level` other than `"trusted"`, or `trusted = false`) is never
overwritten: the dispatch is refused and names the folder. Remove or
change that entry to dispatch there. A Grok seat with no `GROK_HOME` in
its launch environment is skipped rather than falling back to `~/.grok`.

If a trust dialog shows up anyway (a seat not configured as above, or a
Claude "Do you trust the files in this folder?"), the dispatch never
answers it. It waits up to the readiness budget for someone to clear it,
then delivers the brief. If the dialog swallowed a brief that had
already been typed, the brief is sent again once. If the dialog is still
up when the wait ends, the pane is kept, the task is **not** recorded
`failed-to-start` (it stays awaiting on that live pane), and
`.multiverse/evidence/<task>-failed-to-start.txt` is written with
`reason: needs-attention:trust-dialog`. If the pane has left the dialog
by then, the dispatch is recorded `failed-to-start` with that reason as
usual.

**Failed-to-start evidence.** When a dispatch fails after a pane exists,
`finalize_dispatch_attempt` writes
`.multiverse/evidence/<task>-failed-to-start.txt`
(`capture_failed_to_start_evidence` in `crates/mv-engine/src/runner.rs`)
unless a tripped breaker already wrote pane evidence under that
directory. A failure before the pane exists writes none. The file
starts with `reason: <tag>`. The same tag is the routing-log `reason=`
field (`failed_to_start_reason`):

- `needs-attention` — a parked dialog or menu other than a still-up
  trust dialog
- `needs-attention:trust-dialog` — a trust dialog (evidence is written;
  the task is not recorded `failed-to-start` while the dialog is still
  up)
- `not-ready-timeout` — the pane did not become readable and ready
- `unreadable-pane` — the pane is unreadable
- `blank-after-prompt` — the pane went blank after the prompt was typed
- `start-error` — every other start failure

## Redaction

Config and state files may contain account identifiers or paths that
shouldn't appear verbatim in logs, activity records, or bug reports.
`mv-core::redact` implements that cleaning (PEM blocks, bearer tokens,
AWS-shaped keys, and credential-named assignments) and is called from
`backlog`, `activity`, `result`, `inspect`, brief rendering, and the
`[secrets_scan]` merge gate's TruffleHog path (above). When
filing a bug report, still redact anything the
[bug report template](../.github/ISSUE_TEMPLATE/bug_report.yml) asks you to
check for — automated redaction is a safety net, not a substitute for
reading what you are about to paste.

## Migrations

When a config shape changes between releases — for example, a role key
rename — loading an old-format file is refused, not auto-migrated: every
retired role key, wherever it appears (`[roles.<old>]`, a chain entry,
or a `roles_allowed` value), fails config loading with a message naming
the key and pointing at `multiverse config migrate`
(`crates/mv-config/src/load.rs`). Migration itself is always the
explicit, manual step below — a human decides when to rewrite the live
file, keeping comments and a backup. No alias is accepted on new input
after migration; history (existing task directories, routing logs) is
still read under the old names. See
[legacy on-disk names](roles.md#legacy-on-disk-names).

`mv-config::migrate` implements the rewrite (`migrate_role_keys` /
`migrate_file`). `multiverse config migrate` is the operator-facing CLI for
it — a manual step run once per fleet at cutover, not something triggered
automatically on every start:

```sh
multiverse config migrate [--config PATH] [--dry-run] [--backup]
```

- `--config PATH` selects the file to migrate, with the same precedence as
  everywhere else: `--config` beats `MULTIVERSE_CONFIG` beats the default
  `~/.config/multiverse/config.toml`.
- `--dry-run` prints a diff of what would change and writes nothing.
- `--backup` is required to actually write. It writes a timestamped
  `<file>.bak-<UTC timestamp>` copy of the original next to it, then
  replaces the file atomically. Without `--backup` (and without
  `--dry-run`), the command refuses and names both flags — there is no way
  to migrate a file in place without a safety copy.
- Running it again against an already-migrated file reports nothing to do
  and writes neither a backup nor a new file.

## See also

- [`docs/roles.md`](roles.md) — what each role is allowed to do once
  configured.
- [`docs/architecture.md`](architecture.md) — which crate owns configuration
  loading and validation.
- The README's [Configuration](../README.md#configuration) section for the
  minimal path to a working config.
- [`docs/install.md`](install.md) for plugin config/state directories and
  the wrapper's refusal to guess a config path.

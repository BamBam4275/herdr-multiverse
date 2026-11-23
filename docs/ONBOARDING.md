# Onboarding

You have installed Multiverse, opened the board, and found it full of words
you have not been told about. This page explains them. Read it top to
bottom on day one, then come back to the [Board legend](#board-legend)
whenever a card says something new.

1. [Setup](#setup): where the config lives, how to add a seat for each
   vendor, and how to check a seat works.
2. [Roles](#roles): the nine roles on one screen.
3. [Chains](#chains): which seat each role runs on, and in what order.
4. [Model choice](#model-choice): which model tier to give each role.
5. [Day-one cheat sheet](#day-one-cheat-sheet): the dozen commands you
   will actually run.
6. [Board legend](#board-legend): every word the board and `workers` print,
   and the one command that fixes each.

`multiverse check` links its notes and warnings to sections of this page,
as `see docs/ONBOARDING.md#<section>`. The shorter command list that
`multiverse onboard` prints at the end, and a word-by-word table of what
`check`, `workers` and the board print, are in
[`docs/cheat-sheet.md`](cheat-sheet.md).

## Setup

### Where the config file lives

There is one fleet config file, `config.toml`. Multiverse uses the first
of these locations that applies:

| How you run Multiverse | Config file |
|---|---|
| Through Herdr (plugin actions, the Start action, dashboards) | `$HERDR_PLUGIN_CONFIG_DIR/config.toml`. Find the directory with `herdr plugin config-dir multiverse` |
| `multiverse --config <path> ...` | `<path>` |
| `MULTIVERSE_CONFIG=<path>` exported | `<path>` |
| Neither of the above | `~/.config/multiverse/config.toml` |

The default path is the same on Linux and macOS. Multiverse does not read
`$XDG_CONFIG_HOME` or `~/Library/Application Support`. A leading `~` in
`--config` or `MULTIVERSE_CONFIG` is expanded to your home directory.

When Multiverse runs through Herdr, the plugin wrapper sets
`MULTIVERSE_CONFIG` from Herdr's config directory and ignores
`~/.config/multiverse/config.toml` and `$XDG_CONFIG_HOME`. If the file is
missing there, the wrapper exits 3 and does nothing else. A standalone
`multiverse onboard` (run from a plain terminal, with no `--config`)
writes `~/.config/multiverse/config.toml` and says
`Config location (default location)`. That is the one file the plugin
wrapper will not see: copy it to `$HERDR_PLUGIN_CONFIG_DIR/config.toml`,
or pass `--config` / export `MULTIVERSE_CONFIG`, if you also run through
Herdr.

Fleet runtime state (locks, breakers, gauges, halt marker) lives in a
separate directory. Inside Herdr that is `$HERDR_PLUGIN_STATE_DIR`.
`MULTIVERSE_STATE_DIR` overrides it. With neither set, it is the directory
the config file sits in. `multiverse check` prints both paths on its first
two lines. Each project's own history (briefs, results, routing log,
backlog, worktrees) lives in that project's `.multiverse/` directory.

Create the config with `multiverse onboard`. It starts from
[`config.example.toml`](../config.example.toml) with its comments kept,
walks you through one seat per vendor, offers the recommended chains and
model tiers below, and can register your first project. Add `--dry-run`
to see the plan without writing anything, or `--non-interactive --config
<path>` to apply the defaults from a script. `onboard` prints
`Would write` only on `--dry-run` or before a `[Y/n]` confirmation; when
it is actually writing it says `Writing`, then `Wrote`. Run over a config
that already has accounts, it merges into it only after you confirm
(`--yes` when non-interactive).

You can also copy `config.example.toml` to the location above by hand.
Either way, fill in every `<<PLACEHOLDER: ...>>` that is left. Multiverse
will not guess a placeholder: `check` lists each one you have left, and
real dispatch is refused until they are all filled in.

### The built-in multiverse hub

`config.example.toml` ships a `[projects.multiverse]` table, and `onboard`
keeps it. It is the home hub: the home workspace for cross-project chat,
and what `multiverse start` opens with no project argument. It is not a
repository you added. `check` marks it `(built-in home hub: cross-project
chat; not a project you added)` in its `projects:` list, and `workers`
marks it `(home hub)` on its `projects:` line. Leave its `path` unset so
it defaults to `<state dir>/home`. Before the first `start`, `check
--verbose` may say `multiverse worktree pile check unavailable`, because
that directory does not exist yet; `start` creates it.

### Accounts

A seat is one vendor login, declared as one `[accounts.<name>]` table:

```toml
[accounts.claude-personal]
kind = "claude"                  # which CLI: claude, codex, or grok
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/home/you/.claude-personal"   # absolute; `~` is not expanded
pool = "claude:personal"         # quota pool: two logins means two pools
available = true
short = "cp"                     # 2-3 letters shown in agent names
```

What each field does:

- **`config_dir_var` + `config_dir`** choose the login. Multiverse sets
  this variable to this directory when it launches the seat. That is the
  only thing that chooses an account. An API key in your environment
  chooses a billing route, not an account. Never export
  `ANTHROPIC_API_KEY`: a spawned `claude` uses it and bills the API
  instead of your subscription.
- **`kind`** must agree with `config_dir_var`. `claude` reads
  `CLAUDE_CONFIG_DIR`, `codex` reads `CODEX_HOME`, and `grok` reads
  `GROK_HOME`. A mismatch is a config error.
- **`pool`** decides concurrency. The part before the first `:` is the
  vendor, and the vendor decides diversity: Critic avoids the author's
  vendor, and Council seats one account per vendor.
- **`interactive = true`** marks the seat you talk to. `multiverse start`
  runs each project's Nexus on it. Only one account may set it. Worker
  chains may not name it unless the top-level
  `lend_interactive_to_workers = true` is set.
- **`available = false`**, optionally with `unavailable_reason` or
  `unavailable_until`, takes a seat out of rotation without deleting it.

### Claude seats

```toml
[accounts.claude-personal]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "/home/you/.claude-personal"
pool = "claude:personal"
command = "claude-personal"      # required for every claude seat
```

`CLAUDE_CONFIG_DIR` is where Claude Code keeps that login's credentials,
settings and session history. A different directory is a different
account. Sign in once, by hand:

```sh
CLAUDE_CONFIG_DIR=/home/you/.claude-personal claude    # then /login and /status
```

Every Claude seat also needs `command`, the command that launches that
seat. It is usually a shell function in your interactive shell's rc file:

```sh
claude-personal() { CLAUDE_CONFIG_DIR="$HOME/.claude-personal" claude "$@"; }
```

Multiverse adds the model and effort arguments to that command. A Claude
seat without `command` makes every chain that names it fail, and `check`
reports it. Keep `~/.claude/CLAUDE.md` empty: when `CLAUDE_CONFIG_DIR` is
set, Claude Code loads `CLAUDE.md` from both the config directory and
`~/.claude`, so text in the default directory leaks into every seat.

### Codex seats

```toml
[accounts.chatgpt-personal]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/home/you/.codex-personal"
pool = "chatgpt:personal"
short = "gp"
```

`CODEX_HOME` is where the Codex CLI keeps its `auth.json`, its
`config.toml`, and its session logs. Sign in once with
`CODEX_HOME=/home/you/.codex-personal codex login`. Codex seats need no
`command`, because Herdr starts them directly. Just before each launch,
Multiverse adds the project's directory to that seat's
`$CODEX_HOME/config.toml` as trusted, so Codex does not stop to ask.

### Grok seats

```toml
[accounts.grok]
kind = "grok"
config_dir_var = "GROK_HOME"
config_dir = "/home/you/.grok"
pool = "grok"
```

`GROK_HOME` is where the Grok CLI keeps its login and session logs. If you
leave it unset, Herdr uses whatever its server process had, so always set
it. Grok seats need no `command`. Grok has no real usage counter, so
`multiverse accounts usage` estimates usage from the session logs in
`<config_dir>/sessions/`.

### Local seats

A local model (served by ollama or LM Studio) has no agent loop of its own.
It runs behind the Codex CLI's open-source mode, so it is a `codex` seat
with its own `CODEX_HOME`:

```toml
[accounts.local]                 # keep this exact name
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "/home/you/.codex-local"      # never the cloud seat's directory
pool = "local"                             # vendor "local"
command = "codex --oss --local-provider ollama"   # or lmstudio
model = "<the model id your server serves>"
health_url = "http://127.0.0.1:11434/"     # check skips the seat while this is down
roles_allowed = ["scout", "archivist"]
max_brief_chars = 5000
```

For LM Studio, use `--local-provider lmstudio` and that server's URL. Add
`CODEX_OSS_BASE_URL=<url>` in front of the command if the server is not on
its default port. Keep the table name `local`: Multiverse recognises that
name and sends this seat a shorter brief written for a small model.
`roles_allowed` and `max_brief_chars` stop long or open-ended work from
reaching it. Leave it out of Seer, Watcher and Council chains (see
[Chains](#chains)).

### Verify a seat

```sh
multiverse accounts          # one row per seat: ACCOUNT, POOL, STATE, CONFIG DIR
multiverse check             # placeholders, chains, and what each would pick
```

A healthy seat shows `available`. Other states are explained in the
[Board legend](#seats-card). `multiverse accounts usage` shows the last
usage gauge read for each pool.

### Host readiness

`multiverse check` also reports problems with the host machine. Each note
below is one that `check` links to.

#### gitleaks

`worktrees <path> --merge <task>` scans a lane's commits with gitleaks
before it merges them. With `[secrets_scan] required = true` (the default)
and no gitleaks installed, every merge is refused with exit 3. Install
gitleaks, or set `[secrets_scan] required = false` to merge with only a
warning.

#### Docker build context

A project with a root `Dockerfile` and no `.multiverse` entry in
`.dockerignore` sends every lane's worktree to Docker as build context.
Add `.multiverse` to that project's `.dockerignore`.

#### Idle sleep

When a Mac sleeps, background lanes stop with it. If `check` reports a
non-zero AC-power idle-sleep setting, set AC-power sleep to 0 for an
always-on fleet. "Unknown" means the `pmset` probe failed or timed out.
Run `pmset -g custom` yourself to see the setting. Linux hosts get no such
note.

#### Scratch copies in tmp

`check --verbose` warns when copies of a project in `$TMPDIR` or
`/private/tmp` (left behind by tools) together pass 10 GB. The warning
names the directories and gives a command to remove them. Nothing deletes
them automatically.

#### Routing log size

A project's `.multiverse/routing.log` is append-only. Past 10,000 lines
or 2 MB, `check` mentions its size. Nothing is wrong, and there is no
compaction command. When you want it smaller, archive or truncate it by
hand.

#### Outside a Herdr pane

`dispatch` opens a pane by splitting the pane it runs in, so it fails
outside Herdr. `multiverse start` works from any terminal. Run dispatches
from a project's own pane, where `HERDR_PANE_ID` is set.

<!-- source: crates/mv-config/src/load.rs (default_config_path, config_path,
config_state_dir), scripts/plugin-action.sh, config.example.toml
([accounts.*]), crates/mv-cli/src/commands/accounts.rs (account_state),
crates/mv-cli/src/commands/check.rs (hub_project_note),
crates/mv-cli/src/commands/doctor.rs (resolve_config_path),
crates/mv-cli/src/commands/onboard.rs, docs/install.md -->

## Roles

Each role is a process boundary: a write scope, a typical job, and the
role it escalates to. The vendor and model come from your config. The full
contract for each role is in [`docs/roles.md`](roles.md).

| Role | Config key | Does | Writes | Dispatched |
|---|---|---|---|---|
| Nexus | `nexus` | Coordinates one project: plans, routes, reconciles, verifies | Briefs, routing log; never the worktree | By `multiverse start`, on the interactive seat; never `--role` |
| Scout | `scout` | Fast read-only reconnaissance; returns a map, not file contents | Nothing (result/status only) | `dispatch --role scout`, before a change is planned |
| Builder | `builder` | Carries out a change that has already been decided, in its own worktree | Exactly the files the brief names | `dispatch --role builder`, with a declared write scope |
| Archivist | `archivist` | Looks things up outside the repo: docs, APIs, release notes | Nothing | `dispatch --role archivist` |
| Critic | `critic` | Adversarial review on a vendor other than the author's | Nothing | `dispatch --role critic [--reviewing <task>]`, before a merge |
| Seer | `seer` | Architecture, risk, hard debugging | Nothing | `dispatch --role seer`; waits for capacity rather than settling for a weaker seat |
| Council | `council` | Several vendors weigh in on a high-stakes decision; the disagreement is the output | Nothing | `multiverse council --yes` only |
| Artisan | `artisan` | UI/UX design and the edits that ship it | The visual files the brief names (nothing on a review card) | `dispatch --role artisan` |
| Watcher | `watcher` | Reads images, PDFs and screenshots and reports them as text | Nothing | `dispatch --role watcher`; multimodal seats only |

Roles marked "Nothing" write only their result and status files, which
live under `.multiverse/`. History written under the old names
(`orchestrator`, `explorer`, `fixer`, `librarian`, `reviewer`, `oracle`,
`designer`, `observer`) still reads correctly. Config files must use the
new keys; `multiverse config migrate` rewrites old ones.

<!-- source: docs/roles.md, crates/mv-config/src/roles.rs (Role::ALL),
config.example.toml (Roles section) -->

## Chains

A chain is an ordered list of account names for one role in one project:

```toml
[projects.webapp.chains]
builder = ["claude-personal", "chatgpt-personal"]
critic  = ["chatgpt-personal", "grok"]
scout   = ["claude-personal", "grok", "local"]
```

The entries mean:

1. **First entry**: the seat this role runs on whenever that seat can take
   the work.
2. **Second entry**: the fallback, used when the first is unavailable.
   That covers `available = false`, a future `unavailable_until`, a
   FENCED breaker, DRAINING below the quota floor, AT CAP, a local seat
   whose `health_url` is down, or a brief the seat may not take
   (`roles_allowed`, `max_brief_chars`). The card and the routing log say
   which seat was used and why.
3. **Third entry**: the last resort, usually the cheapest seat or the
   local floor.

How far down the list dispatch will go is set by the role's
`on_exhausted` policy. Skips caused by vendor exclusion, a missing model,
or brief length never count against it.

| `on_exhausted` | Default for | Behaviour |
|---|---|---|
| `degrade` | Scout, Archivist, Watcher | Try every entry in turn |
| `degrade_once` | Builder, Artisan | Allow one step down past an unavailable or full seat, then stop |
| `queue` | Seer, Critic | Wait when a seat is full or back soon, rather than settle for a weaker one |
| `warn` | Nexus | Never swap seats; warn and stop |
| `shrink` | Council | Seat fewer councillors rather than substitute |

A role with no chain does not fall back to a default: dispatch asks.

### A recommended default set

For a fleet with one Claude, one ChatGPT/Codex, one Grok and one local
seat, this is the set `multiverse onboard` offers and writes:

```toml
[projects.webapp.chains]
nexus     = ["claude-personal"]                      # the interactive seat
builder   = ["claude-personal", "chatgpt-personal"]
seer      = ["chatgpt-personal", "claude-personal"]
critic    = ["chatgpt-personal", "grok"]
artisan   = ["grok", "chatgpt-personal"]
scout     = ["grok", "claude-personal", "local"]
archivist = ["grok", "chatgpt-personal", "local"]
watcher   = ["claude-personal", "chatgpt-personal"]  # multimodal seats only
council   = ["claude-personal", "chatgpt-personal", "grok"]
```

Why each one looks like this:

- **Critic leads on a vendor other than Builder's.** A model reviewing
  its own vendor's code shares that code's blind spots and tends to
  approve them. `multiverse add` puts Critic's first entry on a different
  vendor from Builder's for this reason. Nothing in this set keeps Seer
  off Critic's vendor: both lead on `chatgpt-personal`, so a Seer
  checking a Critic's finding runs on the same vendor unless you reorder
  one of the two chains.
- **Artisan does not run on the house vendor** (the vendor that leads
  Builder). Design work gains from a different style, and it keeps your
  main seat free.
- **Read-only roles run on the cheap seat first.** Scout and Archivist
  compress and fetch; a cheaper seat does that well and saves your
  capable seats for Builder, Seer and Critic. The local floor goes last,
  and only in these two chains.
- **No local seat for Seer, Watcher or Council.** Seer takes the hardest
  questions and waits rather than settle for a weaker answer. Watcher
  needs a model that can see images. Council seats every entry at once,
  one per vendor, so a local seat in its list would always sit on the
  council.
- **Single-seat Claude fleets:** if your only Claude login is the
  interactive seat, worker chains cannot name it unless you set
  `lend_interactive_to_workers = true` at the top of the config. With
  that set, Builder and Nexus share one budget.

`multiverse add <path>` and `multiverse new <name>` write a starting chain
table for a new project along these lines. They follow the same vendor
preferences and always leave out the interactive seat. They also apply
one rule this set does not: when Seer would lead on Critic's vendor,
they move Seer's first seat from another vendor to the front of its
chain and say so in a `# seer:` comment.

### Editing chains

- **Fleet-wide:** edit `[projects.<name>.chains]` in `config.toml`.
- **Per project:** a project's own `.multiverse/roster.toml` may override
  chains, and nothing else:

  ```toml
  # <project>/.multiverse/roster.toml
  [chains]
  critic = ["grok", "chatgpt-personal"]
  ```

  An override replaces that role's chain entirely; roles the file does
  not mention keep the `config.toml` chain. Unknown tables are refused.
  `multiverse check` marks an overridden chain, and its `chains:` block
  shows which seat each role would pick right now.

<!-- source: crates/mv-cli/src/commands/onboard.rs (RECOMMENDED, CHAIN_RULES,
recommended_chains), crates/mv-cli/src/commands/add.rs (lead_vendor_family,
no_local, chain_lines, lead_differs_from), crates/mv-engine/src/selector.rs
(should_stop),
crates/mv-cli/src/commands/check.rs (default_on_exhausted),
config.example.toml (Projects and account chains, roster overrides) -->

## Model choice

A role's models are set per kind of seat, in `[roles.<key>.models]`. The
key is the account's `kind` (`claude`, `codex`, `grok`). The value is the
exact model id that vendor's CLI accepts:

```toml
[roles.builder]
write_scope = "declared"
effort = "high"                       # low / medium / high / max

[roles.builder.models]
claude = "<capable-tier model id>"
codex  = "<capable-tier model id>"
```

That table also decides which seats a role can use: a role can run on an
account only if `[roles.<key>.models]` has an entry for that account's
kind. A chain that names a seat with no matching entry is a config error.
Do not make up a model id. Leave the placeholder until you have the exact
string from the vendor's CLI. `check` lists every placeholder still
unfilled.

Recommended tiers (vendor-neutral, because model names change):

| Role | Tier | Why |
|---|---|---|
| Builder, Seer, Critic | Each vendor's **capable** tier | Mistakes here cost the most: a wrong change, a wrong diagnosis, or a missed review finding |
| Artisan, Council, Nexus | **Capable** tier | Design judgement, high-stakes decisions, and the coordinator every other lane depends on |
| Scout, Archivist, Watcher | The **fast/cheaper** tier | Compressing, fetching and describing work fine on a cheaper model; Watcher's must still accept images |
| Scout, Archivist on the local seat | A **local** model, with `max_brief_chars` and `roles_allowed` set on the account | Free and never rate-limited, but small; the brief cap keeps long work away from it |

Two other settings change cost without changing the model:

- `roles.<key>.effort` sets the reasoning effort. Scout suits `low`.
  Critic's effort comes from `[review_policy] review_effort` instead.
- `accounts.<name>.model` overrides the role's model for that one seat.
  The local seat needs this, because it cannot run the cloud model named
  for its kind.

<!-- source: config.example.toml (Roles section, [accounts.local]),
crates/mv-config/src/load.rs (Account.model, role models) -->

## Day-one cheat sheet

| Command | What it shows or does | Run it when |
|---|---|---|
| `multiverse start [<project>...] [--all]` | Opens the home workspace and each project's Nexus pane | After configuring, and after a reboot |
| `multiverse board [--all]` | BLOCKED / IN PROGRESS / BACKLOG cards plus the seat, mail and Nexus status cards | Any time you want to see the state of the fleet |
| `multiverse workers` | Every live worker pane: project, task, age, role, seat, STATUS, outcome; plus `workers N/CAP`. Records finished-but-unrecorded outcomes before printing; `--read-only` (or `MULTIVERSE_READ_ONLY=1`) skips that write | A card looks stuck, or the fleet looks full |
| `multiverse tasks <project> --reconcile` | Records outcomes that finished after their dispatch gave up | A card says `unrecorded`, or `workers --read-only` still shows `done-unrecorded` |
| `multiverse backlog list` | Open and asked cards for the project | Deciding what to do next |
| `multiverse backlog add "<title>" --why "<evidence>"` | Files a durable card | You notice something that should not be lost |
| `multiverse activity inbox` | Unread messages between projects | The MAIL card shows unread messages |
| `multiverse check [--verbose]` | Read-only readiness report; exit 0 ready, 3 needs config | After any config edit, and when something won't dispatch |
| `multiverse down` / `multiverse up` | `down` halts dispatch, asks each Nexus to wrap up, then closes its workspace; `up` reopens them and clears the halt | Before leaving the fleet idle, and when coming back |
| `multiverse stop [<project>...] --force` | Closes project sessions even when an agent is not idle or done | A plain `stop` refused because an agent was still busy |
| `multiverse worktrees <path> --prune [--dry-run]` | Reclaims, archives or removes finished lanes and branches | `check --verbose` reports prunable lanes |

`multiverse onboard` prints a shorter version of this list when it
finishes; [`docs/cheat-sheet.md`](cheat-sheet.md) keeps the two in step.

### Reading check output

A few `check` lines are easy to misread:

| Words | Meaning |
|---|---|
| `short=(none — derived slug)` | The project has no `short = "..."`, so agent names use a shortened form of its name. Set `short` to choose the letters |
| `slots: N open, floor F, ceiling C` | Live agents in that project, its configured minimum, and its ceiling (`max_slots`, or the fleet `max_concurrent`) |
| `advisory scans skipped` / `worktree pile check skipped` | A plain `check` skips the slower scans (lane pile, scratch size, stale seed, review policy). Run `check --verbose` for them |
| ``scratch size is unknown (`du` timed out)`` | The verbose scratch scan could not finish in time. This is not the 10 GB warning ([Scratch copies in tmp](#scratch-copies-in-tmp)); look at the directory by hand if you care |
| `unfilled placeholders` | `<<PLACEHOLDER: ...>>` values still in the config. Dry-runs work; real dispatch is blocked until they are set |

### Stale Nexus seed

A project's Nexus reads its instructions from `.multiverse/nexus.md` when
it starts. When the dispatch table or role prompt changes after that,
`check --verbose` reports the seed as stale. Run `multiverse stop
<project>` and then `multiverse start <project>` to regenerate it. `start`
alone does not, because it skips a workspace that is already open. A seed
`check` cannot read is reported as a warning naming the file.

### Prunable lanes

Finished lanes keep their worktree and branch until you reclaim them.
Nothing removes them automatically. `multiverse worktrees <path> --prune
--dry-run` lists what would be reclaimed, archived or removed; run it
again without `--dry-run` to do it.

<!-- source: crates/mv-cli/src/args.rs (start, board, workers, tasks,
backlog, activity, check, down, up, stop, worktrees),
crates/mv-cli/src/commands/check.rs (stale_seed_note, prunable_count) -->

## Board legend

### Columns

| Column | What lands there |
|---|---|
| **BLOCKED** | Something needs you: a task whose status file says `blocked`, an agent stopped at a prompt, a lane whose pane is gone, a `done` lane with uncommitted changes, a status file Multiverse cannot read, or a backlog question nobody has answered |
| **IN PROGRESS** | Dispatched tasks that are still running, waiting, stalled, or finished but not yet recorded |
| **BACKLOG** | Open backlog cards and answered questions, by priority. `OVERDUE` means the card's due date has passed |

Tasks recorded `done` or `failed` leave the board.

### Task cards

A task card's status line reads `<seat> <elapsed>`, then any of these:

| Word on the card | Meaning | Fix |
|---|---|---|
| `unrecorded, needs --reconcile` | The agent wrote its status file, but no result has been recorded yet | `multiverse tasks <project> --reconcile` |
| `unrecorded, but its pane is still working` | A status exists, but the agent is still working on it (a fix round) | Wait for it to finish |
| `pane gone, status <s>` | The pane closed after writing status `<s>`; sits in BLOCKED | `multiverse tasks <project> --reconcile` |
| `done but lane has uncommitted changes(...)` | The agent said done but left its worktree dirty | Commit in the lane, then `multiverse tasks <project> --reconcile`; to record a different outcome instead, see `multiverse tasks --help` for `--supersede` |
| `awaiting, no live agent` | No status file and no pane | `multiverse tasks <project> --close-stranded`, or let `sweep` record it `failed` after the lease (60 minutes by default) |
| `stale: pane gone <age>` | Same as above, for longer than 30 minutes | `multiverse tasks <project> --close-stranded` |
| `stalled, agent is live but not working` | The pane exists but Herdr says it is idle | `multiverse workers` to see the pane, then prompt the agent |
| `possibly frozen: agent reports idle ...` | Idle with no status file for more than 30 minutes (often after host sleep) | `herdr agent prompt <agent>` or `multiverse dispatch --into <task>` |
| `waiting-on-shell: a background shell is still running` | A Claude lane looks idle but its own background job is still running | Nothing; leave it alone |
| `blocked on prompt: <rule>` | The agent is stopped at a menu or a trust dialog | Answer it in that pane |
| `status file holds an unrecognized value` | The status file contains something other than done/blocked/failed | `multiverse tasks <project> --close-stranded` |
| `limited ×N` | The seat hit its rate limit N times during this task | Nothing, unless N keeps growing; then check `multiverse accounts` |
| `nudged: <task>` | A reminder to finish has already been sent once | Wait; it is not sent again |
| `acknowledged` | Someone already looked at this card | Nothing |

### Unrecorded and stranded lanes

A task is **recorded** when a result line for it is written to the
project's routing log. An agent finishes by writing its status file. If
the dispatch that started it had already stopped waiting, nothing records
that status, and the task stays **unrecorded**. `multiverse workers`
records every finished one before it prints (unless `--read-only`);
`multiverse tasks <project> --reconcile` does the same for one project
without listing workers, and `multiverse sweep` for every project. A task with neither a status file nor a
live pane is **awaiting**. `sweep` records it `failed` once its lease
(`awaiting_lease_minutes`, default 60) runs out, and `multiverse tasks
<project> --close-stranded` decides it straight away.

### workers STATUS and outcome

`multiverse workers` prints one row per live worker pane:

| Column value | Meaning | Fix |
|---|---|---|
| STATUS `working` | Herdr reports the agent busy | Nothing |
| STATUS `idle` / `done` | Ready for input; the same state in Herdr. `done` does not mean the session ended | Look at the outcome column |
| STATUS `stalled` | Idle with no status file | Prompt the agent, or `dispatch --into <task>` |
| STATUS `waiting-on-shell` | A Claude pane is waiting on its own background shell | Nothing; do not reclaim it |
| STATUS `blocked` | Waiting at a prompt | Answer it in the pane |
| outcome `running` | No status file yet | Nothing |
| outcome `done-unrecorded` | The status file is written, but no result is recorded yet; the pane still uses a slot. A plain `workers` records it first, so you see this mostly with `--read-only` | `multiverse tasks <project> --reconcile`, or `workers` without `--read-only` |
| outcome `done` / `failed` / `blocked` | Recorded; the pane no longer uses a slot | Close the pane, or `worktrees <path> --prune` |

The `workers N/CAP` line counts every unrecorded lane against
`max_concurrent`. If it reads full while nothing is working, reconcile.
Its `projects:` line marks the built-in hub `(home hub)` (see
[The built-in multiverse hub](#the-built-in-multiverse-hub)).

### Scope line and ACCOUNTS row

| Words | Meaning |
|---|---|
| `project: NAME only (one-project view; --all for the fleet)` | This board shows one project. Pass `--all`, or name several projects, for the fleet |
| `projects: a, b, c` | The fleet view of those projects |
| `fleet N/CAP` | Live workers against `max_concurrent` |
| `name · model` | A seat card's title: the account and the model it is showing |
| `✓ AVAILABLE` | The seat can take work. The other states are in the [Seats card](#seats-card) table below |
| `window: n/a (no usage gauge)` | A Claude seat with no usage reading recorded yet. It is not a billing warning; a subscription seat has no window to show until something records one |
| `window: n/a (not reported)` | A non-Claude seat with no usage reading |
| `tok 24h n/a` | No 24-hour token count yet |

### Seats card

| Line | Meaning | Fix |
|---|---|---|
| `N working` | Agents Herdr reports busy | Nothing |
| `N held / C cap` | Live agents using a seat, against the fleet cap (`max_concurrent`) | At the cap: `multiverse workers`, then reconcile or prune |
| `AT CAP` | The seat's open-agent limit is reached | Wait, or raise it with `multiverse accounts limit` |
| `FENCED` | The pool's breaker tripped (rate limit or failures) | It closes by itself, or `multiverse accounts reset <pool>` |
| `DRAINING` | The usage gauge is at or below `dispatch_min_quota_percent` | Wait for the window to reset; dispatch uses the next seat in the chain |
| `HALF-OPEN` | One trial task is testing a fenced pool | Wait for the trial to finish |
| `unrecognized: ...` | Live agents whose names match no configured seat | `multiverse workers`; close anything you did not start |

### Nexus last pass and mail

The NEXUS LAST PASS card's header line reads
`mail/reconcile/backlog (? never, 0s just now) · ⚠ overdue · warn=overdue`.
Each project row then shows three ages: the last inbox read, the last
reconcile pass, and the last backlog pass. `?` means that pass has never
happened, so a Nexus you have just started shows `⚠ overdue` until it
runs its first passes.

| Word | Meaning | Fix |
|---|---|---|
| `✓` | The project's Nexus read its mail and ran reconcile and backlog passes in the last 30 minutes | Nothing |
| `⚠ overdue` (`warn` on a narrow board) | One of those passes is more than 30 minutes old, or has never happened | Prompt that project's Nexus, or run `multiverse sweep` |
| `✗ parked` | The project's Nexus pane is blocked at a prompt | Answer it in that pane |
| `offline (no live orchestrator)` | The project has no live Nexus pane, so nobody is reading its mail | `multiverse start <project>` |
| `never opened` | The inbox has never been read | `multiverse activity inbox` from that project |
| `OWED REPLIES (N)` | The card listing unanswered `activity --ask` messages | One reply per line below |
| `<project> owes <sender> a reply to #N` | An `--ask` message is waiting for a reply (marked ⚠ after 24 hours) | `multiverse activity reply <project>#N "..."`, or `activity close` |

### Plain terminals

The board and `workers` draw Unicode boxes and status marks. With
`TERM=dumb`, a non-empty `NO_COLOR`, or `board --no-color`, they draw
one-cell ASCII instead: `+` `-` `|` for boxes and `*` `x` `!` for the
status marks, so `✓ AVAILABLE` reads `* AVAILABLE`. `board --color never`
only drops the colours and keeps the Unicode.

### Questions for you

When a Nexus or worker needs a decision from you, it files a backlog
question with `multiverse backlog ask`. The card sits in **BLOCKED** with
`asked` on its bottom border until you answer. A Nexus may also title
such a card `FOR <your name>: ...`; it is the same kind of card. Read it with `multiverse backlog
show <id>` and answer with `multiverse backlog answer <id> --note "..."`.
An answered card moves to BACKLOG and shows `answered` instead.

<!-- source: crates/mv-board/src/columns.rs, crates/mv-board/src/card.rs
(split_dispatched, task_card, backlog_card),
crates/mv-board/src/status_cards.rs, crates/mv-board/src/layout.rs
(STALE_AFTER_MINUTES), crates/mv-cli/src/commands/board_data.rs
(resolve_dispatched state_note, mail_entries, cycling_and_parked,
scope_header, account_snapshot), crates/mv-board/src/accounts.rs,
crates/mv-board/src/layout.rs (to_ascii_plain),
crates/mv-cli/src/commands/workers.rs (build_report, render_text,
auto_reconcile_unrecorded) -->

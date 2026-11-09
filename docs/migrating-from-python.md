# Migrating from the Python Multiverse

This page is for an operator who already runs the Python `multiverse`
reference implementation and wants to move a fleet onto this Rust
rewrite. It assumes you have at least one project already registered in
a working Python `config.toml`, with task history under that project's
`.multiverse/` directory.

Read [Status](../README.md#status) first if you have not already —
it lists exactly which commands are real today and which still carry
scope cuts short of full Python parity. `multiverse start` opens a live
Nexus pane, and `multiverse council --yes` convenes a real roster; the
scope cuts that matter most for a migration are called out again in
Step 7 below. Everything this page walks through — halting, installing,
importing config/state, migrating config, clearing the halt, starting
each project, and reading existing history — is real and
conformance-tested.

`scripts/rehearsal/cutover.sh` rehearses the post-install command
sequence against disposable copies. The steps below are that sequence,
plus the plugin install that the rehearsal takes as already done, and a
rollback to Python.

`scripts/cutover-rehearsal.sh [config-dir]` is a narrower, read-only
companion: it builds the release binaries itself, copies your real
`config.toml` and every configured project's `.multiverse` (minus
worktree checkout contents) into a disposable sandbox under
`target/cutover/` — real files are only ever read, never written — then
migrates the copy's role keys and replays the read-only half of the
cutover (`config migrate --dry-run`/`--backup`, `check`, `accounts`,
`tasks`, `board --all`, `health`, `backlog list`, `workers`, `sweep
--dry-run`, `worktrees`) against it through a fake Herdr. It never runs
`start`, `dispatch`, or `halt`. Run it against your actual config before
Step 4's real `config migrate --backup` to see the real migration diff
and read-only command set exercised without touching anything live.

Step 1 is run with whichever `multiverse` is installed today. Python and
Rust share the halt marker only when they share a state directory — true
for a standalone install, not for a Herdr-managed one; see Step 1 and
Step 3 below. From Step 3 on, `multiverse` is this rewrite's CLI — the
plugin checkout's `bin/multiverse` after a Herdr install, or
`target/release/multiverse` from a source build. `import-python` (Step 3)
does have a plugin action; `config migrate`, `halt`, and `sweep` do not —
run those three through the CLI.

## At a glance

- **Nothing about your project history changes.** Task briefs, results,
  status files, the routing log, worktrees, and backlog under each
  project's `.multiverse/` directory are read as-is; this rewrite does
  not rewrite them.
- **Role names on the write side change.** Old role names
  (`orchestrator`, `explorer`, `fixer`, `librarian`, `reviewer`,
  `oracle`, `designer`, `observer`) become `Nexus`, `Scout`, `Builder`,
  `Archivist`, `Critic`, `Seer`, `Artisan`, `Watcher`. `Council` was not
  renamed. See [Legacy on-disk names](roles.md#legacy-on-disk-names) for
  the full mapping.
- **`config.toml` needs a one-shot migration** to rewrite those same
  retired role keys (`[roles.fixer]`, a chain's `fixer = [...]`, an
  account's `roles_allowed` entries) to their current spelling.
  `multiverse config migrate` does this for you, in place, keeping your
  comments, with a backup. It also rewrites every configured project's
  `.multiverse/roster.toml` that still has retired keys.
- **History readers accept both spellings; new input does not.** A task
  directory, routing-log line, or brief written under an old role name
  still reads correctly after migration — see the `history-*`
  conformance scenarios under `conformance/scenarios/` for the cases
  this is tested against. Writing a *new* `[roles.fixer]` table (or
  passing `--role fixer`) after migration is refused.

## Command sequence

Once Step 2 has installed this rewrite's CLI, the cutover itself is:

```sh
multiverse config migrate --dry-run
multiverse config migrate --backup
multiverse halt off
multiverse start <project>        # once per project named in the config
multiverse sweep --tick
multiverse board
```

`multiverse start --all` is the one-shot equivalent of the per-project
`start` loop. Pass `--config` explicitly on both `migrate` invocations
whenever you installed through Herdr (see Step 4) — the commands above
are the standalone-install shape, where the default path is correct. A
Herdr-managed install also runs `multiverse import-python` once, before
the first `config migrate` above — see Step 3.

## Step 1 — Halt the fleet

Before touching configuration or installing anything, stop new work
from starting against the project(s) you are migrating:

```sh
multiverse halt on --reason "migrating to the Rust rewrite"
multiverse halt status
```

The halt marker is a file named `halt` in the fleet's runtime state
directory — by default the same directory as `config.toml`
(`$HOME/.config/multiverse/halt`). A Herdr-managed install uses
`HERDR_PLUGIN_STATE_DIR`; a standalone override uses
`MULTIVERSE_STATE_DIR` (`crates/mv-cli/src/commands/halt.rs`,
`crates/mv-state/src/halt.rs`). The marker is fleet-wide, not
per-project. `multiverse sweep --tick` honors it: the maintenance pass
is skipped while the fleet is halted. Clear it in Step 6, after the
config has been migrated, so start / sweep / board run against a live
fleet the same way the cutover rehearsal does.

This step does not depend on which binary (Python or Rust) you run it
with — run it with whichever one you have installed right now. Python
and Rust read and write the *same* halt marker file only when they share
a state directory, which is true for a standalone install
(`MULTIVERSE_STATE_DIR` pointing at the same `$HOME/.config/multiverse`
Python already uses) but **not** for a Herdr-managed install:
`$HERDR_PLUGIN_STATE_DIR` is a directory Herdr created for this plugin,
and Python never wrote to it. Step 3 below (`multiverse import-python`)
copies this halt marker — along with the rest of your fleet's state —
into that directory before you touch `config.toml`.

## Step 2 — Install the Rust plugin

Follow [`docs/install.md`](install.md) for the full install path,
platform support, and troubleshooting. In outline, install the plugin
through Herdr — replace `vX.Y.Z` with a published tag:

```sh
herdr plugin install BamBam4275/herdr-multiverse --ref vX.Y.Z --yes
```

or link a local checkout for development:

```sh
herdr plugin link /absolute/path/to/herdr-multiverse
```

Installing or linking does not touch your existing `config.toml` or any
project's `.multiverse/` history (including a project's own
`roster.toml` chain override, if it has one) — those live under
Herdr-provided directories (`HERDR_PLUGIN_CONFIG_DIR`,
`HERDR_PLUGIN_STATE_DIR`) and each project's own repository, never
inside the managed plugin checkout.

The plugin id stays `multiverse`. After install, invoke the rewrite's
`bin/multiverse` for the remaining steps. Plugin actions exist for
`doctor`, `check`, `start --all`, `board`, and `import-python`; they do
not exist for `config migrate`, `halt`, or `sweep`.

## Step 3 — Import your Python config and state (Herdr-managed installs only)

A standalone install already shares a state directory with Python
(`$HOME/.config/multiverse`, or your own `MULTIVERSE_STATE_DIR`
override) — Step 1's halt marker, and every other state file, are
already in the one place both binaries read. **Skip this step there.**

A Herdr-managed install is the case this step exists for: `config.toml`
and every state file Rust reads — `halt`, `directives/`,
`breaker-state.json`, `gauge-state.json`, `seat-state.json`,
`notify-state.json` — are sitting in your Python install's
`$HOME/.config/multiverse`, not in `$HERDR_PLUGIN_CONFIG_DIR`/
`$HERDR_PLUGIN_STATE_DIR`, the directories Herdr created for this
plugin and the only ones any managed action or bare `bin/multiverse`
invocation reads. Nothing copies that split for you automatically.
`multiverse import-python` does, once, before you touch `config.toml`:

```sh
herdr plugin action invoke multiverse-import-python --plugin multiverse
```

or, equivalently, from the CLI directly — the plugin action only wires
the same `HERDR_PLUGIN_CONFIG_DIR`/`HERDR_PLUGIN_STATE_DIR` environment
Herdr already manages for every other managed action:

```sh
multiverse import-python --from "$HOME/.config/multiverse" --dry-run
multiverse import-python --from "$HOME/.config/multiverse"
```

`--dry-run` lists every file it would copy or skip, without writing
anything. The real run prints one line per file as it copies it, then
the resolved config and state directory paths, then a reminder to run
`config migrate` next (Step 4). It only ever copies — it never moves or
deletes anything at `--from` — so a failed or partial cutover never
costs you the original Python install. It refuses to overwrite a file
already at the destination unless you pass `--force`, and refuses the
whole run (writing nothing) rather than leaving a partial import behind
when it finds one; re-running it after a conflict or failure is always
safe.

`config.toml` and `ntfy-topic` go to the config directory; `halt`,
`directives/*.md`, `breaker-state.json`, `gauge-state.json`,
`seat-state.json`, and `notify-state.json` go to the state directory —
every file an existing `mv-state`/`mv-engine` module actually reads
today (`crates/mv-cli/src/commands/notify.rs:130-137` for why
`ntfy-topic` goes with `config.toml` rather than the state files: Rust
reads it next to the config file). It never prints `ntfy-topic`'s
contents — it is a secret, same as any other credential.

Left behind on purpose, since nothing in this tree reads them yet:
`usage-cache.json`, `private-names.txt`, `shared/`, and every `*.lock`
sibling (a lock is advisory and re-created on demand — the writing
process's `flock` releases when it exits, so a copied-over lock file
would carry no stale lock state, only a stale, meaningless file). They
stay in your Python install's directory; nothing deletes them.

## Step 4 — Migrate `config.toml`

`config migrate` has no plugin action (Step 2), so running it bare picks
the *standalone* default, `$HOME/.config/multiverse/config.toml` or
`$MULTIVERSE_CONFIG` if you have that set — never
`$HERDR_PLUGIN_CONFIG_DIR/config.toml`, the file every managed action and
`bin/multiverse <action>` actually reads. On a Herdr-managed install
those are almost always two different files: running `migrate` without
`--config` edits the wrong one (or nothing, if the standalone default
does not exist) and leaves the live plugin config un-migrated, still
refusing every managed action with the retired-key error below.

If you installed through Herdr, pass `--config` explicitly on every
`migrate` invocation in this step, pointing at the directory the plugin
wrapper uses:

```sh
multiverse config migrate --config "$(herdr plugin config-dir multiverse)/config.toml" --dry-run
multiverse config migrate --config "$(herdr plugin config-dir multiverse)/config.toml" --backup
```

Only a standalone install — no Herdr plugin involved, `bin/multiverse`
invoked directly with your own `MULTIVERSE_CONFIG`/`MULTIVERSE_STATE_DIR`
— can safely omit `--config` and rely on the default path shown in the
Command sequence above.

Any Rust command that loads a config still using an old key
(`[roles.fixer]`, a `fixer = [...]` chain entry, or a retired name
inside `roles_allowed`) refuses to load it at all, naming the exact key
and pointing you at `multiverse config migrate`
(`crates/mv-config/src/load.rs`). That refusal is expected on an
unmigrated file.

First preview the migration — this only reads the file and prints a
unified-style diff; it writes nothing:

```sh
multiverse config migrate --dry-run
```

Review the diff: every retired role key becomes its current name, in
place — a renamed key keeps its original position in the table, so the
diff is a plain rename with no reordering or line shift elsewhere in the
file. Nothing else in the file should change; comments are preserved.
If a configured project's `.multiverse/roster.toml` still has retired
keys, the same dry-run prints a diff for that file too.

When the diff looks right, run it for real. `--backup` is required to
actually write — there is no way to migrate in place without a safety
copy:

```sh
multiverse config migrate --backup
```

This writes a timestamped `config.toml.bak-<UTC stamp>` copy of the
original next to the file (for example `config.toml.bak-20261103T073008Z`),
then replaces `config.toml` atomically. Each project roster it rewrites
gets the same treatment (`roster.toml.bak-<UTC stamp>` next to that
roster). The command prints which keys it renamed and where each backup
landed. Running it again against an already-migrated file is a safe
no-op: it reports `already on current role keys` and writes neither a
new backup nor a new file.

The bare `--dry-run`/`--backup` commands above are the standalone-default
shape; a Herdr-managed install repeats them with the same `--config
"$(herdr plugin config-dir multiverse)/config.toml"` shown earlier, and
any other non-default location uses its own `--config /path/to/config.toml`.

Confirm the migrated file still loads cleanly. `check --read-only
--no-reconcile` is the load check the cutover rehearsal runs; `doctor`
is the broader prerequisite checklist. `doctor` and `check` do have
plugin actions (Step 2), so on a Herdr-managed install prefer running
them through Herdr, which sets `MULTIVERSE_CONFIG` for you; invoked
bare like below, they need the same `--config` as this step's `migrate`
calls:

```sh
multiverse doctor
multiverse check --read-only --no-reconcile
```

`scripts/rehearsal/verify_config_migration.sh` runs dry-run, real
migrate, idempotency, a comment-count check, `doctor`, and `check`
against a disposable copy of a config file, if you want to rehearse
this step before touching your real one.

## Step 5 — What the role rename means in practice

| History / on-disk name | Current name |
|---|---|
| `orchestrator` | Nexus |
| `explorer` | Scout |
| `fixer` | Builder |
| `librarian` | Archivist |
| `reviewer` | Critic |
| `oracle` | Seer |
| `designer` | Artisan |
| `observer` | Watcher |
| `council` | Council |

Council was not renamed. Nothing on disk under a project's
`.multiverse/` directory is rewritten by this migration — only
`config.toml` (and a project's `roster.toml`, when it still had retired
keys) changes. After migration:

- **New writes use only the current names.** `--role fixer` is unknown
  and exits 3, the same as any other unrecognized role; a new
  `[roles.fixer]` table in `config.toml` is also refused.
- **Existing history keeps reading.** A task id such as
  `20250101-fixer-ab12cd`, a `role=fixer` field in `routing.log`, or an
  existing brief under the old spelling still reads through `tasks`,
  `result`, `backlog`, `board`, and `worktrees` — all seven `history-*`
  conformance scenarios pass today, including `board --no-reconcile`
  (`history-board-reads-unrecorded-old-role`).
- Nexus seeds are unaffected by the role rename itself: a project's seed
  file is renamed from the legacy `.multiverse/orchestrator.md` to
  `.multiverse/nexus.md` only the next time its seed is written, not by
  this migration step.

## Step 6 — Clear the halt

The cutover rehearsal clears the halt after migrate and before start.
`start` will still open Nexus panes while the fleet is halted; `sweep
--tick` will not run its maintenance pass until the marker is gone
(`skipped (fleet halted)` per project). Clear it now:

```sh
multiverse halt off
multiverse halt status
```

`halt off` prints `fleet halt cleared.` when it removed the marker, or
`fleet was not halted.` when there was nothing to clear. `halt status`
should then read `fleet is not halted.` The `halt` file (and a leftover
`halt.state` from an earlier Rust build, if one is sitting beside it)
is gone from the state directory.

## Step 7 — Start each project

Name every project the config registers, once each — the loop
`scripts/rehearsal/cutover.sh` runs:

```sh
multiverse projects --names
multiverse start <project>
```

or the one-shot equivalent:

```sh
multiverse start --all
```

`start` validates every requested project against the migrated config
(unknown project names, missing paths, and config-load failures are all
refused before anything else runs) and resolves which project(s) you
mean from `--all`, an explicit name, or your current working directory
(`crates/mv-cli/src/commands/start.rs`).

What to check on each start: the command exits 0, prints a `handoff:`
line for that project, and the summary line is `opened N of M`. A
project whose workspace is already open prints `already open` and
counts toward `M` but not toward `N`. Seats are named
`<project>-nexus`.

`start` opens a live Nexus pane through `mv-engine::fleet_start`.
`--reseed` on a project whose workspace is already open now re-points
that live session — finding the named agent's pane by identity,
reporting a blocked/dialog state rather than typing into it, and
re-seeding a clear pane through
`mv_engine::runner::deliver_prompt_to`'s retry-with-probe landing
confirmation — rather than only rewriting the seed file. The
project-level concurrency lock around the check-then-create race, the
Claude deny-list launch wrapper (`mv-engine::trust::claude_orchestrator_launch`),
and the dashboard tab (`--dashboard`) are all ported now. **What is still
short of full Python parity**: the per-role dispatch-availability table
the seed would otherwise render (it renders empty, described as "No
configured role is currently routable" rather than claiming availability
it hasn't checked); and Python's exact agent-naming scheme (seats are
named `<project>-nexus` instead). See
`crates/mv-cli/src/commands/start.rs`'s own module doc for the complete
list.

A Codex seat's own directory-trust prompt is handled automatically —
`ensure_codex_trusted` writes `trust_level = "trusted"` into that seat's
`CODEX_HOME/config.toml` immediately before launch, so a migrated Codex
account never parks on it (`crates/mv-engine/src/codex_trust.rs`). A
Claude seat's one-time "do you trust the files in this folder?" dialog is
not automated the same way: it is detected and left for a human to
accept once per project, the same as any other blocked/dialog pane
(`crates/mv-engine/src/runner.rs`'s `attention_reason`). For a freshly
migrated project you are starting here for the first time, expect to
accept that dialog once per Claude seat before its pane goes live.

## Step 8 — Confirm the fleet

```sh
multiverse sweep --tick
multiverse board
```

`sweep --tick` should print `fleet is not halted.` and must not print
`skipped (fleet halted)` for a project. `board` prints the fleet board
across every configured project. Both commands exit 0 on a healthy
cutover.

## How each project finds its handoff

A project's Nexus seed (`.multiverse/nexus.md`, or the legacy
`.multiverse/orchestrator.md` until the project's next seed write
migrates it) points at that project's handoff card — the file recording
open work and decisions a new Nexus session should resume from — at
`.multiverse/handoff.md` inside the project's own repository
(`crates/mv-engine/src/nexus_seed.rs`). Nothing about this path changes
with the migration: a project that already has a handoff card keeps
using it; the seed's own digest-based staleness check decides whether a
re-render is needed, never the role rename itself.

## Rolling back to Python

The role-key rewrite is a one-way door for this binary. Restoring the
pre-migration `config.toml` makes the file load in Python and be
refused by Rust (`retired role key`; `doctor` exits 1, `check` exits
3). Project `.multiverse/` history was never rewritten, so it does not
need restoring.

1. Halt the fleet if you already cleared it, so neither binary
   dispatches during the switch:

   ```sh
   multiverse halt on --reason "rolling back to Python"
   ```

2. Stop each project this rewrite already started:

   ```sh
   multiverse stop <project>
   ```

3. Restore the timestamped backup `config migrate --backup` wrote next
   to `config.toml`:

   ```sh
   cp /path/to/config.toml.bak-<UTC-stamp> /path/to/config.toml
   ```

   Restore any `roster.toml.bak-<UTC-stamp>` the same command wrote
   next to a project's `.multiverse/roster.toml`.

4. Point Herdr back at the Python plugin you were running. Disable or
   unlink/uninstall this rewrite as in [`docs/install.md`](install.md)
   (`herdr plugin disable` / `herdr plugin unlink` / `herdr plugin
   uninstall`); relink or reinstall the Python plugin; enable it.
   Config and state survive plugin removal.

5. Confirm with the **Python** CLI (`doctor` / `check`). Do not expect
   the Rust binary to load the restored file.

6. Clear the halt and start each project with the Python CLI:

   ```sh
   multiverse halt off
   multiverse start <project>        # once per project, or start --all
   ```

If you have not yet installed the Rust plugin, restoring the backup
(and keeping the Python binary) is the whole rollback.

## What this guide does not cover

- Converting a Python-only config field that has no Rust equivalent —
  `mv-config::load` implements the schema this tree's
  `config.example.toml` documents; a field the Python reference
  supports but this schema does not is a gap this guide cannot paper
  over. Run `multiverse doctor`/`check` after migrating and read any
  refusal literally.
- The scope cuts short of full Python parity in a live dispatch, a live
  Nexus session, and a Council session — all three are real and
  conformance-tested, but none has full Python parity yet (see
  [Status](../README.md#status)). This guide only gets your
  configuration and project registration onto the new binary; it does
  not claim the new binary matches the Python reference's fleet
  behavior end to end.
- Anything about the Python reference implementation's own internals —
  this page is based on the Rust tree's code and the scripts under
  `scripts/rehearsal/`, not on reading the frozen Python checkout.
- The Herdr plugin install/link lifecycle itself. Follow
  [`docs/install.md`](install.md) for that; `scripts/rehearsal/simulate-plugin-install.sh`
  rehearses the manifest's own scripts against a fake Herdr.

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

`MULTIVERSE_CONFIG` and `MULTIVERSE_STATE_DIR` override these locations for
local development and the conformance test harness when you invoke
`bin/multiverse` directly; they are not meant to be set in a normal
Herdr-managed install.

Each project's own task history — briefs, results, status files, the
routing log, worktrees, backlog, and the Nexus seed
(`.multiverse/orchestrator.md`, legacy filename) — lives under that
project's `.multiverse/` directory, not in the plugin's config or state
directories. That history is read as-is across upgrades; Multiverse does
not rewrite it except through an explicit, versioned migration (see
[Migrations](#migrations) below).

## `config.toml`

The example file, [`config.example.toml`](../config.example.toml), is the
intended fleet-wide schema:

- Top-level knobs such as `max_concurrent`, quota floors, and board
  settings.
- `[accounts.<name>]` — one seat per vendor login (`kind`,
  `config_dir_var`, `config_dir`, `pool`, `available`, optional `short`,
  `interactive`, `command`, `model`). `command` is required for every
  `kind = "claude"` account, not only an `interactive` one — a chain that
  reaches a `claude` seat with no `command` set loads fine but refuses at
  dispatch time ("no command wrapper configured"). Codex and Grok don't
  use it.
- `[roles.<key>]` — write scope, effort, `on_exhausted`, skills, and a
  `[roles.<key>.models]` table keyed by agent kind. The stable keys are
  `scout`, `builder`, `archivist`, `critic`, `seer`, `artisan`, `watcher`,
  plus `nexus` / `council` chains. Seven worker roles dispatch with
  `--role`; Council is its own subcommand; Nexus is seeded by `start`.
- `[projects.<name>]` with `path` and `[projects.<name>.chains]` — ordered
  account lists per role. `path` is required for every project except the
  reserved `multiverse` hub project, whose `path` is optional and defaults
  to `<state dir>/home` when omitted — `multiverse start multiverse` creates
  that directory if it does not exist yet. If no chain is configured for a
  role, loading that dispatch is a configuration error (exit code 3) naming
  the role and project, not a silent default.
- Optional `[review_policy]` and `[activity.pairs]`.

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

## Roster files

Accounts and roles both live in `config.toml` — there is no separate
fleet-wide roster file. The one `roster.toml` the loader reads is
per-project: **`<project>/.multiverse/roster.toml`**, inside the
project's own repository, naming chain overrides only. An override
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
| `MULTIVERSE_CONFIG` | Override the config file path (development/testing) | plugin wrapper; `mv_config::load::resolve_config_path_from_env` |
| `MULTIVERSE_STATE_DIR` | Override the state directory (development/testing) | plugin wrapper |
| `MULTIVERSE_INVOCATION` | The command a pane uses to re-invoke Multiverse; set by the start action | plugin wrapper, Nexus seed renderer |
| `MULTIVERSE_READ_ONLY` | Set on every plugin action except `start` | plugin wrapper |
| `MULTIVERSE_PROMPTS_DIR` | Override embedded `prompts/` | `mv-engine::prompts` |
| `MULTIVERSE_TEMPLATES_DIR` | Override embedded `templates/` | `mv-engine::prompts` |
| `MULTIVERSE_DIRECTIVES` | Override the fleet directives directory | `mv-engine::directives` |
| `MULTIVERSE_TASK_ID` | Full task id a dispatched lane's own session is expected to export; read by `scratch`/`backlog` to find that task's pane/record | skill playbooks (the docker skill body, `mv-engine::skills`); `mv-engine::runner`'s `PreparedLaunch` does not set it in the launched process's environment — `dispatch --yes` and `start` do not export it today, so a lane must `export MULTIVERSE_TASK_ID` itself before relying on it |
| `HERDR_BIN_PATH` | Override the `herdr` binary Multiverse shells out to | `mv-herdr`; tests and conformance against `fake-herdr` |
| `MV_FAKE_HERDR_STATE` | Disposable JSON path for `fake-herdr` mutations | `fake-herdr` |

## Redaction

Config and state files may contain account identifiers or paths that
shouldn't appear verbatim in logs, activity records, or bug reports.
`mv-core::redact` implements that cleaning (PEM blocks, bearer tokens,
AWS-shaped keys, and credential-named assignments) and is called from
`backlog`, `activity`, `result`, `inspect`, and brief rendering. When
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

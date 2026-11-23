# Day-one cheat sheet

Commands a first-run user actually runs, plus the words `board`,
`workers`, and `check` print that are easy to misread. The same command
list is printed at the end of `multiverse onboard`
(`crates/mv-cli/src/commands/onboard.rs`, `CHEAT_SHEET`).

## Commands

| Command | What it shows or does | Run it when |
|---|---|---|
| `multiverse start [<project>...] [--all]` | Opens the home workspace and each project's Nexus pane | After configuring, and after a reboot |
| `multiverse board [--all]` | BLOCKED / IN PROGRESS / BACKLOG cards plus the seat, mail and Nexus status cards | Any time you want to see the state of the fleet |
| `multiverse workers` | Every live worker pane (project, task, age, role, seat, STATUS, outcome) plus occupancy totals. Auto-reconciles finished-but-unrecorded outcomes before printing; `--read-only` skips that write | A card looks stuck, or the fleet looks full |
| `multiverse tasks <project> --reconcile` | Records outcomes that finished after their dispatch gave up | `workers --read-only` still shows `done-unrecorded`, or you want to reconcile without listing workers |
| `multiverse backlog list` | Open and asked cards for the project | Deciding what to do next |
| `multiverse backlog add "<title>" --why "<evidence>"` | Files a durable card | You notice something that should not be lost |
| `multiverse activity inbox` | Unread messages between projects | The MAIL card shows unread messages |
| `multiverse check [--verbose]` | Read-only readiness report; exit 0 ready, 3 needs config | After any config edit, and when something will not dispatch |
| `multiverse down` / `multiverse up` | `down` halts dispatch, asks each Nexus to wrap up, then closes its workspace; `up` reopens them and clears the halt | Before leaving the fleet idle, and when coming back |
| `multiverse stop [<project>...] --force` | Closes project sessions even when an agent is not idle or done | A plain `stop` refused because an agent was still busy |
| `multiverse worktrees <path> --prune [--dry-run]` | Reclaims, archives or removes finished lanes and branches | `check --verbose` reports prunable lanes |

`workers` auto-reconcile lives in `crates/mv-cli/src/commands/workers.rs`
(`auto_reconcile_unrecorded`, skipped when `--read-only` or
`MULTIVERSE_READ_ONLY=1`).

## Where the config lives

There is one fleet config file, `config.toml`. Multiverse uses the first
of these that applies (`crates/mv-cli/src/commands/doctor.rs`,
`resolve_config_path`):

| How you run Multiverse | Config file |
|---|---|
| Through Herdr (plugin actions, Start, dashboards) | `$HERDR_PLUGIN_CONFIG_DIR/config.toml`. Find the directory with `herdr plugin config-dir multiverse` |
| `multiverse --config <path> ...` | `<path>` |
| `MULTIVERSE_CONFIG=<path>` exported | `<path>` |
| None of the above (`default location`) | `~/.config/multiverse/config.toml` |

The default path is the same on Linux and macOS. Multiverse does not read
`$XDG_CONFIG_HOME` or `~/Library/Application Support`. When Multiverse
runs through Herdr, the plugin wrapper sets `MULTIVERSE_CONFIG` from
Herdr's config directory and ignores a standalone
`~/.config/multiverse/config.toml`. If the file is missing there, the
wrapper exits 3 and does nothing else.

`multiverse onboard` prints `Config location (default location): …` when
it is about to write that standalone path. A config written from a plain
terminal is the one the plugin wrapper will not see; copy it to
`$HERDR_PLUGIN_CONFIG_DIR/config.toml` or pass `--config` / export
`MULTIVERSE_CONFIG` if you need the same file in both places.

Fleet runtime state lives in `$HERDR_PLUGIN_STATE_DIR` (or
`MULTIVERSE_STATE_DIR`). Each project's own history lives in that
project's `.multiverse/` directory. `multiverse check` prints the config
and state paths on its first two lines.

## The built-in `multiverse` hub

`config.example.toml` ships `[projects.multiverse]` (`short = "mv"`).
`onboard` keeps it. It is the home workspace: a cross-project chat and
the place `start` with no project argument opens, not a repository you
added. `check` labels it `(built-in home hub: cross-project chat; not a
project you added)` on the `projects:` list
(`crates/mv-cli/src/commands/check.rs`, `hub_project_note`). `workers`
labels it `(home hub)` on the `projects:` totals line
(`crates/mv-cli/src/commands/workers.rs`, `render_text`). Leave `path`
unset so it defaults to `<state dir>/home`; do not point it at a
development checkout of this repository.

A `check --verbose` line `multiverse worktree pile check unavailable`
before the first `start` means that home directory does not exist yet.
`multiverse start` creates it.

## `check` wording

Printed by `crates/mv-cli/src/commands/check.rs` `run`:

| Words | Meaning |
|---|---|
| `short=(none — derived slug)` | The project has no `short = "…"` in config, so agent names use a derived, truncated slug |
| `slots: N open, floor F, ceiling C` | Live dispatched agents in that project (`N`), configured minimum (`F`, usually 0), and `max_slots` or fleet `max_concurrent` (`C`) |
| `advisory scans skipped` / `worktree pile check skipped` | The non-verbose run skipped the slower worktree-pile, scratch-size, stale-seed, and review-policy scans. Re-run `check --verbose` |
| `review policy:` | Effective `[review_policy]` for Critic / Council (verbose only) |
| `scratch size is unknown (\`du\` timed out)` | Verbose scratch scan could not finish a `du` in time. This is not the >10 GB warning; check the directory by hand if you care about tmp size |
| `unfilled placeholders` | `<<PLACEHOLDER: …>>` values still in config. Multiverse will not guess them. Dry-runs work; real dispatch is blocked until they are set |

## `workers` totals

Printed after the table by `render_text` in
`crates/mv-cli/src/commands/workers.rs`:

| Line | Meaning |
|---|---|
| `workers N/CAP` | Live workers against fleet `max_concurrent`. Unrecorded finished lanes still count until reconcile |
| `seats: <account> N` | Open worker panes on that seat (working/blocked, plus startup grace). `?` means Herdr was unreachable |
| `projects: <name> N open, ceiling C` | Live panes attributed to that project, against that project's ceiling. `(home hub)` marks `[projects.multiverse]` |
| `nexus sessions N (excluded from workers; they do not count against max_concurrent)` | Live Nexus panes, counted separately from workers |

STATUS `stalled` / `waiting-on-shell` are display-only overlays on the
table (see `workers.rs` module docs). `done` on STATUS is Herdr's idle
state, not "the session ended".

## Board legend

Rendered by `crates/mv-board` (`accounts.rs`, `status_cards.rs`,
`columns.rs`) and scoped by `crates/mv-cli/src/commands/board_data.rs`
(`scope_header`, `account_snapshot`).

### Scope line

| Words | Meaning |
|---|---|
| `project: NAME only (one-project view; --all for the fleet)` | This board is one project. Pass `--all` (or name several) for the fleet view |
| `projects: a, b, c` | Pooled fleet view of those projects |

### ACCOUNTS row

| Words | Meaning |
|---|---|
| `fleet N/CAP` | Live workers vs `max_concurrent` |
| `✓ AVAILABLE` | This seat can take work |
| `◐ AT CAP` / `HALF-OPEN` | Seat cap reached, or one trial task is testing a fenced pool |
| `✗ FENCED` | Breaker tripped |
| `◔ DRAINING` | Usage gauge at or below `dispatch_min_quota_percent` |
| `name · model` | Card title: account name and the model that card is showing |
| `window: n/a (no usage gauge)` | Claude with nothing in the gauge store. This is not an API-billing warning; a Claude subscription seat has no usage window to show until something records one |
| `window: n/a (not reported)` | Non-Claude seat with no gauge |
| `tok 24h n/a` | No 24h token reading yet |

### STATUS cards

| Words | Meaning |
|---|---|
| `N working` | Agents Herdr reports busy |
| `N held / C cap` | Live workers using a seat, against `max_concurrent` |
| `OWED REPLIES (N)` | Unanswered `activity --ask` messages. A ⚠ prefix means older than 24 hours |
| `offline (no live orchestrator)` | That project has no live Nexus pane |
| `never opened` | The inbox has never been read |
| NEXUS LAST PASS `mail/reconcile/backlog` | Last inbox read, last reconcile pass, last backlog pass. `?` means never; `0s just now` means this second |
| `⚠ overdue` | One of those passes is older than 30 minutes, or has never happened |
| `warn` | The same overdue flag on a narrow board |
| `✓` | All three passes in the last 30 minutes |
| `✗ parked` | That project's Nexus is blocked at a prompt |

### Columns

| Column | What lands there |
|---|---|
| **BLOCKED** | Needs you: `blocked` status, a prompt, a gone pane, dirty `done`, unreadable status, or an unanswered backlog question |
| **IN PROGRESS** | Still running, waiting, stalled, or finished but not yet recorded |
| **BACKLOG** | Open cards and answered questions. `OVERDUE` means the card's due date has passed |

## `Would write` vs `Wrote` / `Writing`

`onboard` prints `Would write` only for a dry-run or a preview that still
needs a `[Y/n]` confirmation (a seat block before `Write this seat`, or
the chain table before `Write the recommended … chains`). Once it is
actually writing with no further prompt, the label is `Writing` and the
success line is `Wrote`. Dry-run ends with `Dry-run: config not written.`
(`crates/mv-cli/src/commands/onboard.rs`).

## TERM=dumb and NO_COLOR

`board` and `workers` draw Unicode boxes and markers by default. With
`TERM=dumb`, a non-empty `NO_COLOR`, or `board --no-color`, those glyphs
become one-cell ASCII (`+` `-` `|` for boxes, `*` `x` `!` for status)
via `crates/mv-board/src/layout.rs` `to_ascii_plain`. `start` never
prints literal `**Nexus**` markdown
(`crates/mv-cli/src/commands/start.rs`, `batch_summary`).

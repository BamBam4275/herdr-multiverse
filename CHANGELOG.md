# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it reaches `1.0.0`. Before `1.0.0`, minor versions may include breaking
changes.

## [Unreleased]

## [0.1.2] - 2026-11-07

### Added

- Docs: replace the README demo placeholder with a real board capture in
  `docs/assets/board-demo.svg`, reproducible against fake Herdr with
  `scripts/docs/render-demo.sh`.

### Changed

- Dependencies: update `unicode-width` to 0.2.3.
- Docs: reconcile the README status with the current release, link
  `docs/parity.md` from the contribution guide, and document troubleshooting
  for a missing plugin wrapper, a halted fleet, and Herdr not being on PATH.
- CLI: point the `check` installation hint at `docs/install.md`.
- Plugin: describe the read-only reports, explicit Start action that opens
  live agent sessions, and Import Python action that copies config and state.

### Fixed

- Linux: skip `/proc` metadata checks denied with EACCES/EPERM while
  retaining fail-closed handling for unreadable current-user working
  directories.
- Tests: close fake-Herdr script write handles before executing them to
  avoid Linux ETXTBSY failures, and synchronize the council lock probe with
  admission instead of relying on a fixed sleep.
- CI: compile the presence idle-time parser and its tests only on macOS.
- Docs: preserve board demo column alignment across SVG renderers.

## [0.1.1] - 2026-11-06

### Added

- Release: `scripts/release/sync-public.sh` synchronizes public releases
  without rewriting history.

### Changed

- Dependencies: update `toml` to 1.1, `toml_edit` to 0.25, `thiserror` to 2,
  `ratatui` to 0.30, `crossterm` to 0.29, `rustix` to 1.1, `sha2` to 0.11,
  `fancy-regex` to 0.20, and `unicode-width` to 0.2.
- CI: use `macos-15` for arm64 release builds and initialize conformance
  and test fixture repositories with `git init -b main`.
- Dependabot: group Cargo minor and patch updates separately from major
  updates.
- Docs: the public install command is `herdr plugin install
  BamBam4275/herdr-multiverse --ref vX.Y.Z --yes` in README,
  `docs/install.md`, and `docs/migrating-from-python.md`; the
  required-check list (fmt, clippy, tests, conformance, rustdoc `-D
  warnings`, cargo deny) matches in `AGENTS.md` and `CONTRIBUTING.md`;
  `docs/parity.md` records that `activity inbox --replay-since` /
  `--replay-last` are on the Rust clap surface.
- Docs: README Status, `docs/parity.md`, and `docs/install.md` match
  this tree: 1995 passing crate tests (sum of `cargo test -j 4`
  `test result` passed lines), 161 conformance scenarios,
  `conformance/diff_fuzz.py` (read probes after each write; four
  accepted `dispatch --dry-run` rendering differences: role-rename
  agent short-codes and prompt-file names, prompt-file line counts,
  this binary's `WOULD RUN` pane commands, and prompt wrapping that
  can split a sandbox path), and one-machine timings (warm `board
  --all --read-only --no-reconcile --no-color` 4.56 s; warm `sweep
  --stale-only` 6.94 s → 2.50 s).

### Fixed

- Worktrees: skip other users' processes when checking `/proc` working
  directories on Linux, retaining fail-closed behavior for unreadable
  processes owned by the current user.
- Sweep: reclaim a task's pane in the same pass that reconciles the task.
- Workers: match Python's seat occupancy counts.
- Worktrees: report each rescued artifact during `--prune --yes` and
  include the rich per-lane summary labels.
- Board: correct padding for agent short codes.
- Dispatch: equalize the remaining panes' row sizes during teardown.
- Runner: guard the bare-Enter nudge against ghost echoes of short prompts.
- CLI: cwd-scoped backlog commands explain surplus project/title or
  project/item arguments in the handler with exit 1, matching Python.
- CLI: task listings retain prior outcome ordinals during stale-task
  convergence; newly written ordinals appear on the next invocation.

## [0.1.0] - 2026-11-02

A from-scratch Rust port of the Python `multiverse` reference
implementation: a single `multiverse` binary, built across seven crates,
packaged as a Herdr plugin, and checked for behavioral parity against
the frozen Python original. Most of the CLI surface is real and
conformance-tested, including the three highest-risk write paths — live
(`--yes`) dispatch, a live Nexus-pane `start`/`--reseed`, and live
(`--yes`) council — which now drive the real `mv-engine` engine against
Herdr rather than refusing, each with its own documented scope cuts short
of full Python parity. See the README's [Status](README.md#status)
section for the exact, current boundary between the two.

### Added

- Seven-crate workspace (`mv-core`, `mv-config`, `mv-herdr`, `mv-state`,
  `mv-engine`, `mv-board`, `mv-cli`) plus `config.example.toml`,
  `prompts/`, and `templates/`. All seven crates carry real
  implementations, not module-list stubs.
- Clap surface for 28 `multiverse` subcommands, global `--config`, exit
  codes 0/1/2/3, `--yes` / `--dry-run` authorization on `dispatch` and
  `council`, and rejection of retired `--role` names (exit 3).
- Real command handlers for `halt`, `projects`, `stop`, `add`, `new`,
  `check`, `doctor`, `tasks`, `scratch`, `worktrees`, `result`,
  `activity`, `backlog`, `workers`, `notify`, `inspect`, `directives`,
  `board`, `accounts`, `seat`, `health`, `review-policy`, `sweep`,
  `presence`, and `config migrate`, each backed by its own crate-level
  tests and, where applicable, a conformance scenario.
- `dispatch --dry-run`: real seat selection against a project's
  configured chain, including breaker state and degrade/fallback policy,
  rendered as a plan. `dispatch --yes` drives the real
  `mv-engine::runner::dispatch` against the real Herdr runner;
  `multiverse start` opens a live Nexus pane through
  `mv-engine::fleet_start`, and `--reseed` on an already-open project
  re-points that live session rather than only rewriting the seed file;
  `multiverse council --yes` convenes a real roster through
  `mv-engine::council::convene` against the same runner — each with its
  own scope cuts short of full Python parity rather than a blanket
  refusal. See the README's Status section for exactly what each still
  skips.
- Directory-trust handling before a seat launches: a Codex seat's
  `trust_level = "trusted"` is written into `CODEX_HOME/config.toml`
  automatically (`mv-engine::codex_trust`); a Claude seat's own
  "do you trust the files in this folder?" dialog is detected and left
  for a human to accept once per project rather than typed into
  (`mv-engine::runner::attention_reason`); a live `dispatch --yes` that
  lands on one refuses with a non-zero exit naming the pane instead of
  reporting a successful dispatch.
- `mv-config`: a config loader that honors `<<PLACEHOLDER: ...>>` values
  without ever guessing at one, `toml_edit`-based comment-preserving
  appends for `add`/`new`, and a one-shot role-key migrator
  (`multiverse config migrate --dry-run` / `--backup`) with a
  timestamped backup file.
- Herdr CLI client (`mv-herdr`): argv builders, JSON envelopes, subprocess
  runner, `HERDR_BIN_PATH`, capture-backed shapes, and a stateful
  `fake-herdr` binary.
- Conformance harness under `conformance/` (scenario runner, normalizer,
  72 scenarios against fake Herdr, including seven history-reader
  scenarios for retired role names). All 72 currently pass against Rust
  (`python3 conformance/run_all.py`).
- 1610 passing crate unit/integration tests across the workspace
  (`cargo test --workspace --all-features --locked`).
- Routing log and sidecar index (`mv-state`).
- Task directories, `done` / `blocked` / `failed` status tokens, and
  brief rendering (`mv-state`, `mv-engine`).
- Embedded prompts, skills, and templates (`mv-engine` `build.rs`), with
  `MULTIVERSE_PROMPTS_DIR` / `MULTIVERSE_TEMPLATES_DIR` overrides and the
  `*` / `!name` / `!*` skill-grant algebra.
- Standing directives, Nexus seed rendering (legacy filename
  `orchestrator.md`), and Docker reports.
- Herdr plugin packaging: `herdr-plugin.toml`, bootstrap/build/action
  scripts, CI, and release workflows that install a checksummed
  `multiverse` binary with a Cargo fallback.
- Project documentation, licensing, and community health files: README,
  contributing guide, code of conduct, security policy, support guide,
  issue/PR templates, `docs/architecture.md`, `docs/roles.md`,
  `docs/configuration.md`, `docs/install.md`, and
  `docs/migrating-from-python.md` for operators moving off the Python
  reference.

### Changed

- Dual license MIT OR Apache-2.0.
- Write-side role names are Nexus, Scout, Builder, Archivist, Critic,
  Seer, Council, Artisan, and Watcher. History already on disk — task
  directories, routing-log entries, briefs — still reads under the
  retired names (`orchestrator`, `explorer`, `fixer`, `librarian`,
  `reviewer`, `oracle`, `designer`, `observer`); new write/config
  surfaces accept only the current names. Role prompt files live under
  the new keys.

[Unreleased]: https://github.com/BamBam4275/herdr-multiverse/compare/main...HEAD
[0.1.2]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.2
[0.1.1]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.1
[0.1.0]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.0

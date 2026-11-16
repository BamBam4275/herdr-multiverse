# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it reaches `1.0.0`. Before `1.0.0`, minor versions may include breaking
changes.

## [Unreleased]

## [0.1.8] - 2026-11-16

First release since v0.1.6; v0.1.7 was prepared but never published.

### Added

- CLI: `activity reply Holder#SEQ`, run by the ask's own sender rather than
  its holder, still posts the answer as the holder — but the stored
  message and every place it's printed (`inbox`, `show`, `replay`) now
  mark it relayed (`relayed_by=` on disk, `(relayed by <sender>)` wherever
  it's shown), so nobody reading the log mistakes it for the holder's own
  words. A reply the holder posts itself is unaffected: the field is
  omitted and the line is byte-identical to before. Relaying a reply also
  re-checks the sender's own `[activity.pairs]` permission into the
  holder at reply time, the same re-check `close` already applies before
  closing a remote ask — being that ask's own sender authorizes acting on
  it, not writing into the holder's log regardless of pairing.
- CLI: `worktrees --prune --reap` and `tasks --reclaimable --reap` can now
  end a lane's own lingering host process (SIGTERM, a grace window, a
  re-confirmed identity check, then SIGKILL) once the lane's pane is
  otherwise clear — report-only by default, exactly as before. `--reap`
  is never folded into a plain `--prune`/`--reclaimable --yes`, never
  runs under `--force`, and `--dry-run` (or `--reap` without `--yes` on
  `--reclaimable`) previews "would reap" and signals nothing. `sweep`
  itself is unchanged: it still only ever reports this class of leftover
  process, never signals it. A shell, Herdr itself, pid 1, or any
  ancestor of the running `multiverse` process is never a reap target.
- CLI: `worktrees <path> --prune` (including `--dry-run`) now also frees a
  removed, archived, or eligible kept lane's own per-worktree cargo
  build-dir under the shared build cache `.cargo/config.toml` configures,
  reporting the bytes freed the same way it already reports a reclaimed
  `target/`. The lane's build-dir is found read-only by scanning the
  shared build root's own dep-info/fingerprint files for evidence naming
  the lane's path — never by invoking `cargo` or `rustup` — and is deleted
  only when it resolves strictly under the configured shared root and
  differs from the main checkout's own. Only a full two-level cargo hash
  directory (e.g. `shared-build/2a/45625e58230a70/`) is ever a candidate,
  never a bucket or a legacy un-namespaced tree such as `debug/`. Sweep's
  disk-critical notice also names cargo shared-build bytes orphaned under that same root. (A first
  attempt resolved the build-dir via `cargo metadata`, which made `--prune`
  — including `--dry-run` — invoke rustup inside every lane; reverted,
  redone here with no cargo/rustup subprocess at all.)
- CLI: the shared build-dir cache `worktrees <path> --prune` scans for a
  lane's cargo build-dir now honours `CARGO_BUILD_BUILD_DIR` ahead of
  `.cargo/config.toml`'s `build-dir`, the same precedence cargo itself
  gives the env var, resolving a relative override against the current
  working directory the way cargo does.
- CLI: `worktrees <path> --prune` (including `--dry-run`) now also
  reclaims every *other* full workspace hash dir under the shared build
  root whose own build evidence names only workspace roots gone from
  disk — not just this project's own lanes, so a worktree removed outside
  `--prune` entirely, or one this project never tracked, is reclaimed the
  same way, reporting the bytes freed the same way a lane's own build-dir
  already is. A build-evidence line's own named path is live iff it still
  exists on disk — never an ancestor directory holding a `Cargo.toml`,
  which a lane nested under its own live main checkout
  (`<project>/.multiverse/worktrees/<task>/`) always has further up,
  making its hash dir unreclaimable. A hash dir naming a still-existing
  workspace, the main checkout among them, or with no readable evidence at
  all, is always kept; a cargo registry/git-checkout dependency source
  under `CARGO_HOME`, or a path under the shared build root itself, never
  counts as either live or dead workspace evidence. A scan the shared
  sweep budget cut short before finishing is never read as orphaned,
  regardless of what it saw before running out. No `cargo`/`rustup`
  subprocess, same as the per-lane scan.
- CLI: add `multiverse down`/`multiverse up`, two fleet-wide commands built
  as thin orchestration over existing primitives — never `docker`/
  `compose`/`sync-public`, never a `git` write, never `stop --force`.
  `down` halts dispatch with a `fleet down: ` reason, asks each open
  project's live Nexus to wrap (the fixed wrap prompt the seed hook below
  defines), waits for `.multiverse/passdown.state` to read `wrapped`
  behind a non-empty `.multiverse/PASSDOWN.md`, then closes the
  workspace once every live agent in it — not only the Nexus — is idle
  or done; a project with no open Nexus is skipped with a warning rather
  than started. `up` starts every targeted project in-process through
  the same path `start` uses (so a pending PASSDOWN resumes through the
  seed hook below), waits on whichever projects were pending or wrapped
  before the call, and clears the halt only when its reason still
  carries the `fleet down: ` prefix `down` wrote. Both take
  `--timeout SECONDS` (`0` means forever; default `1800`) and
  `--dry-run`.
- Nexus: a project's `.multiverse/passdown.state` sidecar — never
  `.multiverse/PASSDOWN.md`'s mere existence — now gates a resume hook in
  every seed: a `pending` or `wrapped` sidecar prepends a PASSDOWN resume
  block naming the absolute path and BRING-UP to the seed body and first
  prompt (and their home equivalents), ahead of the existing handoff
  sentence, and marks the seed stale so an ordinary `start`/`--reseed`
  re-points onto it — the hook `up` above relies on. Both Nexus role
  prompts gained matching "Fleet down / Fleet up" sections.
- CLI: `activity broadcast` takes `--ask`, giving each recipient the same
  per-recipient ask semantics `activity send --ask` already has — every
  project gets its own open ask, any one reply resolves it, and an
  unanswered recipient stays open.
- CLI: `activity reply <Project>#<SEQ>` now resolves cross-project (the
  same qualified syntax `close` already accepts), authorized when the
  caller is that project or the ask's own sender, so a reply to a
  hub-sent ask lands where the asker reads it. Added
  `activity show <SEQ>` / `show <Project>#<SEQ>`, which prints one
  message in full, read-only, with no ack. `activity inbox --as-nexus`
  no longer conflicts with `--replay-since`/`--replay-last` at parse
  time.
- CLI: `workers` now flags a pane stalled by its own unchanged content,
  not an unchanged Herdr revision — a revision can repeat under load
  without the pane actually being stuck.
- CLI: `seat show <account>` now renders the same
  `workers N/<cap>, nexus M/<nexus_slots>` split `accounts` already
  shows once a seat's `nexus_slots` is set, in both human and `--json`
  output.
- CLI: `activity inbox` now prints a footer listing every still-open ask
  this project holds, oldest first (`<holder> owes <sender> #<seq>
  <age>`), sourced from the project's own ask history; `--peek` and both
  replay flags show the same footer, and nothing prints when there are
  no open asks.
- Sweep: the disk-critical notice now also names each configured
  project's `.multiverse/worktrees/*/target` lane build caches —
  total size, the three largest lanes, and the prune command that
  frees them — alongside the existing Docker and Go build cache
  totals.
- CLI: `worktrees <path> --merge <task>` merges a finished lane's
  branch only after its own commits pass a `[secrets_scan]` gate
  (gitleaks, and trufflehog when configured and present), alongside
  the existing checks that the task holds a terminal `done` status
  and a clean lane. A finding writes redacted evidence to
  `.multiverse/evidence/<task>-secrets.txt` and refuses without
  merging; a missing required scanner refuses (exit 3) with an
  install hint, `required = false` warns and merges unscanned, and a
  clean scan merges with `git merge --no-ff --no-edit`. `check` now
  reports when `[secrets_scan]` is required and no scanner can be
  found.
- Cargo: the shared build-dir cache is now namespaced per worktree
  (`build.build-dir` in `.cargo/config.toml` adds the
  `{workspace-path-hash}` template variable), closing a freshness-check
  gap where one lane could silently reuse another lane's stale
  compiled crate; the cost is that a fresh lane's first
  `cargo build --workspace --locked` pays a full cold compile instead
  of reusing a warm shared cache.
- Sweep: a RUNNING lane whose own dispatch brief is still sitting
  unsubmitted in its `claude`/`codex`/local composer now gets one
  send-keys Enter before the finish/resume/cap nudges act, instead of
  risking a canned prompt typed over an unsubmitted brief; unknown
  non-empty composer text instead reports "unsubmitted composer:
  <id>" and holds the lane out of those three senders without acting.
- Dispatch: a failed-to-start result now carries a classified
  `reason=` (`not-ready-timeout`, `needs-attention`,
  `unreadable-pane`, `start-error`) in the routing log instead of
  naming only the pane; a not-ready timeout also saves the last
  matching agent-list entry plus a visible and a recent pane read to
  `.multiverse/evidence/<task>-failed-to-start.txt`.
- Config/Dispatch: an account's open-agent cap and the fleet-wide
  `max_concurrent` check now count worker panes only, across
  `dispatch`, `council`, `check`, `accounts`, and `review_policy` —
  a busy Nexus pane dispatched to the same seat or project no longer
  eats a worker's capacity. A new per-account `nexus_slots` (default
  0) declares a seat's Nexus capacity explicitly, and `accounts` shows
  "workers N/<cap>, nexus M/<nexus_slots>" for a seat with
  `nexus_slots` set, instead of the old merged "open: N/cap" line.
- Board: every project's BLOCKED/IN PROGRESS/BACKLOG card boxes are
  now colored by a stable per-project hash on the single-project
  board and the `--by-project` view, not only the merged fleet view;
  color is still suppressed by `--no-color`/`NO_COLOR`, and now also
  for `TERM=dumb` and a non-terminal stdout.
- Conformance: `run_all.py` now also runs `test_diff_fuzz.py`'s own
  unit tests in-process before the scenario matrix, printing a pass
  count and failing if any unit test fails; previously they only ran
  by hand.
- Tests: added regression coverage pinning already-correct behavior:
  `stop --force`'s workspace close across busy/roster/pane/lane shapes,
  `dispatch` dropping its routing-log lock before pane creation, a
  fake-Herdr pane-label round trip, and a worktree rescue quarantine
  copy that differs from the rescued file being kept.
- Internal: added missing docstrings across `conformance/*.py` and
  `scripts/**`, and strengthened or added tests covering the stalled-
  pane probe-determinism findings, a flaky dispatch-lock-ordering
  proof, and three mutation-weak stop/lifecycle assertions;
  comment/test-only, no behavior change.
- Added `docs/release/publishing-checklist.md`, documenting the full
  private-to-public release flow (`export-fresh.sh`, `sync-public.sh`,
  `check-private-names.sh`, gitleaks, CI/release workflows, required
  root files, GitHub settings) plus gotchas, for other projects
  preparing their own first publish.
- `docs/configuration.md` now documents who sets a dispatched pane's
  `MULTIVERSE_CONFIG`/`MULTIVERSE_STATE_DIR`, that the values are
  absolute, and that a worker's own `multiverse` therefore uses the
  dispatcher's config and state.

### Changed

- CLI: `activity send` against more than one of the recipient's open
  asks now refuses (exit 3), naming each open ask and
  `activity reply <seq>`, instead of printing the candidates as
  informational output that read like a successful send.
- Dependencies: update `crc32fast` to 1.5.5 and `fancy-regex` to
  0.20.1.
- Conformance: `diff_fuzz.py` now normalizes eight classes of
  intentional Rust-only output difference (stalled-pane content reads
  and wording, `pane-watch.json`'s tree exclusion, dispatcher-resolved
  config/state env pairs, quote-insensitive launch-tail comparison,
  `start --reseed`'s extra pane list, a reclaim listing's "stale
  bookkeeping" line matching Python's "would close" line for the same
  task, and sweep's host disk-floor line folded to a placeholder when
  both sides report it), narrowly enough that a genuine near-miss
  still diverges.
- Internal: added missing `///` and short `//` doc comments across all
  seven workspace crates (mv-core, mv-config, mv-herdr, mv-state,
  mv-engine, mv-board, mv-cli); comment-only, no behavior change.
- Tests: hardened test temp-directory cleanup across the workspace so
  a mid-test panic no longer leaks into `TMPDIR` — `mv-cli`'s
  `sweep.rs` (around 50 call sites via a shared sandbox-root type),
  `mv-state`'s cargo-home/fake-bin/notify-episodes/build-override temp
  dirs, `mv-engine`'s council fake-binary cache, and several commands'
  own test fixtures now clean up via `Drop` instead of a trailing
  `remove_dir_all`.
- Sweep: the private `close_merged_backlog_links` duplicate is gone;
  sweep now calls `mv_state::tasks::close_merged_backlog_links`
  directly, the two bodies having been behaviorally identical (same
  filters, writer-role gate, supersede exclusions, merged evidence, and
  `backlog::done` error handling).
- Dependabot: ignore cargo major updates for `dirs` — `dirs` 5+ pulls
  in `option-ext` (MPL-2.0), which `deny.toml` does not allow, so the
  workspace pin stays at 4.0.
- `cargo build`/`test`/`run` now force a sentinel
  `MULTIVERSE_STATE_DIR`/`MULTIVERSE_CONFIG` so a test can no longer
  silently read or write the real fleet's config or state; plain
  `cargo run` now requires its own `--config`.
- Tests: renamed fixture account/pool/chain identifiers in `workers.rs`
  and `check.rs` off the operator's private-names deny list; the
  behaviour each test pins is unchanged.
- Conformance: updated the `workers` fixtures for the compact
  seven-column table (`run_all.py` 163/163).

### Fixed

- CLI: `worktrees <path> --prune --deep` now also sweeps
  `.multiverse/worktree-state/`'s own quarantine copies — the per-lane
  snapshot `remove_lane` retains forever against a descriptor that might
  still be writing to it during Git's own removal, which previously
  accumulated without end. A copy is dropped once every file it holds is
  byte-identical to what rescue already copied into the project's own
  `.multiverse/`, regardless of age, and otherwise once 14 days have
  passed since its own newest write. `--dry-run` previews what each copy
  would become without removing anything. Never part of a plain `--prune`.
- Tests: `mv-state`'s and `mv-engine`'s own `shared_build_root` fixture
  tests no longer fail when the process inherits a real
  `CARGO_BUILD_BUILD_DIR` (every lane and the Nexus export one, pointed at
  their own `$TMPDIR`, with no `{workspace-path-hash}` placeholder) —
  cargo's own higher-precedence override shadowed each test's fixture
  `.cargo/config.toml`, resolving `shared_build_root` to `None` instead of
  the fixture's template. Each affected test now clears the variable for
  its own duration through a `BuildDirEnvGuard::cleared()` guard that
  shares its lock with the existing override-setting guard, so a test
  pinning the variable to one value can never race a concurrent test
  pinning it to another. Production behaviour is unchanged.
- CLI: `down` now checks every live agent in a project's workspace, not
  only the Nexus, before closing it — a worker lane still `working` when
  the Nexus wrote `wrapped` previously got killed mid-task along with
  the pane; closing now waits for every agent to be idle or done, the
  same rule `stop` uses.
- CLI: `worktrees --rescue` no longer copies a rescued lane file's own
  permission bits onto its destination — a lane file left group/
  world-writable now still lands owner-only in the shared project tree.
- CLI: `tasks --supersede`/`--acknowledge`/`--state` reject a task/
  status/state value containing a tab, CR, LF, `=`, or a leading/
  trailing space before writing, instead of letting it smuggle a second
  `status=` field into the routing log's tab-separated line.
- Redaction: generic `*_KEY` credential stems (`PRIVATE_KEY`, `SSH_KEY`,
  `ACCESS_KEY`, `DEPLOY_KEY`, `SIGNING_KEY`, `ENCRYPTION_KEY`, and
  camelCase equivalents) are now redacted; `foreign_key`/`primary_key`/
  `sort_key`/`cache_key`/`partition_key`/`key_count` stay exempt.
- Dispatch: `--into`/collision-supersede no longer hold
  `routing.log.lock` across an `agent list` round trip to Herdr; the
  liveness probe now runs under the fleet lock alone, before any
  project lock exists, re-verifying every local condition (and failing
  closed on a change) instead of asking Herdr a second time under the
  lock.
- CLI: `activity reply SENDER#SEQ` (and the bare `reply SEQ` form) now
  always checks the caller's own log first for an ask addressed to
  them, instead of opening the named project's log and reporting
  "no ask found" when the true recipient qualifies with the ask's
  sender.
- Dispatch: a collision predecessor's pane, and a reclaimable task's
  recorded pane, are now only closed once the pane's own label names
  the owning task id and (when that task has a worktree) its live cwd
  canonicalizes to that lane — a Herdr restart reissuing the same
  deterministic agent name into a recycled pane id could previously
  defeat every other identity check.
- CLI: `activity send --ask` now attempts a best-effort notify alert the
  same way the project's prompts already point operators at; a missing
  or failing notify hook prints a one-line warning instead of silently
  dropping the alert, and the command still exits 0 since the message
  itself already landed.
- mv-herdr: a transient spawn failure (`EAGAIN`/`ENOMEM`/`EMFILE`/
  `ENFILE`/`EINTR`) is no longer reported as a missing binary; it shares
  the existing busy-retry backoff and, if still failing after that
  budget, reports as a spawn failure instead of sending an operator
  chasing an install/PATH problem that was never there.
- CLI: `check`'s idle-sleep probe now reports "unknown" for any probe
  failure, including a transient spawn error, not only a timeout — a
  probe that silently vanished under host load previously dropped the
  warning instead of reporting it.
- CLI: `seat show` no longer counts a busy Nexus pane on the same seat
  toward its reported open-agent count, matching `check`/`accounts`/
  `review_policy`.
- CLI: sweep/reconcile/close-stranded/`--reclaimable` now dedup
  per-lane `git status --porcelain` probes within one pass, the same
  way the board already does, instead of re-spawning one per task id
  sharing a lane; sweep's own dirty-done detail text now reuses that
  same pass-wide probe cache instead of building a fresh one.
- Nexus: a Nexus landing a finished lane now always goes through
  `worktrees <path> --merge <task>` — the secrets-scan choke point —
  instead of a plain `git merge`/`-m`, so this release's `[secrets_scan]`
  gate can no longer be bypassed by the Nexus's own prompt.
- CLI: sweep's per-project summary now prints composer-hold outcomes
  (submitted composer ids, and unsubmitted-composer warnings) alongside
  the existing finish/resume/cap lines.
- CLI: `down` now finds a project's Nexus pane through the same
  fallback `start --reseed` already uses, instead of relying only on
  the renamed-agent lookup, so a project whose first seed never reached
  agent rename (but whose root pane `start` already labelled) no longer
  reports "its Nexus pane could not be found."
- CLI: `down` no longer waits out the full `--timeout` on a project it
  already reports `UNREACHABLE`; that project is recorded as a problem
  immediately instead, so `down`'s exit status still reflects it, while
  every reachable project is still waited on exactly as before.
- CLI: `sweep`'s stranded/expired/superseded reconciliation and
  `tasks --close-stranded --yes` no longer treat a still-live agent as
  gone just because Herdr has not yet reported its name for that pane
  (no status file written yet); only a pane now confirmed to list a
  different, non-empty agent name still reads as gone.
- CLI: `sweep`'s and `tasks --close-stranded --yes`'s stranded-task
  reconciliation now closes nothing at all when Herdr's live agent
  listing fails, instead of reading the failure as an empty listing
  where every AWAITING task looks abandoned.
- CLI: `down` now refuses to write its `.multiverse/passdown.state`
  sidecar through a symlink planted at the sidecar path or its
  `.multiverse` parent, instead of truncating whatever it pointed at;
  `workers`' pane-content fetch now guards a dash-leading pane id the
  same way `pane get`/`agent get`/`agent wait` already do.
- CLI: `activity`'s and `mail`'s append-only log files are now created
  at mode `0600` directly, instead of at the process umask's mode and
  chmod'd afterward — closing the brief window where a freshly created
  log could land group/world-readable before the tightening chmod ran.
- CLI: `worktrees <path> --merge`'s TruffleHog secrets-scan evidence is
  now redacted of raw secret values the same way gitleaks's own output
  already is, instead of writing TruffleHog's raw findings to
  `.multiverse/evidence/<task>-secrets.txt`; the evidence file's own
  leaf path is also checked for a symlink immediately before writing,
  not only the directory components above it.
- Dispatch: a not-ready-timeout's
  `.multiverse/evidence/<task>-failed-to-start.txt` is now written
  atomically, at mode `0600`, with the captured pane scrollback
  redacted, and a symlink planted at the evidence directory or file
  path is refused rather than followed.
- Dispatch: a role config's `skills = ["!*"]` deny-all grant (Watcher's
  documented config) no longer fails with "playbook() grants unknown
  skill"; the grant algebra `skills::resolve()` already handled is now
  applied before `--dry-run` or a live dispatch renders the brief.
- Dispatch: a finished lane (status already done/blocked/failed) no
  longer holds a `max_concurrent` fleet slot until a Nexus reconciles
  it, and `dispatch --wait-for-seat` against a full fleet now polls for
  a freed slot instead of refusing immediately.
- Dispatch: a dispatched worker pane now inherits the dispatcher's own
  resolved `MULTIVERSE_CONFIG`/`MULTIVERSE_STATE_DIR`, so the worker's
  own `multiverse` no longer falls back to a stale
  `~/.config/multiverse/config.toml` when the dispatcher resolved a
  different config or state dir.
- CLI: `worktrees --rescue --dry-run` and `--prune --dry-run` change
  nothing and preview exactly what the same flags without `--dry-run`
  would do; `--prune` also treats `__pycache__`, `target`, and
  `node_modules` directories at any depth, and `*.pyc` files, as
  regenerable, and frees a kept
  finished lane's own `target/` once nothing in the lane is still live.
- CLI: `check` resolves the Nexus chain the same way `start` does
  instead of running it through the worker-dispatch capacity gate;
  the board drops a task corrected via `--supersede` to a non-standard
  status from the active columns instead of leaving it in IN PROGRESS;
  the Start action's description matches the board's one-shot tab.
- CLI: `workers` sizes each table column from its own sanitized cell
  contents instead of letting long metadata run columns together, and
  separates the live Herdr pane `STATUS` column from the task's own
  recorded `OUTCOME` column (an em dash when no task is joined),
  covering long metadata, unrecorded completions, and recorded
  outcomes.
- CLI: a backlog single-item subcommand (`done`, `reopen`, `annotate`,
  `reprioritize`, `show`, `mark-asked`, `answer`) no longer renders
  `<item>...` in its `--help`/usage text; a hidden trailing positional
  now absorbs surplus tokens so they still reach the same "takes only
  an item id" refusal (exit 1) as before, rather than clap's own usage
  error. `add`/`ask` are unaffected.
- Dispatch: `review_policy.budget_per_window` is now enforced against
  real dispatch, not only reported — a spent rolling 24h Critic budget
  now refuses the next critic dispatch, and `review-policy` reads the
  same counter so its `used`/`limit` can no longer diverge from what
  dispatch actually enforces.
- CLI: `start --reseed` now finds the project's labelled Nexus pane
  even when the first seed was blocked on a folder-trust dialog (which
  skips the agent rename `find_orchestrator_agent` relied on), by
  falling back to a pane in the project's workspace whose title already
  carries the Nexus agent name. A pane still parked on a trust dialog
  is still refused rather than sent to.
- Sweep: a task whose recorded pane id is now occupied by a different
  live agent — reusable after a Herdr restart — no longer reads as
  still running forever; `is_awaiting` and the `close_stranded`/
  `expire_awaiting`/`auto_supersede` liveness checks now require the
  live listing's agent name at that pane to match the task's own
  recorded agent, the same same-occupant check already applied to the
  pane-close sites.
- Board: `board --all --read-only` no longer re-spawns `git status
  --porcelain` once per task id naming a lane reused by more than one
  historical dispatch into the same branch; a lane is now probed at
  most once per project per board build, with output unchanged.
- Sweep: an idle `claude`-kind lane past its startup grace with no
  status file now receives the finish nudge the same way a
  `codex`-kind lane does (same startup-grace/dedupe/claim-then-act
  contract); `grok` and any other kind stay excluded.
- CLI: `worktrees`' merge-readiness check now compares against HEAD of
  the project's own main checkout instead of a hardcoded `main` ref, so
  the "main moved since this lane's branch point" warning now fires on
  a repo whose trunk is `master` too, alongside the existing `main`
  case.
- CLI: `council` now honors a global `--config` flag when seating a
  council instead of always resolving config from
  `MULTIVERSE_CONFIG`/the default path, matching `dispatch`.
- Tests: `scripts/test_plugin.py`'s required check now also discovers
  `scripts/tests/test_private_names.py`, which previously existed with
  no required runner exercising it.
- Internal: `normalize_lexically` no longer collapses a stacked
  leading `..` against the next one for a relative path; not reachable
  by today's callers, which always pass an already-absolute path, but
  corrected with a new unit test for future callers.
- Tests: de-flaked `mv-herdr`'s grandchild-timeout test under host
  load, gave `start.rs`'s remaining `launch_project` tests their own
  state-dir guard, fixed a guard field-drop ordering bug that could let
  a concurrent test observe another test's still-overridden
  environment, and restored (rather than cleared) a test's prior
  `MULTIVERSE_STATE_DIR` after an `import-python` run.
- `docs/architecture.md`'s Commands table and plugin-actions sentence
  now include the `import-python` subcommand and its
  `multiverse-import-python` plugin action, matching all 31 live
  subcommands and `herdr-plugin.toml`; `scripts/rehearsal/README.md`
  no longer describes a dispatched agent using a retired role name.
- `docs/parity.md`'s "Smaller surface notes" section wrongly claimed
  that `dispatch --column` and `notify --priority` accept free text on
  the Rust side; both are closed, case-sensitive value sets identical
  to Python's choices.
- `prompts/role-nexus.md` now documents the `activity inbox --as-nexus`
  task-id refusal (who sees it, why the cursor doesn't advance without
  it, and when to pass it instead of taking the peek) and corrects its
  own duty count from five to six, adding standing idle work;
  `docs/install.md` corrects `down`/`up`'s exit codes — a rejected
  `--timeout` value is exit `1`, not `2`, and exit `2` is a clap parse
  error, not a usage-error label.
- Tests: a test binary invoked directly (bypassing `cargo test`) no
  longer reaches the real fleet's config/state dir through
  `config_state_dir_from_env` — the remaining production `mv-cli`
  callers now route through `doctor::state_dir_from_config`, which
  itself panics under `#[cfg(test)]` when the resolved path isn't
  sandboxed, and every previously-unguarded test in `seat.rs`,
  `tasks.rs`, `start.rs`, and `sweep.rs` now holds its own state-dir
  sandbox guard.
- `conformance/README.md` now records that `dispatch-real-builder` and
  `dispatch-real-wait` already cover `dispatch --yes`, a real brief,
  and worktree creation; a stale comment describing `cwd_project`
  matching as exact-path equality is corrected to longest-prefix.

### Security

- `check-private-names.sh` now fails closed (exit 2) on a
  `git grep` error (e.g. an invalid deny-list pattern) instead of
  treating it as "no hits" through a swallowed `|| true`.
- The gitleaks version pin (`scripts/release/gitleaks-version.sh`)
  is now the one source `export-fresh.sh`, the pre-push hook, and
  `ci.yml`'s secrets job all read, checked as a minimum rather than an
  exact match locally (brew drifts ahead of a fixed pin), and verified
  against a literal sha256 in CI; the pre-push hook also names a
  missing pin file in its refusal instead of surfacing a raw shell
  error.
- Closed four defense-in-depth gaps not reachable from
  today's input — a dash-leading pane/agent name argv guard, a task-id
  path-traversal guard, and a tab/CR/LF/`=` field guard on activity and
  mail log encoding — each mirroring a guard already applied beside it.
- Added `.gitleaks.toml` (default ruleset plus a narrow
  allowlist for the repo's own known-fake credential fixtures), an
  opt-in pre-push hook that refuses to push when gitleaks finds a
  secret or is missing, a CI job that downloads and sha256-verifies a
  pinned gitleaks release and scans full history, and made
  `export-fresh.sh`'s gitleaks step fail the export outright (prefer a
  pinned local binary, fall back to Docker) instead of silently
  skipping when no scanner is available.
- The opt-in pre-push hook also runs
  `scripts/check-private-names.sh` ahead of its gitleaks scan, using
  `MV_PRIVATE_NAMES` from the environment or
  `$HOME/.config/multiverse/private-names.txt` when that file exists;
  a hit refuses the push with the checker's own output, and the hook
  prints one skip line and continues when neither is configured.

## [0.1.6] - 2026-11-10

### Added

- Tests: guard every user-visible CLI surface against retired role names.

### Fixed

- Dispatch: the one-time Enter retry now uses `herdr agent send-keys
  <agent> Enter` instead of an empty `agent prompt`, which Herdr refuses.
  A dropped first Enter (frequent with Claude Code 2.1.321) no longer leaves
  the brief unsubmitted, and a Herdr error on the retry is now a refusal.
- Dispatch: recognise a long brief still wrapped across rows of a Claude
  or Grok composer instead of treating it as submitted.
- CLI: `sweep` now prints `skipped (no Nexus)` instead of the retired role
  name, and `tasks`' ROLE column and `tasks --reclaimable` show current role
  names for history recorded under retired names.

## [0.1.5] - 2026-11-09

### Changed

- CLI: `multiverse start multiverse` now opens the hub's board in its own
  unfocused `board` tab in the `multiverse` workspace, created if absent on
  every `start` (so closing it gets it back), instead of a split pane beside
  the Nexus. The board itself runs once — `board --all --read-only`, no
  `--watch` — instead of a job that keeps refreshing.
- Nexus seeds: new projects now write `.multiverse/nexus.md` instead of
  `.multiverse/orchestrator.md`. A project still carrying the legacy file
  keeps reading and writing it until its next seed write, which migrates it
  onto `nexus.md` (backing up any hand edits first) and removes the legacy
  file. Read-only commands accept either name. `start`'s batch summary no
  longer interpolates the seed filename and now prints a separate project
  paragraph and home paragraph instead of one that assumed a single
  repository.
- Board: `board`'s STATUS card title is now `NEXUS LAST PASS` (was
  `ORCHESTRATOR LAST PASS` / `ORCH LAST PASS`), and a board card footer or
  backlog role hint for history recorded under a legacy role name (e.g.
  `fixer`, `librarian`, `orchestrator`) now shows that role's current key
  instead of the retired one; the on-disk history itself is unchanged.
- CLI: the `backlog done`/`backlog reopen` refusal errors for a dispatched
  caller now say "report ... to your Nexus" instead of "your orchestrator",
  and `dispatch`'s review-budget-exceeded message now says "Critic
  dispatches" instead of "reviewer dispatches".
- CLI: the `workers` summary footer now says "nexus sessions" instead of
  "orchestrators", and `activity`'s Herdr-unreachable nudge ("orchestrator
  panes") and its inbox ack-refusal message on a dispatched task id ("not
  ... orchestrator") now say "Nexus" instead.

### Added

- Docs: note that the first `start` of a model CLI into a folder it has
  never opened before — including the home folder — may pause on that
  CLI's own one-time "do you trust this folder?" prompt, which Multiverse
  detects and leaves for a human to accept rather than typing into.

### Fixed

- Board: rendering no longer spawns a `date` subprocess per card to get
  its local calendar time; `mv_core::time::local_datetime` now resolves
  the ambient `TZ` through the `tz-rs` crate once per process and does a
  pure offset lookup per card, taking a large fleet's `board --all
  --read-only` from ~166s to ~20s.

## [0.1.4] - 2026-11-08

### Changed

- Config: `[projects.multiverse]` is now the home hub project — its `path`
  defaults to `<state dir>/home` when omitted, it is implicitly paired with
  every other project for activity in both directions, and the default
  `[board].dashboard_tab` no longer names it (empty by default).
- CLI: `multiverse start multiverse` opens a single `multiverse` workspace
  holding the home Nexus plus a read-only `board --all --watch --read-only`
  pane split beside it, creating `<state dir>/home` (mode `0700`) when it
  does not exist yet. An already-open hub workspace is left as is, and the
  hub never opens a `--dashboard` tab.
- Plugin: the `Multiverse: Start` action now runs `start multiverse` instead
  of `start --all`; open an individual project with `multiverse start
  <project>` or by asking the home Nexus.
- Breaking: an existing `config.toml` must add a `[projects.multiverse]`
  section (a `short` is enough; `path` is optional) for `multiverse start`
  and the plugin's Start action to open anything.

### Fixed

- Herdr: spawn retries on `ETXTBSY` instead of surfacing `MissingBinary`,
  closing a Linux fork-race window where a forked child inherited a
  fake-binary script's still-open write fd and any exec of it failed.
- Engine: dispatch placement now splits a tab's tallest pane instead of its
  newest, and refuses to split a pane whose halves would drop below a
  12-row minimum, opening a new tab instead of squashing a worker's pane.
- State/CLI: reclaim and reconcile now verify a pane's live agent identity
  before offering or closing it, so a pane id reused after a Herdr restart
  can no longer be mistaken for a different project's live agent.
- CLI: dispatch's unit tests are isolated from the live fleet state dir
  instead of reaching a dev machine's ambient `$MULTIVERSE_STATE_DIR`.
- CLI: board panes launch via the plugin's `multiverse_invocation()` instead
  of a bare `multiverse` token, so they work for a plugin user with no
  `multiverse` on `PATH`.

## [0.1.3] - 2026-11-07

### Changed

- Docs: `herdr plugin action invoke` examples now put the action id before
  `--plugin`, and `install.md` explains actions run asynchronously with
  output in `herdr plugin log list --plugin multiverse`.

### Fixed

- Linux: `worktree prune` and `dispatch --into` no longer fail with
  "Permission denied" when a same-uid non-dumpable process (e.g. a CI
  runner helper) hides its `/proc` cwd.

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
[0.1.8]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.8
[0.1.6]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.6
[0.1.5]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.5
[0.1.4]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.4
[0.1.3]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.3
[0.1.2]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.2
[0.1.1]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.1
[0.1.0]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.0

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html)
once it reaches `1.0.0`. Before `1.0.0`, minor versions may include breaking
changes.

## [Unreleased]

## [0.1.11] - 2026-11-19

### Fixed

- CLI: `worktrees <path> --prune` without `--yes` (or `--force`) no longer
  deletes any Rust-only build cache: a lane's `target/` and shared-cache
  build-dir, the orphaned shared-build sweep, and `--deep`'s
  `.multiverse/worktree-state/` quarantine removals are now each reported
  exactly as `--dry-run` reports them ("would free ..." / "would remove")
  and deleted only with `--yes`. Lane removal, archiving, branch reclaim
  and `--deep`'s task/scratch sweep keep their Python-parity behavior
  (card `20261119-b-73759c`).
- CLI: the `sweep` disk warning for lane `target/` bytes now suggests
  `multiverse worktrees <root> --prune --yes`; since prune frees lane
  `target/` and shared-build caches only under `--yes`, the old
  `--prune` hint reported "would free" and freed nothing.
- `worktrees --prune` now frees a just-removed lane's per-worktree cargo
  build-dir again (card `20261116-b-891676`, seen on 0.1.9). A real
  build-dir holds over a thousand `.fingerprint/` files that sort ahead of
  `deps/`, and none of them name an absolute path, but the scan counted
  them as evidence. They used up its 500-file per-dir budget before it
  reached the one dep-info file naming the lane, so both the per-lane
  reclaim and the orphan sweep kept the dir without saying so. The scan now
  reads only `.d` dep-info. When `--prune` still keeps a lane's build-dir,
  it prints one `kept build-dir — <reason>` line: no hash dir names the
  lane, the scan ran out of budget, a delete guard refused, or the delete
  failed.

## [0.1.10] - 2026-11-19

### Added

- CLI: `workers` and `accounts usage` gained `--json`, emitting one line
  of machine-readable JSON instead of the text table(s) — alphabetical
  keys, `null` where the text shows "unknown"/`\u{2014}`, and no change to
  the existing plain-text output. See `docs/parity.md`'s `workers`/
  `accounts usage` sections for the exact shapes.
- Config: `[projects.<name>]` gained a read-only `autostart` boolean
  (default `true`, no writer sets it). `multiverse start --all` now skips
  a project with `autostart = false`, printing one line naming the
  project it skipped; `multiverse start <name>` still starts that project
  when named explicitly. A non-boolean value is a load-time configuration
  error (exit code 3).
- CLI/board: a STALLED task (`tasks`'s listing and the board's cards) —
  Herdr reports the agent idle/done (ready for input, not working) while
  no terminal status file exists, the host-sleep shape of an idle prompt
  with no status, hours old — past the shared 30-minute
  `mv_board::layout::STALE_AFTER_MINUTES` stall threshold is now noted
  "possibly frozen", naming the signal and `herdr agent prompt <agent>`/
  `dispatch --into <task>` as the next step. A healthy RUNNING task
  (agent reported `working`) is never flagged on age alone, however long
  it runs; an earlier version of this change gated on age alone and
  flagged nearly every long-running, healthy lane — reworked to require
  this real signal instead. Nothing is sent or acted on automatically,
  and `blocked`/`unknown` keep their existing plain "stalled" note
  unconditionally.
- Release: `changelog.d/` now holds one `CHANGELOG.md` fragment per lane
  (`<task-or-slug>.<section>.md`, matching the Keep a Changelog headings
  already used in `[Unreleased]`) instead of every lane hand-editing
  `CHANGELOG.md`'s `[Unreleased]` section directly, which conflicted on
  32 of the last 48 non-merge commits. `scripts/release/collect-changelog.sh`
  (`--dry-run` prints the would-be section) merges fragments into the
  existing `[Unreleased]` text in stable, filename order and deletes the
  fragments it consumes; an unrecognized section name is refused naming
  the accepted set. See `changelog.d/README.md`.
- CLI: `tasks --reconcile` now reports a live Herdr agent whose name
  matches the current project's own lane naming but whose pane has no
  record in that project's routing log — `orphan pane <pane> (agent
  <name>): no task record — close it in Herdr if it is not yours`.
  Report-only: nothing is closed, prompted, or written, and the line is
  printed only when such an agent exists, so every existing scenario's
  output is unaffected.
- CLI: `accounts` now surfaces sweep's own cap/resume escalation evidence
  under a pool with any from the last 7 days — `⚠ N sweep escalation(s)
  since <TIMESTAMP>, latest: <path>` — read-only, printed only when `N > 0`
  so every existing `accounts` listing stays byte-identical. No automatic
  breaker trip; an operator still decides by hand.

### Changed

- Release: `scripts/release/export-fresh.sh` now records the exported
  tree's committed-file status and tree hash right after its one commit,
  and compares both again after the build/test/conformance/scan phase,
  refusing the export (and, through it, `sync-public.sh`) if any of that
  phase modified an already-committed file rather than only producing new,
  disposable build output. The `gitleaks`/`check-private-names.sh` test
  fixtures in `scripts/release/test_export.py` now also log the cwd/
  arguments they were invoked with, so a regression that scans the wrong
  directory or drops `--config` fails the suite instead of passing quietly.
- State: a routing-log append's index update no longer rescans the whole
  log to decide whether its fast path is safe. It now trusts the sidecar
  index file's own identity (size and modification time) instead of
  re-decoding or re-scanning it, so the cost stays flat as history grows
  instead of scaling with the log's current size. Measured on a 250k/500k-
  record fixture: about 601/985 ms per append before this change versus
  about 15/20 ms per append after.
- State: `append_versioned`'s own sequence-number assignment no longer
  scans the whole routing log on every call to find the current high-water
  seq. It now reuses the sidecar index's cached high-water mark through the
  same cheap, identity-validated read, falling back to the full scan only
  when that trust can't be established (a missing/stale index, a torn
  tail). Measured on a 250k/500k-record fixture: about 342/698 ms per
  `append_versioned` call before this change versus about 16/20 ms after.
- CLI: `worktrees <path> --prune` no longer re-reads and re-validates the
  whole routing log, nor re-walks every task directory, once per lane it
  considers. Both the ledger scan behind a missing status file and the
  project-wide retirement-candidate listing behind an unmerged branch's
  archive-authority check are now fetched once per `--prune` run and
  shared across every lane, instead of once per lane. Decisions are
  unchanged; only the repeated work is gone, so the cost no longer grows
  with lanes times history length.
- State: a real (non-dry-run) `--prune`'s removal path no longer re-scans
  the routing ledger once per occupant task to decide whether its outcome
  is already recorded; it now reuses the same ledger records the rest of
  the prune call already fetched once. Recorded outcomes are unchanged.
  Measured on a 16-lane/250k-record fixture: about 34,823 ms per real
  prune before this change versus about 24,656 ms after.
- CI: the required `cargo test` command (`AGENTS.md` and `.github/workflows/ci.yml`'s
  `test` job) now runs with `--no-fail-fast`, so one failing test binary no
  longer silently drops every binary scheduled after it.
- `scripts/test_plugin.py` now discovers and runs `scripts/release/test_collect_changelog.py`,
  so the `CHANGELOG.md` fragment collector is covered by a required check.
- Docs: `docs/migrating-from-python.md` Step 4 now covers the rest of
  `mv-config`'s load-time checks, not just the retired-role-key refusal —
  `kind`/`write_scope` and an explicitly written account `vendor` must be
  in a fixed set (exit code 3; a `pool`-derived vendor is not checked),
  an unrecognized `effort`/`review_effort` is a load-time warning rather
  than a refusal, and `[activity]`/`[mail]` now warn on an unknown key.
  Each gets the exact message shape, the accepted values, and the fix.

### Fixed

- CLI: `backlog show`/`backlog answer` no longer panic if another process
  (a sweep tick, another seat's `done`/`reopen`) mutates the backlog
  between resolving a token to its item id and re-reading the item by that
  id — the two reads used to be independent, and an ordinary concurrent
  write in between could make the second one come up empty, hitting a raw
  `.expect()` panic. Both commands now resolve the token and fetch the
  item from a single read, and the few lines that could still disagree on
  reuse fail with a clean, retryable error instead of panicking.
- `mv-config`: `[activity]` and `[mail]` now warn on an unknown key (a
  typo'd `piars` for `pairs`, say) the same way every other section
  does — they previously had no unknown-key checking at all, so a typo
  loaded silently with that section's pairs left empty and no warning.
  An explicitly written account `vendor` is now validated against the
  same closed set (`claude`, `codex`, `grok`, `chatgpt`, `local`) `kind`
  already was, naming the bad value and suggesting the closest accepted
  one on a mismatch (exit code 3) instead of letting a typo or mis-cased
  value (`vendor = "Grok"`) silently miss every vendor-gated check
  downstream (Critic's same-vendor exclusion, quota gating, `accounts`
  usage detail). `vendor`'s default, derived from `pool`'s prefix, is
  left unchecked, since `pool` is free-form grouping that need not
  resemble a vendor name.
- `mv-engine`/CLI: `multiverse sweep` now discounts the same host-sleep
  total it already subtracts for `expire_awaiting` when it decides whether
  a RUNNING lane's pane looks stuck, is past its `max_runtime_minutes`
  cap, or is still inside its startup grace — `sweep_nudges::run` never
  received `sleep_minutes` before, so a lane the Mac slept through for
  more than `STUCK_AFTER_MINUTES` (30) or its account's runtime cap used
  to get a false `STUCK_RESUME_PROMPT`/`RUNTIME_CAP_PROMPT` on wake, and a
  lane still showing the same connection-loss banner after that false
  resume could be recorded `failed` even though it was healthy and only
  long-asleep. `sweep_nudges::run`/`nudge_idle_lanes_to_finish`/
  `resume_stuck_lanes`/`cap_long_running_lanes`/`stuck_notes`/
  `submit_unsubmitted_composers` all now take `sleep_minutes: i64`,
  subtracted from the raw wall-clock gap and floored at zero before any
  comparison, the same way `expire_awaiting` already floors its own
  lease-vs-sleep subtraction; `commands::sweep` passes its own host-wide
  `hostsleep::report()` total through, and the one other caller
  (`board_data`'s read-only `stuck_notes` display) passes `0`, unchanged
  from before this parameter existed.
- `mv-state`: `prune_quarantine` no longer deletes a `worktree-state/`
  quarantine copy of a lane's `.multiverse` just because it is past the
  retention window — it now rescues the copy into the project's own live
  `.multiverse/` first (refusing only on a genuine byte conflict, which
  keeps the copy instead). This closes the window where `remove_lane`'s
  own rename-into-quarantine, followed by its redirect symlink, is not
  atomic: a crash between those two steps used to leave a copy that had
  never been rescued anywhere, which the next `--prune --deep` could then
  silently and permanently delete once `WORKTREE_STATE_RETENTION` (14
  days) passed.
- CLI/state: `tasks --reconcile` (and the same implicit reconcile a plain
  `tasks` listing already runs) used to silently refuse a `done` status
  file with no usable `result.md` behind it — empty, or never written at
  all — leaving the task `unrecorded` and flagged "need a look" forever,
  with no command able to clear it short of hand-editing the task
  directory, even though the listing's own note claimed `--reconcile`
  would fix it. `is_tasks_reconcile_candidate` now treats that case as a
  candidate too; `reconcile_project` records it with `status=done` (the
  status file's own literal claim, same as every other reconciliation)
  tagged `state=done-without-result` instead of the ordinary `reconciled`,
  so the missing-evidence fact stays visible in `routing.log` while the
  task itself becomes clearable. Sweep's own unattended automatic
  reconcile leg is unchanged and still skips this case outright.
- `mv-board`: the project-name header line (`render`/`render_by_project`)
  is now clipped to the configured width, the same way every card,
  column, and account line already is. A project name longer than the
  terminal, or one made of wide/CJK/emoji characters whose summed cell
  width exceeds it, used to print that one line wider than everything
  else on the board; it now truncates with `"…"` like the rest of the
  renderer.
- `mv-state`: `notify::flush` no longer re-delivers a record it already
  pushed earlier in the same batch. The claim file a flush renames the
  outbox to used to keep every record of the batch on disk, unchanged,
  until the whole replay finished; if a later record's own re-enqueue then
  failed and `flush` returned early, the claim was left behind (as
  designed, for stale-claim recovery) still holding the already-delivered
  record, so a subsequent stale-claim reclaim handed it back to be pushed
  a second time. The claim is now rewritten to drop each record's own copy
  the moment that record is settled (delivered, re-queued into the
  outbox, or dropped as stale/unknown), so only what is still genuinely
  outstanding remains in a claim an error leaves behind.
- CLI: `backlog list` now resolves `.` and relative/absolute project paths
  the same way `dispatch`'s own project argument does, instead of only
  matching a literal `[projects.<name>]` key. `backlog list .` used to
  fail with "unknown project(s): ." even from inside a configured
  project's own directory; it, a relative path, and an absolute path all
  now resolve to that project, falling back to the existing "unknown
  project(s)" message (unchanged text and exit code) only when neither a
  literal name nor a path resolves.
- CLI: `activity nudge` and `tasks`'s own `--projects`/positional project
  arguments now resolve `.` and relative/absolute paths the same way,
  reusing `backlog list`'s own resolver instead of a bare
  `[projects.<name>]` key check. Each used to refuse `.` and paths with
  "no such project: ..."; an unknown name still gets that same message
  and exit code, unchanged.
- `mv-state`: `activity reply` and `activity close` no longer have a
  window where two concurrent calls against the same open ask could both
  succeed. `reply` previously checked whether the ask was still open with
  no lock held at all, so two racing replies could both see it as open and
  both send an answer into the asker's log; `close_ask` locked only its
  own append and never re-checked for an existing close record first, so
  two racing closes (or a close racing a reply) could both append one.
  `reply` now holds both the holder's and the asker's project lock for the
  whole check-then-send-then-close span — acquired in the fleet's
  established hub-first-then-alphabetical order so two projects replying
  to each other's asks at once can't deadlock on opposite lock orders —
  and `close_ask` now re-checks for an existing close record inside its
  own lock before appending. A second call against an already-resolved
  ask now gets the existing "ask not open" refusal instead of landing a
  duplicate answer or close.
- `mv-herdr`: a `HerdrCommand` now carries whether it is a Herdr call
  (`is_herdr`) as a flag set once at construction by every builder,
  instead of `run()` re-deriving it by comparing `argv[0]` against the
  *current* `HERDR_BIN_PATH` resolution. A command built normally and
  then pinned to an explicit binary afterward (`mv-state::worktrees::
  agent_list_snapshot`, used by `HerdrLaneObserver::with_herdr_binary`)
  used to lose the mark the moment that pinned binary didn't match
  `HERDR_BIN_PATH`, which silently dropped the bounded default subprocess
  timeout entirely — a hung Herdr reached this way could block forever
  instead of surfacing a typed timeout error. `accounts`/`board`/
  `backlog`'s own `agent list` probes (`accounts::snapshot_stranded_panes`,
  `backlog::dispatched_caller_name`, `seatstate::live_open_count`,
  `seatstate::live_seat_open_counts`) also no longer inherit the 30s
  Herdr default for what is normally a millisecond-scale listing call:
  each now passes an explicit 10s timeout (the new `mv_herdr::argv::
  LISTING_TIMEOUT_SECONDS`, matching the bound `dispatch.rs`'s
  `live_agent_statuses` already used for the same call).
- CLI: an unresolvable config path (the config-path-resolution step, not
  the file-missing/malformed-content step, which already needed this
  exit code) now exits 3 (`NEEDS_CONFIG`) instead of 1, matching the
  documented exit-code contract and `config migrate`/`review-policy`'s
  existing behavior for the identical failure. Affects every command
  that shares `commands::load_config` (`projects`, `workers`, `inspect`,
  `directives`, `start`, `stop`, `add`, `check`, `lifecycle`, `new`,
  `result`, `backlog`, `activity`, `tasks`, and others), `doctor`/`halt`
  (via `resolve_config_path`), `board` (via `LiveSource::load`), and
  `seat show` (via `show_with_count`). `accounts limit`/`lend`/`prefer`
  against an unknown seat now also exits 3 instead of 1, matching
  `accounts usage --detail`'s identical "name not in config" lookup in
  the same file.
- CLI: `add`/`new --yes` now write `config.toml`'s new `[projects.<name>]`
  block through the same temp-file-plus-rename, fsync-before-rename write
  `mv-config`'s migrator already uses for the live config, instead of a
  plain in-place append. A crash partway through used to be able to leave
  `config.toml` holding a truncated, unparseable block; now the live file
  is always either the untouched original or the fully appended result.
  The file's own permission mode is preserved across the rewrite, and
  a `config.toml` that is a symlink stays a symlink, with the new block
  written into its target.
- CLI/Engine: a dispatched Codex lane now launches with `-C <worktree>`,
  pinning its working root to the lane `prepare` already created instead
  of relying solely on the pane's shell `cd`. Codex has no flag that
  restricts what it can *read* — only this root-pin and an explicit
  "stay inside your worktree" constraint in the dispatched brief, added
  for every Codex-kind seat — so this narrows deliberate re-rooting
  rather than enforcing a read boundary.
- `mv-config`: `[accounts.<name>]`'s `kind` and `[roles.<key>]`'s
  `write_scope` are now validated against the values the rest of the
  codebase actually compares them to (`kind`: `claude`, `codex`, `grok`,
  `local`; `write_scope`: `none`, `declared`) instead of accepting any
  string. A typo'd or mis-cased value used to load cleanly and then
  silently take the wrong branch downstream — `write_scope = "None"`
  skipped the write-deny rules a reader role relies on, and a mis-cased
  `kind` skipped its vendor-specific safety checks — with no warning
  that anything was wrong. Both are now a load-time config error (exit
  code 3) naming the bad value, the accepted set, and a "did you mean"
  suggestion when the value is a close typo; neither is silently
  corrected. **This can reject a config that loaded (silently wrong)
  before.**
- CLI: `council`'s per-seat report no longer drops a councillor that the
  convene-time recheck (a cap or availability check run again under the
  fleet lock immediately before starting) newly skips after it was
  already printed in the roster. That seat now prints with its skip
  reason. As with a seat skipped while seating, the council still
  proceeds when the remaining seats are quorate, and refuses (exit 1)
  only below quorum.
- `worktrees --merge` now refuses outright, before touching anything,
  when the project checkout is already mid-merge — `MERGE_HEAD` present
  or unmerged paths left over from an earlier crashed or conflicted
  `git merge --no-ff`, which nothing previously detected — naming the
  stuck state and a `git` command to inspect it (exit 1). And when its
  own merge attempt conflicts, it now runs `git merge --abort` and
  reports the conflicting paths instead of leaving the checkout
  mid-merge for the next `--merge` to trip over with an opaque
  passthrough error.
- Engine: `resume_stuck_lanes` no longer spends a stuck lane's one-shot
  resume claim when the resume send itself fails (the pane's own agent
  was no longer there to receive it, or the fresh re-check otherwise came
  back false). The claim used to be written to disk before the send was
  attempted and left that way regardless of outcome, so a failed send
  permanently stuck the task with a claim that could never record a
  pane/fingerprint — excluding it forever from both a retried resume and
  the escalate-to-`failed` safety net this mechanism exists to guarantee.
  A failed send now releases the claim so a later pass can retry it;
  a successful send still spends it exactly as before.
- Engine: `cap_long_running_lanes` now has the same escalation follow-up
  `resume_stuck_lanes` already has: a lane still running
  `STUCK_AFTER_MINUTES` (30) past its own `max_runtime_minutes` cap, after
  its one-shot cap prompt was already sent, is now recorded `failed`
  through the same `mv_state::tasks::record_stranded_closure`/breaker-
  evidence pair the stuck-resume escalation uses, instead of getting one
  spent, unconsumed prompt and zero further automated signal ever again.
  A cap-prompt send that never actually goes out (the pane's own agent
  was no longer there to receive it) also no longer spends the one-shot
  claim — it releases the same way the stuck-resume claim does, so a
  later pass can still claim and retry it. Time past the cap plus grace is
  never enough on its own, though: real configs set caps as low as 40
  minutes, and healthy agents routinely run longer, so a lane still
  `Working` escalates only once its pane also shows the same
  connection-loss banner `resume_stuck_lanes`'s own stuck detection
  requires in that state; one no longer `Working` at all (idle/done, no
  terminal status file) needs no further pane-text match. Breaker evidence
  is written, and linked into the recorded outcome, only when that same
  pane text also classifies as a vendor/account error the breaker already
  recognises — never for a plain timeout, which proves nothing about the
  account and must not feed its breaker or risk stopping dispatch to a
  healthy one.
- `mv-state`: `worktrees <path> --prune` (and `--prune-orphans`) no longer
  keep a lane forever when its branch was created but no brief was ever
  written for it — a host dying between `Worktrees::create` returning and
  `write_brief` starting, still under the single project lock. Every
  occupant task's own name is always checked against a terminal status,
  including the lane's own, so this empty lane previously needed `--force`
  with no automatic reclaim path, regardless of age. Once such a lane (no
  brief, no commit past its recorded cut point) is older than the
  existing 14-day retention this module already waits out before
  reclaiming other abandoned state, `prune` now removes the lane and its
  branch on its own; a fresh one, or one with any real work, still needs
  `force` exactly as before.
- `mv-state`/CLI: a status file holding a garbled, unrecognized token (a
  truncated or otherwise corrupted write, e.g. `donedone`) used to read
  exactly like a never-written one — `read_status`'s `None` collapses
  both — so a task whose pane Herdr still reported live under its own
  dispatched agent name was excluded from both `tasks --reconcile`
  (needs a recognized status) and `tasks --close-stranded` (the pane
  still reads as occupied), with nothing able to ever force a terminal
  recording for it. `tasks` now shows such a task as `garbled`, flagged
  and naming the (sanitized) bad token, instead of `running`; and
  `tasks --close-stranded --yes` can now record a `blocked` outcome for
  it (state `unrecognized-status-token`), independent of what the live
  pane reports, since a garbled write is itself the evidence something
  went wrong.
- `mv-config`: `roles.<key>.effort` and `review_policy.review_effort`
  outside `minimal`, `low`, `medium`, `high`, `xhigh`, `max` — the values
  multiverse has seen accepted; the vendor CLI is the authority — now get
  a load-time warning naming the bad value, the `models` kind(s) using
  it, and the known values; the config still loads and the value is
  still passed through verbatim to whichever vendor CLI (`claude`,
  `codex`, `grok`) a seat runs. An empty string remains a load-time error
  (exit code 3). A typo'd value used to load cleanly and only fail later
  as a live seat-launch error; it was briefly a load-time error of its
  own, but the vendors accept different sets this repo cannot state as
  one closed list (Claude Code and codex accept `xhigh`; codex also
  accepts `minimal` but not `max`), so a hard error would have broken a
  working config on a vendor-CLI upgrade.
- `mv-engine`: `sweep`'s nudge/resume/cap pass now discounts each lane's
  own host-sleep overlap individually, instead of computing one flat
  discount per pass from the oldest running candidate and applying it to
  every lane. A lane dispatched after a host sleep had already ended could
  previously have its own, much smaller elapsed time wrongly zeroed out by
  an older lane's whole sleep gap, delaying a legitimate stuck/cap
  detection for it by however long the older lane's sleep lasted.
- `board`: a task whose status file holds an unrecognized token (e.g. a
  truncated `donedone`) now shows as `garbled`, needing a human look,
  instead of plain RUNNING — matching `tasks`'s own `garbled` row. The
  board reads the status file through `read_writer_status_verbose` the
  same way `tasks` already does, instead of the terminal-only reader that
  collapsed a corrupted write to the same `None` a never-written file
  reads as.
- Release: `export-fresh.sh` now refuses to export if `changelog.d/` still
  holds an uncollected fragment, and the publishing checklist's version-bump
  step now runs `collect-changelog.sh` before moving `[Unreleased]`.
- `mv-state`: `finish-nudge-memory.json`, `runtime-cap-memory.json`, and
  `stuck-resume-memory.json` now prune a claim on every read once its task
  has a recorded terminal outcome in the project's routing log, or once the
  claim has sat unrecorded for 14 days (the same retention
  `WORKTREE_STATE_RETENTION` already gives an abandoned worktree-state
  copy), so these claim files no longer grow by one entry per dispatched
  task for a project's lifetime without re-arming a still-running lane's
  one-shot nudge/resume/cap prompt, or dropping a stuck-resume claim's
  recorded pane/fingerprint, the moment that lane runs past a day.
- `mv-engine`/`mv-cli`: `sweep`'s nudge/resume/cap pass no longer mistreats
  a healthy lane the host slept through, and no longer leaves a frozen
  lane with no further signal. The host-sleep discount now accumulates
  across sweep passes (from the durable pending-sleep record, not only
  the pass that first discovered it), so a lane is never capped on the
  pass right after a hostsleep watermark advances past the interval that
  covered it. `resume_stuck_lanes` and `cap_long_running_lanes` now see
  each other's claims within one `sweep` pass, so a lane can get at most
  one canned prompt per pass. A successful resume send now arms the
  escalate-to-`failed` safety net for an idle/done pane the same as it
  already did for a still-`Working` one. A resume or cap send that keeps
  failing (a Herdr outage) now stops retrying after a bounded number of
  consecutive failures and lets the existing escalation path run instead
  of retrying forever.
- Dispatch's at-cap selection reason (distinct from `accounts`'s
  Python-pinned STATE column) now names its clearing lever
  (`accounts limit <seat> <n>` / `accounts limit <seat> --off`), matching
  the adjacent "lent off" reason's own inline convention.
- `accounts usage`'s FENCED suffix for a manually-fenced Grok seat with no
  automatic reset now names the pool and gives the same `accounts reset` fix
  instruction `accounts`'s plain listing already gives for the identical
  case, instead of a generic message that omitted both.
- `tasks`/board: a crash-window lane recorded only as `event=pane` (no
  `event=started` yet) now classifies RUNNING/STALLED like any other live
  pane, instead of a lying AWAITING.
- `tasks --reconcile`'s orphan-pane report now keys on (pane, agent name)
  as the routing log's *current* record, not raw pane-id membership — a
  pane Herdr recycles for a different, never-dispatched agent after the
  one that was recorded for it is now reported as an orphan, and the
  check's own lane-name enumeration now shares its instance budget with
  `naming::unique_agent_name` instead of a hand-copied, truncated range.
- `accounts`'s sweep-escalation evidence count no longer follows a
  symlinked `.multiverse/evidence` directory or a symlinked file inside
  one.
- `sweep`: a runtime-cap escalation's `record_stranded_closure` write (like
  the stuck-resume escalation's before it) now also forces this pass's
  `tasks_snapshot` to re-read the routing log, so a cap escalation's own
  just-recorded failure is never acted on through a stale in-memory snapshot.
- `sweep`'s nudge-eligibility gate (`base_running_candidate` in
  `crates/mv-engine/src/sweep_nudges.rs`) now also refuses a task whose
  status file holds an unrecognized/garbled token, so a garbled-status
  lane is never finish-nudged, stuck-probed, resumed, or runtime-capped —
  it is left for `tasks --close-stranded`'s own human-reviewed reconcile,
  the same way it already was for a readable terminal status file.
- Dispatch: a hard kill between pane creation and the prompt landing no
  longer leaves `close_stranded` free to fabricate a terminal status onto a
  pane Herdr still reports live — the pane id is now appended to the
  routing log (`event=pane`) the instant it exists. A hard kill between
  writing a task's brief and logging its dispatch no longer blocks `prune`
  from reclaiming the lane without `--force` once it is old enough.
- `tasks`/board: a crash-window lane recorded only as `event=pane` (no
  `event=started` yet) now gets the "possibly frozen" stalled note and the
  blocking-prompt probe too, not just the RUNNING/STALLED row state — both
  used to read `started_pane` directly and silently no-op for that lane.
  Every such read in `tasks.rs` and `board_data.rs` now goes through one
  `effective_pane` helper so the fallback cannot drift between call sites
  again.

### Security

- `bootstrap-plugin.sh`'s release-archive install path now clamps the
  extracted `multiverse` binary to mode `0755` instead of only adding
  the execute bit, so a release archive whose packed member carries
  group- or other-write stays writable only by its owner after install.
- Dispatch writes each task's `brief.md` mode 0o600 instead of 0o644,
  matching every other state file. Its directory was already owner-only,
  so this is defense in depth rather than a fix for a reachable leak.
- CLI: `scratch read` now redacts credential-shaped text in the printed
  pane output, after the existing control-sequence sanitizing pass,
  matching every other pane-text-derived consumer (dispatched subprocess
  logs, an agent's own report text). A scratch pane is a live agent
  terminal and can echo a secret.

## [0.1.9] - 2026-11-17

### Fixed

- CLI: `worktrees <path> --prune` now reliably frees a vanished lane's
  shared-build hash dir in the same run that removes it, instead of
  needing several follow-up runs (or sometimes none at all). The sweep
  used to share one whole-root scan budget (2s, 4000 files total) across
  every hash dir it considered, so two big dirs sorted early — the main
  checkout, another live lane — could exhaust it before smaller vanished
  dirs sorted after them were ever read, and a dir the scan never
  finished reading is always kept, never reclaimed. Each hash dir now
  gets its own 500-file cap, decided independently; the whole-run budget
  is wall-clock time only, scaled to the number of hash dirs found, so a
  normal shared root (about a dozen hash dirs, 0.4-4 GB each) is fully
  decided in one pass.
- CLI: `workers`' `workers N/CAP` line now counts the same slots
  `dispatch` enforces. A finished lane whose pane Herdr still lists no
  longer inflates `N`, so `workers` and a `dispatch` refusal at the cap
  report the same number.
- CLI: a worker whose pane is gone no longer holds a fleet slot until
  the next reconcile. When `dispatch` is near its cap, it asks Herdr
  which workers are still live: a task counts only while its agent is
  working or blocked, or while it is still inside its account's startup
  grace. If Herdr can't be asked, or answers without a usable `agents`
  list, every open task still counts, so an unknown never frees a slot.
- Docs: `docs/parity.md` now documents `worktrees --merge`. The install
  and parity docs no longer quote test and scenario counts that went
  stale.
- Tests: the `dockerreport` reaper tests now drive the Linux
  `/proc/<pid>/cwd` lookup through an injectable seam, so they pass on
  Linux CI as well as macOS.
- Tests: added conformance scenarios for `multiverse new` (the `--yes`
  gate, a non-empty unregistered target, an already-registered dry run,
  and a real create and register with a failed start) and for the "no config
  file found" refusal on every command that reports it.

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
[0.1.11]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.11
[0.1.10]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.10
[0.1.9]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.9
[0.1.8]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.8
[0.1.6]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.6
[0.1.5]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.5
[0.1.4]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.4
[0.1.3]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.3
[0.1.2]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.2
[0.1.1]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.1
[0.1.0]: https://github.com/BamBam4275/herdr-multiverse/releases/tag/v0.1.0

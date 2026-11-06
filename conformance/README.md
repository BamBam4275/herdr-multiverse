# Conformance harness

Replays every committed JSON scenario against the Rust CLI built from this
checkout. The frozen Python CLI remains an optional oracle; CI needs only
Rust and Python 3.11+ for the standard-library harness, with no Python
`multiverse` package or oracle checkout.

Each scenario asserts the recorded exit code, normalized output, selected
file contents/modes, and ordered fake Herdr calls. These recorded expectations
live in `scenarios/*.json`; replay never regenerates them from Rust output.
Some output assertions are substrings rather than full snapshots, and tree
assertions cover only the roots/files explicitly declared by the scenario.

## Quick start

Run from the repository root (the scripts also work from other directories):

```sh
# Build this checkout with the pinned toolchain/lockfile and replay all scenarios.
python3 -B conformance/run_all.py

# Build and replay one scenario.
python3 -B conformance/run_scenario.py conformance/scenarios/doctor-missing-accounts.json

# Explicit binary override for debugging (skips the build).
python3 -B conformance/run_all.py --rust-bin target/debug/multiverse

# Optional oracle replay, or both implementations against the recorded assertions.
python3 -B conformance/run_all.py --impl python --python-repo <frozen-checkout>
python3 -B conformance/run_all.py --impl both --python-repo <frozen-checkout>

# Check that drift, filesystem changes, and transcript errors are detected.
python3 -B -m unittest discover -s conformance -p 'test_*.py'
```

The default build is `cargo build -p mv-cli --locked`. It runs from this
repository's root, uses `rust-toolchain.toml`, and discovers the executable
from Cargo's artifact output (including a custom `CARGO_TARGET_DIR`). Relative
`--rust-bin` paths resolve from the invoking directory before entering a sandbox.
Only `--impl python` or `--impl both` requires `--python-repo` (or
`MV_CONFORMANCE_PYTHON_REPO`). No oracle is read by default.

## Matrix and exit status

`run_all.py` prints a matrix row for every scenario step, including the
recorded command arguments, implementation, step number, and PASS/FAIL.
Dependent steps whose capture was not produced are explicitly
`NOT RUN (missing capture)`; their scenario fails. Independent commands still
run after earlier failures. Each scenario also has a PASS/FAIL summary that
includes its final tree assertions, followed by implementation totals.
A command may pass while its scenario fails a final tree assertion.

Exit 0 means all selected implementations pass; exit 1 means a recorded
assertion failed; exit 2 means setup/build failed. Rust failures always count,
including current `not implemented` handlers. There is no expected-failure
allowlist, skipped Rust side, or `continue-on-error` CI gate.

`.github/workflows/conformance.yml` runs harness regression checks and the
Rust replay on pushes to main, pull requests, and manual dispatch. Until the
CLI implementation reaches parity this workflow is expected to fail, with
the complete matrix in its log. The Python oracle is never invoked in CI.

## Scenario format

One JSON file per behavior under `scenarios/`. Shape:

```jsonc
{
  "id": "scenario-name",
  "description": "why this scenario exists and what it pins down",
  "dirs": {                       // optional: extra directories (e.g. a fake CODEX_HOME)
    "codex-home": {               // mode is decimal (448 == 0o700); omit for default
      "mode": 448,
      "files": { "sessions/2026/09/27/rollout-1.jsonl": "...\n" }  // optional, same shape as a project's "files"
    }
  },
  "projects": {                   // each becomes its own project root
    "sandbox": {
      "git": true,                // git init + initial commit, so the CLI sees a real repo
      "files": { "README.md": "hi\n" },
      "lanes": {                  // optional: a second git branch for review-policy --lane etc.
        "20261101-fixer-abcdef": {
          "files": { "src/auth/login.rs": "changed\n" },  // written + committed on multiverse/<task_id>
          "merge": false           // true: --no-ff merge into the original branch, then delete the lane branch
        }
      }
    }
  },
  "home": {
    "config_toml": "...toml text with {{PROJECT:sandbox}} / {{DIR:codex-home}} tokens..."
  },
  "steps": [
    {
      "cwd": "sandbox",            // which project this invocation runs from
      "argv": ["doctor"],          // argv after `multiverse` itself
      "env": {},                   // extra env vars, same token syntax as config_toml
      "transcript": [              // ordered fake-herdr calls this step must make — no more, no fewer
        { "match_argv": ["--version"], "reply": { "exit": 0, "stdout": "herdr 0.9.3\n" } }
      ],
      "capture": {                 // optional: pull a value out of this step's raw stdout
        "item_id": "added (\\S+) to sandbox"   // group 1, available to later steps as {{capture:item_id}}
      },
      "expect": {
        "exit": 1,
        "stdout": "...exact normalized text...",       // exact match, OR:
        "stdout_contains": ["substring one", "substring two"],  // substring checks (also normalized)
        "stderr": "..."
      }
    }
  ],
  "expect_tree": [
    { "root": "sandbox", "unchanged": true },           // byte-identical file set before step 0 / after the last step
    {
      "root": "sandbox2",
      "files": {
        ".multiverse/routing.log": { "regex": "^<TIMESTAMP>\\t...$", "mode": 384 },
        ".multiverse/some-file":   { "equals": "exact normalized content" },
        ".multiverse/should-not-exist": { "absent": true }
      }
    }
  ]
}
```

Template tokens, substituted by `sandbox.py` before a value reaches the
implementation under test:

| Token | Resolves to |
| --- | --- |
| `{{HOME}}` | the sandbox's fake `$HOME` |
| `{{STATE_DIR}}` | the fake fleet config/state dir (`$HOME/.config/multiverse`, containing the config) |
| `{{PROJECT:name}}` | that project's root, as declared under `"projects"` |
| `{{RUNTIME_DIR}}` | isolated fleet runtime directory |
| `{{DIR:key}}` | that extra directory, as declared under `"dirs"` |
| `{{capture:name}}` | a value captured from an earlier step's raw stdout |

`expect`/`expect_tree` values are written in **already-normalized** form —
the same placeholders `normalize.py` produces (`<ID>`, `<TIMESTAMP>`,
`<PROJECT:name>`, `<SANDBOX_ROOT>`, `<CRC32>`, `<PID>`, `<PYVER>`), not the
`{{...}}` template syntax. The two syntaxes point opposite directions: `{{...}}`
goes into the sandbox before the run, `<...>` comes out of the actual output
after it.

### Multi-step scenarios and the capture mechanism

Some behaviors are two commands, not one — `backlog add` mints an id you can
only get by reading its stdout, then `backlog show` needs that id as an
argument. A step's `"capture"` block runs a regex against that step's *raw*
(pre-normalization) stdout and stores the first group under the given name;
a later step's `argv`/`env`/`transcript` can reference it as
`{{capture:name}}`. If a capture was never produced, commands that depend on it are reported
as not run. Later independent commands still execute.

### Lane fixtures

A project's `"lanes"` map (`sandbox.py`'s `_add_lane_branch`) gives
`worktrees.lane_diff` (`review-policy --lane`, and anything else that reads
it) a second branch to diff against, beyond the project's single initial
commit every other scenario here uses. For each `task_id -> {files, merge}`
entry, after the project's initial commit: `refs/multiverse/base/<task_id>`
is recorded at that commit (the cut point `Worktrees.create` would have
written), a `multiverse/<task_id>` branch is cut from it, `files` is written
and committed there, and the sandbox returns to the project's original
branch. `"merge": true` additionally `--no-ff` merges the lane branch back
in (commit subject `Merge branch 'multiverse/<task_id>'`, matching git's own
default message) and deletes the lane branch — simulating a lane
`--prune` already reclaimed, so `lane_diff` must recover the tip from the
merge commit's second parent rather than the branch itself. This is plain
`git` plumbing, independent of either CLI implementation, so it drives the
Python oracle and the Rust binary identically.

### The fake Herdr transcript

`fake_herdr.py` is a tiny scripted stand-in invoked via `HERDR_BIN_PATH`,
exactly where Herdr itself would be invoked (`herdr.herdr_binary()`,
`herdr.py:797-817`). It is not a stateful fake Herdr server — it is a strict
ordered-transcript player: each call must match the next
`{"match_argv": [...]}` entry exactly, in order, and every scripted entry
must be consumed by the end of the step or the scenario fails
("transcript not exhausted"). An unscripted call, or a call out of order,
fails loudly (exit 98/99) and persists the error for the runner to check,
even when the CLI handles the error. Empty transcripts reject all calls.
This is spec-tests-harness.md item 5's "strict exhaustion" requirement, and
it is deliberately narrower than a general Herdr simulator: building one of
those (stateful pane/agent lifecycle, matching `tests/test_runner.py`'s
`FakeHerdr`) is L5's job in the Rust repo (`crates/mv-herdr/src/bin/fake-herdr.rs`),
not this lane's. A scenario that needs a live pane/agent — one that reads
back what a prior Herdr call wrote, rather than a fixed ordered reply —
declares `"herdr_seed"` instead (see below) and runs against that real
binary; anything else (`dispatch --yes`, `start`'s full lifecycle) still
needs additional scenarios and integration.

### Seeding a live pane/agent (`herdr_seed`)

A scenario that declares a top-level `"herdr_seed"` list opts the whole
scenario out of the strict transcript player and into the real, compiled
`crates/mv-herdr/src/bin/fake-herdr.rs` binary instead — the stateful fake
that already has a pane/agent/workspace lifecycle (`workspace create`,
`pane split`/`rename`/`close`, `agent start`/`prompt`/`get`, `script
queue-text`, ...). `herdr_seed` is a list of argv lists, each one a direct
call against that binary (reusing its own scripting hooks rather than
hand-authoring JSON envelopes), run once before the first step to build up
initial state — a workspace with an idle agent for `activity nudge` to
find, a pane with a label for `scratch close`/`read` to drive. State
persists in one `MV_FAKE_HERDR_STATE` file for every step in the scenario,
the same env var `docs/configuration.md` documents for this binary
elsewhere; a step's own `"transcript"` is not supported once `herdr_seed`
is present — there is nothing to script, the live state answers for
itself. Pane/workspace ids this binary assigns are deterministic from a
fresh state file (`next_pane`/`next_workspace` both start at 100, and the
one pre-seeded default workspace is `w52` — see
`crates/mv-herdr/src/bin/fake-herdr.rs`'s `State::default`), so a scenario
can simply hardcode the ids its seed calls will produce rather than
capturing them. Only `--impl rust` can run one of these scenarios — there
is no stateful fake Herdr standing in for the Python oracle.
A step may also set `"cwd_subdir"` (a path relative to its project) when
the command under test needs to run from a specific nested directory
(e.g. `scratch`'s own dispatched-lane check), rather than the project
root `"cwd"` otherwise always resolves to.

## Normalization rules

`normalize.py` is the single place these live; keep this list in sync with
it rather than re-deriving it by reading output diffs cold.

1. **Sandbox paths.** Every concrete path handed out by `sandbox.py`
   (`$HOME`, the state dir, each project, each extra dir, and the sandbox
   root itself as a catch-all) is folded to its placeholder, longest path
   first. Both the literal `mkdtemp()` path and its `os.path.realpath()`
   form are registered, because macOS aliases `/tmp` → `/private/tmp` and
   `/var/folders/...` → `/private/var/folders/...` depending on which
   syscall produced the string.
2. **Task and backlog ids** (`\d{8}-<slug>-[0-9a-f]{6}`) → `<ID>`. Dispatch
   task ids are content-hashed (`dispatch.task_id_for`) from the project
   path and today's UTC date, so they are NOT stable day-to-day even with an
   identical scenario; backlog item ids are `secrets.token_hex(3)`, not
   stable at all. Both must be treated as opaque.
3. **ISO-8601 timestamps** → `<TIMESTAMP>`. There is no clock-injection env
   var in this Python version (confirmed by grep — no `MULTIVERSE_NOW` or
   equivalent exists); every `isoformat(timespec="seconds")` value is real
   wall-clock time.
4. **`crc32=XXXXXXXX`** → `crc32=<CRC32>`. A function of the exact bytes
   before it on the line, which already contain a normalized id and
   timestamp, so the hex digits themselves are never stable either.
5. **`pid=NNNN`** → `pid=<PID>` (no scenario here emits one yet; included
   defensively for routing-log lines that do, per `spec-on-disk-contract.md`).
6. **`Python X.Y (requires 3.11+)`** → `Python <PYVER>`. `doctor`'s first
   line names whatever interpreter actually ran it
   (`sys.version_info`) — a fact about the host running the harness, not
   about the CLI under test.
7. **The pmset idle-sleep warning line** is dropped entirely, not
   normalized in place. `cmd_check` calls the real `pmset` binary
   unconditionally (`cli.py` `idle_sleep_warning`); pytest stubs this via
   `tests/conftest.py`'s `no_real_pmset_calls` monkeypatch, which only
   reaches inside the Python process and has no effect on an external
   subprocess. The warning line's presence and wording depend on the host's
   own power settings, which this harness has no way to control from
   outside — document the gap rather than pretend it is covered.

### A normalization gap worth knowing about

Long lines that embed a path (seen in `dispatch --dry-run`'s `PROMPT`
section, which wraps for display) get hyphenated and split across multiple
printed lines by the CLI's own pretty-printer, mid-path. Plain substring
replacement cannot repair a path that has had a newline and a hyphen spliced
into the middle of it. None of this harness's scenarios assert against
that wrapped text for exactly this reason — assert against the unwrapped
`WOULD LOG` / `WOULD CREATE` / `WOULD RUN` sections instead, which are not
wrapped. A future scenario that needs the `PROMPT` section's content should
normalize *before* the CLI wraps it (not attempted here), or assert a
softer property (e.g. "the brief's `## Goal` line is present") instead of
an exact path match.

### A second one: elapsed-time display is not normalized, and should not be asserted exactly

`board`'s cards and `workers`' AGE column render *elapsed time since a fixed
past timestamp* (`"old-seat 59930h, ..."`, `"Dec 31 2019 · 6y"`), computed
against the real wall clock exactly like the timestamps `normalize.py` rule
3 already calls out — but unlike a plain `isoformat` timestamp, there is no
regex that recognizes "a number of hours/days/years since some fixed point"
as a single pattern, so it is not normalized at all today. A scenario that
seeds a routing-log record with a fixed-past timestamp (as the `history-*`
scenarios here do, to get deterministic old-role-name history) and then
asserts the *exact* rendered line including that elapsed count will pass
the moment it's written and fail later the same day, or the next time the
clock ticks an hour — found live while building
`history-board-reads-unrecorded-old-role`, which briefly asserted an exact
`59930h` that had already become `59931h` an hour later in the same
`run_all.py` pass. Assert with `stdout_contains` on the stable words around
the number (`"old-seat "`, `"unrecorded,"`) instead of an exact match that
includes it, the same way the PROMPT-wrapping gap above is worked around
rather than fixed.

## Isolation: what's curated, not inherited

`run_scenario.py` builds a **fresh environment dict from scratch** for every
implementation invocation — `subprocess.run(..., env=env)` with an
explicitly constructed `env`, never `os.environ` passed through. This is
load-bearing, not incidental: while building the `activity send`/`inbox`
scenario, running the Python CLI with the ambient shell environment inherited
(as an ad hoc manual test, before this harness existed) surfaced that
`MULTIVERSE_TASK_ID` was already set — to this very conformance task's own
dispatched id — because that's how this Fixer task itself was invoked. The
CLI used it to decide `inbox` was running as a dispatched specialist, not an
orchestrator, and refused to advance the mail cursor. That is correct
*production* behavior, but it would have made the recorded scenario's
expected output depend on which pane happened to invoke this harness —
exactly the kind of contamination the brief's "never touch the live fleet"
rule exists to prevent. The curated base env is: `PATH` (sandbox `bin` first, then
`/usr/bin:/bin:/usr/sbin:/sbin`; bare `herdr` also reaches the fake), `HOME`, `MULTIVERSE_CONFIG`,
`MULTIVERSE_STATE_DIR`, `HERDR_PLUGIN_CONFIG_DIR`, `HERDR_PLUGIN_STATE_DIR`,
`MULTIVERSE_ALLOW_TEMP_PROJECTS=1`, `HERDR_BIN_PATH`,
the two `MV_CONFORMANCE_*` variables `fake_herdr.py` reads, plus whatever a
step's own `"env"` block adds. Nothing else survives from the shell that
launched the harness. Config lives under `HERDR_PLUGIN_CONFIG_DIR` (the legacy
`{{STATE_DIR}}`/`<STATE_DIR>` token); runtime state has a separate directory
under `HERDR_PLUGIN_STATE_DIR` (`{{RUNTIME_DIR}}`/`<RUNTIME_DIR>`).
`MULTIVERSE_CONFIG` and `MULTIVERSE_STATE_DIR` point to those same isolated
locations for legacy compatibility. PATH, HOME, these locations, the Herdr executable,
and transcript paths cannot be overridden by a scenario step.

## Role rename: what gets translated at replay time

Multiverse's nine roles were renamed with no aliases on new input
(`orchestrator->nexus`, `explorer->scout`, `fixer->builder`,
`librarian->archivist`, `reviewer->critic`, `oracle->seer`,
`designer->artisan`, `observer->watcher`; `council` is unchanged). Every
scenario here was recorded from the frozen Python oracle, before the rename,
and this lane has not re-recorded or relaxed any of them (see "Status"
below) — so their `config_toml` and `argv` are authored with the *old*
names, exactly as Python produced them. `conformance/role_translate.py`
(used only by `run_scenario.py`, only when `impl == "rust"`) bridges that
gap by rewriting specific, closed-vocabulary input positions old->new in
memory, right before a scenario reaches Rust:

- **`[roles.<name>]` config tables, and `<name> = [...]` chain entries**
  under a `*.chains` table, are rewritten old->new
  (`translate_config_toml`). Rust's config loader refuses a retired key
  outright ("is a retired role key; run the config migrator..."), with no
  in-process migration of its own in most commands — `dispatch.rs` is the
  one exception, calling `mv_config::migrate::migrate_role_keys` itself for
  reasons specific to that file (see its module doc); translating the TOML
  text before Rust ever sees it works whether or not the command being
  exercised also happens to migrate on its own.
- **Every CLI `--role <name>` flag**, on any subcommand that has one
  (`dispatch`, `backlog add`, `backlog ask`, and any future one), is
  rewritten old->new the same way (`translate_role_argv`), because every
  one of these is a write surface and Rust's write surfaces accept only
  the nine current names with no legacy fallback at all ("no aliases on
  new input") — `backlog add`/`ask --role` included: the frozen Python
  oracle validates that field as free text (only the identifier charset,
  never a closed role enum), but Rust's `--role` rejects a retired name
  outright there too, so the same old->new rewrite applies regardless of
  which subcommand the flag is attached to. `backlog-ask-answer.json`
  pins an old name as its recorded input on purpose, to exercise exactly
  this translation; it is not proof that Rust accepts an old name
  untranslated, only that this harness bridges Python's free-text
  acceptance of one to Rust's new-names-only one.
- **Role words embedded in a brief that a live dispatch would write** are
  the third input surface the rename touches in principle, but no committed
  scenario currently exercises it (`dispatch --yes` and anything that
  writes a real brief are out of scope for this harness today — see "What
  these scenarios intentionally do not cover"). Translate those the same
  old->new way, the day a scenario needs it.

**Recorded `expect` text (stdout/stderr) is translated old->new only at the
handful of fixed positions a *fresh* role-word rendering comes from, never
in bulk.** Release policy (operator decision, task `20261103-fixer-fa9c70`):
no aliases, and old names are read from on-disk history and translated,
never written or shown in new output — so `dispatch.rs`'s `--dry-run` card
and a real dispatch now render the current name on every surface
(`Role::display_name()`/`Role::config_key()`), the same as `workers.rs`
always has. This used to differ: `dispatch.rs` deliberately rendered the
retired, capitalized name (`legacy_display_name`, now removed) so its
card/refusal text stayed byte-identical to what Python actually produced.
Rather than re-recording those scenarios' `expect` text by hand,
`translate_expect_text` (`role_translate.py`, used only by `run_scenario.py`
when `impl == "rust"`) rewrites the old word at exactly the few literal
anchors `dispatch.rs`'s own renderer produces it from — the card's
`role        <name>` line, the `WOULD LOG` preview's `role=<key>` field, and
`render_chain_exhausted`'s "every account in the `<name>` chain for" /
"`<name>`'s fallback policy is degrade_once" refusal text — never a blanket
word-boundary rewrite of the whole `expect` block, because that would also
rewrite a `history-*` scenario's *intentional* old-word text (see below).
When a scenario's committed `expect` text is simply wrong for what its
command now renders, and the mismatch isn't one of those few fixed
positions — as `workers-herdr-shape-fidelity.json`'s was, pinning the
retired `explorer` in its `ROLE` column before this task corrected it to
`scout` — fix that scenario's `expect` text by hand, once, matching
whatever the command's own rendering contract actually is; do not add
another generic replay-time transform for it. `add-dry-run-prints-block.json`
and `add-appends-chain-block.json` needed the same by-hand correction: the
retired key they pinned in a `[projects.*.chains]` suggestion was not just
stale display text, it was a config/write surface — the block `add` prints
is exactly what it (or a following real `add`) appends to `config.toml`,
and a retired key there made the very next `multiverse check` refuse the
file `add` had just written.

**On-disk history fixtures are never translated, by either mechanism
above.** A routing-log line, a task id, or a pre-seeded `brief.md` /
`result.md` / `status` file under a scenario's `projects.*.files` — and
everything under `conformance/history/**` — represents data Rust must read
exactly as written, old role names included, because production history
readers must keep accepting those names forever. The `history-*` scenario
family exists specifically to pin this: their `expect` text intentionally
shows the *old* role word (`fixer`, `oracle`, `reviewer`) because that is
what the on-disk record says and there is no live `[roles.*]` config table
to resolve it through — nothing here, and nothing a future scenario adds,
should rewrite that.

## Status

30 recorded scenarios are committed. Their expectations were recorded by the
original harness lane against the frozen Python oracle; this lane has not
re-recorded or relaxed them. They include old role names on config/input
surfaces as well as historical logs. Those input expectations describe the
pre-rename oracle and may need a separately reviewed refresh for the new-role
contract; history readers must continue accepting old on-disk names. Config
and every CLI `--role` input (`dispatch`, `backlog add`, `backlog ask`) is
now bridged old->new at replay time for Rust, and so is the handful of
fixed positions `dispatch.rs`'s own card/refusal renderer produces a role
word from (see "Role rename: what gets translated at replay time" above);
recorded `expect` text is otherwise refreshed by hand only where a specific
command's rendering contract requires the current name, as
`workers-herdr-shape-fidelity.json`'s now is.

The current Rust CLI routes commands to `not implemented` handlers. Replay
reports those as failures, together with parsing/role-validation differences
and dependent steps that cannot run. Run the default command for the current
matrix; the task result records the matrix observed when this wiring landed.

The first five (`doctor-missing-accounts`, `check-missing-accounts`,
`dispatch-dry-run-explorer`, `backlog-add-show`, `activity-send-inbox`) are
Wave 1's original set, documented above. The other 25, added to cover the
remaining `spec-tests-harness.md` "thirty highest-value scenarios" families
(and, per this lane's own brief, scenarios specifically exercising history
written with OLD pre-rename dispatch role names), are:

| Scenario | Families it covers |
| --- | --- |
| `history-tasks-reads-old-role-names` | #19 task reconciliation, old-role history |
| `history-board-reads-unrecorded-old-role` | #19/#22, old-role history |
| `history-result-reads-old-role-task` | #19, old-role history |
| `history-result-status-tolerance` | #19 (status-file tolerance) |
| `history-mixed-old-and-new-role-names` | old-role + new-role history coexisting |
| `backlog-ask-answer` | #18 asked backlog |
| `board-asked-backlog-renders-as-blocked` | #18 (board/tasks treatment) |
| `backlog-reopen-reprioritize` | #17 (round trip, done/open filters) |
| `dispatch-fallback-unavailable` | #3 fallback |
| `dispatch-chain-exhausted` | #3 exhaustion |
| `dispatch-breaker-open` | #6 breaker state |
| `dispatch-vendor-exclusion` | #4 vendor exclusion |
| `dispatch-degrade-once-budget` | #4 degrade_once budget |
| `dispatch-no-wrapper-refusal` | #8 no-wrapper refusal |
| `dispatch-lent-off-fallback` | #1/#3 (seatstate `lend` toggle) |
| `dispatch-capacity-cap` | #5 capacity cap |
| `dispatch-path-outside-project` | #2 path normalization (refusal) |
| `dispatch-path-resolves-nested-project` | #2 path normalization (resolves) |
| `activity-broadcast-partial-fanout` | #15 activity fan-out |
| `activity-inbox-ack-advances-cursor` | #16 mail ack |
| `accounts-list-trip-reset` | #27 accounts CLI |
| `seat-show` | #27 seat CLI |
| `accounts-usage-codex-fixture` | #29 usage/rate-limit fixtures |
| `workers-herdr-shape-fidelity` | #11 herdr shape fidelity |
| `workers-malformed-herdr-json` | #10 herdr protocol matrix (malformed-reply slice) |

Card `20261103-b-64e792`: three more previously-uncovered leaves needed a
live fake-herdr pane/agent, not just an ordered call/reply transcript —
`activity nudge` (`mv_engine::fleet_start`/`runner::deliver_prompt_to`) and
`scratch close`/`read` (drive a real pane through `mv-herdr`). Rather than
hand-authoring the JSON envelopes a transcript step would need, these three
seed the real stateful `fake-herdr` binary via `"herdr_seed"` (see "Seeding
a live pane/agent" above):

| Scenario | Families it covers |
| --- | --- |
| `activity-nudge-reaches-live-orchestrator` | activity nudge, live orchestrator pane |
| `scratch-close-drives-a-live-pane` | scratch close, live pane label/close |
| `scratch-read-drives-a-live-pane` | scratch read, live pane text |

All three passed against the current Rust CLI on first replay — no
divergence from the frozen oracle's behavior was found for any of the
three commands this pass.

## What these scenarios intentionally do not cover

- **`dispatch --yes` and anything that drives a real pane** (families #9
  concurrent dispatch cap, #12 start lifecycle, #13 stop safety). Needs a
  stateful fake Herdr (pane split/agent start/agent prompt/agent read),
  which is L5's `crates/mv-herdr/src/bin/fake-herdr.rs` — this harness's
  `fake_herdr.py` is a strict transcript player, not that; `"herdr_seed"`
  (see above) now gives individual scenarios that same binary, but
  `dispatch --yes`'s own full lifecycle (worktree creation, the live
  dispatch loop) is still out of scope here. #9 is
  approximated instead by `dispatch-capacity-cap`, which exercises the same
  cap-enforcement code path (`accounts.py`'s `_availability` check 5) via a
  pre-set `seat-state.json` cap and an unreachable herdr reply, rather than
  two genuinely concurrent dispatch processes.
- **#20 blocked-prompt classification** (a captured Codex/Claude/Grok
  dialog tripping the right pool without answering a human-owned prompt).
  `limits.classify` needs a real pane-text transcript as input; synthesizing
  one by hand risked pinning a made-up fixture rather than the real
  classifier's actual boundaries, and the existing `tests/fixtures/` dialog
  captures were judged out of scope to import into this lane without a
  closer look at their licensing/redaction status.
- **#21 concurrent reconciliation** (two reconcilers racing on one task).
  Needs genuine process concurrency against a locked routing log; this
  harness runs one implementation process per step, sequentially.
- **#23 worktree *creation*, #25 archive transaction, #26 late-write
  quarantine.** `worktrees.create`/`archive_branch` run inside the real
  dispatch flow (`cli.py` around the `worktrees.create` call near dispatch)
  and, for archival, drive `git update-ref --stdin` through a herdr-adjacent
  flow with its own occupant-liveness bookkeeping
  (`cmd_worktrees`'s `live_guard_reason`/`live_agents_once` — ask Herdr only
  when a lane's occupant is genuinely unsettled, not on every prunable
  lane). Hand-building a worktree lane with raw `git worktree add` to skip
  the dispatch path was considered and rejected on reading `cmd_worktrees`'s
  own docstring and `live_guard_reason` — not tried and found broken, but
  rejected before a scenario was written for it: it would reproduce the git
  state but not the occupant-tracking metadata `cmd_worktrees` actually keys
  its decisions on, which risks pinning behavior this harness cannot tell
  apart from a lane `worktrees.create` itself would never produce. Left for
  a scenario built through a real (not dry-run) dispatch, once `dispatch
  --yes` is itself in scope (see the stateful-Herdr point above).
- **#30 read-only plugin/report boundary.** Not investigated this pass —
  the manifest/report action tests/test_plugin_manifest.py and
  tests/test_read_only_plugin.py exercise looked like it may be a Herdr
  plugin action rather than a `multiverse` CLI subcommand, which would put
  it outside what `run_scenario.py`'s `python -m multiverse.cli` harness can
  invoke directly; confirming that one way or the other needs a closer read
  this lane didn't reach.
- **Exact byte-for-byte dispatch output.** `dispatch-dry-run-explorer` (and
  every other dispatch-card scenario here) uses `stdout_contains` on the
  stable sections rather than an exact match on the entire ~70-line card,
  for the PROMPT-wrapping reason above.
- **File mode assertions beyond the two checked** (`routing.log` at
  `0o600`). `spec-on-disk-contract.md` documents `0o700` dirs and `0o600`
  files broadly; only what each scenario's own files need is asserted here.
- **A config with real accounts that pass every `doctor` check** ("All
  checks passed", exit 0) — still true of `doctor-missing-accounts` as
  originally noted; not revisited this pass.
- **Herdr protocol matrix beyond a malformed JSON reply** (#10: a genuine
  timeout, a plain-text reply where JSON was expected, pane run/read
  specifically). `workers-malformed-herdr-json` covers the one slice
  reachable without a stateful fake Herdr; the rest needs `dispatch --yes`
  machinery too.

Project fixtures may also declare `"worktrees": ["<task-id>"]` alongside
`"git": true`. Each entry creates a real registered worktree at
`.multiverse/worktrees/<task-id>` on `multiverse/<task-id>` at the initial
commit, so lifecycle scenarios can check removal and branch reclamation.
Include `.multiverse/` in the fixture's `.gitignore` to keep its state untracked.

`"orphan_branches": ["<task-id>"]` creates branches at the initial commit
without a checkout, for orphan-sweep scenarios. The existing `lanes` map
can provide an orphan with unique commits.

The `worktrees` fixture also accepts a map of task ids to `{"files": {...}}`.
Those files are written inside the registered lane after checkout, allowing
ignored-data and dirty-checkout cases without touching a host repository.

`"archived_lanes": {"<task-id>": {"unrelated_root": true}}` records an
archive ref at HEAD and recreates that lane's branch at an unrelated root.
Omit `unrelated_root` to seed only the archive ref. Restore scenarios use
this to verify diagnostics without overwriting either saved history.

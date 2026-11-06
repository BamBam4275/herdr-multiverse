# Herdr captures and fake CLI

The four `*-list.json` files are sanitized live captures from herdr 0.9.3.
Project/account labels and paths use generic examples; terminal and session
identifiers use synthetic values consistently across related records.
Field names, omissions, and value types retain the captured shape.
`mv_herdr::shapes` embeds them, clones their records, and checks field names
against the union of measured fields for each surface. Workspace and tab
records cannot have `cwd`. Pane and agent `cwd` overrides also set
`foreground_cwd` unless an explicit foreground override was supplied.
Envelope construction preserves optional fields, deliberate omissions, and
unnamed pre-rename agents.

## Second capture: herdr 0.9.5

The four `*-list-0.9.5.json` files, plus `version-0.9.5.txt` and
`plugin-help-0.9.5.txt`, are sanitized captures of the same read-only
surfaces from the locally installed herdr 0.9.5 (a hotfix release over
0.9.4; see upstream release notes for that series' behavioral changes).
They are not embedded by `mv_herdr::shapes`'s production code — only by
the `shapes::tests::second_capture*` tests, which load them directly and
compare field sets against the committed 0.9.3 captures.

The only field-set change observed between 0.9.3 and 0.9.5: pane and agent
records gained a nested `agent_session` object (`agent`, `kind`, `source`,
`value`). Workspace and tab records are unchanged. The 0.9.5 sample has no
`done` agent, so `agent.completion_seq` does not appear in it; that is a
property of the sampled data, not a field removed in 0.9.5 — see the test
docstring in `crates/mv-herdr/src/shapes.rs` for the same caveat in code.
`--version` now reports `herdr 0.9.5`. No 0.9.3 `plugin --help` capture was
committed to compare against; `plugin-help-0.9.5.txt` is captured for the
record, not for a diff.

Build and test with the workspace's Cargo tooling:

```sh
cargo build -p mv-herdr --bin fake-herdr
cargo test -p mv-herdr
```

Set `HERDR_BIN_PATH` to the built `target/debug/fake-herdr` executable before
running any client. Set `MV_FAKE_HERDR_STATE` to a disposable JSON file in
the scenario sandbox. The parent directory must exist. A missing state file
starts with the four captures; subsequent mutations persist there using a
temporary file and rename. Queries never create or rewrite the state file.
Without this variable, capture queries work and mutations fail closed.
Use a separate state file for each scenario, with sequential calls; this
fake does not coordinate concurrent writers.

Supported commands:

- `workspace list`, `pane list`, `agent list`, `tab list`; pane and tab lists
  accept `--workspace`.
- `pane split [--pane ID | --current] --direction right|down --cwd PATH`
  with repeated `--env` and optional `--focus` or `--no-focus`.
- `pane get ID`, `pane rename ID LABEL`, `pane close ID`.
- `agent start NAME --kind KIND --pane ID [--timeout MS] -- NATIVE_ARGS`.
- `agent rename TARGET NAME`, including command-routed pane targets.
- `agent get NAME`, `agent prompt NAME TEXT [--wait] [--timeout MS]`.
- `agent read NAME` and `pane read ID`, with `--source` and `--lines`.
- `pane run ID SCRIPT` acknowledges the command without executing it; a
  seat-command wrapper script (anything after the script's `&& ` besides
  `unset ...`) auto-classifies the pane with an unnamed, `unknown`-status
  agent entry, the same way real Herdr detects a seat command before it is
  renamed.
- `plugin config-dir ID` returns a synthetic config-dir path for the named
  plugin id; state-free and read-only, the one `plugin` subcommand
  `doctor::run`'s herdr-reachable check shells out to. No other `plugin`
  action is implemented.
- `--version` returns the capture version, `herdr 0.9.3`.

Lists preserve the captured JSON envelopes; split/start/get/prompt replies
use `result.pane` or `result.agent` and a corresponding result type.
The fake allocates unique pane IDs, starts agents idle, and completes prompts
immediately with advancing `state_change_seq` and `completion_seq`. Closing
a pane removes its agent and read text and updates pane counts. Read replies
are plain text. Failures return exit 1 with a JSON error on stderr.

A prompt naming an existing `brief.md` in backticks (including paths with
spaces), or as a whitespace-delimited path, writes a nonempty `result.md`
and `done` status next to that brief. This is a deterministic completion
shortcut for lifecycle tests, not a vendor simulation. Missing briefs do
not produce task artifacts. The fake never contacts Herdr, launches vendor
commands, or executes pane scripts.

The existing `conformance/` runner uses its own strict transcript executable.
This stateful binary supplies the lifecycle seam for scenarios that need it;
selecting it in that runner is owned by the conformance lane. Live Herdr
drift, UI timing, concurrent fake mutations, and provider behavior remain
outside the evidence supplied by these tests.

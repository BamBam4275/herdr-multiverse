# Sandbox / permission-to-act dialogs

Two shapes, one per vendor, ported from the frozen Python reference
(`multiverse/tests/fixtures/sandbox-dialog/`) for cards `20261102-b-bdf745`
and `20261102-b-9ef1da` (task `20261103-fixer-08598b`). **Sanitised, not
byte-for-byte**: the reference captures quote a real project's name and a
machine-specific absolute path in the tool-call lines that precede the
option list, which this repository's own conventions forbid committing.
Those lines were replaced with a generic command and a generic path; the
option list, field labels (`Environment:`/`Reason:`), and footer each vendor
renders are preserved exactly, because the option markers are the actual
point — see "The trap" below.

| file | vendor | options | default marker sits on |
|---|---|---|---|
| `grok-reviewer-tool-approval.txt` | grok | 4 | **`1 (●)` — "don't ask again for anything"** |
| `codex-reviewer-loopback-approval.txt` | codex | 3 | `› 1.` — "Yes, proceed" |

## Folder-trust dialogs

| file | vendor | answer keys |
|---|---|---|
| `codex-directory-trust-prompt.txt` | codex | `› 1.` — "Trust and continue" |
| `grok-trust-directory.txt` | grok | `y`/`n`, no default marker |

`grok-trust-directory.txt` (card `20261122-b-1961a6`, task
`20261122-builder-1b5eb1`) is **reconstructed, not captured**: the only
verbatim part is the question line, "Do you trust the contents of this
directory? y/n", quoted from a Grok Build 1.0.92 pane that parked a lane
on its first run in a repository. The surrounding frame (version line,
neutral path, explanatory paragraph, key footer) is a plausible stand-in
with neutral names; replace it with a real capture when one is taken.
Recognition keys on the question line alone (`runner::attention_reason`'s
generic trust-dialog needles), so the frame is not load-bearing. These are
read as `needs-attention:trust-dialog` — the pane is kept, the dispatch is
not recorded `failed-to-start` while the dialog is still up, and nothing in
this workspace ever answers it. Grok's own store for the answer
(`$GROK_HOME/trusted_folders.toml`) is pre-written at dispatch by
`crates/mv-engine/src/grok_trust.rs`, so a configured seat should not see
this dialog at all.

## The trap: a bare Enter is not the same action on both

On codex the default is the single-shot proceed. On grok the default is
**blanket standing approval** — `1 (●) Yes, and don't ask again for
anything (always-approve mode)`, with options 2-4 all `(○)`. So "press the
default" is safe on one vendor and permanently widens permissions on the
other; any instruction that says "send the single-shot proceed" without
naming the option number is wrong on grok. A classifier built from either
capture alone would misread the other: they differ in option count, in how
the persistent option is scoped, in whether a structured
`Reason:`/`Environment:` field is present, and in which option carries the
default.

## Recognition only

Neither dialog is auto-answered by anything in this workspace. `PromptAudience::ProjectOwner`
names who can actually decide; it is not a grant of authority to decide on
their behalf. See `crates/mv-engine/src/blocking_prompt.rs`.

## Idle pane with trust/update prose in scrollback

| file | vendor | must classify |
|---|---|---|
| `claude-idle-with-trust-prose-in-scrollback.txt` | claude | **idle** for nudge, board, and sweep |

Written for card `20261122-b-8ad25a` (task `20261122-builder-fc9d63`): a
report in scrollback mentions "trust dialog", quotes grok's
"Do you trust the contents of this directory? y/n" question (with its y/n
shape), and quotes the captured `Update available!` vendor frame, then
ends on Claude's `✻ … · done` turn-end and an empty `❯` composer. The
dialog-region helper must not treat that prose as a live dialog; reverting
to a whole-pane substring scan makes it classify as a trust dialog (and
the quoted update frame classifies as `codex_cli_update_prompt` for
codex). Synthesized from the idle composer shape in
`tests/fixtures/composer/claude-done-followup.txt` plus the quoted
dialog/update frames from `grok-trust-directory.txt` and
`tests/fixtures/stall-cause/codex-cli-update-prompt.txt`.

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

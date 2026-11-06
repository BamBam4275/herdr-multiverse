# The codex CLI update prompt — a stall cause, not an approval

Ported verbatim from the frozen Python reference
(`multiverse/tests/fixtures/stall-cause/codex-cli-update-prompt.txt`) for
cards `20261102-b-bdf745`/`20261102-b-9ef1da` (task `20261103-fixer-08598b`).
No sanitisation was needed — this capture names no project or machine path.

Filed under `stall-cause` rather than `sandbox-dialog` deliberately: it is
not a permission request and nothing should ever classify it as one.
`classify_prompt` must still recognise it (`PromptAudience::DoNotAnswer`,
`crates/mv-engine/src/blocking_prompt.rs`), because its default
(`> 1. Update now (runs \`brew upgrade...\`)`) mutates the human's machine
outside any project — an orchestrator has no standing to trigger that on
anyone's behalf, and a pane parked here reads as merely slow rather than
stuck on a destructive default unless something names it.

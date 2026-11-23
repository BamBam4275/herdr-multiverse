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

## `claude-background-command-completed-idle.txt`

A Claude lane that ended its turn to wait on its own background shell,
whose job then finished without the completion notification ever waking
it: the `⏺ Background command "…" completed (exit code N)` line sits below
the last `✻ … · done` turn-end line, nothing follows it, and the composer
below is empty. Written for card `20261122-b-3acaeb` (task
`20261122-builder-b9473d`) from the line shapes Scout
`20261122-scout-daeed4` quoted out of this project's own
`20261115-builder-7abd49` parked-pane evidence capture — not a verbatim
pane capture: the wait, turn-end and completion lines are those quoted
shapes, and the idle `❯` composer between two rules is synthesized. No
field capture of the stuck shape exists yet; replace this with one when a
stalled lane's pane is read before it is closed.
`mv_engine::sweep_nudges`'s stall wake must recognise it; no Codex
equivalent exists because no Codex background-terminal pane text has been
captured.

## `claude-waiting-for-api-response-retry.txt`

A Claude lane whose API calls fail, parked on `✻ Waiting for API response
· will retry in 3m 50s · check your network` with an empty `❯` composer
and an `esc to interrupt` footer. Written for card `20261122-b-1049ab`
(task `20261122-builder-b74852`): the last 13 lines of this project's
own `20261115-builder-7abd49` parked-pane evidence capture, copied
verbatim except that the conformance log path in the tool call is
renamed to the neutral `/tmp/full-suite.log`.
`mv_engine::sweep_nudges`'s `connection_loss_re` must match it, so
`resume_stuck_lanes` resumes the lane. `stuck_pane_re` must not, so
finish-nudge and stall-wake eligibility stay unchanged.

## `claude-shell-still-running-idle.txt`

A Claude lane back at its prompt while one of its two background shells
is still running: the first job's `⏺ Background command "…" completed
(exit code 0)` line sits below the last turn-end line, and the footer
below the composer still reads `⏵⏵ bypass permissions on · 1 shell · ← 1
agent`. Written for card `20261122-b-e98304` (task
`20261122-builder-1be661`) from the footer line the card quotes — not a
verbatim pane capture: the transcript lines reuse
`claude-background-command-completed-idle.txt`'s shapes, and the second
tool call and the footer placement are synthesized. Replace it with a
field capture when a waiting lane's pane is read.
`mv_engine::sweep_nudges::waiting_on_shell` must recognise it, so
`multiverse workers` and the board show the lane `waiting-on-shell` and
the stall wake never wakes it, even though its completed-command line on
its own reads as a finished-but-unwoken job.

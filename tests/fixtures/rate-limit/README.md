# Captured rate-limit pane text

Sanitized real `herdr agent read <agent> --source visible` captures, carried
over from the frozen Python reference's own `tests/fixtures/rate-limit/`
(card `20261102-b-d8e4ac`): the vendor banner text that
`crates/mv-engine/src/health.rs`'s classifier matches against is unchanged
from the original capture. Personal paths, real project names, and other
identifying scrollback content unrelated to the banner itself have been
replaced with generic placeholders (`/home/demo/...`, `demo-fixer-000000`,
etc.) — never the load-bearing banner lines themselves.

Only the subset `crates/mv-engine/src/health.rs`'s tests actually assert
against is carried over; see the frozen reference's own
`tests/fixtures/rate-limit/README.md` for the full set's provenance notes
(not copied here, since those `.md` files quote real project names in their
own prose).

- `codex-out-of-credits.txt` — codex, spent-quota dialog (`LIMIT`).
- `codex-rolling-429.txt` — codex, a rolling 429 that cleared on its own (`RATE`).
- `codex-model-switch-offer.txt` — codex, a model-switch dialog (`DIALOG`).
- `claude-rolling-429.txt` — claude, a rolling 429 (`RATE`), also matching
  `API Error` vocabulary that must not read as `DEAD`.
- `grok-weekly-limit-reached.txt` — grok, the past-tense exhaustion dialog (`LIMIT`).
- `grok-weekly-quota-remaining.txt` — grok, the live percentage gauge, which
  must **not** classify (`None`) even though it shares the phrase "Weekly
  limit" with the exhaustion dialog.
- `grok-weekly-limit-live-pane.txt` — grok, the same exhaustion dialog
  captured inside its own bordered box with a trailing nav-hint and pane
  chrome, pushing the sentence further back in the tail.

# Rehearsal scripts

Disposable-sandbox scripts that exercise the Rust `multiverse` binary (and,
where named, the frozen Python reference) without ever touching a real
project, a real `config.toml`, or real Herdr — every script here except
`live_smoke.sh` points `HERDR_BIN_PATH` at a fake binary (`mv-herdr`'s
`fake-herdr`) and works against `mktemp -d` copies.

- `discover_projects.py` — lists `configkey:dirname` pairs from a
  `config.toml`'s `[projects.*]` tables for the other scripts' matrix.
- `rewrite_config.py` — points a copied `config.toml`'s project paths at
  sandbox copies instead of the real checkout.
- `read_parity.sh` — reproduces the Python-vs-Rust read-command parity
  rehearsal for the fleet projects a given config names.
- `write_parity.sh` — the same, extended to commands that write or decide
  (`dispatch --dry-run`, `sweep --dry-run`, `worktrees --prune --dry-run`,
  `tasks --reconcile`, `halt on/off`, `backlog add/annotate/done`).
- `compare.py` — normalizes and diffs the Python-vs-Rust stdout/stderr
  `read_parity.sh`/`write_parity.sh` capture.
- `diff_trees.py` — the same normalization, applied to two sandbox
  directory trees instead of captured output, so a write command's
  on-disk effect can be checked for parity too.
- `verify_config_migration.sh` — rehearses `multiverse config migrate`
  against private copies of a real `config.toml`.
- `simulate-plugin-install.sh` — rehearses `herdr plugin install`/
  `action invoke` against a freshly cloned temp plugin root and a
  caller-supplied fake Herdr binary.
- `seed-check/` — a standalone Rust crate; see its own README.

## `live_smoke.sh` is the one exception — operator/hub only

`live_smoke.sh` deliberately does **not** point `HERDR_BIN_PATH` at a fake
binary: it exists to prove the Rust binary reaches a *real* `herdr` on
`PATH` and drives a real pane, for a write-nothing role and a role with its
own git lane. That is also what makes it unsafe to run from inside a
dispatched lane's sandbox: a Fixer/Builder/etc. agent's Bash tool permission
layer denies any command that invokes the real `herdr` executable outright
(confirmed independently from two separate Fixer lanes — see backlog card
`20261102-b-c3050e`), so the live-Herdr portion cannot complete end to end
from inside a lane even when the script itself is correct and its
`--dry-run`-equivalent setup (config/env shape) has been verified.

Run `live_smoke.sh` only from an operator/hub session that has real Herdr
permission — never dispatch it as a lane's task. A lane that needs to
verify this script should stop at confirming the config/env shape it
builds (sandbox config, state dir, project) and the command line it would
run, and report the real-Herdr portion as unverified rather than attempting
to run it.

# Installing Multiverse

Multiverse is a pre-1.0 Rust rewrite distributed as a Herdr plugin. Every
crate and command handler is implemented — none is a module-list stub —
but several commands still carry documented scope cuts short of full
Python parity (see the README's [Status](../README.md#status) section).
Packaging checks do not mean every fleet command is ready for production.
Install tagged releases once their release notes confirm the commands you
need.

## Supported platforms

| OS | Architecture | Target triple |
|---|---|---|
| macOS | Apple Silicon (arm64) | `aarch64-apple-darwin` |
| macOS | Intel (x86_64) | `x86_64-apple-darwin` |
| Linux | x86_64 | `x86_64-unknown-linux-gnu` |
| Linux | aarch64 | `aarch64-unknown-linux-gnu` |

CI runs `cargo deny check` with all features against these four release
targets, as configured in `deny.toml`'s `[graph]` section. Dependencies used
only on other platforms are outside this policy check. Keep that target list
in sync with the release matrix and `herdr-plugin.toml`'s supported platforms.

## Install via Herdr

Replace `vX.Y.Z` with a published tag:

```sh
herdr plugin install BamBam4275/herdr-multiverse --ref vX.Y.Z --yes
```

The build hook, `scripts/bootstrap-plugin.sh`, derives the checkout from
its own script path. It reuses a matching `bin/multiverse`, otherwise
fetches `multiverse-<target-triple>.tar.gz` and `SHA256SUMS` from that tag's
[GitHub Release](https://github.com/BamBam4275/herdr-multiverse/releases).
It verifies SHA-256 before extracting. When `gh` is on PATH and the
download is the default GitHub Release (no `MULTIVERSE_RELEASE_BASE_URL`),
the hook then runs `gh attestation verify`. Checksums already passed, so
an old, unauthenticated, or offline `gh`, or a release with no
attestation, prints a warning and keeps the prebuilt; the hook refuses
that archive only when `gh` reports the artifact does not match its
attestation. If download or a matching-attestation check fails, it tries
a locked Cargo source build; without Cargo it exits with an error.
The hook needs POSIX shell utilities, `tar`, curl or wget for downloads,
and `sha256sum` or `shasum` for verification. Rust is unnecessary for a
successful prebuilt install. Downloads use HTTPS; checksums protect against
corruption, but are not independent signatures of the release publisher.

## Configure before invoking an action

Locate the editable configuration directory:

```sh
herdr plugin config-dir multiverse
```

Create `config.toml` there using the repository's
[`config.example.toml`](../config.example.toml), fill its placeholders,
and configure accounts, roles, and projects. Keep secrets private (directory
mode `0700`, file mode `0600`). See [configuration](configuration.md).
The wrapper requires a readable config file and absolute plugin config/state
paths; otherwise it exits 3 before bootstrap or CLI execution. It never
silently selects an existing standalone global fleet config. `import-python`
is the one exception to the readable-config-file requirement: creating that
file there is its job (see [Migrating from Python](migrating-from-python.md)).

| Location | Multiverse use |
|---|---|
| `$HERDR_PLUGIN_CONFIG_DIR/config.toml` | Fleet configuration — accounts, roles, and projects |
| `$HERDR_PLUGIN_STATE_DIR` | Fleet runtime locks, caches, breakers, seat state, gauges, notifications, directives, and halt state as their modules land |
| Each project's `.multiverse/` | Task briefs/results/status, routing history, backlog, worktrees, Nexus seeds, and an optional `roster.toml` chain override |
| Plugin checkout `bin/` and `target/` | Installed executable and optional build artifacts; no user config or fleet state |

There is no `herdr plugin state-dir`-equivalent lookup command for
`$HERDR_PLUGIN_STATE_DIR` the way `herdr plugin config-dir multiverse`
resolves the config directory. `multiverse doctor` and `multiverse check`
both print the resolved state directory alongside the config path, so
running either once through its plugin action
(`herdr plugin action invoke --plugin multiverse multiverse-doctor`) is how
to learn the real path: export it as `MULTIVERSE_STATE_DIR` for a CLI-only
command with no managed action of its own (`config migrate`, `halt`,
`sweep`) run bare, alongside the same `--config` those commands already
need.

Every managed action and dashboard invocation sets `MULTIVERSE_CONFIG` and
`MULTIVERSE_STATE_DIR` from the two Herdr directories, replacing inherited
standalone overrides. To test alternate paths, invoke `bin/multiverse`
directly with explicit overrides. The wrapper preserves `HERDR_BIN_PATH`
and Herdr's session/socket environment. It generates a shell-quoted absolute
`MULTIVERSE_INVOCATION` for seeds used later from project directories.

The [Herdr 0.9.5 plugin contract](https://github.com/herdrdev/herdr/blob/master/docs/versions/0.9.5/website/src/content/docs/plugins.mdx#commands-and-environment)
specifies plugin-directory cwd and config/state variables. Although that
version also documents `HERDR_PLUGIN_ROOT`, Multiverse uses relative manifest
commands and derives paths from the wrapper. Build hooks receive no runtime
plugin context. The manifest's `0.9.0` minimum remains a compatibility floor;
this lane verifies the documented 0.9.5 contract, not every earlier release.

## Actions and their effects

```sh
herdr plugin list --plugin multiverse --json
herdr plugin action list --plugin multiverse
herdr plugin action invoke --plugin multiverse multiverse-doctor
herdr plugin pane open --plugin multiverse --entrypoint dashboard
```

The report actions (`doctor`, `check`, `accounts`, `tasks`, `board`,
`workers`) and dashboard set `MULTIVERSE_READ_ONLY=1`. Once built, reports
should not persist fleet/project changes. A linked checkout's first action
can still bootstrap a missing binary; Herdr also records command logs.
`start` deliberately clears the read-only marker: it opens project
workspaces, starts interactive Nexus sessions, and writes project seeds.
Subsequent dispatches can spend vendor quota and write assigned worktrees.
`import-python` also clears it: it copies a standalone Python install's
config and fleet state into the directories this plugin reads (see
[Migrating from Python](migrating-from-python.md)) — real writes, though it
refuses to overwrite anything already at the destination unless run with
`--force`.

The manifest omits automatic startup/event/link hooks and quota-consuming
`dispatch`/`council` shortcuts. Other CLI reports remain CLI-only to keep the
action menu small. Opening the dashboard is an explicit operation.

## Local development

```sh
scripts/build.sh
mkdir -p bin
cp target/release/multiverse bin/multiverse
chmod +x bin/multiverse
herdr plugin link /absolute/path/to/herdr-multiverse
```

Linking skips build hooks. The wrapper can bootstrap a missing executable,
but build and copy it explicitly for development so you use your local
changes. Repeat those build/copy commands after source changes; plain
`cargo build --release` updates `target/release/`, not `bin/`.
After manifest changes, relink, then compare `plugin list --json` and
`plugin action list` with the checkout manifest. `plugin update` does not
update linked source; update your checkout yourself and rebuild/relink.
Unlink before switching a local registration to a GitHub install.

For a standalone source build:

```sh
cargo build --release --locked -p mv-cli
./target/release/multiverse --version
```

The pinned toolchain is in `rust-toolchain.toml`.

## Updates, removal, and named sessions

Herdr 0.9.5 shares registration/enabled state across sessions. Session
selectors target running workspaces/panes; do not repeat installation per
session. Older session-only registrations need relinking/reinstallation.
The following lifecycle behavior comes from the
[0.9.5 install/link contract](https://github.com/herdrdev/herdr/blob/master/docs/versions/0.9.5/website/src/content/docs/plugins.mdx#install-and-link)
and [CLI reference](https://herdr.dev/tui/docs/cli-reference/#plugins).

For a GitHub install, update the recorded ref:

```sh
herdr plugin update multiverse --yes
```

A pinned tag stays pinned. To select a different tag, rerun `plugin install`
with the new `--ref`. New invocations use the replacement; existing commands
retain old files and are not restarted. Stop active work and reseed project
Nexus sessions with the new invocation before retiring old installations.
Both `stop` and `start --reseed` are implemented; for a project whose
Nexus workspace is already open, `--reseed` now re-points that live
session (not only the seed file) through the same blocked/dialog
detection and retry-with-probe landing confirmation `start` always uses
(see the README's [Status](../README.md#status) section for the scope
cuts that remain). A vendor CLI upgrade can leave a seat's pane parked
on a one-time notice of its own — Claude Code's post-upgrade welcome
screen is the common case — before its old session is reseeded; that
pane is caught by the same generic blocked/dialog detection
(`attention_reason`'s `press enter to continue`/`select an option`
patterns) as a trust dialog, so expect to dismiss it by hand once per
seat the same way.

Disable new plugin invocations before removal:

```sh
herdr plugin disable multiverse
herdr session list --json
```

Inspect each active session explicitly, for example:

```sh
herdr --session work workspace list
herdr --session work plugin log list --plugin multiverse
```

Stop Multiverse work with the release's `multiverse stop` command, close
its dashboard panes, and close its project workspaces in every session.
Verify workers have exited before removing a checkout or shutting down a
server. Disable/unregister is not a Multiverse shutdown hook.

```sh
# Choose the command matching how the plugin was registered:
herdr plugin unlink multiverse     # local checkout
herdr plugin uninstall multiverse  # GitHub installation
```

Config/state survive removal. Managed installations are retained for running
servers and cleaned later; detached processes surviving server exit are not
protected. Local checkout files remain. Back up config/state and project
history; delete them manually only when intentionally retiring the fleet.
Re-enable a retained registration with `herdr plugin enable multiverse`.

## Verification and troubleshooting

Run the isolated packaging regressions (Python 3.11+ for tests only):

```sh
python3 scripts/test_plugin.py
```

These execute manifest commands against a fake binary without contacting
Herdr. They do not validate live fleet behavior — see the README's
[Status](../README.md#status) section for what still carries scope cuts
short of full Python parity.

Workspace verification (separate from the packaging script) is the crate
suite and the conformance harness:

```sh
cargo test --workspace --all-features --locked
python3 conformance/run_all.py
```

This tree currently has 2043 passing crate unit/integration tests (sum of
the `passed` totals from the workspace test results above) and 162 passing
conformance scenarios. `conformance/diff_fuzz.py` is a seeded
Python-versus-Rust fuzzer that runs read probes after each write; four
`dispatch --dry-run` rendering differences are accepted (role-rename
agent short-codes and prompt-file names, prompt-file line counts, this
binary's `WOULD RUN` pane commands, and prompt wrapping that can split
a sandbox path). On one machine, warm `board --all --read-only
--no-reconcile --no-color` is 4.56 s and warm `sweep --stale-only` went
from 6.94 s to 2.50 s. Those timings are a single-host measurement, not
an install requirement, and none of these checks validate live fleet
behavior.

- **Configuration error (exit 3):** use `plugin config-dir multiverse`,
  create a readable `config.toml`, and retry through Herdr so both plugin
  directory variables are present. Global config is intentionally ignored.
- **Missing command wrapper:** if `multiverse check` reports a Claude seat
  "without the required command wrapper", set `accounts.<name>.command`
  in the config located by `herdr plugin config-dir multiverse`. Every
  Claude seat needs its actual launch command, for example
  `command = "claude-interactive"` if that wrapper is defined in the seat's
  interactive shell; installing the vendor CLI alone does not define it.
  See the launch-command comments in
  [`config.example.toml`](../config.example.toml) and
  [account configuration](configuration.md#configtoml), then rerun
  `herdr plugin action invoke --plugin multiverse multiverse-check`.
- **Fleet halted:** `fleet halted since ...` or `skipped (fleet halted)`
  means the halt marker is set. With `HERDR_PLUGIN_CONFIG_DIR` and
  `HERDR_PLUGIN_STATE_DIR` set to the same plugin directories reported by
  Doctor or Check (see [configuration setup](#configure-before-invoking-an-action)),
  inspect the reason and clear the halt when ready to resume:

  ```sh
  export MULTIVERSE_STATE_DIR="$HERDR_PLUGIN_STATE_DIR"
  multiverse --config "$HERDR_PLUGIN_CONFIG_DIR/config.toml" halt status
  multiverse --config "$HERDR_PLUGIN_CONFIG_DIR/config.toml" halt off
  ```
- **Herdr not on PATH:** if `multiverse doctor` reports `herdr reachable`
  as failed, install Herdr and put `herdr` on `PATH`, or set
  `HERDR_BIN_PATH` to its absolute executable path. Retry
  `herdr plugin action invoke --plugin multiverse multiverse-doctor` with
  that environment; the wrapper preserves `HERDR_BIN_PATH`.
- **No release binary and no Cargo:** install Rust from
  [rustup](https://rustup.rs) or choose a published tag for your platform.
  rustup alone is not sufficient: the source fallback also needs the pinned
  toolchain's target standard library (installed automatically from
  `rust-toolchain.toml`), a system C linker (`cc`, e.g. from Xcode Command
  Line Tools or `build-essential`), and network access to fetch the locked
  registry dependencies. The hook only checks that `cargo` is on `PATH`; a
  `cargo build` failure on a minimal host is usually a missing linker.
- **Checksum mismatch:** the downloaded archive is rejected; source build
  may still succeed. Check the release assets and retry before reporting the
  exact tag/platform.
- **Attestation warning:** `gh attestation verify` was skipped (old CLI,
  no login, offline, or no attestation on the tag). The checksum-valid
  prebuilt was still installed. Update `gh` or run `gh auth login` to
  enable provenance checks on the next install.
- **Attestation mismatch:** `gh` reported the archive does not match its
  provenance attestation; the prebuilt was refused. Source build may still
  succeed. Check the release assets and retry before reporting the exact
  tag/platform.
- **A linked action runs old code:** copy the rebuilt executable to `bin/`;
  relink after manifest changes and inspect the registered actions.

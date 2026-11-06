# Repository conventions for agents

## Scope and ownership

Work only in the files assigned by your task. Use the existing task
worktree, keep unrelated changes out, and report required changes outside
your scope to the owning lane. Do not read or modify the frozen Python
reference unless your task explicitly permits it.

The workspace has seven crates with one-way dependencies: `mv-core` at the
base; `mv-config` and `mv-herdr` above it; `mv-state`, then `mv-engine`;
`mv-board` and `mv-cli` at the edge. Keep business rules out of the clap CLI
and writes out of board rendering. Use synchronous subprocesses, not a new
async runtime. Crate module lists were established in Wave 0: never edit
another lane's `lib.rs` or expand its module list without assigned ownership.

Own files within crates, not entire crates by implication. Packaging owns
`herdr-plugin.toml` and `scripts/`; docs, CI, configuration, fixtures, and
crate implementations can have separate owners. A shared manifest edit must
stay within the specific fields assigned to you.

## Build and required checks

Use the toolchain pinned in `rust-toolchain.toml` and the checked-in lockfile.
Build with `cargo build --workspace --locked`. Run from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 conformance/run_all.py
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo deny check
```

Packaging, plugin-action, or lifecycle changes also need
`python3 scripts/test_plugin.py`.

Run `cargo fmt --all` only when its writes remain within your scope. Run the
full suite once after implementation, wait for that run, and rerun only for
a failure or further code change. Report unavailable tools or failed checks
honestly. Keep test/build artifacts out of commits; `target/` and `bin/` are
already ignored. Do not force-add task audit files under `.multiverse/`.

## Behavior and verification

Decide which observation would prove the change correct before editing.
Use crate unit tests for parsers, locks, and byte-stable formats; use the
existing conformance harness for Python/Rust CLI parity. Port relevant
behavioral assertions rather than duplicating the entire Python suite.
Use new role names on config/write surfaces (Nexus, Scout, Builder,
Archivist, Critic, Seer, Council, Artisan, Watcher); history readers must
continue accepting the old on-disk names. Preserve exit codes 0/1/2/3 and
existing file formats. Preserve config comments through targeted edits.

Never exercise the real fleet during tests. Before any CLI run that could
reach Herdr, export `HERDR_BIN_PATH` to an isolated fake executable and use
temporary config, state, HOME, and project directories. Do not fall back to
a real Herdr binary/socket. Keep config in `HERDR_PLUGIN_CONFIG_DIR`, fleet
runtime state in `HERDR_PLUGIN_STATE_DIR`, and project history in each
project's `.multiverse/`. Use script-derived paths for plugin executables.
Document changes to actions, write surfaces, or lifecycle behavior and test
them with `scripts/test_plugin.py`. Shell scripts must remain POSIX sh.

## Commits

Use one scoped Conventional Commit per assigned task (`fix:`, `feat:`,
`docs:`, `chore:`, etc.). Include `Task: <task-id>` on its own line in the
body when working a dispatched task. Commit before reporting completion;
never merge, push, or publish without task authorization. Include what
changed, its evidence, and any verification gap in the task result.

The project is dual licensed under MIT OR Apache-2.0; retain both license
files and use the contribution terms in `CONTRIBUTING.md`. Do not commit
credentials, personal data, internal hostnames, or machine-specific paths.

# seed-check

A cutover-rehearsal tool: renders the Nexus seed `mv-engine::nexus_seed`
would produce for one or more real projects, given a config file passed
as an argument. It is its own Cargo workspace (`[workspace]` in
`Cargo.toml`) so it builds independently of the root `herdr-multiverse`
workspace, path-depending on `mv-core`/`mv-config`/`mv-engine`.

**Never point `--config` at an operator's real
`~/.config/multiverse/config.toml`.** Point it at a scratch copy —
this tool does not write to the config it reads, but the fleet config
and directives it loads may contain real account/project data, and
this tool's whole purpose is rehearsing changes against a disposable
copy before they touch anything live.

## Build

```sh
cargo build --release --manifest-path scripts/rehearsal/seed-check/Cargo.toml
```

## Usage

```sh
seed-check --config PATH --directives DIR --out DIR \
    [--project NAME]... [--handoff NAME=PATH]...
```

- `--config`: a `mv_config::load::load_config`-readable TOML file.
- `--directives`: a directory of `<project>.md` standing-directive files
  (may be empty).
- `--out`: directory to write one `<project>.md` rendered seed per
  selected project.
- `--project`: repeatable; selects which configured projects to render.
  Omit to render every project in the config.
- `--handoff NAME=PATH`: overrides the handoff-card path shown for one
  project. A project with no override gets
  `<repository>/.multiverse/handoff.md` — a convention this tool
  assumes, not one `mv-engine` enforces today (see task
  `20261102-fixer-4e4057`'s `result.md` for why that convention does
  not match most real projects' actual handoff records).

Account availability is a first-hop approximation (the chain's first
account, `Ok` if `available = true` in the config, `Err` otherwise) —
not the real selector's quota/breaker-aware choice
(`mv-engine::selector`). Good enough to confirm the seed renderer
labels roles correctly; not a stand-in for real dispatch selection.

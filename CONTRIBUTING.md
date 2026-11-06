# Contributing to Herdr Multiverse

Thanks for your interest in contributing. This project is a pre-1.0 Rust
rewrite of a Python reference implementation, built in disjoint, parallel
lanes — read this guide before opening a pull request so your change lands
in the right place.

## Before you start

- Check [open issues](https://github.com/BamBam4275/herdr-multiverse/issues)
  and pull requests for related work.
- For anything larger than a small fix, open an issue first using the
  [feature request template](.github/ISSUE_TEMPLATE/feature_request.yml) so
  the approach can be discussed before you invest time.
- Changes to `herdr-plugin.toml`, release scripts, permissions, or role
  semantics require accompanying documentation and test updates in the same
  pull request.

## Development setup

1. Install a stable Rust toolchain (see `rust-toolchain.toml` at the repo
   root for the exact pinned version).
2. Clone the repository and build it:

   ```sh
   git clone https://github.com/BamBam4275/herdr-multiverse.git
   cd herdr-multiverse
   cargo build
   ```

3. To exercise it as a Herdr plugin without a release install, point Herdr
   at your local checkout:

   ```sh
   herdr plugin link /absolute/path/to/herdr-multiverse
   ```

   `herdr plugin link` skips the `[[build]]` hook, so run `cargo build
   --release` yourself first.

## Required checks

Run these before opening a pull request; CI enforces all of them:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-features --locked
python3 conformance/run_all.py
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo deny check
```

## Commit and PR conventions

- Use [Conventional Commits](https://www.conventionalcommits.org/) style
  commit subjects (`feat:`, `fix:`, `docs:`, `chore:`, …) so changelog and
  release tooling can rely on them.
- Keep pull requests scoped to one logical change.
- Fill out the [pull request template](.github/pull_request_template.md)
  completely, including the security/permission review checklist for any
  change that touches actions, panes, worktrees, or subprocess execution.
- Add a `CHANGELOG.md` entry under `[Unreleased]` for any user-visible
  change.
- Update `docs/roles.md` or `docs/configuration.md` alongside any change in
  role behavior, write scope, or configuration shape.

## MSRV policy

This project tracks the stable Rust toolchain pinned in
`rust-toolchain.toml`. The `rust-version` field in `Cargo.toml` is the
minimum supported Rust version (MSRV); raising it is a deliberate,
documented decision, not an incidental side effect of a dependency bump.

## Code style

Match the style of the code around your change — naming, module layout, and
error handling already established in the crate you're editing. `rustfmt`
and `clippy` catch mechanical issues; use your judgment for everything else.

## Reporting bugs

Use the [bug report template](.github/ISSUE_TEMPLATE/bug_report.yml). For
security-sensitive issues, follow [`SECURITY.md`](SECURITY.md) instead of
opening a public issue.

## Code of Conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By
participating, you agree to abide by its terms.

## License

This project is licensed under either MIT or Apache-2.0, at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions. See [LICENSE-MIT](LICENSE-MIT) and
[LICENSE-APACHE](LICENSE-APACHE).

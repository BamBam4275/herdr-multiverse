# Release bootstrap checks

Run `python3 scripts/test_plugin.py` for the fast plugin, bootstrap, and
private-names tests: `test_bootstrap.py` and `test_private_names.py` are
both loaded by that script's `load_tests`, alongside the plugin boundary
tests in `scripts/test_plugin.py` itself. `load_tests` also loads
`RealReleaseTests` from `test_release.py` there, skipping its two
build-dependent tests (`test_missing_release_builds_from_source`,
`test_each_packaged_target_installs_exact_binary`) with a clear reason —
they need a real macOS cargo/rustup toolchain and network access that only
running `test_release.py` directly (below) sets up. Everything else
`RealReleaseTests` inherits from `BootstrapTests` runs against the same
fake-release fixture, so it is covered by the fast, required check too.

The bootstrap tests use local `file://` assets, a fixed tool allowlist without
cargo, and temporary plugin roots containing spaces and apostrophes. They
cover verified installation, repeat installation, bad/missing checksums,
source fallback, recovery instructions when neither release nor cargo is
available, and stub-`gh` provenance cases on the default GitHub URL (too
old, unauthenticated, offline, missing attestation, mismatch, success).
Cargo is a fixture in this fast check.

`test_private_names.py` covers `check-private-names.sh`'s `git grep` ERE
validation, including that an invalid ERE exits 2 with a message rather
than silently passing.

On macOS, run the real release path from the repository root:

```sh
PATH="$HOME/.cargo/bin:$PATH" python3 scripts/tests/test_release.py
shellcheck scripts/*.sh
```

This is also the command for a macOS CI runner with Python 3.11+, Xcode command
line tools, curl, and the pinned Rust toolchain. It installs missing Rust
targets with rustup, builds both Apple architectures using `scripts/build.sh`,
and reproduces `.github/workflows/release.yml`: an archive containing just
`multiverse`, one SHA-256 sidecar per archive, and an aggregate `SHA256SUMS`.
Packaging sets `COPYFILE_DISABLE=1` to omit macOS AppleDouble metadata files.
Linux targets are included only when their GNU cross compiler already exists
on PATH. Builds use two cargo jobs by default (`CARGO_BUILD_JOBS` can override).

The test installs each archive through the real bootstrap hook using a local
`file://` release directory and compares the installed binary byte for byte.
The native binary must run `--version`; other architectures are checked
without execution, so Rosetta is unnecessary. It corrupts checksums and
requires refusal with no installed binary. It removes the release directory
and performs a real cargo fallback from a fresh source copy without build
artifacts, then checks the clear error when cargo is unavailable as well.
Temporary sources, archives, HOME, project, config, state, and fake Herdr are
removed automatically. Only ignored `target/` build output remains. Nothing
is published or installed into Herdr.

`MULTIVERSE_RELEASE_BASE_URL` overrides the bootstrap hook's **versioned asset
directory**. It must contain `multiverse-<target-triple>.tar.gz` and
`SHA256SUMS`; a trailing slash is accepted. The default remains the manifest
version's GitHub release URL. Use `file:///absolute/path/to/dist` for a local
test or an HTTPS mirror for a private distribution. An override selects the
trust source for both archive and checksum. Checksum mismatches print a
diagnostic and refuse that archive; the existing cargo fallback still runs
when available. Neither download nor extraction executes the payload before
verification.

# Fresh public export

Run `sh scripts/release/export-fresh.sh OUTPUT_DIR` with the four Git author
and committer identity variables listed in the script set explicitly. The
output directory must be empty. The script exports committed `HEAD`, creates
one new commit on `main`, and runs the build, Rust tests, conformance checks,
and available name/secret checks. It never adds a remote or pushes.

Archive creation and extraction are checked separately. An archive failure
stops before creating the new repository or running checks, even if Git has
already emitted an otherwise readable archive. The temporary archive is
removed on success, failure, or an intercepted termination signal.

Run `python3 -B scripts/release/test_export.py` for fast regressions covering
that failure, fresh history, source config preservation, and exclusion of
uncommitted, untracked, and ignored files. These tests use a synthetic source
repository and fixture build commands; a real export is still required to
verify the Rust build and tests. Private data must remain untracked: this
script exports every tracked file in `HEAD`.

Set `MV_PRIVATE_NAMES` to the operator's external deny-list to enable the
name check. The exporter skips the Docker-based gitleaks scan when Docker is
absent; a successful export with a skipped check does not prove that check
passed.

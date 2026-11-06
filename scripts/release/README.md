# Public releases

Use `export-fresh.sh` only to seed the initial public repository. For later
releases, use `sync-public.sh` with an existing public clone so its commits
and tags remain intact. Never replace the public history or force-push.

## Later releases

Commit the release source, including matching `[workspace.package].version`
in `Cargo.toml` and top-level `version` in `herdr-plugin.toml`. Pass the
version without a `v` prefix. Set `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`,
`GIT_COMMITTER_NAME`, and `GIT_COMMITTER_EMAIL` explicitly, then run:

```sh
sh scripts/release/sync-public.sh /path/to/public-clone 0.2.0
```

The public clone must be separate from the source checkout, have a clean
index and working tree (including untracked files), and be on `main`.
The script fetches `origin`'s `main` and requires it to be an ancestor of
local `main`; behind or diverged clones are refused. Local commits ahead of
`origin/main` are preserved. A pre-existing local `v<version>` tag is refused.
The clone needs access to its configured origin; the script does not change
remote URLs. It requires Git, rsync, Python 3.11+ (`tomllib`), and the same
build/check tools as the fresh exporter.

The script runs `export-fresh.sh` in a disposable temporary repository,
exporting committed source `HEAD` and running every check described below.
Its temporary single-commit history never enters the public clone. Both
manifest versions must match the requested version before the clone's tree
is changed. Export/check failures leave that tree and its commits untouched.
The temporary export is removed on exit or an intercepted termination signal.

After verification, the script removes build/check artifacts from the
temporary tree, repeats the public-clone safety checks, and replaces the
clone's files with `rsync --checksum --delete`, excluding `.git`. Comparing
contents also replaces stale files with identical sizes and timestamps. This deletes
obsolete files and any ignored local files in the public clone: use a
dedicated release clone. It commits as `release: v<version>` with the supplied
identity and creates the annotated tag `v<version>` (the committer identity
also supplies the tagger). Review the commit and tag before publishing.
Failures during copying, committing, or tagging can leave a partially
prepared local clone; inspect its status and history before retrying.

The script prints this command and never executes it. Run it from the public
clone when ready; it publishes `main` and the new tag without force:

```sh
git push origin main v0.2.0
```

## Initial fresh export

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

The same suite covers later releases against a local bare origin, with no
network or actual Docker execution: history/tag/identity preservation,
tracked-tree replacement, no push, failed-check isolation, matching versions,
and refusal of dirty, off-main, behind, or diverged public clones. Build,
conformance, and scanner commands are fixtures in these tests.

Set `MV_PRIVATE_NAMES` to the operator's external deny-list to enable the
name check. The exporter skips the Docker-based gitleaks scan when Docker is
absent; a successful export with a skipped check does not prove that check
passed.

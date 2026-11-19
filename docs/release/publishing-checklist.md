# Publishing checklist

This records, in order, what it actually took to prepare and publish
`herdr-multiverse` as a public, open-source Rust project from a private
development checkout, so the same shape of work can be repeated for other
projects. Every item below names the file, script, workflow, or command
that does it; nothing here is invented. Anything that could not be
confirmed from a file, a commit, or a read-only `gh` query is listed
explicitly at the end under "Verify manually" instead of being stated as
fact.

## 1. Repo hygiene and required files

- **Dual license**: `LICENSE-MIT` and `LICENSE-APACHE` at the repo root;
  `CONTRIBUTING.md`'s "License" section states contributions are dual
  licensed under both, matching Rust-ecosystem convention.
- **`README.md`**: badges (CI, release, license, MSRV), a "Status" line
  stating the currently released version, and a "Features"/architecture
  summary. The Status line is hand-edited on every release (see section 5).
- **`CONTRIBUTING.md`**: dev setup, the exact required-checks command list
  (kept identical to `AGENTS.md`'s own list), MSRV policy, commit/PR
  conventions (Conventional Commits), and a pointer to the PR template and
  `SECURITY.md`.
- **`SECURITY.md`**: a supported-versions table (pre-1.0 here: only the
  latest tagged release), GitHub private vulnerability reporting
  instructions, response-time targets, and an explicit in/out of scope
  list (what the binary covers vs. upstream tools it launches).
- **`CODE_OF_CONDUCT.md`**: Contributor Covenant, unmodified boilerplate.
- **`SUPPORT.md`**: where to ask questions, how to file a bug, and a
  pointer back to `SECURITY.md` for vulnerabilities instead of a public
  issue.
- **`CHANGELOG.md`**: Keep a Changelog format with an `[Unreleased]`
  section at the top, one `## [X.Y.Z] - date` section per release, and a
  link-reference block at the bottom (`[Unreleased]: .../compare/main...HEAD`,
  `[0.1.N]: .../releases/tag/v0.1.N`, one per released version).
- **`.github/CODEOWNERS`**: a single default-owner rule (`* @<owner>`) —
  the only reviewer-routing mechanism checked into the repo itself (branch
  protection is what gives it teeth; see section 6).
- **`.github/pull_request_template.md`**: a required-checks checklist
  matching the command list in `CONTRIBUTING.md`, a CHANGELOG-entry
  checkbox, a docs-update checkbox, and a "Security / permission review"
  checklist specific to this project (plugin manifest, role read/write/exec
  scope, release build hooks).
- **`.github/ISSUE_TEMPLATE/{bug_report.yml,feature_request.yml,config.yml}`**.
- **`.github/dependabot.yml`**: two `package-ecosystem` blocks (`cargo` and
  `github-actions`), both weekly, both grouping all `patterns: ["*"]`
  updates together; the cargo block additionally restricts the group to
  `minor`/`patch` update types and caps `open-pull-requests-limit` at 10.
- **`rust-toolchain.toml`**: pins the exact channel and components
  (`rustfmt`, `clippy`) every CI job and every contributor build against.
- **`deny.toml`**: `cargo-deny` configuration — `[advisories]`,
  `[licenses]` (an explicit allow-list plus a `confidence-threshold`),
  `[bans]`, and `[sources]` (`unknown-registry`/`unknown-git` both
  `"deny"`). `[graph].targets` is scoped to exactly the four platforms the
  release workflow actually ships (see section 7, commit `0b55cfe`) so the
  dependency graph isn't flagged for platforms nobody builds.
- **`AGENTS.md`** (or the project's equivalent contributor/agent-facing
  conventions file): the authoritative required-checks list and
  commit/scope conventions that every other doc above points back to.

## 2. Scrub: private-names deny list, secrets, and history

- **Private-names deny list** — `scripts/check-private-names.sh`. The
  deny list itself is **not** in the repo: it is an external file (one
  name or regex fragment per line, blank lines and `#` comments ignored)
  that the operator points at via the `MV_PRIVATE_NAMES` environment
  variable. If `MV_PRIVATE_NAMES` is unset, or points at a missing/empty
  file, the script prints a notice to stderr and exits `0` — "skip," not
  "pass." The check itself runs `git grep -niE "$PATTERN"` against the
  tracked tree from the repo root, so it only ever sees committed content.
  CI's `private-names` job in `.github/workflows/ci.yml` runs
  `scripts/check-private-names.sh` with no `MV_PRIVATE_NAMES` set, so that
  job is a structural no-op in CI by design — the check only has teeth
  when an operator exports `MV_PRIVATE_NAMES` locally, or when
  `export-fresh.sh` inherits it (see below).
- **Secrets** — `export-fresh.sh`'s gitleaks step: if `docker` is on
  `PATH`, it runs
  `docker run --rm -v "$OUT_DIR:/export:ro" zricethezav/gitleaks:v8.30.1 dir /export -v`
  against the exported tree; if Docker is unavailable it prints "docker
  not available, skipping gitleaks scan" and continues. `scripts/release/README.md`
  states this plainly: "a successful export with a skipped check does not
  prove that check passed."
- **History** — `export-fresh.sh` never ships any dev-repo commit history.
  It runs `git archive --output=... HEAD` (tracked files at `HEAD` only;
  untracked and gitignored paths can never leak in, by construction of
  `git archive`) into a scratch directory, extracts it, then `git init -b
  main`, `git add -A`, and a single `chore: initial public release`
  commit. That one-commit tree is what gets checked and, for a first
  publish, pushed.
- **Later releases keep public history** — `sync-public.sh` does not
  repeat the fresh-history step against the public remote. It runs
  `export-fresh.sh` into a disposable temp directory to get a
  fully-checked tree, then replaces the *files* of an existing, durable
  public clone with `rsync -a --checksum --delete --exclude=/.git`, and
  commits+tags on top of that clone's own accumulated history. The public
  repo therefore has its own linear history, separate from and shorter
  than the private dev repo's.

## 3. CI and release workflows

- **`.github/workflows/ci.yml`** (push to `main`, and every PR): `fmt`
  and `clippy` run on both `ubuntu-latest` and `macos-latest`; `clippy` and
  `rustdoc` (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked`)
  run with warnings denied; `test` runs `cargo test --workspace --all-features --locked`
  on both OSes; `release-build` runs `scripts/build.sh`, then
  `python3 -B scripts/test_plugin.py` and `python3 -B scripts/release/test_export.py`
  as release-boundary regressions on every PR, not just at release time;
  `msrv` builds with `RUSTUP_TOOLCHAIN` pinned to the `rust-version` in
  `Cargo.toml` to catch MSRV drift; `deny` runs `cargo-deny-action`; and
  `private-names` runs `scripts/check-private-names.sh` (a no-op gate in
  CI, per section 2). GitHub Actions are pinned to full commit SHAs with a
  version comment (e.g. `actions/checkout@<sha> # v7.0.1`), not floating
  tags.
- **`.github/workflows/conformance.yml`**: `python3 -B -m unittest discover -s conformance -p 'test_*.py'`
  verifies the conformance harness can itself detect a failure, then
  `python3 -B conformance/run_all.py --impl rust` builds the CLI and
  replays every recorded scenario against it.
- **`.github/workflows/release.yml`** (triggered on `v*` tag push):
  `verify-version` cross-checks `${GITHUB_REF_NAME#v}` against both
  `Cargo.toml`'s workspace version and `herdr-plugin.toml`'s version
  field and fails the run if any of the three disagree. `build` cross-
  compiles the release matrix (`aarch64-apple-darwin`,
  `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`,
  `aarch64-unknown-linux-gnu`) via `scripts/build.sh <target>`, packages a
  `.tar.gz` plus a `.sha256`. `provenance` runs
  `actions/attest-build-provenance` over the built archives. `release`
  assembles an aggregate `SHA256SUMS`, extracts the matching
  `## [version]` section out of `CHANGELOG.md` with an inline `awk`
  script (falling back to a one-line "Release vX.Y.Z." note if nothing
  matches), and runs `gh release create` with all assets. A
  `concurrency` group keyed on the tag ref, with `cancel-in-progress:
  false`, prevents a re-pushed tag from racing a release already in
  flight.
- **`.github/dependabot.yml`** — see section 1.

## 4. First publish

1. Run `scripts/release/export-fresh.sh OUTPUT_DIR` with
   `GIT_AUTHOR_NAME`, `GIT_AUTHOR_EMAIL`, `GIT_COMMITTER_NAME`, and
   `GIT_COMMITTER_EMAIL` all exported explicitly (these become the single
   export commit's identity; the script hardcodes none of its own). The
   output directory must not already exist with content in it.
2. Before trusting a real export, run
   `python3 -B scripts/release/test_export.py` — it exercises export
   and later-sync behavior against synthetic fixtures (failure detection,
   history/tag/identity preservation, exclusion of untracked/ignored
   files) without touching the real toolchain, so it's fast to run
   repeatedly.
3. `export-fresh.sh` itself never adds a remote and never pushes
   (documented in its own header comment); creating the GitHub repository
   and running the first `git push` is a deliberate, separate, manual
   step the operator takes by hand.
4. Immediately re-clone that pushed repository to a durable location with
   `gh repo clone <owner>/<repo>` — not a scratch or temp directory — and
   use that clone as the target for every later `sync-public.sh` run
   (section 5). A scratch-directory clone used as the release target is a
   known gotcha; see section 7.

## 5. Each release (steady state)

1. Run `scripts/release/collect-changelog.sh` first, so every
   `changelog.d/` fragment (see `changelog.d/README.md`) is merged into
   `[Unreleased]` and its fragment file removed before that section gets
   moved below — a fragment left uncollected would otherwise ship into
   the public tree unchanged, since `export-fresh.sh`'s `git archive`
   exports whatever is still tracked. Then land one version-bump commit
   on the dev repo's trunk: the workspace `[workspace.package].version`
   in `Cargo.toml`, every crate's own pinned version under
   `[workspace.dependencies]` in the same file, `Cargo.lock`'s matching
   entries, `herdr-plugin.toml`'s top-level `version`, the `README.md`
   Status line, and `CHANGELOG.md` (move the `[Unreleased]` entries under
   a new `## [X.Y.Z] - YYYY-MM-DD` heading and add its link reference at
   the bottom). Merge it to `main` before releasing. As a backstop,
   `export-fresh.sh`'s "changelog.d fragments collected" check refuses
   the export (and therefore `sync-public.sh`) if any file besides
   `README.md` is still present in `changelog.d/` at `HEAD`, in case this
   step is skipped. No script performs the `[Unreleased]` → dated-version
   move itself; it stays a hand edit.
2. From the dev repo, with all four `GIT_AUTHOR_*`/`GIT_COMMITTER_*`
   variables exported, run:

   ```sh
   sh scripts/release/sync-public.sh <public-clone> <version>
   ```

   (version without a `v` prefix). This refuses if the public clone is
   dirty, not on `main`, behind or diverged from its own `origin/main`, or
   if a `v<version>` tag already exists there; and it refuses (before any
   check runs) if both manifest versions in the fresh export don't exactly
   match `<version>`.
3. Review the commit and tag `sync-public.sh` created in the public clone
   before publishing anything — it only prepares them locally.
4. From the public clone, run the printed command yourself:

   ```sh
   git push origin main v<version>
   ```

   Treat this push as the one deliberate, human-run step in the whole
   flow (see section 7) — nothing in this repo runs it automatically.
5. Tag the dev repo `v<version>-source` so the private history has a
   matching marker for exactly what tree was published.
6. If the project is also distributed as an installable plugin/package,
   reinstall from the new tag (e.g. `herdr plugin install <owner>/<repo>
   --ref v<version> --yes`), then re-link whatever symlink or `PATH` entry
   points at the installed binary — each install can resolve to a
   different install root — and confirm with the tool's own
   `--version` output.
7. Pushing the tag triggers `.github/workflows/release.yml` automatically,
   which builds, attests, and publishes the GitHub Release with assets
   and `SHA256SUMS`.

## 6. Manual / GitHub settings

Nothing in this repository's scripts or workflows touches these; they are
GitHub repository settings, set by hand, outside of version control.

- **Visibility**: confirmed `PUBLIC` via
  `gh repo view <owner>/<repo> --json visibility`.
- **Default branch**: confirmed `main` via the same query
  (`defaultBranchRef.name`).
- **Issues**: confirmed enabled (`hasIssuesEnabled: true`).
- **Security policy page**: confirmed present
  (`securityPolicyUrl` resolves to `.../security/policy`, backed by
  `SECURITY.md`) — this only confirms the file is recognized as a security
  policy, not that private vulnerability reporting itself is toggled on
  (see "Verify manually").
- **Secret scanning**: confirmed enabled, with push protection also
  enabled, via `gh api repos/<owner>/<repo>` →
  `security_and_analysis.secret_scanning.status: "enabled"` and
  `secret_scanning_push_protection.status: "enabled"`.
  `dependabot_security_updates` and `secret_scanning_validity_checks` were
  both confirmed `"disabled"` in that same query.
- **Branch protection on `main`**: confirmed **not configured** —
  `gh api repos/<owner>/<repo>/branches/main/protection` returns `404
  Branch not protected`. `CODEOWNERS` therefore has no enforcement behind
  it today; a repo that wants required-reviewer enforcement needs branch
  protection (or a ruleset) added by hand.
- **Merge button options**: confirmed via `gh api` — squash, merge-commit,
  and rebase merges are all allowed; "delete branch on merge" is off.
- **Repository push**: the one release-flow step GitHub itself requires a
  human (or a credential outside the automation) for — `git push origin
  main v<version>` against the public clone (section 5, step 4).

## 7. Gotchas we hit

- **A POSIX pipeline hides the first command's exit status.** The
  original export piped `git archive HEAD | tar -x`; a failing `git
  archive` could be masked by a successful `tar -x` on a truncated
  stream. Fixed (commit `f8e8a80`) by staging the archive to a file and
  checking its own exit status before ever extracting it — this is why
  `export-fresh.sh` writes to a `mktemp` file first.
- **Linux-only CI failures that don't reproduce on macOS** (commit
  `7939faf`, discovered on the first public CI run): enumerating `/proc`
  on a shared Linux runner hits other users' (and root's) processes and
  fails closed with a permission error that looks like a flake but isn't
  — the fix skips PIDs owned by a different UID before reading their
  `cwd`. Separately, a conformance test queried `git log main ...` by a
  literal branch name; Git's own default initial branch is `master`
  unless something overrides it, and the two CI hosts' ambient git
  configs disagreed — fixed by querying `HEAD` instead of a branch name.
  Reproducing Linux-only failures required running the suite inside
  Docker **as a non-root user**; a root-run container did not reproduce
  the same permission-shaped bugs as the real CI runner user.
- **A macOS runner label can stop existing mid-project.** The
  `macos-14` label used for `aarch64-apple-darwin` in
  `.github/workflows/release.yml` stopped being backed by a live runner;
  the job just hung cancelled with nothing assigned. Fixed by moving to
  `macos-15` (same commit, `7939faf`).
- **`cargo-deny`'s dependency graph defaults to every target it knows
  about.** Scoping `deny.toml`'s `[graph].targets` to exactly the four
  shipped release targets (commit `0b55cfe`) avoids false positives on
  platform-specific dependencies nobody actually ships, without changing
  any license allowance or advisory exception.
- **In-repo test fixtures can collide with the deny list.** A Nexus-only
  interactive test fixture used an account name that happened to be on
  the operator's `MV_PRIVATE_NAMES` list; the fix (commit `5935b4c`) was
  simply renaming the fixture. The general shape: a private-names
  false-positive can come from your own fixtures, not just from leaked
  real data.
- **Docker being down blocks the only secrets scan.** When the operator's
  Docker was down for maintenance, the gitleaks step in `export-fresh.sh`
  silently skipped (it only runs `if command -v docker`). The release
  notes for that cycle record running gitleaks through a separate,
  checksum-verified Docker shim instead of skipping outright — a reminder
  that a green export with Docker absent is not evidence gitleaks ran.
  Homebrew's locally-installed gitleaks (a newer version than the pinned
  `v8.21.2` image) was also observed to flag the redaction module's own
  test placeholders as false positives — a locally-installed scanner can
  disagree with the pinned one used by the script.
- **Every plugin reinstall changes the install root.** Reinstalling from
  a new release tag resolves to a new install directory each time, so
  anything that hardcoded the previous install's path (a symlink, a
  shell alias) silently keeps pointing at the old binary until re-linked.
- **The version bump is five hand-edited files, checked only after the
  tag is pushed.** `Cargo.toml` (workspace version plus every crate's own
  pin), `Cargo.lock`, `herdr-plugin.toml`, `README.md`, and
  `CHANGELOG.md` all have to agree; `release.yml`'s `verify-version` job
  only catches a mismatch once the tag is already on GitHub, not before.

## 8. What we'd do differently

- Automate the version bump (`Cargo.toml` + `Cargo.lock` +
  `herdr-plugin.toml` + `README.md` Status line + `CHANGELOG.md` section
  move) instead of hand-editing five files from a commit template every
  release; `verify-version` currently only catches a mismatch after the
  tag has already been pushed.
- Don't let the only secrets scan silently no-op when Docker is absent.
  `export-fresh.sh` prints a one-line notice and continues; a missing
  scanner should either fail the export or be much louder than a note
  easy to miss in long build output.
- Enable branch protection on `main` (and decide whether it should
  require review from `CODEOWNERS`) before or at first publish, rather
  than leaving it as an open gap discovered later — it was still unset as
  of the state recorded in section 6.
- Run the private-names check in CI with a deny list actually populated,
  rather than relying entirely on an operator remembering to export
  `MV_PRIVATE_NAMES` locally before every push; as configured today, CI's
  `private-names` job always passes regardless of what the tree contains.
- Treat the final `git push origin main v<version>` as deliberately
  human-only, and say so up front in the docs for anyone adopting this
  flow, rather than having an operator discover it only when an
  automated run refuses to perform it.

## Adapting for a private, proprietary repo

- **License files**: this project ships a dual MIT/Apache-2.0 license
  because it is open source. A private, proprietary repo needs its own
  license terms (or no license file, if the repo is "all rights
  reserved" under its employer/company agreement) instead of copying
  `LICENSE-MIT`/`LICENSE-APACHE` verbatim — update `CONTRIBUTING.md`'s
  license section to match whatever the repo actually grants.
- **No public export step needed.** Sections 2 and 4's export/scrub/sync
  machinery (`export-fresh.sh`, `sync-public.sh`, `test_export.py`,
  history-free fresh export) exists specifically because this project
  keeps a separate, durable public mirror with its own independent
  history. A single private repo with no public mirror does not need any
  of those three files — commit and tag directly on the one repo instead.
- **Parts that still apply regardless of visibility**: the CI workflow
  shape (`fmt`/`clippy`/`test`/`rustdoc`/`deny` across the matrix you
  ship), the conformance-style regression workflow if the project has an
  equivalent oracle, `CHANGELOG.md` discipline, the PR template's
  required-checks and security/permission-review checklist, `CODEOWNERS`,
  `dependabot.yml`, and the tag-triggered build-then-`gh release create`
  shape in `release.yml` (confirm each pinned third-party Action's own
  terms cover use on a private repo).
- **`check-private-names.sh` can still be worth keeping** on a private
  repo if it has its own names that must never reach a further-downstream
  artifact or a different, less-trusted audience (a client name, an
  internal codename) — point `MV_PRIVATE_NAMES` at whatever list applies
  there.
- **Private visibility**: confirm it the same way this repo's public
  visibility was confirmed — `gh repo view <owner>/<repo> --json
  visibility` should read `PRIVATE`. Re-check secret scanning, push
  protection, and Dependabot settings the same way (section 6); GitHub's
  plan-level availability of those features on private repos can differ
  by account/org type — verify per org rather than assuming parity with
  this public repo.
- **`CODEOWNERS`** is equally useful on a private repo, but — as noted in
  section 6 — it only enforces anything once branch protection (or a
  ruleset) on that repo actually requires code-owner review; the file
  alone does not gate a merge.
- **`SECURITY.md`/`SUPPORT.md` wording**: the "do not open a public
  issue" framing assumes outside, anonymous reporters; keep the private
  reporting mechanism (it works identically on a private repo with
  outside collaborators) but adjust language that assumes a fully public
  audience.

## Verify manually

Not confirmed from any file, commit, or the read-only `gh` queries run
for this checklist — look these up directly rather than assuming the
value below (or the value for this repo) carries over:

- Whether GitHub's **private vulnerability reporting** feature itself
  (distinct from secret scanning) is toggled on for the repo —
  `securityPolicyUrl` only confirms `SECURITY.md` is recognized as a
  security policy document, not that the reporting feature is enabled.
- Whether **GitHub Discussions** is enabled — `SUPPORT.md` references it
  conditionally ("If this repository has Discussions enabled"); the
  queried repo fields did not cover this.
- **Repository topics** — the read-only query returned `null`.
- Whether **Dependabot alerts / security updates** are turned on at the
  repository level, as opposed to `dependabot.yml` merely configuring
  update PRs; this is a separate repo-level toggle not covered by the
  file alone.
- The **collaborator/team access model** beyond `CODEOWNERS` (org teams,
  outside collaborators, who can push directly to `main` given branch
  protection is currently unset) — not visible in any file or in the
  read-only queries run here.
- Any **required-status-checks or rulesets** a downstream adopter adds to
  `main` — this repo currently has none (branch protection returned
  `404`), so there is nothing here to model; look up GitHub's current
  branch-protection/rulesets UI directly when adding one.

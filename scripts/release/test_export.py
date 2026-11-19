#!/usr/bin/env python3
"""Fast release regressions; build/conformance/scan commands are fixtures."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("export-fresh.sh")


class ExportFixture(unittest.TestCase):
    """Shared fixture: a synthetic source checkout plus fixture `git`,
    `cargo`, `python3`, and `gitleaks` stand-ins on PATH, so export/sync
    behavior can be exercised without a real build, conformance run, or
    scan."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mv-export-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.source = self.work / "source checkout's files"
        (self.source / "scripts/release").mkdir(parents=True)
        for name in ("export-fresh.sh", "sync-public.sh", "gitleaks-version.sh"):
            shutil.copy2(SCRIPT.with_name(name), self.source / "scripts/release" / name)
        (self.source / "README.md").write_text("public source\n")
        (self.source / "Cargo.toml").write_text('[workspace.package]\nversion = "0.2.0"\n')
        (self.source / "herdr-plugin.toml").write_text('version = "0.2.0"\n')
        (self.source / ".gitignore").write_text(".multiverse/\nevidence/\nconfig.toml\ntarget/\n")
        check = self.source / "scripts/check-private-names.sh"
        check.write_text(
            '#!/bin/sh\n'
            'printf "private-names %s\\n" "$(pwd)" >> "$CHECK_LOG"\n'
            '[ "${FAIL_CHECK:-}" != private-names ]\n'
        )
        check.chmod(0o755)
        tools = self.work / "tools"
        tools.mkdir()
        for name in ("dirname", "find", "mkdir", "mktemp", "rm", "rsync", "sh", "tar"):
            (tools / name).symlink_to(shutil.which(name))
        git = tools / "git"
        git.write_text(
            '#!/bin/sh\n'
            'if [ "${FAIL_STATUS:-}" = 1 ] && [ "${3:-}" = status ]; then\n'
            '    echo "fixture status failure" >&2\n    exit 42\nfi\n'
            '"$REAL_GIT" "$@"\nrc=$?\n'
            'if [ "$1" = archive ] && [ "${FAIL_ARCHIVE:-}" = 1 ] && [ "$rc" -eq 0 ]; then\n'
            '    exit 41\nfi\nexit "$rc"\n'
        )
        git.chmod(0o755)
        for name in ("cargo", "python3"):
            tool = tools / name
            tool.write_text(
                '#!/bin/sh\n'
                'if [ "$1" = - ]; then exec "$REAL_PYTHON" "$@"; fi\n'
                f'check="{name} $*"\n'
                'printf "%s\\n" "$check" >> "$CHECK_LOG"\n'
                'mkdir -p target\nprintf "artifact\\n" > target/build-output\n'
                'printf "artifact\\n" > untracked-check-output\n'
                '[ "${FAIL_CHECK:-}" != "$check" ]\n'
            )
            tool.chmod(0o755)
        scanner = self.work / "scanner"
        scanner.mkdir()
        gitleaks = scanner / "gitleaks"
        gitleaks.write_text(
            '#!/bin/sh\n'
            'if [ "$1" = version ]; then printf "8.30.1\\n"; exit 0; fi\n'
            'printf "gitleaks %s\\n" "$*" >> "$CHECK_LOG"\n'
            '[ "${FAIL_CHECK:-}" != gitleaks ]\n'
        )
        gitleaks.chmod(0o755)
        self.tools_dir = tools
        self.scanner_dir = scanner
        self.env = {
            "PATH": f"{tools}:{scanner}",
            "HOME": str(self.work),
            "TMPDIR": str(self.work),
            "REAL_GIT": shutil.which("git"),
            "REAL_PYTHON": sys.executable,
            "CHECK_LOG": str(self.work / "checks.log"),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_AUTHOR_NAME": "Export Test",
            "GIT_AUTHOR_EMAIL": "export-test@example.invalid",
            "GIT_COMMITTER_NAME": "Export Test",
            "GIT_COMMITTER_EMAIL": "export-test@example.invalid",
        }
        self.git(self.source, "init", "-q", "-b", "main")
        self.git(self.source, "add", "-A")
        self.git(self.source, "commit", "-q", "-m", "source fixture")
        self.output = self.work / "public export's files"

    def git(self, cwd, *args):
        """Run the real `git` (not the fixture one on PATH) against `cwd`
        and return its stripped stdout."""
        return subprocess.check_output(
            [self.env["REAL_GIT"], *args], cwd=cwd, env=self.env, text=True,
        ).strip()

    def export(self):
        """Run export-fresh.sh against the fixture source, writing to
        self.output."""
        return subprocess.run(
            ["sh", str(self.source / "scripts/release/export-fresh.sh"), str(self.output)],
            env=self.env, text=True, capture_output=True, check=False,
        )


class ExportTests(ExportFixture):
    """export-fresh.sh against the fixture source checkout."""

    def test_success_exports_only_head_with_fresh_history_and_no_remote(self):
        """Uncommitted, untracked, and gitignored paths never reach the
        export; the result is a single fresh commit on main with no
        remote, and the source checkout's own git config is untouched."""
        original_config = (self.source / ".git/config").read_bytes()
        (self.source / "README.md").write_text("uncommitted change\n")
        for name in (".multiverse/private", "evidence/private", "config.toml", "untracked"):
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("must stay local\n")
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((self.output / "README.md").read_text(), "public source\n")
        for name in (".multiverse", "evidence", "config.toml", "untracked"):
            self.assertFalse((self.output / name).exists(), name)
        self.assertEqual(self.git(self.output, "rev-list", "--count", "HEAD"), "1")
        self.assertEqual(self.git(self.output, "branch", "--show-current"), "main")
        self.assertEqual(self.git(self.output, "remote"), "")
        self.assertEqual((self.source / ".git/config").read_bytes(), original_config)

    def test_archive_failure_after_valid_output_stops_before_commit_or_checks(self):
        """A `git archive` failure reported only after it already produced
        readable output still aborts before the new repo is committed or
        any check runs, and leaves no temp archive behind."""
        self.env["FAIL_ARCHIVE"] = "1"
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("could not archive", result.stderr)
        self.assertFalse((self.output / ".git").exists())
        self.assertFalse((self.work / "checks.log").exists())
        self.assertEqual(list(self.work.glob("multiverse-export.*")), [])

    def test_wrong_local_gitleaks_version_refused(self):
        """A local gitleaks older than the pinned minimum fails the export
        and names both the found and required versions."""
        gitleaks = self.scanner_dir / "gitleaks"
        gitleaks.write_text(
            '#!/bin/sh\nif [ "$1" = version ]; then printf "8.18.1\\n"; exit 0; fi\nexit 1\n'
        )
        gitleaks.chmod(0o755)
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("8.18.1", result.stderr)
        self.assertIn("8.30.1", result.stderr)
        self.assertIn("overall: FAIL", result.stdout)

    def test_newer_local_gitleaks_version_accepted(self):
        """A local gitleaks newer than the pin is accepted, not just an
        exact match."""
        # Minimum-version, not exact-match: a locally-installed gitleaks
        # newer than the pin (as brew's continuously-updated formula will be
        # between pin bumps) must still be allowed to run the export.
        gitleaks = self.scanner_dir / "gitleaks"
        gitleaks.write_text(
            '#!/bin/sh\n'
            'if [ "$1" = version ]; then printf "9.0.0\\n"; exit 0; fi\n'
            'printf "gitleaks %s\\n" "$*" >> "$CHECK_LOG"\n'
            '[ "${FAIL_CHECK:-}" != gitleaks ]\n'
        )
        gitleaks.chmod(0o755)
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.work / "checks.log").read_text().splitlines()[-1],
            f"gitleaks dir {self.output} --config {self.output}/.gitleaks.toml -v",
        )

    def test_uses_docker_when_local_gitleaks_absent(self):
        """With no local gitleaks binary on PATH but docker available, the
        export falls back to the containerized scanner."""
        docker_dir = self.work / "docker-tools"
        docker_dir.mkdir()
        docker = docker_dir / "docker"
        docker.write_text(
            '#!/bin/sh\n'
            'printf "gitleaks %s\\n" "$*" >> "$CHECK_LOG"\n'
            '[ "${FAIL_CHECK:-}" != gitleaks ]\n'
        )
        docker.chmod(0o755)
        self.env["PATH"] = f"{self.tools_dir}:{docker_dir}"
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(
            (self.work / "checks.log").read_text().splitlines()[-1],
            f"gitleaks run --rm -v {self.output}:/export:ro zricethezav/gitleaks:v8.30.1 "
            "dir /export --config /export/.gitleaks.toml -v",
        )

    def test_scanner_missing_export_fails(self):
        """With neither gitleaks nor docker on PATH, the export fails
        outright rather than silently skipping the secrets scan."""
        self.env["PATH"] = str(self.tools_dir)
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("install", result.stderr.lower())
        self.assertIn("overall: FAIL", result.stdout)

    def test_leftover_changelog_fragment_refuses_export(self):
        """A changelog.d/ fragment still tracked at HEAD -- a release that
        skipped scripts/release/collect-changelog.sh -- refuses the export
        instead of shipping the fragment into the public tree."""
        (self.source / "changelog.d").mkdir()
        (self.source / "changelog.d/README.md").write_text("fragment docs\n")
        (self.source / "changelog.d/20261118-test.added.md").write_text("- test fragment\n")
        self.git(self.source, "add", "-A")
        self.git(self.source, "commit", "-q", "-m", "add uncollected fragment")
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("changelog.d", result.stderr)
        self.assertIn("20261118-test.added.md", result.stderr)
        self.assertIn("FAIL: changelog.d fragments collected", result.stdout)
        self.assertIn("overall: FAIL", result.stdout)

    def test_changelog_d_with_only_readme_passes(self):
        """changelog.d/ holding only README.md -- the fully-collected
        state -- does not trip the uncollected-fragment guard."""
        (self.source / "changelog.d").mkdir()
        (self.source / "changelog.d/README.md").write_text("fragment docs\n")
        self.git(self.source, "add", "-A")
        self.git(self.source, "commit", "-q", "-m", "changelog.d with readme only")
        result = self.export()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("PASS: changelog.d fragments collected", result.stdout)

    def test_check_phase_modifying_a_committed_file_refuses_the_export(self):
        """A build/test/conformance/scan step that edits an already-
        committed file -- rather than only producing new, disposable build
        output -- fails the export even though every individual check it
        ran still reported success on its own."""
        cargo = self.tools_dir / "cargo"
        cargo.write_text(
            '#!/bin/sh\n'
            'printf "modified during checks\\n" > README.md\n'
            'mkdir -p target\nprintf "artifact\\n" > target/build-output\n'
            'exit 0\n'
        )
        cargo.chmod(0o755)
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("FAIL: tracked tree unchanged by checks", result.stdout)
        self.assertIn("overall: FAIL", result.stdout)


class SyncTests(ExportFixture):
    """sync-public.sh against a fixture public clone with an existing
    v0.1.0 release, cloned from a local bare origin (no network)."""

    def setUp(self):
        super().setUp()
        self.origin = self.work / "origin.git"
        self.git(self.work, "init", "--bare", "-q", "-b", "main", str(self.origin))
        self.git(self.work, "clone", "-q", str(self.origin), str(self.output))
        (self.output / "README.md").write_text("old release\n")
        (self.output / "obsolete.txt").write_text("remove at next release\n")
        self.git(self.output, "add", "-A")
        self.git(self.output, "commit", "-q", "-m", "initial public release")
        self.git(self.output, "tag", "-a", "v0.1.0", "-m", "initial release")
        self.git(self.output, "push", "-q", "origin", "main", "v0.1.0")
        self.initial_head = self.git(self.output, "rev-parse", "HEAD")
        self.initial_tag = self.git(self.output, "rev-parse", "v0.1.0")

    def sync(self, version="0.2.0", target=None):
        """Run sync-public.sh against the fixture public clone (or
        `target`, when the test needs a different directory)."""
        return subprocess.run(
            ["sh", str(self.source / "scripts/release/sync-public.sh"),
             str(self.output if target is None else target), version],
            env=self.env, text=True, capture_output=True, check=False,
        )

    def assert_unchanged(self):
        """Assert the public clone's history, tags, and working tree are
        exactly as `setUp` left them, and no temp export/sync dir survived."""
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD"), self.initial_head)
        self.assertEqual(self.git(self.output, "rev-parse", "v0.1.0"), self.initial_tag)
        self.assertEqual(self.git(self.output, "tag", "--list", "v0.2.0"), "")
        self.assertEqual((self.output / "README.md").read_text(), "old release\n")
        self.assertTrue((self.output / "obsolete.txt").exists())
        self.assertEqual(list(self.work.glob("multiverse-sync.*")), [])
        self.assertEqual(list(self.work.glob("multiverse-export.*")), [])

    def test_success_preserves_history_tags_identity_and_only_prints_push(self):
        """A full sync adds exactly one commit and tag on top of the
        existing release history, uses the supplied identity, replaces the
        tracked tree, pushes nothing, and only prints the push command."""
        config = (self.output / ".git/config").read_bytes()
        source_config = (self.source / ".git/config").read_bytes()
        self.env["GIT_AUTHOR_NAME"] = "Release Author"
        self.env["GIT_COMMITTER_NAME"] = "Release Committer"
        (self.source / "README.md").write_text("uncommitted source change\n")
        for name in ("untracked", ".multiverse/private", "config.toml"):
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("local only\n")
        result = self.sync()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD^"), self.initial_head)
        self.assertEqual(self.git(self.output, "rev-list", "--count", "HEAD"), "2")
        self.assertEqual(self.git(self.output, "rev-parse", "v0.1.0"), self.initial_tag)
        self.assertEqual(self.git(self.output, "cat-file", "-t", "v0.2.0"), "tag")
        self.assertEqual(self.git(self.output, "rev-parse", "v0.2.0^{}"),
                         self.git(self.output, "rev-parse", "HEAD"))
        self.assertEqual(self.git(self.output, "log", "-1", "--format=%s"), "release: v0.2.0")
        self.assertEqual(self.git(self.output, "log", "-1", "--format=%an|%ae|%cn|%ce"),
                         "Release Author|export-test@example.invalid|"
                         "Release Committer|export-test@example.invalid")
        self.assertEqual(self.git(self.output, "for-each-ref", "--format=%(taggername)",
                                  "refs/tags/v0.2.0"), "Release Committer")
        self.assertEqual(self.git(self.output, "branch", "--show-current"), "main")
        self.assertEqual((self.output / "README.md").read_text(), "public source\n")
        self.assertFalse((self.output / "obsolete.txt").exists())
        for name in ("untracked", ".multiverse", "config.toml", "target", "untracked-check-output"):
            self.assertFalse((self.output / name).exists(), name)
        self.assertEqual(self.git(self.output, "status", "--porcelain"), "")
        self.assertEqual((self.output / ".git/config").read_bytes(), config)
        self.assertEqual((self.source / ".git/config").read_bytes(), source_config)
        self.assertEqual(self.git(self.origin, "rev-parse", "main"), self.initial_head)
        self.assertEqual(self.git(self.origin, "tag", "--list", "v0.2.0"), "")
        self.assertEqual(result.stdout.splitlines()[-1], "git push origin main v0.2.0")
        log_lines = (self.work / "checks.log").read_text().splitlines()
        self.assertEqual(log_lines[:3], [
            "cargo build --locked", "cargo test --workspace --locked",
            "python3 conformance/run_all.py",
        ])
        self.assertEqual(len(log_lines), 5)
        # The inner export-fresh.sh run scans a disposable mktemp directory,
        # not self.output -- assert the private-names cwd and the gitleaks
        # scan dir/--config agree with each other (and are not the public
        # clone itself), so a regression that drops --config or points
        # either check at the wrong directory would fail here.
        private_names_line, gitleaks_line = log_lines[3], log_lines[4]
        self.assertTrue(private_names_line.startswith("private-names "))
        scanned_dir = private_names_line[len("private-names "):]
        self.assertNotEqual(scanned_dir, str(self.output))
        self.assertEqual(
            gitleaks_line,
            f"gitleaks dir {scanned_dir} --config {scanned_dir}/.gitleaks.toml -v",
        )
        self.assertEqual(list(self.work.glob("multiverse-sync.*")), [])

    def test_dirty_clone_refused_before_export(self):
        """An unstaged, staged, or untracked change in the public clone
        refuses the sync before the (costly) export ever runs."""
        for kind in ("unstaged", "staged", "untracked"):
            with self.subTest(kind=kind):
                dirty = self.output / ("untracked" if kind == "untracked" else "README.md")
                dirty.write_text("keep this edit\n")
                if kind == "staged":
                    self.git(self.output, "add", "README.md")
                result = self.sync()
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("uncommitted changes", result.stderr)
                self.assertEqual(dirty.read_text(), "keep this edit\n")
                self.assertFalse((self.work / "checks.log").exists())
                self.git(self.output, "reset", "--hard", "HEAD")
                if kind == "untracked":
                    dirty.unlink()
                self.assert_unchanged()

    def test_ahead_main_preserved_and_same_size_timestamp_content_replaced(self):
        """A local commit ahead of origin/main is preserved, and content
        is still replaced even when the stale file matches the new one's
        size and mtime (rsync --checksum, not a quick size/time compare)."""
        readme = self.output / "README.md"
        readme.write_text("stale content\n")
        self.assertEqual(readme.stat().st_size, (self.source / "README.md").stat().st_size)
        self.git(self.output, "add", "README.md")
        self.git(self.output, "commit", "-q", "-m", "local commit ahead of origin")
        ahead = self.git(self.output, "rev-parse", "HEAD")
        stamp = int(self.git(self.source, "log", "-1", "--format=%ct"))
        os.utime(readme, (stamp, stamp))
        result = self.sync()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(readme.read_text(), "public source\n")
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD^"), ahead)
        self.assertEqual(self.git(self.origin, "rev-parse", "main"), self.initial_head)

    def test_branch_and_detached_head_refused(self):
        """A public clone on a topic branch or in detached-HEAD state is
        refused; only main is a valid target."""
        for branch in ("topic", "--detach"):
            with self.subTest(branch=branch):
                if branch == "topic":
                    self.git(self.output, "checkout", "-q", "-b", branch)
                else:
                    self.git(self.output, "checkout", "-q", "--detach")
                result = self.sync()
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("must be on main", result.stderr)
                self.assertFalse((self.work / "checks.log").exists())
                self.assert_unchanged()
                self.git(self.output, "checkout", "-q", "main")

    def test_status_command_failure_stops_before_export(self):
        """A failing `git status` in the public clone aborts the sync with
        its own error before the export runs, leaving the clone untouched."""
        self.env["FAIL_STATUS"] = "1"
        result = self.sync()
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertIn("fixture status failure", result.stderr)
        self.assertFalse((self.work / "checks.log").exists())
        self.assert_unchanged()

    def test_behind_and_diverged_main_refused_after_fetch(self):
        """The sync fetches origin/main fresh before comparing, and
        refuses both a clone that is behind it and one that has since
        diverged from it."""
        other = self.work / "other"
        self.git(self.work, "clone", "-q", str(self.origin), str(other))
        (other / "remote-file").write_text("new remote commit\n")
        self.git(other, "add", "-A")
        self.git(other, "commit", "-q", "-m", "remote advanced")
        self.git(other, "push", "-q", "origin", "main")
        self.assertEqual(self.git(self.output, "rev-parse", "origin/main"), self.initial_head)
        result = self.sync()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("behind origin/main", result.stderr)
        self.assert_unchanged()
        (self.output / "local-file").write_text("local divergence\n")
        self.git(self.output, "add", "-A")
        self.git(self.output, "commit", "-q", "-m", "local advanced")
        local_head = self.git(self.output, "rev-parse", "HEAD")
        result = self.sync()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("has diverged", result.stderr)
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD"), local_head)
        self.assertFalse((self.work / "checks.log").exists())

    def test_each_export_check_failure_keeps_clone_unchanged(self):
        """A failure in any one of the export's checks (build, test,
        conformance, private-names, gitleaks) leaves the public clone
        exactly as it was."""
        for check in ("cargo build --locked", "cargo test --workspace --locked",
                      "python3 conformance/run_all.py", "private-names", "gitleaks"):
            with self.subTest(check=check):
                self.env["FAIL_CHECK"] = check
                result = self.sync()
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("overall: FAIL", result.stdout)
                self.assert_unchanged()

    def test_check_phase_modifying_a_committed_file_keeps_clone_unchanged(self):
        """A build/test/conformance/scan step inside the disposable inner
        export that edits an already-committed file is refused before the
        public clone's tracked tree is ever replaced -- the scenario
        sync-public.sh's own `git clean` (untracked/ignored paths only)
        cannot catch on its own."""
        cargo = self.tools_dir / "cargo"
        cargo.write_text(
            '#!/bin/sh\n'
            'printf "modified during checks\\n" > README.md\n'
            'exit 0\n'
        )
        cargo.chmod(0o755)
        result = self.sync()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("overall: FAIL", result.stdout)
        self.assert_unchanged()

    def test_archive_failure_keeps_clone_unchanged(self):
        """An archive failure inside the inner export-fresh.sh run leaves
        the public clone exactly as it was."""
        self.env["FAIL_ARCHIVE"] = "1"
        result = self.sync()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("could not archive", result.stderr)
        self.assertFalse((self.work / "checks.log").exists())
        self.assert_unchanged()

    def test_both_manifest_versions_must_match_argument(self):
        """A mismatch between the requested version and either manifest's
        own version (Cargo.toml or herdr-plugin.toml) refuses the sync."""
        for name in ("Cargo.toml", "herdr-plugin.toml"):
            with self.subTest(manifest=name):
                path = self.source / name
                original = path.read_text()
                path.write_text(original.replace("0.2.0", "0.3.0"))
                self.git(self.source, "add", name)
                self.git(self.source, "commit", "-q", "-m", "mismatched version")
                result = self.sync()
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("version mismatch", result.stderr)
                self.assert_unchanged()
                path.write_text(original)
                self.git(self.source, "add", name)
                self.git(self.source, "commit", "-q", "-m", "restore version")

    def test_existing_tag_refused(self):
        """A public clone that already has the requested release's tag is
        refused rather than overwriting it."""
        self.git(self.output, "tag", "-a", "v0.2.0", "-m", "already released")
        tag = self.git(self.output, "rev-parse", "v0.2.0")
        result = self.sync()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(self.git(self.output, "rev-parse", "v0.2.0"), tag)
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD"), self.initial_head)
        self.assertFalse((self.work / "checks.log").exists())

    def test_missing_identity_and_invalid_version_refused(self):
        """Any missing git identity variable, or a version string that
        isn't a bare, unprefixed release version, refuses the sync."""
        for name in ("GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL",
                     "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"):
            with self.subTest(identity=name):
                value = self.env.pop(name)
                result = self.sync()
                self.env[name] = value
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assertIn("must all be set", result.stderr)
                self.assert_unchanged()
        for version in ("", "v0.2.0", "0.2.0;echo", "../main"):
            with self.subTest(version=version):
                result = self.sync(version)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assert_unchanged()
        self.assertFalse((self.work / "checks.log").exists())

    def test_source_checkout_and_subdirectory_targets_refused(self):
        """The public clone target must be separate from the source
        checkout: the source itself, and a path nested inside either
        checkout, are both refused."""
        for target in (self.source, self.output / ".git", self.source / "scripts"):
            with self.subTest(target=target):
                result = self.sync(target=target)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assert_unchanged()
        self.assertFalse((self.work / "checks.log").exists())


if __name__ == "__main__":
    unittest.main()

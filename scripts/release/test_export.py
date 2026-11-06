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
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mv-export-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.source = self.work / "source checkout's files"
        (self.source / "scripts/release").mkdir(parents=True)
        for name in ("export-fresh.sh", "sync-public.sh"):
            shutil.copy2(SCRIPT.with_name(name), self.source / "scripts/release" / name)
        (self.source / "README.md").write_text("public source\n")
        (self.source / "Cargo.toml").write_text('[workspace.package]\nversion = "0.2.0"\n')
        (self.source / "herdr-plugin.toml").write_text('version = "0.2.0"\n')
        (self.source / ".gitignore").write_text(".multiverse/\nevidence/\nconfig.toml\ntarget/\n")
        check = self.source / "scripts/check-private-names.sh"
        check.write_text(
            '#!/bin/sh\nprintf "private-names\\n" >> "$CHECK_LOG"\n'
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
        self.env = {
            "PATH": str(tools),
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
        return subprocess.check_output(
            [self.env["REAL_GIT"], *args], cwd=cwd, env=self.env, text=True,
        ).strip()

    def export(self):
        return subprocess.run(
            ["sh", str(self.source / "scripts/release/export-fresh.sh"), str(self.output)],
            env=self.env, text=True, capture_output=True, check=False,
        )


class ExportTests(ExportFixture):
    def test_success_exports_only_head_with_fresh_history_and_no_remote(self):
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
        self.env["FAIL_ARCHIVE"] = "1"
        result = self.export()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("could not archive", result.stderr)
        self.assertFalse((self.output / ".git").exists())
        self.assertFalse((self.work / "checks.log").exists())
        self.assertEqual(list(self.work.glob("multiverse-export.*")), [])


class SyncTests(ExportFixture):
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
        docker = self.work / "tools/docker"
        docker.write_text(
            '#!/bin/sh\nprintf "gitleaks\\n" >> "$CHECK_LOG"\n'
            '[ "${FAIL_CHECK:-}" != gitleaks ]\n'
        )
        docker.chmod(0o755)

    def sync(self, version="0.2.0", target=None):
        return subprocess.run(
            ["sh", str(self.source / "scripts/release/sync-public.sh"),
             str(self.output if target is None else target), version],
            env=self.env, text=True, capture_output=True, check=False,
        )

    def assert_unchanged(self):
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD"), self.initial_head)
        self.assertEqual(self.git(self.output, "rev-parse", "v0.1.0"), self.initial_tag)
        self.assertEqual(self.git(self.output, "tag", "--list", "v0.2.0"), "")
        self.assertEqual((self.output / "README.md").read_text(), "old release\n")
        self.assertTrue((self.output / "obsolete.txt").exists())
        self.assertEqual(list(self.work.glob("multiverse-sync.*")), [])
        self.assertEqual(list(self.work.glob("multiverse-export.*")), [])

    def test_success_preserves_history_tags_identity_and_only_prints_push(self):
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
        self.assertEqual((self.work / "checks.log").read_text().splitlines(), [
            "cargo build --locked", "cargo test --workspace --locked",
            "python3 conformance/run_all.py", "private-names", "gitleaks",
        ])
        self.assertEqual(list(self.work.glob("multiverse-sync.*")), [])

    def test_dirty_clone_refused_before_export(self):
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
        self.env["FAIL_STATUS"] = "1"
        result = self.sync()
        self.assertEqual(result.returncode, 42, result.stdout + result.stderr)
        self.assertIn("fixture status failure", result.stderr)
        self.assertFalse((self.work / "checks.log").exists())
        self.assert_unchanged()

    def test_behind_and_diverged_main_refused_after_fetch(self):
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
        for check in ("cargo build --locked", "cargo test --workspace --locked",
                      "python3 conformance/run_all.py", "private-names", "gitleaks"):
            with self.subTest(check=check):
                self.env["FAIL_CHECK"] = check
                result = self.sync()
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("overall: FAIL", result.stdout)
                self.assert_unchanged()

    def test_archive_failure_keeps_clone_unchanged(self):
        self.env["FAIL_ARCHIVE"] = "1"
        result = self.sync()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("could not archive", result.stderr)
        self.assertFalse((self.work / "checks.log").exists())
        self.assert_unchanged()

    def test_both_manifest_versions_must_match_argument(self):
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
        self.git(self.output, "tag", "-a", "v0.2.0", "-m", "already released")
        tag = self.git(self.output, "rev-parse", "v0.2.0")
        result = self.sync()
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("already exists", result.stderr)
        self.assertEqual(self.git(self.output, "rev-parse", "v0.2.0"), tag)
        self.assertEqual(self.git(self.output, "rev-parse", "HEAD"), self.initial_head)
        self.assertFalse((self.work / "checks.log").exists())

    def test_missing_identity_and_invalid_version_refused(self):
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
        for target in (self.source, self.output / ".git", self.source / "scripts"):
            with self.subTest(target=target):
                result = self.sync(target=target)
                self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
                self.assert_unchanged()
        self.assertFalse((self.work / "checks.log").exists())


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Fast fresh-export regressions; build/conformance commands are fixtures."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("export-fresh.sh")


class ExportTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mv-export-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.source = self.work / "source checkout's files"
        (self.source / "scripts/release").mkdir(parents=True)
        shutil.copy2(SCRIPT, self.source / "scripts/release/export-fresh.sh")
        (self.source / "README.md").write_text("public source\n")
        (self.source / ".gitignore").write_text(".multiverse/\nevidence/\nconfig.toml\n")
        check = self.source / "scripts/check-private-names.sh"
        check.write_text("#!/bin/sh\nexit 0\n")
        check.chmod(0o755)
        tools = self.work / "tools"
        tools.mkdir()
        for name in ("dirname", "find", "mkdir", "mktemp", "rm", "sh", "tar"):
            (tools / name).symlink_to(shutil.which(name))
        git = tools / "git"
        git.write_text(
            '#!/bin/sh\n"$REAL_GIT" "$@"\nrc=$?\n'
            'if [ "$1" = archive ] && [ "${FAIL_ARCHIVE:-}" = 1 ] && [ "$rc" -eq 0 ]; then\n'
            '    exit 41\nfi\nexit "$rc"\n'
        )
        git.chmod(0o755)
        for name in ("cargo", "python3"):
            tool = tools / name
            tool.write_text('#!/bin/sh\nprintf "check\\n" >> "$CHECK_LOG"\n')
            tool.chmod(0o755)
        self.env = {
            "PATH": str(tools),
            "HOME": str(self.work),
            "TMPDIR": str(self.work),
            "REAL_GIT": shutil.which("git"),
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


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""pre-push private-names and gitleaks regressions.

Run with: python3 scripts/hooks/test_pre_push.py
"""

import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
SECRET = "ExampleSecretName"
LIST_ONLY = "ListOnlyTokenNeverInRepo_9f3c"
SKIP_LINE = "pre-push: private-names check skipped"
GITLEAKS_VERSION_SCRIPT = REPO / "scripts/release/gitleaks-version.sh"
PINNED_GITLEAKS_VERSION = re.search(
    r"^GITLEAKS_VERSION=(\S+)$", GITLEAKS_VERSION_SCRIPT.read_text(), re.MULTILINE
).group(1)


class PrePushPrivateNamesTests(unittest.TestCase):
    """scripts/hooks/pre-push regressions: the gitleaks version/availability
    gate and the private-names check it runs before a push, exercised
    against a throwaway git repo and remote with gitleaks/Herdr stubbed out."""

    def setUp(self):
        """Build an isolated HOME with a stub `gitleaks` (reports
        FAKE_GITLEAKS_VERSION and logs its own invocations) and a stub
        fake-herdr that fails loudly if ever called, plus the env each
        test pushes with."""
        self.temp = tempfile.TemporaryDirectory(prefix="pre-push-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.home = self.work / "home"
        self.home.mkdir()
        self.tools = self.work / "tools"
        self.tools.mkdir()
        fake_herdr = self.home / "fake-herdr"
        fake_herdr.write_text("#!/bin/sh\nexit 99\n")
        fake_herdr.chmod(0o755)
        self.gitleaks_log = self.work / "gitleaks.log"
        gitleaks = self.tools / "gitleaks"
        gitleaks.write_text(
            "#!/bin/sh\n"
            'if [ "$1" = "version" ]; then\n'
            '    printf "%s\\n" "$FAKE_GITLEAKS_VERSION"\n'
            "    exit 0\n"
            "fi\n"
            'if [ -n "${GITLEAKS_LOG:-}" ]; then\n'
            '    printf "gitleaks %s\\n" "$*" >> "$GITLEAKS_LOG"\n'
            "fi\n"
            "exit 0\n"
        )
        gitleaks.chmod(0o755)
        path_without_gitleaks = os.pathsep.join(
            part for part in os.environ.get("PATH", os.defpath).split(os.pathsep)
            if part and not (Path(part) / "gitleaks").exists()
        )
        self.path_without_gitleaks = path_without_gitleaks
        self.env = {
            "PATH": str(self.tools) + os.pathsep + path_without_gitleaks,
            "HOME": str(self.home),
            "TMPDIR": str(self.work),
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
            "GIT_AUTHOR_NAME": "Hook Test",
            "GIT_AUTHOR_EMAIL": "hook-test@example.com",
            "GIT_COMMITTER_NAME": "Hook Test",
            "GIT_COMMITTER_EMAIL": "hook-test@example.com",
            "GITLEAKS_LOG": str(self.gitleaks_log),
            "FAKE_GITLEAKS_VERSION": PINNED_GITLEAKS_VERSION,
            "HERDR_BIN_PATH": str(fake_herdr),
            "HERDR_PLUGIN_CONFIG_DIR": str(self.work / "plugin-config"),
            "HERDR_PLUGIN_STATE_DIR": str(self.work / "plugin-state"),
        }
        (self.work / "plugin-config").mkdir()
        (self.work / "plugin-state").mkdir()

    def git_env(self, extra=None):
        """Build the env for a git/hook invocation: the real environment
        overlaid with this test's isolated fixtures, with any ambient
        Multiverse state vars stripped so a test host's own fleet config
        can't leak in, then `extra` applied last so callers can override."""
        env = os.environ.copy()
        env.update(self.env)
        env.pop("MV_PRIVATE_NAMES", None)
        env.pop("MULTIVERSE_STATE_DIR", None)
        env.pop("MULTIVERSE_CONFIG", None)
        env.pop("MULTIVERSE_INVOCATION", None)
        if extra:
            env.update(extra)
        return env

    def git(self, repo, args, extra=None, check=True):
        """Run a git command against `repo` with the isolated test env."""
        return subprocess.run(
            ["git", "-C", str(repo), *args],
            env=self.git_env(extra),
            text=True,
            capture_output=True,
            check=check,
        )

    def make_repo(self):
        """Build a throwaway git repo with the real pre-push, install.sh and
        check-private-names.sh copied in, a committed SECRET, the hook
        installed via install.sh, and a bare remote wired up as origin."""
        repo = self.work / "hook checkout's files"
        if repo.exists():
            shutil.rmtree(repo)
        (repo / "scripts/hooks").mkdir(parents=True)
        (repo / "scripts/release").mkdir(parents=True)
        shutil.copy2(REPO / "scripts/hooks/pre-push", repo / "scripts/hooks/pre-push")
        shutil.copy2(REPO / "scripts/hooks/install.sh", repo / "scripts/hooks/install.sh")
        shutil.copy2(REPO / "scripts/check-private-names.sh", repo / "scripts/check-private-names.sh")
        shutil.copy2(GITLEAKS_VERSION_SCRIPT, repo / "scripts/release/gitleaks-version.sh")
        (repo / ".gitleaks.toml").write_text("title = 'fixture'\n")
        (repo / "note.txt").write_text(f"{SECRET} appears in this commit\n")
        env = self.git_env()
        subprocess.run(
            ["git", "init", "-q", "-b", "main"],
            cwd=repo, env=env, check=True, capture_output=True, text=True,
        )
        self.git(repo, ["config", "user.name", "Hook Test"])
        self.git(repo, ["config", "user.email", "hook-test@example.com"])
        self.git(repo, ["config", "commit.gpgsign", "false"])
        self.git(repo, ["add", "note.txt", "scripts", ".gitleaks.toml"])
        self.git(repo, ["commit", "-q", "-m", "add secret word"])
        install = subprocess.run(
            ["sh", str(repo / "scripts/hooks/install.sh")],
            cwd=repo, env=env, text=True, capture_output=True, check=True,
        )
        self.assertIn("scripts/hooks", install.stdout)
        remote = self.work / "remote.git"
        if remote.exists():
            shutil.rmtree(remote)
        subprocess.run(
            ["git", "init", "-q", "--bare", "-b", "main", str(remote)],
            env=env, check=True, capture_output=True, text=True,
        )
        self.git(repo, ["remote", "add", "origin", str(remote)])
        return repo

    def push(self, repo, extra=None):
        """Push `repo`'s HEAD to its origin, resetting the gitleaks
        invocation log first so each call's assertions see only its own
        push."""
        if self.gitleaks_log.exists():
            self.gitleaks_log.unlink()
        return subprocess.run(
            ["git", "-C", str(repo), "push", "origin", "HEAD:main"],
            env=self.git_env(extra),
            text=True,
            capture_output=True,
            check=False,
        )

    def test_push_refused_when_list_contains_committed_name(self):
        """A pushed commit containing a deny-listed name is refused, the
        refusal names the matched name but never an unrelated list-only
        entry, and gitleaks still ran."""
        repo = self.make_repo()
        names = self.work / "private-names.txt"
        names.write_text(f"{SECRET}\n{LIST_ONLY}\n")
        result = self.push(repo, extra={"MV_PRIVATE_NAMES": str(names)})
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, combined)
        self.assertIn("check-private-names", result.stderr)
        self.assertIn(SECRET, result.stderr)
        self.assertNotIn(LIST_ONLY, combined)
        self.assertTrue(self.gitleaks_log.exists())
        self.assertIn("gitleaks", self.gitleaks_log.read_text())

    def test_push_allowed_when_list_absent_prints_skip(self):
        """With MV_PRIVATE_NAMES unset and no default list file, the push
        succeeds, prints exactly one skip line, and gitleaks still ran."""
        repo = self.make_repo()
        result = self.push(repo)
        combined = result.stdout + result.stderr
        self.assertEqual(result.returncode, 0, combined)
        self.assertIn(SKIP_LINE, result.stderr)
        self.assertEqual(result.stderr.count(SKIP_LINE), 1)
        self.assertNotIn(LIST_ONLY, combined)
        self.assertTrue(self.gitleaks_log.exists())
        self.assertIn("gitleaks", self.gitleaks_log.read_text())

    def test_default_home_list_refuses_push(self):
        """With MV_PRIVATE_NAMES unset but $HOME/.config/multiverse/
        private-names.txt present, the hook falls back to that file and
        still refuses the push."""
        repo = self.make_repo()
        config_dir = self.home / ".config" / "multiverse"
        config_dir.mkdir(parents=True)
        (config_dir / "private-names.txt").write_text(f"{SECRET}\n{LIST_ONLY}\n")
        result = self.push(repo)
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, combined)
        self.assertIn("check-private-names", result.stderr)
        self.assertNotIn(LIST_ONLY, combined)
        self.assertNotIn(SKIP_LINE, result.stderr)

    def test_missing_gitleaks_still_refuses(self):
        """No `gitleaks` on PATH refuses the push -- never a silent skip of
        the secret scan."""
        repo = self.make_repo()
        (self.tools / "gitleaks").unlink()
        result = self.push(repo, extra={"PATH": self.path_without_gitleaks})
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, combined)
        self.assertIn("gitleaks is not installed", result.stderr)
        self.assertNotIn(SKIP_LINE, result.stderr)

    def test_outdated_gitleaks_version_refuses_push(self):
        """A `gitleaks` older than the repo's pinned minimum version refuses
        the push with a message naming both versions."""
        repo = self.make_repo()
        result = self.push(repo, extra={"FAKE_GITLEAKS_VERSION": "0.0.0"})
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, combined)
        self.assertIn(
            f"gitleaks is v0.0.0, this repo requires >= v{PINNED_GITLEAKS_VERSION}",
            result.stderr,
        )
        self.assertNotIn(SKIP_LINE, result.stderr)

    def test_missing_gitleaks_version_pin_refuses_push(self):
        """A missing scripts/release/gitleaks-version.sh (the version pin
        the hook sources) refuses the push rather than skipping the version
        check."""
        repo = self.make_repo()
        (repo / "scripts/release/gitleaks-version.sh").unlink()
        result = self.push(repo)
        combined = result.stdout + result.stderr
        self.assertNotEqual(result.returncode, 0, combined)
        self.assertIn("scripts/release/gitleaks-version.sh", result.stderr)
        self.assertIn("is missing", result.stderr)
        self.assertNotIn(SKIP_LINE, result.stderr)


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    unittest.main()

#!/usr/bin/env python3
"""check-private-names.sh exit-code regressions.

Fails closed: a git grep error (e.g. an invalid ERE in the deny-list) must
exit 2, never read as "no hits". Run with:
    python3 scripts/tests/test_private_names.py
"""

from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


REPO = Path(__file__).resolve().parents[2]
SCRIPT = REPO / "scripts/check-private-names.sh"
NAME = "ExampleDenyListedName"
INVALID_ERE = "zzzunmatched("


class CheckPrivateNamesTests(unittest.TestCase):
    """scripts/check-private-names.sh exit-code regressions (0 clean,
    1 found, 2 check itself failed), run against a throwaway git repo so
    the real tree is never scanned."""

    def setUp(self):
        """Create a throwaway git repo containing one committed file whose
        text includes NAME, the name the deny-list tests look for."""
        self.temp = tempfile.TemporaryDirectory(prefix="check-private-names-test-")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.repo = self.work / "repo"
        self.repo.mkdir()
        subprocess.run(
            ["git", "init", "-q", "-b", "main", str(self.repo)],
            check=True, capture_output=True, text=True,
        )
        (self.repo / "note.txt").write_text(f"{NAME} appears in this file\n")
        subprocess.run(
            ["git", "-C", str(self.repo), "add", "note.txt"],
            check=True, capture_output=True, text=True,
        )

    def deny_list(self, *lines):
        """Write `lines` to a deny-list file and return its path."""
        path = self.work / "deny-list.txt"
        path.write_text("\n".join(lines) + "\n")
        return path

    def run_check(self, deny_list):
        """Run check-private-names.sh against the fixture repo with
        MV_PRIVATE_NAMES pointed at `deny_list`."""
        return subprocess.run(
            ["sh", str(SCRIPT)],
            cwd=self.repo,
            env={"MV_PRIVATE_NAMES": str(deny_list), "PATH": "/usr/bin:/bin:/usr/local/bin"},
            text=True,
            capture_output=True,
            check=False,
        )

    def test_matching_name_exits_1(self):
        """A deny-listed name present in the tree exits 1 and names it."""
        result = self.run_check(self.deny_list(NAME))
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("check-private-names", result.stderr)
        self.assertIn(NAME, result.stderr)

    def test_no_match_exits_0(self):
        """A deny-list with no hits in the tree exits 0 silently."""
        result = self.run_check(self.deny_list("zzznomatch"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stderr, "")

    def test_invalid_ere_exits_2_with_message(self):
        """An invalid regex in the deny-list fails closed: exit 2 (checker
        error), never 0 (read as "no hits")."""
        result = self.run_check(self.deny_list(INVALID_ERE))
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("check-private-names", result.stderr)
        self.assertIn("git grep failed", result.stderr)

    def test_missing_deny_list_skips(self):
        """MV_PRIVATE_NAMES pointing at a nonexistent file skips the check."""
        result = self.run_check(self.work / "does-not-exist.txt")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping", result.stderr)

    def test_empty_deny_list_skips(self):
        """A deny-list with only comments/blank lines skips the check."""
        result = self.run_check(self.deny_list("# nothing but comments", ""))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping", result.stderr)

    def test_unset_env_skips(self):
        """With MV_PRIVATE_NAMES unset entirely, the check skips -- this is
        CI's default (and every push without the var set locally)."""
        result = subprocess.run(
            ["sh", str(SCRIPT)],
            cwd=self.repo,
            env={"PATH": "/usr/bin:/bin:/usr/local/bin"},
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("skipping", result.stderr)


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    unittest.main()

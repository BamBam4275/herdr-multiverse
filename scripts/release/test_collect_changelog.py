#!/usr/bin/env python3
"""Fragment-collection regressions for collect_changelog.py and its
scripts/release/collect-changelog.sh wrapper."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parent))
import collect_changelog  # noqa: E402

SH_SCRIPT = Path(__file__).with_name("collect-changelog.sh")

CHANGELOG_BEFORE = """\
# Changelog

## [Unreleased]

### Added

- existing added bullet
  continuation line.

### Changed

- existing changed bullet.

## [0.1.0] - 2026-01-01

### Added

- first release.
"""


class CollectPure(unittest.TestCase):
    """Exercises collect_changelog.collect() directly: no filesystem."""

    def test_appends_under_existing_section(self):
        merged = collect_changelog.collect(
            CHANGELOG_BEFORE, {"Added": ["- new added bullet."]}
        )
        self.assertIn(
            "- existing added bullet\n  continuation line.\n- new added bullet.\n\n### Changed",
            merged,
        )
        # Nothing before [Unreleased] or after [0.1.0] moves.
        self.assertTrue(merged.startswith("# Changelog\n\n## [Unreleased]\n"))
        self.assertTrue(merged.endswith("- first release.\n"))

    def test_creates_missing_section_in_keep_a_changelog_order(self):
        merged = collect_changelog.collect(CHANGELOG_BEFORE, {"Fixed": ["- a fix."]})
        # Fixed belongs after Changed, before the next version heading.
        self.assertIn("### Changed\n\n- existing changed bullet.\n\n### Fixed\n\n- a fix.\n\n## [0.1.0]", merged)

    def test_two_sections_collect_in_order(self):
        merged = collect_changelog.collect(
            CHANGELOG_BEFORE,
            {"Fixed": ["- a fix."], "Removed": ["- a removal."]},
        )
        # Keep a Changelog order: Added, Changed, Deprecated, Removed, Fixed, Security.
        removed_at = merged.index("### Removed")
        fixed_at = merged.index("### Fixed")
        self.assertLess(removed_at, fixed_at)

    def test_does_not_move_existing_unreleased_text(self):
        merged = collect_changelog.collect(CHANGELOG_BEFORE, {})
        self.assertEqual(merged, CHANGELOG_BEFORE)

    def test_unknown_section_name_refused(self):
        with self.assertRaises(collect_changelog.FragmentError) as ctx:
            collect_changelog.parse_fragment_name(Path("slug.notasection.md"))
        message = str(ctx.exception)
        self.assertIn("notasection", message)
        for name in collect_changelog.SECTIONS:
            self.assertIn(name, message)

    def test_fragment_name_without_section_refused(self):
        with self.assertRaises(collect_changelog.FragmentError):
            collect_changelog.parse_fragment_name(Path("no-section-here.md"))

    def test_section_name_case_insensitive(self):
        _, canonical = collect_changelog.parse_fragment_name(Path("slug.ADDED.md"))
        self.assertEqual(canonical, "Added")

    def test_missing_unreleased_section_refused(self):
        with self.assertRaises(collect_changelog.FragmentError):
            collect_changelog.collect("# Changelog\n\n## [0.1.0]\n", {"Added": ["- x"]})


class CollectFixtures(unittest.TestCase):
    """End-to-end: real fragment files, real CHANGELOG.md, via the sh wrapper."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mv-changelog-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "CHANGELOG.md").write_text(CHANGELOG_BEFORE)
        self.fragments = self.root / "changelog.d"
        self.fragments.mkdir()
        (self.fragments / "README.md").write_text("not a fragment\n")
        shutil.copy2(
            Path(__file__).with_name("collect_changelog.py"),
            self.root / "collect_changelog.py",
        )

    def write_fragment(self, name, text):
        (self.fragments / name).write_text(text)

    def run_sh(self, *args):
        scripts_release = self.root / "scripts" / "release"
        scripts_release.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SH_SCRIPT, scripts_release / SH_SCRIPT.name)
        shutil.copy2(
            Path(__file__).with_name("collect_changelog.py"),
            scripts_release / "collect_changelog.py",
        )
        return subprocess.run(
            ["sh", str(scripts_release / SH_SCRIPT.name), *args],
            cwd=self.root,
            capture_output=True,
            text=True,
            check=False,
        )

    def test_two_fragments_in_two_sections_collect_under_existing_text(self):
        self.write_fragment("20261117-a.fixed.md", "- fragment fix one.\n")
        self.write_fragment("20261117-b.removed.md", "- fragment removal.\n")
        result = self.run_sh()
        self.assertEqual(result.returncode, 0, result.stderr)
        merged = (self.root / "CHANGELOG.md").read_text()
        self.assertIn(
            "### Changed\n\n- existing changed bullet.\n\n### Removed\n\n- fragment removal.\n\n### Fixed\n\n- fragment fix one.\n\n## [0.1.0]",
            merged,
        )
        # Consumed fragments are deleted; README.md is not a fragment and stays.
        self.assertEqual(sorted(p.name for p in self.fragments.glob("*")), ["README.md"])

    def test_dry_run_changes_nothing(self):
        self.write_fragment("20261117-a.added.md", "- dry run bullet.\n")
        before = (self.root / "CHANGELOG.md").read_text()
        result = self.run_sh("--dry-run")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("dry run bullet", result.stdout)
        self.assertIn("## [Unreleased]", result.stdout)
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), before)
        self.assertTrue((self.fragments / "20261117-a.added.md").exists())

    def test_collection_is_idempotent(self):
        self.write_fragment("20261117-a.added.md", "- idempotent bullet.\n")
        first = self.run_sh()
        self.assertEqual(first.returncode, 0, first.stderr)
        after_first = (self.root / "CHANGELOG.md").read_text()
        # Fragment is consumed; a second run has nothing left to collect and
        # must leave CHANGELOG.md byte-identical rather than duplicating it.
        second = self.run_sh()
        self.assertEqual(second.returncode, 0, second.stderr)
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), after_first)
        self.assertIn("no fragments to collect", second.stdout)

    def test_order_is_stable_regardless_of_creation_order(self):
        # Created in reverse of filename order; the collected result must
        # not depend on creation order, only on the sorted filenames.
        self.write_fragment("20261117-z.added.md", "- z sorts last.\n")
        self.write_fragment("20261117-a.added.md", "- a sorts first.\n")
        result = self.run_sh()
        self.assertEqual(result.returncode, 0, result.stderr)
        merged = (self.root / "CHANGELOG.md").read_text()
        a_at = merged.index("a sorts first")
        z_at = merged.index("z sorts last")
        self.assertLess(a_at, z_at)

    def test_unknown_section_refused_with_clear_message_and_changes_nothing(self):
        self.write_fragment("20261117-a.notarealsection.md", "- x.\n")
        before = (self.root / "CHANGELOG.md").read_text()
        result = self.run_sh()
        self.assertEqual(result.returncode, 2)
        self.assertIn("notarealsection", result.stderr)
        self.assertIn("Added", result.stderr)
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), before)
        self.assertTrue((self.fragments / "20261117-a.notarealsection.md").exists())

    def test_collected_output_matches_hand_written_expected_file(self):
        self.write_fragment("20261117-a.added.md", "- hand-checked bullet.\n")
        result = self.run_sh()
        self.assertEqual(result.returncode, 0, result.stderr)
        expected = CHANGELOG_BEFORE.replace(
            "- existing added bullet\n  continuation line.\n",
            "- existing added bullet\n  continuation line.\n- hand-checked bullet.\n",
        )
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), expected)

    def test_no_fragments_leaves_changelog_untouched(self):
        before = (self.root / "CHANGELOG.md").read_text()
        result = self.run_sh()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.root / "CHANGELOG.md").read_text(), before)

    def test_usage_error_on_unexpected_argument(self):
        result = self.run_sh("--bogus")
        self.assertEqual(result.returncode, 2)
        self.assertIn("usage", result.stderr)


if __name__ == "__main__":
    unittest.main()

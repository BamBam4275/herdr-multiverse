"""Regression checks for replay/reporting; no oracle or live fleet required."""

from __future__ import annotations

import contextlib
import copy
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

import run_all
from run_scenario import run_scenario


class ReplayTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="conformance-test-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.binary = self.root / "test cli"
        self.scenario = {
            "id": "regression",
            "projects": {"sandbox": {"files": {"input": "keep\n"}}},
            "steps": [{"cwd": "sandbox", "argv": ["check"],
                       "expect": {"exit": 0, "stdout": "recorded", "stderr": ""}}],
            "expect_tree": [{"root": "sandbox", "unchanged": True}],
        }

    def cli(self, source):
        self.binary.write_text(
            f"#!{sys.executable}\n" + source, encoding="utf-8"
        )
        self.binary.chmod(0o700)

    def replay(self, scenario=None):
        return run_scenario(
            scenario or self.scenario, impl="rust", python_repo=None,
            rust_bin=str(self.binary),
        )

    def test_recorded_expectation_passes_and_stdout_drift_fails(self):
        self.cli("print('recorded')\n")
        self.assertTrue(self.replay().ok)
        self.cli("print('drift')\n")
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertEqual(result.steps[0].status, "FAIL")
        self.assertIn("stdout mismatch", "\n".join(result.failures))

    def test_file_mutation_fails_even_when_command_passes(self):
        self.cli("from pathlib import Path\nPath('input').write_text('changed')\nprint('recorded')\n")
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertEqual(result.steps[0].status, "PASS")
        self.assertIn("changed when it should not", "\n".join(result.failures))

    def test_empty_transcript_rejects_swallowed_unexpected_call(self):
        self.cli(
            "import os, subprocess\n"
            "subprocess.run([os.environ['HERDR_BIN_PATH'], 'agent', 'list'], "
            "capture_output=True)\nprint('recorded')\n"
        )
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertIn("unexpected argv", "\n".join(result.failures))

    def test_bare_herdr_is_also_isolated(self):
        self.cli(
            "import subprocess\n"
            "subprocess.run(['herdr', 'agent', 'list'], capture_output=True)\n"
            "print('recorded')\n"
        )
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertIn("unexpected argv", "\n".join(result.failures))

    def test_mismatched_call_is_not_erased_by_later_matching_call(self):
        self.cli(
            "import os, subprocess\n"
            "for argv in [['wrong'], ['--version']]:\n"
            "    subprocess.run([os.environ['HERDR_BIN_PATH'], *argv], capture_output=True)\n"
            "print('recorded')\n"
        )
        self.scenario['steps'][0]['transcript'] = [{"match_argv": ["--version"]}]
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertIn("argv mismatch", "\n".join(result.failures))

    def test_unused_transcript_fails(self):
        self.cli("print('recorded')\n")
        self.scenario['steps'][0]['transcript'] = [{"match_argv": ["--version"]}]
        self.assertIn("transcript not exhausted", "\n".join(self.replay().failures))

    def test_missing_capture_is_reported_and_independent_commands_still_run(self):
        self.cli("print('recorded')\n")
        dependent = copy.deepcopy(self.scenario['steps'][0])
        dependent['argv'] = ['result', '{{capture:task}}']
        self.scenario['steps'].extend([dependent, copy.deepcopy(self.scenario['steps'][0])])
        result = self.replay()
        self.assertFalse(result.ok)
        self.assertEqual([step.status for step in result.steps],
                         ['PASS', 'NOT RUN (missing capture)', 'PASS'])

    def test_environment_is_isolated_and_runtime_is_separate(self):
        self.cli(
            "import os\n"
            "assert 'MULTIVERSE_TASK_ID' not in os.environ\n"
            "assert os.environ['MULTIVERSE_CONFIG'].startswith(os.environ['HERDR_PLUGIN_CONFIG_DIR'])\n"
            "assert os.environ['MULTIVERSE_STATE_DIR'] == os.environ['HERDR_PLUGIN_STATE_DIR']\n"
            "assert os.environ['HERDR_PLUGIN_CONFIG_DIR'] != os.environ['HERDR_PLUGIN_STATE_DIR']\n"
            "print('recorded')\n"
        )
        with patch.dict(os.environ, {'MULTIVERSE_TASK_ID': 'ambient-task'}):
            self.assertTrue(self.replay().ok)
        self.scenario['steps'][0]['env'] = {'HERDR_BIN_PATH': '/not-the-fake'}
        self.assertIn("cannot override isolated HERDR_BIN_PATH", "\n".join(self.replay().failures))

    def test_batch_builds_rust_by_default_and_returns_parity_failure(self):
        self.cli("print('drift')\n")
        (self.root / 'regression.json').write_text(json.dumps(self.scenario))
        output = io.StringIO()
        with patch.object(run_all, 'SCENARIOS_DIR', self.root), \
             patch.object(run_all, 'build_rust_binary', return_value=str(self.binary)) as build, \
             contextlib.redirect_stdout(output):
            status = run_all.main([])
        build.assert_called_once_with()
        self.assertEqual(status, 1)
        self.assertIn('regression | rust | 0 | ["check"] | FAIL', output.getvalue())
        self.assertIn('rust: 0/1 passed', output.getvalue())

    def test_relative_binary_override_is_resolved_before_changing_cwd(self):
        self.cli("print('recorded')\n")
        (self.root / 'regression.json').write_text(json.dumps(self.scenario))
        with patch.object(run_all, 'SCENARIOS_DIR', self.root), \
             patch.object(run_all, 'build_rust_binary') as build, \
             contextlib.redirect_stdout(io.StringIO()):
            status = run_all.main(['--rust-bin', os.path.relpath(self.binary)])
        build.assert_not_called()
        self.assertEqual(status, 0)


if __name__ == '__main__':
    unittest.main()

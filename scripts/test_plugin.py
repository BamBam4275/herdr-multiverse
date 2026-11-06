#!/usr/bin/env python3
"""Plugin boundary regressions, run with python3 scripts/test_plugin.py."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest


REPO = Path(__file__).resolve().parent.parent


class PluginTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="plugin-test-", dir=REPO / "scripts")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "plugin checkout's files"
        (self.root / "scripts").mkdir(parents=True)
        (self.root / "bin").mkdir()
        shutil.copy2(REPO / "scripts/plugin-action.sh", self.root / "scripts")
        self.manifest = tomllib.loads((REPO / "herdr-plugin.toml").read_text())
        self.config = Path(self.temp.name) / "plugin config"
        self.state = Path(self.temp.name) / "plugin state"
        self.config.mkdir()
        self.state.mkdir()
        (self.config / "config.toml").write_text("# isolated config\n")
        fake_herdr = Path(self.temp.name) / "fake-herdr"
        fake_herdr.write_text("#!/bin/sh\nexit 99\n")
        fake_herdr.chmod(0o755)
        self.env = {
            "PATH": str(Path(sys.executable).parent) + os.pathsep + os.defpath,
            "HOME": self.temp.name,
            "HERDR_BIN_PATH": str(fake_herdr),
            "HERDR_PLUGIN_CONFIG_DIR": str(self.config),
            "HERDR_PLUGIN_STATE_DIR": str(self.state),
            # Stale ambient overrides must not retarget managed actions.
            "MULTIVERSE_CONFIG": "/unrelated/config.toml",
            "MULTIVERSE_STATE_DIR": "/unrelated/state",
            "MULTIVERSE_INVOCATION": "unrelated-command",
            "MULTIVERSE_READ_ONLY": "1",
        }
        self.binary = self.root / "bin/multiverse"
        self.binary.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "print(json.dumps({'argv': sys.argv[1:], 'env': dict(os.environ)}))\n"
            "sys.exit(int(os.environ.get('FIXTURE_EXIT', '0')))\n"
        )
        self.binary.chmod(0o755)
        bootstrap = self.root / "scripts/bootstrap-plugin.sh"
        bootstrap.write_text("#!/bin/sh\necho 'unexpected bootstrap' >&2\nexit 97\n")
        bootstrap.chmod(0o755)

    def run_entry(self, entry, env=None):
        return subprocess.run(
            entry["command"], cwd=self.root, env=self.env if env is None else env,
            text=True, capture_output=True, check=False,
        )

    def test_all_manifest_commands_without_plugin_root(self):
        expected = {
            "multiverse-doctor": ["doctor"],
            "multiverse-check": ["check", "--read-only"],
            "multiverse-accounts": ["accounts"],
            "multiverse-tasks": ["tasks", "--read-only"],
            "multiverse-board": ["board", "--all", "--read-only"],
            "multiverse-start": ["start", "--all"],
            "multiverse-workers": ["workers"],
            "multiverse-import-python": ["import-python"],
            "dashboard": ["board", "--all", "--watch", "--read-only"],
        }
        entries = self.manifest["actions"] + self.manifest["panes"]
        self.assertEqual({entry["id"] for entry in entries}, set(expected))
        for entry in entries:
            with self.subTest(entry=entry["id"]):
                result = self.run_entry(entry)
                self.assertEqual(result.returncode, 0, result.stderr)
                output = json.loads(result.stdout)
                self.assertEqual(output["argv"], expected[entry["id"]])
                env = output["env"]
                self.assertEqual(env["MULTIVERSE_CONFIG"], str(self.config / "config.toml"))
                self.assertEqual(env["MULTIVERSE_STATE_DIR"], str(self.state))
                self.assertEqual(env["HERDR_BIN_PATH"], self.env["HERDR_BIN_PATH"])
                self.assertEqual(shlex.split(env["MULTIVERSE_INVOCATION"]), [str(self.binary)])
                if entry["id"] in ("multiverse-start", "multiverse-import-python"):
                    self.assertNotIn("MULTIVERSE_READ_ONLY", env)
                else:
                    self.assertEqual(env["MULTIVERSE_READ_ONLY"], "1")

    def test_missing_config_refuses_global_fallback_before_bootstrap(self):
        (self.config / "config.toml").unlink()
        self.binary.unlink()
        # `import-python`'s whole job is creating this file; it has its own
        # exemption, checked separately below.
        entries = [
            entry
            for entry in self.manifest["actions"] + self.manifest["panes"]
            if entry["id"] != "multiverse-import-python"
        ]
        for entry in entries:
            with self.subTest(entry=entry["id"]):
                result = self.run_entry(entry)
                self.assertEqual(result.returncode, 3, result.stderr)
                self.assertIn("config.toml", result.stderr)
                self.assertNotIn("unexpected bootstrap", result.stderr)
                self.assertEqual(result.stdout, "")

    def test_import_python_runs_without_an_existing_config(self):
        (self.config / "config.toml").unlink()
        entry = next(
            entry
            for entry in self.manifest["actions"]
            if entry["id"] == "multiverse-import-python"
        )
        result = self.run_entry(entry)
        self.assertEqual(result.returncode, 0, result.stderr)
        output = json.loads(result.stdout)
        self.assertEqual(output["argv"], ["import-python"])
        env = output["env"]
        self.assertEqual(env["MULTIVERSE_CONFIG"], str(self.config / "config.toml"))
        self.assertEqual(env["MULTIVERSE_STATE_DIR"], str(self.state))
        self.assertNotIn("MULTIVERSE_READ_ONLY", env)

    def test_missing_or_relative_plugin_dirs_are_configuration_errors(self):
        for key in ("HERDR_PLUGIN_CONFIG_DIR", "HERDR_PLUGIN_STATE_DIR"):
            for value in (None, "", "relative/path"):
                with self.subTest(key=key, value=value):
                    env = self.env.copy()
                    if value is None:
                        env.pop(key)
                    else:
                        env[key] = value
                    result = self.run_entry(self.manifest["actions"][0], env)
                    self.assertEqual(result.returncode, 3, result.stderr)
                    self.assertIn(key, result.stderr)

    def test_invocation_survives_another_working_directory(self):
        result = self.run_entry(self.manifest["actions"][5])
        self.assertEqual(result.returncode, 0, result.stderr)
        env = json.loads(result.stdout)["env"]
        invocation = subprocess.run(
            ["sh", "-c", env["MULTIVERSE_INVOCATION"] + " --version"],
            cwd=self.temp.name, env=env, text=True, capture_output=True, check=False,
        )
        self.assertEqual(invocation.returncode, 0, invocation.stderr)
        self.assertEqual(json.loads(invocation.stdout)["argv"], ["--version"])

    def test_binary_exit_status_is_preserved(self):
        self.env["FIXTURE_EXIT"] = "7"
        result = self.run_entry(self.manifest["actions"][0])
        self.assertEqual(result.returncode, 7, result.stderr)

    def test_missing_binary_uses_last_mile_bootstrap(self):
        self.binary.unlink()
        result = self.run_entry(self.manifest["actions"][0])
        self.assertEqual(result.returncode, 97, result.stderr)
        self.assertIn("unexpected bootstrap", result.stderr)

    def test_manifest_commands_are_executable_in_the_checkout(self):
        # Herdr execs these paths directly, with no `sh` prefix (plugin-action.sh's
        # own header: "Herdr runs these from the plugin directory"). A script that
        # loses its +x bit — a hand-edit without re-chmod, an editor or transfer
        # that resets permissions, a `git update-index --chmod=-x` typo — would
        # fail every fresh `herdr plugin install`/action invoke with "Permission
        # denied", not just in this checkout: the git-tracked mode is what a new
        # clone gets.
        commands = {self.manifest["build"][0]["command"][0]}
        commands.update(
            entry["command"][0] for entry in self.manifest["actions"] + self.manifest["panes"]
        )
        for relative in sorted(commands):
            path = REPO / relative
            with self.subTest(command=relative):
                self.assertTrue(path.is_file(), f"{relative} is missing")
                self.assertTrue(os.access(path, os.X_OK), f"{relative} is not executable")

    def test_manifest_contract_and_license(self):
        workspace = tomllib.loads((REPO / "Cargo.toml").read_text())
        self.assertEqual(self.manifest["version"], workspace["workspace"]["package"]["version"])
        self.assertEqual(workspace["workspace"]["package"]["license"], "MIT OR Apache-2.0")
        self.assertTrue((REPO / "LICENSE-MIT").is_file())
        self.assertTrue((REPO / "LICENSE-APACHE").is_file())
        self.assertFalse((REPO / "LICENSE").exists())
        self.assertEqual(self.manifest["platforms"], ["linux", "macos"])
        self.assertEqual(self.manifest["panes"][0]["placement"], "tab")
        for key in ("startup", "events", "link_handlers"):
            self.assertNotIn(key, self.manifest)
        for action in self.manifest["actions"]:
            self.assertEqual(action["contexts"], ["global"])
            self.assertNotIn(action["id"], ("multiverse-dispatch", "multiverse-council"))


def load_tests(loader, tests, _pattern):
    # Keep bootstrap lifecycle regressions in the plugin boundary check.
    tests.addTests(loader.discover(str(REPO / "scripts/tests"), pattern="test_bootstrap.py"))
    return tests


if __name__ == "__main__":
    sys.dont_write_bytecode = True
    unittest.main()

#!/usr/bin/env python3
"""Fast bootstrap regressions; also reused with real release artifacts."""

from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import unittest


REPO = Path(__file__).resolve().parents[2]
TARGETS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "aarch64"): "aarch64-unknown-linux-gnu",
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu",
}


class BootstrapTests(unittest.TestCase):
    # The macOS end-to-end runner supplies archives built by scripts/build.sh.
    release_source = None

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bootstrap-test-", dir=REPO / "scripts")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.root = self.work / "plugin checkout's files"
        (self.root / "scripts").mkdir(parents=True)
        for name in ("bootstrap-plugin.sh", "build.sh"):
            shutil.copy2(REPO / "scripts" / name, self.root / "scripts" / name)
        shutil.copy2(REPO / "herdr-plugin.toml", self.root)
        self.version = tomllib.loads((self.root / "herdr-plugin.toml").read_text())["version"]
        self.target = TARGETS[(platform.system(), platform.machine())]
        self.archive_name = f"multiverse-{self.target}.tar.gz"
        self.binary = self.root / "bin/multiverse"
        self.tools = self.work / "tools"
        self.tools.mkdir()
        # Explicit allowlist: absence of cargo must not depend on the host PATH.
        for name in (
            "awk", "chmod", "cp", "curl", "dirname", "grep", "gzip", "head", "mkdir",
            "mktemp", "mv", "rm", "sed", "sh", "sha256sum", "shasum", "tar", "uname",
        ):
            tool = shutil.which(name)
            if tool:
                (self.tools / name).symlink_to(tool)
        self.assertTrue((self.tools / "curl").exists(), "release tests require curl")
        fake_herdr = self.work / "fake-herdr"
        fake_herdr.write_text("#!/bin/sh\necho 'unexpected Herdr call' >&2\nexit 99\n")
        fake_herdr.chmod(0o755)
        for name in ("home", "config", "state", "tmp", "project"):
            (self.work / name).mkdir()
        (self.work / "config/config.toml").write_text("# isolated config\n")
        self.releases = self.work / "releases"
        self.releases.mkdir()
        self.env = {
            "PATH": str(self.tools),
            "HOME": str(self.work / "home"),
            "TMPDIR": str(self.work / "tmp"),
            "HERDR_BIN_PATH": str(fake_herdr),
            "HERDR_PLUGIN_CONFIG_DIR": str(self.work / "config"),
            "HERDR_PLUGIN_STATE_DIR": str(self.work / "state"),
            "MULTIVERSE_RELEASE_BASE_URL": self.releases.as_uri() + "/",
        }
        if self.release_source is not None:
            shutil.copytree(self.release_source, self.releases, dirs_exist_ok=True)
        else:
            stage = self.work / "stage"
            stage.mkdir()
            payload = stage / "multiverse"
            payload.write_text(f"#!/bin/sh\nprintf 'multiverse {self.version}\\n'\n")
            payload.chmod(0o755)
            archive = self.releases / self.archive_name
            with tarfile.open(archive, "w:gz") as bundle:
                bundle.add(payload, arcname="multiverse")
            checksum = subprocess.check_output(
                ["shasum", "-a", "256", archive.name], cwd=self.releases, text=True,
            )
            (self.releases / "SHA256SUMS").write_text(checksum)

    def bootstrap(self, caller="install"):
        return subprocess.run(
            [str(self.root / "scripts/bootstrap-plugin.sh"), caller],
            cwd=self.work / "project", env=self.env,
            text=True, capture_output=True, check=False,
        )

    def assert_version(self):
        result = subprocess.run(
            [str(self.binary), "--version"], cwd=self.work / "project", env=self.env,
            text=True, capture_output=True, check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), f"multiverse {self.version}")

    def remove_release(self):
        shutil.rmtree(self.releases)

    def use_default_github_url(self):
        """Serve local assets when bootstrap hits the default GitHub release URL.

        Attestation runs only when MULTIVERSE_RELEASE_BASE_URL is unset, so
        tests that exercise `gh attestation verify` must take this path.
        """
        del self.env["MULTIVERSE_RELEASE_BASE_URL"]
        curl = self.tools / "curl"
        if curl.exists() or curl.is_symlink():
            curl.unlink()
        archive = self.archive_name
        version = self.version
        archive_src = self.releases / archive
        sums_src = self.releases / "SHA256SUMS"
        base = f"https://github.com/BamBam4275/herdr-multiverse/releases/download/v{version}"
        curl.write_text(
            "#!/bin/sh\n"
            "dest=\"\"\n"
            "url=\"\"\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "    case \"$1\" in\n"
            "    --output) dest=\"$2\"; shift 2 ;;\n"
            "    --fail|--location|--silent|--show-error) shift ;;\n"
            "    -*) shift ;;\n"
            "    *) url=\"$1\"; shift ;;\n"
            "    esac\n"
            "done\n"
            f"if [ \"$url\" = '{base}/{archive}' ]; then\n"
            f"    cp '{archive_src}' \"$dest\"\n"
            f"elif [ \"$url\" = '{base}/SHA256SUMS' ]; then\n"
            f"    cp '{sums_src}' \"$dest\"\n"
            "else\n"
            "    echo \"curl stub: unexpected url: $url\" >&2\n"
            "    exit 22\n"
            "fi\n"
        )
        curl.chmod(0o755)

    def install_gh(self, body, *, help_ok=True):
        gh = self.tools / "gh"
        if help_ok:
            script = (
                "#!/bin/sh\n"
                "for arg in \"$@\"; do\n"
                "    case \"$arg\" in\n"
                "    --help) exit 0 ;;\n"
                "    esac\n"
                "done\n"
                + body
            )
        else:
            script = "#!/bin/sh\n" + body
        gh.write_text(script)
        gh.chmod(0o755)

    def test_verified_release_and_existing_binary(self):
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assert_version()
        with tarfile.open(self.releases / self.archive_name) as bundle:
            self.assertEqual(bundle.getnames(), ["multiverse"])
            self.assertEqual(self.binary.read_bytes(), bundle.extractfile("multiverse").read())
        self.remove_release()
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("nothing to do", result.stdout)

    def test_stale_binary_redownloads_from_release(self):
        # Tag-upgrade path: an old bin/multiverse is already installed and a
        # newer release archive is available (herdr plugin install --ref vNext).
        stale_version = "0.0.0"
        self.assertNotEqual(stale_version, self.version)
        self.binary.parent.mkdir(parents=True)
        self.binary.write_text(f"#!/bin/sh\nprintf 'multiverse {stale_version}\\n'\n")
        self.binary.chmod(0o755)
        stale = self.binary.read_bytes()
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertNotIn("nothing to do", result.stdout)
        self.assert_version()
        with tarfile.open(self.releases / self.archive_name) as bundle:
            self.assertEqual(self.binary.read_bytes(), bundle.extractfile("multiverse").read())
        self.assertNotEqual(self.binary.read_bytes(), stale)

    def test_custom_release_url_skips_gh_attestation(self):
        # Offline mirrors and the local file:// fixture have no GitHub
        # attestation. A present `gh` must not fail the checksum-verified
        # install (bootstrap-plugin.sh skips attestation whenever
        # MULTIVERSE_RELEASE_BASE_URL is set).
        gh = self.tools / "gh"
        gh.write_text("#!/bin/sh\necho 'unexpected attestation' >&2\nexit 1\n")
        gh.chmod(0o755)
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertNotIn("unexpected attestation", result.stderr)
        self.assertNotIn("attestation failed", result.stderr)
        self.assertNotIn("does not match", result.stderr)
        self.assert_version()

    def test_default_url_gh_success_installs_release(self):
        self.use_default_github_url()
        self.install_gh(
            "echo '✓ Verification succeeded!' >&2\n"
            "exit 0\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertNotIn("refusing release binary", result.stderr)
        self.assertNotIn("checksum already verified", result.stderr)
        self.assert_version()

    def test_default_url_old_gh_warns_and_installs(self):
        self.use_default_github_url()
        self.install_gh(
            "echo 'unknown command \"attestation\" for \"gh\"' >&2\n"
            "exit 1\n",
            help_ok=False,
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertIn("too old", result.stderr)
        self.assertIn("checksum already verified", result.stderr)
        self.assertNotIn("refusing release binary", result.stderr)
        self.assert_version()

    def test_default_url_unauthenticated_gh_warns_and_installs(self):
        self.use_default_github_url()
        self.install_gh(
            "echo 'To get started with GitHub CLI, please run:  gh auth login' >&2\n"
            "exit 4\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertIn("not authenticated", result.stderr)
        self.assertIn("checksum already verified", result.stderr)
        self.assertNotIn("refusing release binary", result.stderr)
        self.assert_version()

    def test_default_url_offline_gh_warns_and_installs(self):
        self.use_default_github_url()
        self.install_gh(
            "echo 'failed to fetch attestations for subject: dial tcp: lookup api.github.com: no such host' >&2\n"
            "exit 1\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertIn("could not reach GitHub", result.stderr)
        self.assertIn("checksum already verified", result.stderr)
        self.assertNotIn("refusing release binary", result.stderr)
        self.assert_version()

    def test_default_url_no_attestation_warns_and_installs(self):
        self.use_default_github_url()
        self.install_gh(
            "echo '✗ No attestations found for subject sha256:deadbeef' >&2\n"
            "exit 1\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from release", result.stdout)
        self.assertIn("no provenance attestation", result.stderr)
        self.assertIn("checksum already verified", result.stderr)
        self.assertNotIn("refusing release binary", result.stderr)
        self.assert_version()

    def test_default_url_attestation_mismatch_refuses_release(self):
        self.use_default_github_url()
        self.install_gh(
            "echo 'Loaded 1 attestation from GitHub API' >&2\n"
            "echo '✗ Verification failed' >&2\n"
            "echo 'Error: the artifact does not match its attestation' >&2\n"
            "exit 1\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("does not match", result.stderr)
        self.assertIn("refusing release binary", result.stderr)
        self.assertNotIn("from release", result.stdout)
        self.assertFalse(self.binary.exists())

    def test_default_url_attestation_mismatch_falls_back_to_cargo(self):
        self.use_default_github_url()
        self.install_gh(
            "echo '✗ Verification failed' >&2\n"
            "echo 'Error: the artifact does not match its attestation' >&2\n"
            "exit 1\n"
        )
        cargo = self.tools / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" > cargo-args\n"
            "mkdir -p target/release\n"
            "cp fallback-binary target/release/multiverse\n"
        )
        cargo.chmod(0o755)
        (self.root / "fallback-binary").write_text(
            f"#!/bin/sh\nprintf 'multiverse {self.version}\\n'\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("does not match", result.stderr)
        self.assertIn("refusing release binary", result.stderr)
        self.assertIn("from source with cargo", result.stdout)
        self.assert_version()

    def test_corrupt_checksum_refuses_install(self):
        (self.releases / "SHA256SUMS").write_text(f"{'0' * 64}  {self.archive_name}\n")
        result = self.bootstrap()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(self.binary.exists())
        self.assertEqual(list((self.work / "tmp").iterdir()), [])

    def test_missing_checksum_refuses_install(self):
        (self.releases / "SHA256SUMS").unlink()
        result = self.bootstrap()
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertFalse(self.binary.exists())

    def test_missing_release_builds_from_source(self):
        self.remove_release()
        cargo = self.tools / "cargo"
        cargo.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" > cargo-args\n"
            "mkdir -p target/release\n"
            "cp fallback-binary target/release/multiverse\n"
        )
        cargo.chmod(0o755)
        (self.root / "fallback-binary").write_text(
            f"#!/bin/sh\nprintf 'multiverse {self.version}\\n'\n"
        )
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("from source with cargo", result.stdout)
        self.assertEqual(
            (self.root / "cargo-args").read_text().strip(),
            "build --release --locked -p mv-cli",
        )
        self.assert_version()

    def test_missing_release_and_cargo_explains_recovery(self):
        self.remove_release()
        for caller, hint in (("install", "re-run 'herdr plugin install'"), ("action", "retry the action")):
            with self.subTest(caller=caller):
                result = self.bootstrap(caller)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn("no release binary available", result.stderr)
                self.assertIn("no cargo found on PATH", result.stderr)
                self.assertIn("install rustc", result.stderr)
                self.assertIn(hint, result.stderr)
                self.assertFalse(self.binary.exists())


if __name__ == "__main__":
    unittest.main()

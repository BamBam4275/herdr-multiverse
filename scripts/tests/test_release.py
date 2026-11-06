#!/usr/bin/env python3
"""Build/package real macOS releases and exercise the isolated install hook."""

import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.dont_write_bytecode = True
from test_bootstrap import BootstrapTests, REPO


class RealReleaseTests(BootstrapTests):
    build_env = None
    built_binaries = None

    def test_missing_release_builds_from_source(self):
        self.remove_release()
        # Copy only build inputs, never the frozen Python reference or user data.
        for name in ("crates", "fixtures/herdr", "prompts", "templates"):
            shutil.copytree(REPO / name, self.root / name)
        for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
            shutil.copy2(REPO / name, self.root / name)
        self.assertFalse((self.root / "target").exists())
        self.env.update({
            "PATH": self.build_env["PATH"],
            "CARGO_HOME": self.build_env["CARGO_HOME"],
            "RUSTUP_HOME": self.build_env["RUSTUP_HOME"],
            "CARGO_BUILD_JOBS": self.build_env["CARGO_BUILD_JOBS"],
        })
        result = self.bootstrap()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("from source with cargo", result.stdout)
        self.assertTrue((self.root / "target/release/multiverse").is_file())
        self.assert_version()

    def test_each_packaged_target_installs_exact_binary(self):
        # Verify platform selection and bytes for every archive. Only the native
        # architecture is executed; Intel translation is not required on ARM CI.
        for target, built in self.built_binaries.items():
            with self.subTest(target=target):
                self.binary.unlink(missing_ok=True)
                system, machine = {
                    "aarch64-apple-darwin": ("Darwin", "arm64"),
                    "x86_64-apple-darwin": ("Darwin", "x86_64"),
                    "aarch64-unknown-linux-gnu": ("Linux", "aarch64"),
                    "x86_64-unknown-linux-gnu": ("Linux", "x86_64"),
                }[target]
                uname = self.tools / "uname"
                uname.unlink(missing_ok=True)
                uname.write_text(
                    f'#!/bin/sh\ncase "$1" in -s) echo {system};; -m) echo {machine};; *) exit 1;; esac\n'
                )
                uname.chmod(0o755)
                result = self.bootstrap()
                self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                self.assertIn(f"({target}) from release", result.stdout)
                self.assertEqual(self.binary.read_bytes(), built.read_bytes())
                if target == self.target:
                    self.assert_version()


def run(command, env, cwd=REPO):
    print("+ " + " ".join(map(str, command)), flush=True)
    subprocess.run(command, cwd=cwd, env=env, check=True)


def main():
    if platform.system() != "Darwin":
        raise SystemExit("test_release.py requires macOS to build both Apple release targets")
    with tempfile.TemporaryDirectory(prefix="release-test-", dir=REPO / "scripts") as temp:
        work = Path(temp)
        dist = work / "dist"
        dist.mkdir()
        home = work / "home"
        home.mkdir()
        env = os.environ.copy()
        env.pop("RUSTUP_TOOLCHAIN", None)
        env.update({
            "HOME": str(home),
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")),
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(Path.home() / ".rustup")),
            "CARGO_TARGET_DIR": str(REPO / "target"),
            "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS", "2"),
        })
        targets = ["aarch64-apple-darwin", "x86_64-apple-darwin"]
        for arch in ("aarch64", "x86_64"):
            compiler = shutil.which(f"{arch}-linux-gnu-gcc")
            if compiler:
                target = f"{arch}-unknown-linux-gnu"
                targets.append(target)
                env[f"CARGO_TARGET_{target.upper().replace('-', '_')}_LINKER"] = compiler
            else:
                print(f"Skipping {arch} Linux: no existing cross compiler", flush=True)
        built_binaries = {}
        for target in targets:
            run(["rustup", "target", "add", target], env)
            run([str(REPO / "scripts/build.sh"), target], env)
            binary = REPO / "target" / target / "release/multiverse"
            built_binaries[target] = binary
            # Keep these operations identical to release.yml's packaging step.
            with tempfile.TemporaryDirectory(dir=work) as stage:
                staged = Path(stage) / "multiverse"
                shutil.copy2(binary, staged)
                staged.chmod(staged.stat().st_mode | 0o111)
                archive = f"multiverse-{target}.tar.gz"
                run(
                    ["tar", "-C", stage, "-czf", archive, "multiverse"],
                    {**env, "COPYFILE_DISABLE": "1"}, dist,
                )
                with (dist / f"{archive}.sha256").open("wb") as checksum:
                    subprocess.run(
                        ["shasum", "-a", "256", archive], cwd=dist, env=env,
                        stdout=checksum, check=True,
                    )
        with (dist / "SHA256SUMS").open("wb") as checksums:
            for checksum in sorted(dist.glob("*.sha256")):
                checksums.write(checksum.read_bytes())
                checksum.unlink()
        run(["shasum", "-a", "256", "-c", "SHA256SUMS"], env, dist)
        RealReleaseTests.release_source = dist
        RealReleaseTests.build_env = env
        RealReleaseTests.built_binaries = built_binaries
        suite = unittest.TestSuite(RealReleaseTests(name) for name in (
            "test_verified_release_and_existing_binary",
            "test_each_packaged_target_installs_exact_binary",
            "test_corrupt_checksum_refuses_install",
            "test_missing_checksum_refuses_install",
            "test_missing_release_builds_from_source",
            "test_missing_release_and_cargo_explains_recovery",
        ))
        result = unittest.TextTestRunner(verbosity=2).run(suite)
        return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())

"""Build the CLI in this checkout using its pinned toolchain and lockfile."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent


def build_rust_binary() -> str:
    proc = subprocess.run(
        ["cargo", "build", "-p", "mv-cli", "--locked", "--message-format=json"],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    # Cargo reports the actual executable, including a custom CARGO_TARGET_DIR.
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "multiverse"
            and message.get("executable")
        ):
            return message["executable"]
    raise RuntimeError("cargo did not report a multiverse executable")


def build_fake_herdr_binary() -> str:
    """Build `crates/mv-herdr/src/bin/fake-herdr.rs` — the real, stateful
    pane/agent/workspace fake (`MV_FAKE_HERDR_STATE`), as opposed to this
    harness's own strict transcript player (`fake_herdr.py`). A scenario that
    declares `"herdr_seed"` is run against this binary instead."""
    proc = subprocess.run(
        [
            "cargo", "build", "-p", "mv-herdr", "--bin", "fake-herdr", "--locked",
            "--message-format=json",
        ],
        cwd=REPO_ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    for line in proc.stdout.splitlines():
        message = json.loads(line)
        if (
            message.get("reason") == "compiler-artifact"
            and message.get("target", {}).get("name") == "fake-herdr"
            and message.get("executable")
        ):
            return message["executable"]
    raise RuntimeError("cargo did not report a fake-herdr executable")

#!/usr/bin/env python3
"""Replay every recorded scenario against Rust (default), Python, or both.

Rust is built from this checkout unless --rust-bin explicitly overrides it.
Exit status: 0 all selected implementations pass, 1 parity failure, 2 setup error.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_scenario  # noqa: E402
from rust_binary import build_rust_binary  # noqa: E402

SCENARIOS_DIR = Path(__file__).resolve().parent / "scenarios"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--impl", choices=["rust", "python", "both"], default="rust")
    parser.add_argument(
        "--python-repo", default=os.environ.get("MV_CONFORMANCE_PYTHON_REPO"),
        help="frozen oracle checkout; required only for python/both",
    )
    parser.add_argument("--rust-bin", help="explicit binary override (skips cargo build)")
    args = parser.parse_args(argv)
    implementations = ["python", "rust"] if args.impl == "both" else [args.impl]
    if "python" in implementations and not args.python_repo:
        print("Python replay requires --python-repo or MV_CONFORMANCE_PYTHON_REPO")
        return 2

    if "rust" in implementations:
        try:
            args.rust_bin = (
                str(Path(args.rust_bin).resolve()) if args.rust_bin else build_rust_binary()
            )
        except (OSError, subprocess.CalledProcessError, RuntimeError) as exc:
            print(f"Rust build failed: {exc}", file=sys.stderr)
            if isinstance(exc, subprocess.CalledProcessError):
                print(exc.stdout, file=sys.stderr)
                print(exc.stderr, file=sys.stderr)
            return 2

    scenarios = sorted(SCENARIOS_DIR.glob("*.json"))
    if not scenarios:
        print(f"no scenarios found under {SCENARIOS_DIR}")
        return 2

    failed = {impl: 0 for impl in implementations}
    print("scenario | implementation | step | command | result")
    print("--- | --- | --- | --- | ---")
    for path in scenarios:
        scenario = json.loads(path.read_text())
        for impl in implementations:
            result = run_scenario.run_scenario(
                scenario, impl=impl, python_repo=args.python_repo, rust_bin=args.rust_bin
            )
            for step in result.steps:
                print(f"{scenario['id']} | {impl} | {step.index} | "
                      f"{json.dumps(step.argv)} | {step.status}")
            status = "PASS" if result.ok else "FAIL"
            print(f"{scenario['id']} [{impl}]: {status}")
            if not result.ok:
                failed[impl] += 1
                for failure in result.failures:
                    print(f"  - {failure}")

    print("; ".join(
        f"{impl}: {len(scenarios) - failed[impl]}/{len(scenarios)} passed"
        for impl in implementations
    ))
    return 1 if any(failed.values()) else 0


if __name__ == "__main__":
    raise SystemExit(main())

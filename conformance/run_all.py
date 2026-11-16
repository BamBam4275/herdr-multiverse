#!/usr/bin/env python3
"""Replay every recorded scenario against Rust (default), Python, or both.

Also runs the diff_fuzz normalizer unit tests, which need no frozen Python
reference. Rust is built from this checkout unless --rust-bin explicitly
overrides it.
Exit status: 0 all selected implementations and unit tests pass, 1 parity
or unit test failure, 2 setup error.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import run_scenario  # noqa: E402
import test_diff_fuzz  # noqa: E402
from rust_binary import build_rust_binary  # noqa: E402

SCENARIOS_DIR = Path(__file__).resolve().parent / "scenarios"


def run_diff_fuzz_unit_tests() -> bool:
    """Run the diff_fuzz normalizer unit tests; no frozen Python reference needed."""
    suite = unittest.TestLoader().loadTestsFromModule(test_diff_fuzz)
    result = unittest.TextTestRunner(verbosity=0, stream=sys.stdout).run(suite)
    passed = result.testsRun - len(result.failures) - len(result.errors)
    print(f"diff_fuzz unit tests: {passed}/{result.testsRun} passed")
    return result.wasSuccessful()


def main(argv: list[str] | None = None) -> int:
    """Parse CLI args, build Rust if needed, replay every scenario against
    each selected implementation, and print a pass/fail summary."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--impl", choices=["rust", "python", "both"], default="rust")
    parser.add_argument(
        "--python-repo", default=os.environ.get("MV_CONFORMANCE_PYTHON_REPO"),
        help="frozen oracle checkout; required only for python/both",
    )
    parser.add_argument("--rust-bin", help="explicit binary override (skips cargo build)")
    args = parser.parse_args(argv)
    unit_tests_ok = run_diff_fuzz_unit_tests()
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
    return 1 if any(failed.values()) or not unit_tests_ok else 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Scripted fake Herdr binary for conformance scenarios.

Invoked exactly as Herdr itself would be invoked — via `HERDR_BIN_PATH` —
by whichever implementation (Python oracle today, Rust binary once it
exists) the scenario runner is driving. It never talks to a real Herdr
socket; it replays one scenario's `transcript` in order and fails loudly on
drift, per spec-tests-harness.md item 5 ("strict exhaustion: an unexpected
external call or unused expected response fails the scenario").

Two files, both paths given by environment variables set by
`run_scenario.py` for this one process invocation's call:

  MV_CONFORMANCE_TRANSCRIPT  - path to a JSON list of
                               {"match_argv": [...], "reply": {...}} objects,
                               already placeholder-substituted into concrete
                               sandbox paths by the runner.
  MV_CONFORMANCE_STATE       - path to a small state file this script reads
                               and rewrites on every call: how many
                               transcript entries have been consumed, plus
                               (if not already pytest-visible to this file
                               by any other means) a line-per-call log of
                               every argv actually received, for the
                               runner's own exhaustion/drift check after the
                               implementation process exits.

Exit codes are deliberately outside Herdr's own 0/1/2 so a scenario failure
caused by *this* script is never confused with a real Herdr refusal:
  98 - argv did not match the next expected entry
  99 - called again after the transcript was already exhausted
"""

from __future__ import annotations

import json
import os
import sys


def _die(code: int, message: str) -> None:
    print(f"fake-herdr: {message}", file=sys.stderr)
    raise SystemExit(code)


def main() -> int:
    transcript_path = os.environ.get("MV_CONFORMANCE_TRANSCRIPT")
    state_path = os.environ.get("MV_CONFORMANCE_STATE")
    if not transcript_path or not state_path:
        _die(
            97,
            "MV_CONFORMANCE_TRANSCRIPT / MV_CONFORMANCE_STATE not set — this "
            "binary only runs inside the conformance harness.",
        )

    with open(transcript_path, "r", encoding="utf-8") as handle:
        transcript = json.load(handle)

    try:
        with open(state_path, "r", encoding="utf-8") as handle:
            state = json.load(handle)
    except FileNotFoundError:
        state = {"index": 0, "calls": []}

    argv = sys.argv[1:]
    state.setdefault("calls", []).append(argv)

    index = state["index"]
    if index >= len(transcript):
        state["index"] = index + 1
        state.setdefault("errors", []).append(f"unexpected argv: {argv!r}")
        with open(state_path, "w", encoding="utf-8") as handle:
            json.dump(state, handle)
        _die(
            99,
            f"unexpected call beyond the scripted transcript (entry "
            f"{index + 1}, transcript has {len(transcript)}): argv={argv!r}",
        )

    expected = transcript[index]
    if expected["match_argv"] != argv:
        state.setdefault("errors", []).append(
            f"argv mismatch: expected {expected['match_argv']!r}, got {argv!r}"
        )
        with open(state_path, "w", encoding="utf-8") as handle:
            json.dump(state, handle)
        _die(
            98,
            f"argv mismatch at transcript entry {index}: "
            f"expected {expected['match_argv']!r}, got {argv!r}",
        )

    state["index"] = index + 1
    with open(state_path, "w", encoding="utf-8") as handle:
        json.dump(state, handle)

    reply = expected.get("reply", {})
    stdout = reply.get("stdout", "")
    stderr = reply.get("stderr", "")
    if stdout:
        sys.stdout.write(stdout)
    if stderr:
        sys.stderr.write(stderr)
    return int(reply.get("exit", 0))


if __name__ == "__main__":
    raise SystemExit(main())

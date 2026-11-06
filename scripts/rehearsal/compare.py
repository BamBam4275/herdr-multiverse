#!/usr/bin/env python3
"""Normalize and diff Python-vs-Rust CLI output recorded by read_parity.sh.

Takes no project-specific knowledge of its own: `read_parity.sh` appends one
JSON line per command pair it ran to a manifest file, and this script reads
that manifest, normalizes both sides' captured stdout/stderr the same way
`conformance/normalize.py` normalizes scenario output (sandbox paths, task
and backlog ids, ISO timestamps, crc32, pid), and prints a PASS/DIFF table
followed by a unified diff for every DIFF.

Deliberately does NOT auto-translate old-role-name text to new-role-name
text before comparing (see conformance/README.md "Role rename: what gets
translated at replay time" for why a blanket rewrite of rendered text would
hide real rendering-contract differences rather than surface them). A
diff caused only by the role rename is still printed; the caller
(read_parity.sh's invoker) annotates which diffs are the expected rename
and which are not, in the task result, rather than this script silently
deciding for them.
"""

from __future__ import annotations

import json
import re
import sys
from dataclasses import dataclass

_ID_RE = re.compile(r"\b\d{8}-[a-z][a-z0-9]*(?:-[a-z0-9]+)?-[0-9a-f]{6}\b")
_TIMESTAMP_RE = re.compile(
    r"\b\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})\b"
)
_CRC32_RE = re.compile(r"crc32=[0-9a-f]{8}\b")
_PID_RE = re.compile(r"\bpid=\d+\b")
_PYVER_RE = re.compile(r"Python \d+\.\d+(?:\.\d+)? \(requires 3\.11\+\)")
_IDLE_SECONDS_RE = re.compile(r'"idle_seconds": [0-9.]+')


def normalize(text: str, roots: list[tuple[str, str]]) -> str:
    """`roots` is a list of (literal_path, placeholder) pairs, longest
    literal first, specific to whichever side (python/rust) produced
    `text` — each side's own sandbox paths collapse to the same shared
    placeholder names so the two sides become comparable."""
    out = text
    for literal, placeholder in roots:
        if literal:
            out = out.replace(literal, placeholder)
    out = _ID_RE.sub("<ID>", out)
    out = _TIMESTAMP_RE.sub("<TIMESTAMP>", out)
    out = _CRC32_RE.sub("crc32=<CRC32>", out)
    out = _PID_RE.sub("pid=<PID>", out)
    out = _PYVER_RE.sub("Python <PYVER> (requires 3.11+)", out)
    out = _IDLE_SECONDS_RE.sub('"idle_seconds": <IDLE>', out)
    return out


@dataclass
class Entry:
    label: str
    argv: str
    py_out: str
    py_err: str
    py_exit: int
    rs_out: str
    rs_err: str
    rs_exit: int


def read_text(path: str) -> str:
    try:
        with open(path, "r", errors="replace") as f:
            return f.read()
    except FileNotFoundError:
        return "<file not captured>"


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: compare.py <manifest.jsonl>", file=sys.stderr)
        return 2
    manifest_path = sys.argv[1]

    # Root substitution pairs come from environment, set by read_parity.sh,
    # since this script has no business knowing the sandbox layout itself.
    import os

    def roots_for(prefix: str) -> list[tuple[str, str]]:
        pairs = []
        raw = os.environ.get(prefix, "")
        for chunk in raw.split("\n"):
            if not chunk.strip() or "=" not in chunk:
                continue
            literal, placeholder = chunk.split("=", 1)
            pairs.append((literal, placeholder))
        pairs.sort(key=lambda p: -len(p[0]))
        return pairs

    py_roots = roots_for("RP_PY_ROOTS")
    rs_roots = roots_for("RP_RS_ROOTS")

    total = 0
    diffs = 0
    diff_report = []
    with open(manifest_path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            row = json.loads(line)
            total += 1
            py_text = normalize(
                read_text(row["py_out"]) + "\n--stderr--\n" + read_text(row["py_err"]),
                py_roots,
            )
            rs_text = normalize(
                read_text(row["rs_out"]) + "\n--stderr--\n" + read_text(row["rs_err"]),
                rs_roots,
            )
            py_exit = row["py_exit"]
            rs_exit = row["rs_exit"]
            same_text = py_text == rs_text
            same_exit = py_exit == rs_exit
            status = "PASS" if (same_text and same_exit) else "DIFF"
            if status == "DIFF":
                diffs += 1
            print(f"{status}\t{row['label']}\t{row['argv']}\tpy_exit={py_exit}\trs_exit={rs_exit}")
            if status == "DIFF":
                import difflib

                d = "\n".join(
                    difflib.unified_diff(
                        py_text.splitlines(),
                        rs_text.splitlines(),
                        fromfile=f"python:{row['label']}",
                        tofile=f"rust:{row['label']}",
                        lineterm="",
                    )
                )
                diff_report.append(f"=== {row['label']} ({row['argv']}) ===\n{d}\n")

    print(f"\n{total - diffs}/{total} matched after normalization\n")
    if diff_report:
        print("---- diffs ----")
        print("\n".join(diff_report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

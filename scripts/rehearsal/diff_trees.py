#!/usr/bin/env python3
"""Recursively diff two sandbox directory trees, normalizing each side's own
paths/ids/timestamps the same way `compare.py` normalizes command
stdout/stderr -- so a write command's *on-disk effect* (not just its printed
report) can be checked for Python/Rust parity.

Imports `normalize` from `compare.py` (same directory) rather than
redefining the regexes, so the two forms of comparison -- stdout/stderr in
`compare.py`, on-disk files here -- can never silently drift apart. Reads
the same `RP_PY_ROOTS`/`RP_RS_ROOTS` environment variables `compare.py`
already expects (set once by the caller script, shared by both tools).

Usage:
  diff_trees.py <python-dir> <rust-dir> [--exclude PREFIX]...

Walks both directories (skipping `.git`, a symlink, or any relative path
starting with an `--exclude` prefix), compares every file that exists on at
least one side, and prints one PASS/DIFF line per relative path followed by
a unified diff for each DIFF. A file missing from one side is itself a DIFF
(normalized against the literal text "<missing>").
"""

from __future__ import annotations

import difflib
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from compare import normalize  # noqa: E402

_MISSING = "<missing>"


def roots_for(env_var: str) -> list[tuple[str, str]]:
    """Parse one side's newline-separated `literal=placeholder` env var into
    (literal, placeholder) pairs, longest literal first, for `normalize`.
    Same parsing as `compare.py`'s own `roots_for`, duplicated rather than
    imported since it's a closure there."""
    pairs = []
    for chunk in os.environ.get(env_var, "").split("\n"):
        if not chunk.strip() or "=" not in chunk:
            continue
        literal, placeholder = chunk.split("=", 1)
        pairs.append((literal, placeholder))
    pairs.sort(key=lambda p: -len(p[0]))
    return pairs


def walk(root: Path, excludes: list[str]) -> set[str]:
    """Return every regular file's path relative to `root`, skipping `.git`,
    symlinks, and anything under an `--exclude` prefix. Returns an empty set
    for a root that doesn't exist, so a side that was never created shows
    every file on the other side as missing rather than erroring."""
    paths: set[str] = set()
    if not root.is_dir():
        return paths
    for p in root.rglob("*"):
        if p.is_dir() or p.is_symlink():
            continue
        rel = p.relative_to(root).as_posix()
        if rel == ".git" or rel.startswith(".git/"):
            continue
        if any(rel == e or rel.startswith(e + "/") or rel.startswith(e) for e in excludes):
            continue
        paths.add(rel)
    return paths


def read(path: Path) -> str | None:
    """Return a file's text, or None if it doesn't exist or can't be read as
    a regular file -- the caller treats None as "missing on this side"."""
    try:
        return path.read_text(errors="replace")
    except (FileNotFoundError, IsADirectoryError, PermissionError):
        return None


def main() -> int:
    """Parse argv, walk both directory trees, normalize and diff every file
    that exists on at least one side, print one PASS/DIFF line per relative
    path followed by unified diffs for the DIFFs, and return 2 on a usage
    error or 0 otherwise -- a DIFF is reported in the table, not via a
    nonzero exit."""
    args = sys.argv[1:]
    excludes: list[str] = []
    positional: list[str] = []
    # Hand-rolled flag parsing rather than argparse, so `--exclude` can
    # repeat and interleave freely with the two positional directories.
    i = 0
    while i < len(args):
        if args[i] == "--exclude" and i + 1 < len(args):
            excludes.append(args[i + 1])
            i += 2
        else:
            positional.append(args[i])
            i += 1
    if len(positional) != 2:
        print(
            "usage: diff_trees.py <python-dir> <rust-dir> [--exclude PREFIX]...",
            file=sys.stderr,
        )
        return 2
    py_dir, rs_dir = Path(positional[0]), Path(positional[1])
    py_roots = roots_for("RP_PY_ROOTS")
    rs_roots = roots_for("RP_RS_ROOTS")

    rels = sorted(walk(py_dir, excludes) | walk(rs_dir, excludes))
    total = 0
    diffs = 0
    diff_report = []
    for rel in rels:
        total += 1
        py_raw = read(py_dir / rel)
        rs_raw = read(rs_dir / rel)
        py_norm = normalize(py_raw if py_raw is not None else _MISSING, py_roots)
        rs_norm = normalize(rs_raw if rs_raw is not None else _MISSING, rs_roots)
        status = "PASS" if py_norm == rs_norm else "DIFF"
        if status == "DIFF":
            diffs += 1
            d = "\n".join(
                difflib.unified_diff(
                    py_norm.splitlines(),
                    rs_norm.splitlines(),
                    fromfile=f"python:{rel}",
                    tofile=f"rust:{rel}",
                    lineterm="",
                )
            )
            diff_report.append(f"=== {rel} ===\n{d}\n")
        print(f"{status}\t{rel}")

    print(f"\n{total - diffs}/{total} files matched after normalization\n")
    if diff_report:
        print("---- file diffs ----")
        print("\n".join(diff_report))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

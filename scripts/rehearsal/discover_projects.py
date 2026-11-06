#!/usr/bin/env python3
"""Discover configkey:dirname pairs from a config.toml's `[projects.*]`
tables, for read_parity.sh's rehearsal matrix -- so that script never has
to carry a hardcoded list of real project names.

Line-oriented and narrow on purpose, matching rewrite_config.py's style:
finds each top-level `[projects.<key>]` section header (not a deeper
table such as `[projects.<key>.chains]`) and that section's own
`path = "..."` line, and prints `<key>:<basename of path>` pairs, one
per line, in the order they appear in the file. A section with a
placeholder or missing path (as in config.example.toml) is skipped.
"""

from __future__ import annotations

import re
import sys

_SECTION_RE = re.compile(r"^\[projects\.([^.\]]+)\]$")
_OTHER_SECTION_RE = re.compile(r"^\[.*\]$")
_PATH_RE = re.compile(r'^path = "([^"]*)"')


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: discover_projects.py <config.toml>", file=sys.stderr)
        return 2

    current_key = None
    with open(sys.argv[1]) as f:
        for raw in f:
            line = raw.strip()
            m = _SECTION_RE.match(line)
            if m:
                current_key = m.group(1)
                continue
            if _OTHER_SECTION_RE.match(line):
                current_key = None
                continue
            if current_key is None:
                continue
            m = _PATH_RE.match(line)
            if not m:
                continue
            path = m.group(1).rstrip("/")
            if path and "<<PLACEHOLDER" not in path:
                dirname = path.rsplit("/", 1)[-1]
                print(f"{current_key}:{dirname}")
            current_key = None
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Rewrite `path = "..."` project lines in a copied config.toml so every
configured project points at a sandbox copy instead of the real checkout.

Line-oriented and narrow on purpose, matching the existing
`conformance/role_translate.py` style: it only ever rewrites the quoted
value of a line matching `^path = "..."` (optionally followed by an inline
`#` comment, which is preserved verbatim), never touches anything else in
the file, and keeps the rest of the document -- including every comment --
byte-identical.

Any `path = "..."` value NOT named in the mapping file is redirected to
`--unmapped-dir` rather than left pointing at a real checkout. This is the
safety net for a project this script's caller does not know about (e.g. a
project added to the real fleet config after this script was written): it
guarantees the rewritten copy never references a real project path, even
for a project nobody remembered to list.
"""

from __future__ import annotations

import argparse
import re

_PATH_LINE_RE = re.compile(r'^path = "([^"]*)"(\s*#.*)?$')


def load_mapping(path: str) -> dict[str, str]:
    mapping = {}
    with open(path) as f:
        for line in f:
            line = line.rstrip("\n")
            if not line or "=" not in line:
                continue
            old, new = line.split("=", 1)
            mapping[old] = new
    return mapping


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("input_config")
    parser.add_argument("output_config")
    parser.add_argument("--mapping", required=True, help="file of old_path=new_path lines")
    parser.add_argument("--unmapped-dir", required=True)
    args = parser.parse_args()

    mapping = load_mapping(args.mapping)

    with open(args.input_config) as f:
        lines = f.readlines()

    out = []
    rewritten = 0
    unmapped = 0
    for line in lines:
        m = _PATH_LINE_RE.match(line.rstrip("\n"))
        if m is None:
            out.append(line)
            continue
        old_value, comment = m.group(1), m.group(2) or ""
        new_value = mapping.get(old_value)
        if new_value is None:
            new_value = args.unmapped_dir
            unmapped += 1
        else:
            rewritten += 1
        out.append(f'path = "{new_value}"{comment}\n')

    with open(args.output_config, "w") as f:
        f.writelines(out)

    print(f"rewrote {rewritten} mapped project path(s), {unmapped} unmapped path(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

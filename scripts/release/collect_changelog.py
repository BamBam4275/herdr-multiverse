#!/usr/bin/env python3
"""Merge changelog.d/ fragments into CHANGELOG.md's existing [Unreleased]
section, in stable filename order, and delete the fragments consumed.

See changelog.d/README.md for the fragment naming convention this expects.
Existing [Unreleased] text is never reordered or rewritten -- fragments are
only appended under the section heading they name, creating that heading
(in Keep a Changelog order) if it doesn't exist yet.
"""

import argparse
from pathlib import Path
import re
import sys

SECTIONS = ["Added", "Changed", "Deprecated", "Removed", "Fixed", "Security"]
SECTION_BY_LOWER = {name.lower(): name for name in SECTIONS}

UNRELEASED_RE = re.compile(r"^## \[Unreleased\]\s*$")
VERSION_RE = re.compile(r"^## \[")
HEADER_RE = re.compile(r"^### (\w+)\s*$")


class FragmentError(ValueError):
    """A fragment file, or CHANGELOG.md itself, doesn't match what this
    collector expects."""


def parse_fragment_name(path):
    """Returns (slug, canonical_section) for changelog.d/<slug>.<section>.md,
    or raises FragmentError naming the file and the accepted section set."""
    stem = path.name[:-3] if path.name.endswith(".md") else path.name
    if "." not in stem:
        raise FragmentError(
            f"{path.name}: fragment files must be named <slug>.<section>.md "
            f"(section one of {', '.join(SECTIONS)}, case-insensitive)"
        )
    slug, _, section = stem.rpartition(".")
    canonical = SECTION_BY_LOWER.get(section.lower())
    if canonical is None:
        raise FragmentError(
            f"{path.name}: unknown changelog section {section!r} -- "
            f"must be one of {', '.join(SECTIONS)}"
        )
    return slug, canonical


def load_fragments(fragments_dir):
    """Returns ({canonical_section: [line, ...]}, [consumed_path, ...]).

    Fragments are read in filename-sorted order so collection is
    independent of directory-listing order and gives a stable result."""
    by_section = {}
    paths = sorted(p for p in fragments_dir.glob("*.md") if p.name != "README.md")
    for path in paths:
        _, canonical = parse_fragment_name(path)
        text = path.read_text().strip("\n")
        if not text:
            continue
        by_section.setdefault(canonical, []).extend(text.split("\n"))
    return by_section, paths


def _content_end(lines, content_start, boundary):
    """boundary is the index right after a content block; a single blank
    separator line sits at boundary - 1 in every real fragment this expects,
    but this stays correct if that line is absent (e.g. end of file)."""
    if boundary - 1 >= content_start and lines[boundary - 1].strip() == "":
        return boundary - 1
    return boundary


def collect(changelog_text, new_by_section):
    """Returns CHANGELOG.md's text with new_by_section's lines appended
    under [Unreleased], creating any section heading that doesn't exist yet.
    Raises FragmentError if CHANGELOG.md has no [Unreleased] section."""
    lines = changelog_text.split("\n")
    start = next((i for i, line in enumerate(lines) if UNRELEASED_RE.match(line)), None)
    if start is None:
        raise FragmentError("CHANGELOG.md has no [Unreleased] section")
    end = next(
        (i for i in range(start + 1, len(lines)) if VERSION_RE.match(lines[i])),
        len(lines),
    )

    headers = [i for i in range(start + 1, end) if HEADER_RE.match(lines[i])]
    existing = {}
    order = []
    for idx, header_idx in enumerate(headers):
        name = HEADER_RE.match(lines[header_idx]).group(1)
        canonical = SECTION_BY_LOWER.get(name.lower(), name)
        content_start = header_idx + 2
        boundary = headers[idx + 1] if idx + 1 < len(headers) else end
        content_end = _content_end(lines, content_start, boundary)
        existing[canonical] = lines[content_start:content_end]
        order.append(canonical)

    for section, new_lines in new_by_section.items():
        existing.setdefault(section, [])
        existing[section] = existing[section] + new_lines
        if section not in order:
            order.append(section)

    ordered_sections = [s for s in SECTIONS if s in existing]
    ordered_sections += [s for s in order if s not in SECTIONS]

    body = ["## [Unreleased]", ""]
    for section in ordered_sections:
        body.append(f"### {section}")
        body.append("")
        body.extend(existing[section])
        body.append("")

    return "\n".join(lines[:start] + body + lines[end:])


def unreleased_section(changelog_text):
    """Returns just the [Unreleased] block's text (for --dry-run output)."""
    lines = changelog_text.split("\n")
    start = next(i for i, line in enumerate(lines) if UNRELEASED_RE.match(line))
    end = next(
        (i for i in range(start + 1, len(lines)) if VERSION_RE.match(lines[i])),
        len(lines),
    )
    return "\n".join(lines[start:end]).rstrip("\n")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("changelog", type=Path, help="path to CHANGELOG.md")
    parser.add_argument("fragments_dir", type=Path, help="path to changelog.d/")
    parser.add_argument(
        "--dry-run", action="store_true", help="print the would-be section; change nothing"
    )
    args = parser.parse_args(argv)

    try:
        new_by_section, paths = load_fragments(args.fragments_dir)
    except FragmentError as error:
        print(f"collect-changelog: {error}", file=sys.stderr)
        return 2

    if not new_by_section:
        print("collect-changelog: no fragments to collect")
        return 0

    try:
        changelog_text = args.changelog.read_text()
        merged = collect(changelog_text, new_by_section)
    except FragmentError as error:
        print(f"collect-changelog: {error}", file=sys.stderr)
        return 2

    if args.dry_run:
        print(unreleased_section(merged))
        return 0

    args.changelog.write_text(merged)
    for path in paths:
        path.unlink()
    print(f"collect-changelog: merged {len(paths)} fragment(s) into {args.changelog}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

# Changelog fragments

Every lane that needs a `CHANGELOG.md` entry adds one file here instead of
hand-editing `CHANGELOG.md`'s `[Unreleased]` section directly. Hand-editing
is what makes almost every parallel lane conflict in that one file; a
fragment per lane removes the conflict at the source instead of resolving
it after the fact. `scripts/release/collect-changelog.sh` merges these
fragments into `[Unreleased]` and deletes the ones it consumes.

## Naming

    changelog.d/<task-or-slug>.<section>.md

`<task-or-slug>` is your dispatched task id, or another short, unique slug
if you're not working a dispatched task. `<section>` is one of (matching
the Keep a Changelog headings already used in `CHANGELOG.md`,
case-insensitive): `added`, `changed`, `deprecated`, `removed`, `fixed`,
`security`. A file named with any other section is refused with a message
naming the accepted set.

## Content

One or more `CHANGELOG.md`-style bullets, exactly as they should appear
under that section's heading — a leading `- `, continuation lines indented
two spaces, no blank line between bullets within one fragment:

    - CLI: `workers` gained `--json`, emitting one line of machine-readable
      JSON instead of the text table.

## Collecting

    scripts/release/collect-changelog.sh --dry-run   # print the would-be section, change nothing
    scripts/release/collect-changelog.sh              # merge into CHANGELOG.md, delete the fragments

Fragments collect in filename order — stable, independent of directory
listing order — and are appended after any text already under that
section's heading in `[Unreleased]`; a heading that doesn't exist yet is
created in Keep a Changelog order. Existing `[Unreleased]` text is never
reordered or rewritten, only appended to. Collecting with no fragments
present is a no-op.

Run this before moving `[Unreleased]` into a dated version section at
release time, so that step sees the full accumulated text.

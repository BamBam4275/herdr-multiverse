#!/bin/sh
# Merge changelog.d/ fragments into CHANGELOG.md's existing [Unreleased]
# section and delete the fragments it consumed. See changelog.d/README.md
# for the fragment naming convention.
#
# The merge itself runs in scripts/release/collect_changelog.py; this
# script only resolves paths and forwards --dry-run. POSIX sh only.
#
# Usage:
#   scripts/release/collect-changelog.sh [--dry-run]
#
#   --dry-run   print the would-be [Unreleased] section; change nothing.
#
# Exit status: 0 collected (or nothing to collect), 2 usage or fragment error.
set -eu

DRY_RUN=0
case "${1:-}" in
    --dry-run) DRY_RUN=1; shift ;;
    -*) echo "usage: $0 [--dry-run]" >&2; exit 2 ;;
esac
if [ "$#" -ne 0 ]; then
    echo "usage: $0 [--dry-run]" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)

if [ "$DRY_RUN" -eq 1 ]; then
    exec python3 "$SCRIPT_DIR/collect_changelog.py" \
        "$PLUGIN_ROOT/CHANGELOG.md" "$PLUGIN_ROOT/changelog.d" --dry-run
fi
exec python3 "$SCRIPT_DIR/collect_changelog.py" \
    "$PLUGIN_ROOT/CHANGELOG.md" "$PLUGIN_ROOT/changelog.d"

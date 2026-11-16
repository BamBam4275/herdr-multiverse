#!/bin/sh
# Fails the tracked tree against a deny-list of names that must never appear
# in it (operator project names, hostnames, anything that would de-anonymize
# a real deployment). The deny-list itself lives outside the repo — one name
# or regex fragment per line, blank lines and '#' comments ignored — so this
# script never ships the list it checks against. Point MV_PRIVATE_NAMES at
# it to run the check; leaving it unset skips the check entirely, which is
# what CI does by default. Run this locally before every push once
# MV_PRIVATE_NAMES is set in your shell.
#
# POSIX sh only.
set -eu

if [ -z "${MV_PRIVATE_NAMES:-}" ]; then
    echo "check-private-names: MV_PRIVATE_NAMES is unset, skipping" >&2
    exit 0
fi

if [ ! -f "$MV_PRIVATE_NAMES" ]; then
    echo "check-private-names: $MV_PRIVATE_NAMES not found, skipping" >&2
    exit 0
fi

PATTERN=$(grep -vE '^[[:space:]]*(#|$)' "$MV_PRIVATE_NAMES" | paste -sd '|' -)

if [ -z "$PATTERN" ]; then
    echo "check-private-names: $MV_PRIVATE_NAMES has no entries, skipping" >&2
    exit 0
fi

REPO_ROOT=$(git rev-parse --show-toplevel)

GREP_ERR=$(mktemp "${TMPDIR:-/tmp}/check-private-names.XXXXXX")
trap 'rm -f "$GREP_ERR"' 0

set +e
HITS=$(cd "$REPO_ROOT" && git grep -niE "$PATTERN" -- . 2>"$GREP_ERR")
STATUS=$?
set -e

# git grep's exit code is inverted from the usual success/failure sense:
# 0 means a match was found (bad, here), 1 means no match (the clean case
# this check wants), anything else is a real failure to even run the search.
case "$STATUS" in
    0)
        echo "check-private-names: tracked tree references names from $MV_PRIVATE_NAMES:" >&2
        echo "$HITS" >&2
        exit 1
        ;;
    1)
        exit 0
        ;;
    *)
        echo "check-private-names: git grep failed (exit $STATUS), cannot verify the tree is clean -- refusing to report clean:" >&2
        cat "$GREP_ERR" >&2
        exit 2
        ;;
esac

#!/bin/sh
# Exports the tracked tree at HEAD into a brand-new, history-free git repo
# and verifies that export is fit to publish as this project's public
# mirror: one fresh commit, no prior history.
#
# `git archive HEAD` only ever emits tracked, committed content, so
# untracked and gitignored paths (.multiverse/, the conformance/history
# corpus, a local config.toml) can never leak into the export no matter
# what sits in this working tree or index. Nothing below reads the working
# tree directly.
#
# This script never adds a remote and never pushes. Publishing the result
# to GitHub is a separate, deliberate step the operator takes by hand.
#
# POSIX sh only.
#
# Usage:
#   scripts/release/export-fresh.sh OUTPUT_DIR
#
#   OUTPUT_DIR is created if missing and must be empty.
#
# Required environment (the new repo's single commit identity — this
# script hardcodes no name or email of its own):
#   GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL,
#   GIT_COMMITTER_NAME, GIT_COMMITTER_EMAIL
#
# Optional environment:
#   MV_PRIVATE_NAMES   inherited by the private-names check below exactly
#                       as scripts/check-private-names.sh already defines
#                       it: unset skips that check.
#
# Exit status: 0 export produced and every check passed, 1 a verification
# step failed, 2 usage/setup error.

set -eu

OUT_DIR=${1:-}

if [ -z "$OUT_DIR" ]; then
    echo "usage: $0 OUTPUT_DIR" >&2
    exit 2
fi

if [ -z "${GIT_AUTHOR_NAME:-}" ] || [ -z "${GIT_AUTHOR_EMAIL:-}" ] ||
    [ -z "${GIT_COMMITTER_NAME:-}" ] || [ -z "${GIT_COMMITTER_EMAIL:-}" ]; then
    echo "export-fresh: GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, GIT_COMMITTER_NAME, GIT_COMMITTER_EMAIL must all be set" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)

mkdir -p "$OUT_DIR"
OUT_DIR=$(CDPATH='' cd -- "$OUT_DIR" && pwd)
if [ -n "$(find "$OUT_DIR" -mindepth 1 -maxdepth 1 2>/dev/null)" ]; then
    echo "export-fresh: $OUT_DIR is not empty" >&2
    exit 2
fi

note() { printf '%s\n' "$1"; }
ok() { printf 'PASS: %s\n' "$1"; }
bad() { printf 'FAIL: %s\n' "$1"; fail=1; }
fail=0

note "== exporting tracked tree at HEAD =="
# POSIX pipelines report only the last command's status. Stage the archive
# first so successful extraction cannot hide a failed git archive.
ARCHIVE=$(mktemp "${TMPDIR:-/tmp}/multiverse-export.XXXXXX")
trap 'rm -f "$ARCHIVE"' 0
trap 'exit 1' HUP INT TERM
if ! (cd "$PLUGIN_ROOT" && git archive --output="$ARCHIVE" HEAD) ||
    ! tar -x -f "$ARCHIVE" -C "$OUT_DIR"; then
    echo "export-fresh: could not archive $PLUGIN_ROOT into $OUT_DIR" >&2
    exit 1
fi
rm -f "$ARCHIVE"
trap - 0 HUP INT TERM
ok "exported $(git -C "$PLUGIN_ROOT" rev-parse --short HEAD) into $OUT_DIR"

note "== starting fresh history =="
git -C "$OUT_DIR" init -q -b main
git -C "$OUT_DIR" add -A
git -C "$OUT_DIR" commit -q -m "chore: initial public release"
ok "single commit on main ($(git -C "$OUT_DIR" rev-parse --short HEAD))"

note "== cargo build --locked =="
if (cd "$OUT_DIR" && cargo build --locked); then
    ok "cargo build --locked"
else
    bad "cargo build --locked"
fi

note "== cargo test --workspace --locked =="
if (cd "$OUT_DIR" && cargo test --workspace --locked); then
    ok "cargo test --workspace --locked"
else
    bad "cargo test --workspace --locked"
fi

note "== conformance/run_all.py =="
if (cd "$OUT_DIR" && python3 conformance/run_all.py); then
    ok "conformance/run_all.py"
else
    bad "conformance/run_all.py"
fi

note "== private-names check =="
if (cd "$OUT_DIR" && ./scripts/check-private-names.sh); then
    ok "check-private-names.sh"
else
    bad "check-private-names.sh"
fi

note "== gitleaks dir scan =="
if command -v docker >/dev/null 2>&1; then
    if docker run --rm -v "$OUT_DIR:/export:ro" zricethezav/gitleaks:v8.21.2 dir /export -v; then
        ok "gitleaks dir scan"
    else
        bad "gitleaks dir scan"
    fi
else
    note "docker not available, skipping gitleaks scan"
fi

note ""
if [ "$fail" -eq 0 ]; then
    note "overall: PASS -- export at $OUT_DIR"
else
    note "overall: FAIL -- export at $OUT_DIR"
fi
exit "$fail"

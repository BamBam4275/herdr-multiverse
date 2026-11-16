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

# $1=have $2=want, each "MAJOR.MINOR.PATCH" -- true if have >= want.
# Pure parameter expansion, no external tools, so this never depends on
# what else happens to be on PATH.
version_ge() {
    have_major=${1%%.*}; have_rest=${1#*.}
    have_minor=${have_rest%%.*}; have_patch=${have_rest#*.}
    want_major=${2%%.*}; want_rest=${2#*.}
    want_minor=${want_rest%%.*}; want_patch=${want_rest#*.}
    [ "$have_major" -gt "$want_major" ] 2>/dev/null && return 0
    [ "$have_major" -lt "$want_major" ] 2>/dev/null && return 1
    [ "$have_minor" -gt "$want_minor" ] 2>/dev/null && return 0
    [ "$have_minor" -lt "$want_minor" ] 2>/dev/null && return 1
    [ "$have_patch" -ge "$want_patch" ] 2>/dev/null
}

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
# Minimum-version check, not exact: brew (and other package managers) track
# upstream continuously and will drift ahead of this pin between bumps, so
# an exact match here reproduces the very failure this pin exists to fix
# (see 20261115-b-d23447). A minimum still refuses anything older than what
# .gitleaks.toml has actually been verified against.
. "$SCRIPT_DIR/gitleaks-version.sh"
if command -v gitleaks >/dev/null 2>&1; then
    found_version=$(gitleaks version 2>/dev/null)
    found_version=${found_version#v}
    if ! version_ge "$found_version" "$GITLEAKS_VERSION"; then
        echo "export-fresh: local gitleaks is v${found_version:-unknown}, this export requires >= v$GITLEAKS_VERSION -- install that version or newer (https://github.com/gitleaks/gitleaks/releases) or run with Docker instead." >&2
        bad "gitleaks dir scan"
    elif gitleaks dir "$OUT_DIR" --config "$OUT_DIR/.gitleaks.toml" -v; then
        ok "gitleaks dir scan"
    else
        bad "gitleaks dir scan"
    fi
elif command -v docker >/dev/null 2>&1; then
    if docker run --rm -v "$OUT_DIR:/export:ro" "zricethezav/gitleaks:v$GITLEAKS_VERSION" dir /export --config /export/.gitleaks.toml -v; then
        ok "gitleaks dir scan"
    else
        bad "gitleaks dir scan"
    fi
else
    echo "export-fresh: gitleaks is not installed and docker is not available -- install gitleaks >= v$GITLEAKS_VERSION (https://github.com/gitleaks/gitleaks/releases) or Docker, then retry." >&2
    bad "gitleaks dir scan"
fi

note ""
if [ "$fail" -eq 0 ]; then
    note "overall: PASS -- export at $OUT_DIR"
else
    note "overall: FAIL -- export at $OUT_DIR"
fi
exit "$fail"

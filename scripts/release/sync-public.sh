#!/bin/sh
# Prepare a later public release without replacing the public Git history.
# Usage: scripts/release/sync-public.sh PUBLIC_CLONE_DIR VERSION
# Requires the same four Git identity variables as export-fresh.sh.
# Never pushes; prints the exact command for the operator after verification.
# POSIX sh only.
set -eu

if [ "$#" -ne 2 ]; then
    echo "usage: $0 PUBLIC_CLONE_DIR VERSION" >&2
    exit 2
fi
PUBLIC_DIR=$1
VERSION=$2
case "$VERSION" in
    ''|*[!0-9A-Za-z.+-]*|[!0-9]*)
        echo "sync-public: VERSION must be an unprefixed release version" >&2
        exit 2
        ;;
esac
if [ -z "${GIT_AUTHOR_NAME:-}" ] || [ -z "${GIT_AUTHOR_EMAIL:-}" ] ||
    [ -z "${GIT_COMMITTER_NAME:-}" ] || [ -z "${GIT_COMMITTER_EMAIL:-}" ]; then
    echo "sync-public: GIT_AUTHOR_NAME, GIT_AUTHOR_EMAIL, GIT_COMMITTER_NAME, GIT_COMMITTER_EMAIL must all be set" >&2
    exit 2
fi

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd -P)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd -P)
PUBLIC_DIR=$(CDPATH='' cd -- "$PUBLIC_DIR" && pwd -P)
if [ "$(git -C "$PUBLIC_DIR" rev-parse --show-toplevel)" != "$PUBLIC_DIR" ]; then
    echo "sync-public: PUBLIC_CLONE_DIR must be a repository root" >&2
    exit 2
fi
# rsync --delete must never target the source checkout or one of its parents.
case "$PLUGIN_ROOT/" in
    "$PUBLIC_DIR/"*)
        echo "sync-public: public clone must be separate from the source checkout" >&2
        exit 2
        ;;
esac
case "$PUBLIC_DIR/" in
    "$PLUGIN_ROOT/"*)
        echo "sync-public: public clone must be separate from the source checkout" >&2
        exit 2
        ;;
esac

check_public() {
    PUBLIC_STATUS=$(git -C "$PUBLIC_DIR" status --porcelain --untracked-files=all)
    if [ -n "$PUBLIC_STATUS" ]; then
        echo "sync-public: public clone has uncommitted changes" >&2
        exit 2
    fi
    if [ "$(git -C "$PUBLIC_DIR" symbolic-ref --quiet --short HEAD)" != main ]; then
        echo "sync-public: public clone must be on main" >&2
        exit 2
    fi
    # Refresh the comparison; a stale origin/main must not hide newer commits.
    git -C "$PUBLIC_DIR" fetch --no-tags origin refs/heads/main:refs/remotes/origin/main
    if ! git -C "$PUBLIC_DIR" merge-base --is-ancestor origin/main HEAD; then
        echo "sync-public: main is behind origin/main or has diverged" >&2
        exit 2
    fi
    if git -C "$PUBLIC_DIR" show-ref --verify --quiet "refs/tags/v$VERSION"; then
        echo "sync-public: tag v$VERSION already exists" >&2
        exit 2
    fi
}
check_public

EXPORT_DIR=$(mktemp -d "${TMPDIR:-/tmp}/multiverse-sync.XXXXXX")
trap 'rm -rf "$EXPORT_DIR"' 0
trap 'exit 1' HUP INT TERM
# This disposable repo provides exactly the same export and checks, including
# git-backed private-name checking. Its fresh history never enters the clone.
sh "$SCRIPT_DIR/export-fresh.sh" "$EXPORT_DIR"

python3 - "$EXPORT_DIR" "$VERSION" <<'PY'
from pathlib import Path
import sys
import tomllib

root = Path(sys.argv[1])
version = sys.argv[2]
try:
    workspace = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    plugin = tomllib.loads((root / "herdr-plugin.toml").read_text())["version"]
except (OSError, ValueError, KeyError) as error:
    sys.exit(f"sync-public: could not read release versions: {error}")
if workspace != version or plugin != version:
    sys.exit(
        f"sync-public: version mismatch: requested {version}, "
        f"workspace {workspace}, plugin {plugin}"
    )
PY

# Build/check output is not part of the tracked release tree.
git -C "$EXPORT_DIR" clean -q -ffdx
# Checks can take a while; refuse changes made in the clone during that time.
check_public
printf '%s\n' '== replacing public tree with verified export =='
rsync -a --checksum --delete --exclude=/.git "$EXPORT_DIR/" "$PUBLIC_DIR/"
git -C "$PUBLIC_DIR" add -A
git -C "$PUBLIC_DIR" commit -m "release: v$VERSION"
git -C "$PUBLIC_DIR" tag -a "v$VERSION" -m "release: v$VERSION"
printf '\n%s\n' 'Release prepared. To publish, run:'
printf 'git push origin main v%s\n' "$VERSION"

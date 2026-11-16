#!/bin/sh
# Opts this checkout in to the repo's git hooks (pre-push: gitleaks scan
# and private-names check). Run once per clone or worktree; see
# CONTRIBUTING.md's dev setup. Opt-in only: nothing runs this automatically.
#
# POSIX sh only.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)

git -C "$REPO_ROOT" config core.hooksPath scripts/hooks
echo "install-hooks: core.hooksPath set to scripts/hooks (pre-push now runs gitleaks and private-names)."

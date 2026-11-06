#!/bin/sh
# Builds the multiverse binary from source: the bootstrap-plugin.sh fallback
# when no release asset matches this platform, and the CI release job's
# per-target build step.
#
# Usage: scripts/build.sh [target-triple]
#   No argument: host build, output at target/release/multiverse.
#   With a target triple: cross/native target build, output at
#   target/<triple>/release/multiverse.
set -eu

TARGET=${1:-}

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
cd "$PLUGIN_ROOT"

if [ -n "$TARGET" ]; then
    cargo build --release --locked -p mv-cli --target "$TARGET"
else
    cargo build --release --locked -p mv-cli
fi

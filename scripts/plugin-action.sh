#!/bin/sh
# Runtime entry point for every action/pane. Herdr runs these from the
# plugin directory; derive the binary path from this script, not an env var.
# Herdr 0.9.5 contract: https://herdr.dev/tui/docs/plugins/#commands-and-environment
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
BIN="$PLUGIN_ROOT/bin/multiverse"

# Managed actions always use Herdr's config/state, even when the parent has
# standalone MULTIVERSE_* overrides. A missing config must never select an
# unrelated global fleet. Use bin/multiverse directly for standalone overrides.
case "${HERDR_PLUGIN_CONFIG_DIR:-}" in
    /*) ;;
    *) echo "plugin-action: HERDR_PLUGIN_CONFIG_DIR must be an absolute directory" >&2; exit 3 ;;
esac
case "${HERDR_PLUGIN_STATE_DIR:-}" in
    /*) ;;
    *) echo "plugin-action: HERDR_PLUGIN_STATE_DIR must be an absolute directory" >&2; exit 3 ;;
esac
MULTIVERSE_CONFIG="$HERDR_PLUGIN_CONFIG_DIR/config.toml"
MULTIVERSE_STATE_DIR="$HERDR_PLUGIN_STATE_DIR"
# `import-python` is the one action that may run before config.toml
# exists: copying it into place is its job, so it alone skips this check.
if [ "${1:-}" != "import-python" ] && { [ ! -r "$MULTIVERSE_CONFIG" ] || [ ! -f "$MULTIVERSE_CONFIG" ]; }; then
    echo "plugin-action: configure $MULTIVERSE_CONFIG first (herdr plugin config-dir multiverse); see docs/install.md" >&2
    exit 3
fi
export MULTIVERSE_CONFIG MULTIVERSE_STATE_DIR

# The seed stores a shell command used later from a project's cwd. Quote
# both spaces and apostrophes, and keep the real Herdr binary/socket intact.
# This value is parsed as a command by the seed, rather than expanded as argv.
# shellcheck disable=SC2089
MULTIVERSE_INVOCATION="'$(printf '%s' "$BIN" | sed "s/'/'\\\\''/g")'"
# shellcheck disable=SC2090
export MULTIVERSE_INVOCATION
case "${1:-}" in
    start|import-python) unset MULTIVERSE_READ_ONLY ;;
    *) MULTIVERSE_READ_ONLY=1; export MULTIVERSE_READ_ONLY ;;
esac

# Linked checkouts skip [[build]]. Only bootstrap after config validation.
if [ ! -x "$BIN" ]; then
    "$SCRIPT_DIR/bootstrap-plugin.sh" action
fi

exec "$BIN" "$@"

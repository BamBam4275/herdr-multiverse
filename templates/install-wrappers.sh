#!/bin/sh
# Generate the per-account wrapper scripts from the template.
#
# A fleet commonly needs more than one Claude account wrapper — say
# `claude-work` and `claude-personal` for two subscriptions on one machine.
# They differ only in a config directory, so one parameterized template plus
# this generator beats several near-identical scripts that can drift apart.
#
#   ./templates/install-wrappers.sh <dest-dir> work=<abs-dir> personal=<abs-dir>
#
# Example (the paths are yours to supply):
#
#   ./templates/install-wrappers.sh ~/.local/bin \
#       work=/absolute/path/to/work-config \
#       personal=/absolute/path/to/personal-config
#
# Writes ONLY into <dest-dir>. Refuses relative paths, because `~` is not
# expanded downstream and a relative config dir silently resolves against
# whatever cwd the agent happened to start in.

set -eu

usage() {
  echo "usage: $0 <dest-dir> <account>=<absolute-config-dir> [...]" >&2
  exit 64  # EX_USAGE
}

[ $# -ge 2 ] || usage

DEST="$1"; shift
TEMPLATE="$(dirname "$0")/claude-account.sh.template"

[ -f "$TEMPLATE" ] || { echo "missing template: $TEMPLATE" >&2; exit 66; }
if [ ! -d "$DEST" ]; then
  echo "creating wrapper destination: $DEST" >&2
  mkdir -p "$DEST"
fi

for pair in "$@"; do
  case "$pair" in
    *=*) ;;
    *) usage ;;
  esac

  account="${pair%%=*}"
  config_dir="${pair#*=}"

  case "$config_dir" in
    /*) ;;
    *) echo "$account: config dir must be absolute, got: $config_dir" >&2; exit 78 ;;
  esac

  target="$DEST/claude-$account"
  sed -e "s|{{CONFIG_DIR}}|$config_dir|g" \
      -e "s|{{ACCOUNT_NAME}}|$account|g" \
      "$TEMPLATE" > "$target"
  chmod +x "$target"
  echo "wrote $target  (CLAUDE_CONFIG_DIR=$config_dir)"
done

echo
echo "Verify each seat by hand before dispatching against it:"
for pair in "$@"; do
  echo "  $DEST/claude-${pair%%=*}    then run /status"
done

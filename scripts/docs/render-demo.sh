#!/bin/sh
# Renders docs/assets/board-demo.svg from a real `multiverse board|workers|
# tasks` run against a throwaway fixture fleet: two fake projects
# ("web-app", "api"), a fake account, and the repository's own fake-herdr
# binary standing in for a live Herdr install. Nothing here is a real
# project, account, or deployment — every name is generic by design, and
# `scripts/check-private-names.sh` runs over the result before it lands.
#
# Reproducible and self-contained: everything lives under a fresh mktemp
# directory, HOME/config/state are all isolated there, and HERDR_BIN_PATH
# always points at the repo's own fake-herdr build — never a real `herdr`
# command or socket.
#
# Usage: scripts/docs/render-demo.sh [output.svg]
#   No argument: writes docs/assets/board-demo.svg.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
REPO_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/../.." && pwd)
cd "$REPO_ROOT"

OUT_SVG=${1:-docs/assets/board-demo.svg}

echo "render-demo: building multiverse and fake-herdr..." >&2
cargo build --locked -p mv-cli --bin multiverse >&2
cargo build --locked -p mv-herdr --bin fake-herdr >&2

TARGET_DIR=$(cargo metadata --no-deps --format-version 1 --locked 2>/dev/null |
    python3 -I -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')
MULTIVERSE_BIN="$TARGET_DIR/debug/multiverse"
FAKE_HERDR_BIN="$TARGET_DIR/debug/fake-herdr"

WORKDIR=$(mktemp -d "${TMPDIR:-/tmp}/mv-render-demo.XXXXXX")
trap 'rm -rf "$WORKDIR"' EXIT

HOME_DIR="$WORKDIR/home"
CONFIG_DIR="$WORKDIR/config"
STATE_DIR="$WORKDIR/state"
CODEX_HOME="$WORKDIR/codex-home"
mkdir -p "$HOME_DIR" "$CONFIG_DIR" "$STATE_DIR" "$CODEX_HOME" "$WORKDIR/projects"

for project in web-app api; do
    proj_dir="$WORKDIR/projects/$project"
    mkdir -p "$proj_dir"
    git -C "$proj_dir" init -q -b main
    git -C "$proj_dir" config user.email "demo@example.invalid"
    git -C "$proj_dir" config user.name "Demo"
    git -C "$proj_dir" config commit.gpgsign false
    printf '# %s\n' "$project" >"$proj_dir/README.md"
    git -C "$proj_dir" add README.md
    git -C "$proj_dir" commit -qm "initial"
done

WEB_APP_DIR="$WORKDIR/projects/web-app"
API_DIR="$WORKDIR/projects/api"

cat >"$CONFIG_DIR/config.toml" <<EOF
[accounts.demo-seat]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = "$CODEX_HOME"
pool = "codex:demo"

[projects.web-app]
path = "$WEB_APP_DIR"

[projects.web-app.chains]
scout = ["demo-seat"]
builder = ["demo-seat"]

[projects.api]
path = "$API_DIR"

[projects.api.chains]
scout = ["demo-seat"]
builder = ["demo-seat"]

[roles.scout]
write_scope = "none"
effort = "low"
on_exhausted = "queue"
skills = []

[roles.scout.models]
codex = "demo-model"

[roles.builder]
write_scope = "declared"
effort = "low"
on_exhausted = "queue"
skills = []

[roles.builder.models]
codex = "demo-model"
EOF

export HOME="$HOME_DIR"
export XDG_CONFIG_HOME="$HOME_DIR/.config"
export XDG_STATE_HOME="$STATE_DIR"
export MULTIVERSE_CONFIG="$CONFIG_DIR/config.toml"
export MULTIVERSE_STATE_DIR="$STATE_DIR"
export MULTIVERSE_ALLOW_TEMP_PROJECTS=1
export HERDR_PLUGIN_CONFIG_DIR="$CONFIG_DIR"
export HERDR_PLUGIN_STATE_DIR="$STATE_DIR"
export HERDR_BIN_PATH="$FAKE_HERDR_BIN"
export MV_FAKE_HERDR_STATE="$WORKDIR/fake-herdr.json"
export HERDR_PANE_ID="w52:p1"
export CODEX_HOME="$CODEX_HOME"
export GIT_CONFIG_NOSYSTEM=1
export GIT_CONFIG_GLOBAL="$HOME_DIR/.gitconfig"

echo "render-demo: seeding fixture backlog and a live dispatch..." >&2
(cd "$WEB_APP_DIR" && "$MULTIVERSE_BIN" backlog add "add dark mode toggle" \
    --why "most-requested issue this quarter") >&2
(cd "$WEB_APP_DIR" && "$MULTIVERSE_BIN" backlog add "cache the health check response" \
    --why "health endpoint is polled every few seconds") >&2
(cd "$API_DIR" && "$MULTIVERSE_BIN" backlog ask "which queue backend should workers use?" \
    --option "redis" --option "sqlite" --recommend 1) >&2
"$MULTIVERSE_BIN" dispatch "$API_DIR" "add a rate limiter to the health endpoint" \
    --role builder --scope README.md --yes >&2

TRANSCRIPT="$WORKDIR/transcript.txt"
{
    echo '$ multiverse board --no-reconcile --read-only'
    "$MULTIVERSE_BIN" board --all --no-reconcile --read-only
    echo
    echo '$ multiverse workers'
    "$MULTIVERSE_BIN" workers
    echo
    echo '$ multiverse tasks --no-reconcile --read-only'
    "$MULTIVERSE_BIN" tasks web-app api --no-reconcile --read-only
} >"$TRANSCRIPT"

mkdir -p "$(dirname "$OUT_SVG")"
python3 -I "$SCRIPT_DIR/text_to_svg.py" "$TRANSCRIPT" "$OUT_SVG" \
    --title "Multiverse board, workers, and tasks"

echo "render-demo: wrote $OUT_SVG" >&2

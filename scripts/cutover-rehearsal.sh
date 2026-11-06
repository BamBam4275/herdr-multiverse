#!/bin/sh
# Rehearses the read-only half of the real cutover: builds the release
# binaries, copies the real fleet config and every configured project's
# `.multiverse` directory (minus worktrees/) into a disposable sandbox under
# target/cutover/, migrates the copied config's role keys, then runs the
# operator's read-only command set against the copies.
#
# Real files are READ-ONLY throughout: ~/.config/multiverse (or whatever
# config-dir is given) and every project's real .multiverse are only ever
# copied from, never written to. HERDR_BIN_PATH always points at this repo's
# own fake-herdr -- never a real herdr binary or socket. This script never
# runs `dispatch --yes`, `start`, or `halt`.
#
# Usage:
#   scripts/cutover-rehearsal.sh [config-dir]
#
#   [config-dir]  the real fleet config+state directory to rehearse against.
#                 Defaults to $HOME/.config/multiverse. Only config.toml is
#                 read from it; nothing under it is ever modified.
#
# target/cutover/ (already covered by the repo-root /target/ gitignore
# entry) is wiped and rebuilt from the source config on every run, so its
# target/cutover/out/*.out captures always reflect the most recent run.
#
# Exit status: 0 if every assertion below passed, 1 if any failed. A command
# this script drives exiting non-zero is not itself a script failure -- an
# unmigrated config refusing to load is exactly what this rehearsal exists
# to catch -- but it is always reported as a FAIL line and reflected in the
# exit status.

set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd -P)

CONFIG_DIR=${1:-"$HOME/.config/multiverse"}

if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    echo "no config.toml under $CONFIG_DIR" >&2
    exit 2
fi

echo "building release binaries (multiverse, fake-herdr)..." >&2
( cd "$REPO_ROOT" && cargo build --release --locked -p mv-cli )
( cd "$REPO_ROOT" && cargo build --release --locked -p mv-herdr --bin fake-herdr )

RUST_BIN="$REPO_ROOT/target/release/multiverse"
FAKE_HERDR="$REPO_ROOT/target/release/fake-herdr"
WORK="$REPO_ROOT/target/cutover"

rm -rf "$WORK"
mkdir -p "$WORK/home/.config/multiverse" "$WORK/projects" "$WORK/out" "$WORK/map"

echo "sandbox: $WORK" >&2

# --- copy one project's .multiverse, minus worktree checkout contents ----
# Same shape as scripts/rehearsal/read_parity.sh's and cutover.sh's own
# `copy_project` (duplicated rather than shared: every rehearsal script in
# this repo is self-contained).
copy_project() {
    src_dir="$1"
    dest_root="$2"
    mkdir -p "$dest_root"
    if [ -d "$src_dir/.multiverse" ]; then
        mkdir -p "$dest_root/.multiverse"
        rsync -a --exclude='/worktrees' "$src_dir/.multiverse/" "$dest_root/.multiverse/"
        if [ -d "$src_dir/.multiverse/worktrees" ]; then
            mkdir -p "$dest_root/.multiverse/worktrees"
            for d in "$src_dir/.multiverse/worktrees"/*/; do
                [ -d "$d" ] || continue
                base=$(basename "$d")
                mkdir -p "$dest_root/.multiverse/worktrees/$base"
            done
        fi
    fi
    # `multiverse worktrees <path>` requires the path to be a real git
    # toplevel (mv-state::worktrees::Worktrees::new does `git rev-parse
    # --show-toplevel`); a fresh throwaway repo satisfies that without
    # copying any real git history.
    (
        cd "$dest_root"
        git init -q
        git config user.email rehearsal@local
        git config user.name rehearsal
        touch .rehearsal-placeholder
        git add .rehearsal-placeholder
        git commit -q -m "rehearsal placeholder"
    )
}

# key:abspath pairs, one per `[projects.<key>]` table's own `path = "..."`
# line, in file order.
list_projects() {
    awk '
        /^\[projects\.[^.]+\]$/ {
            key = $0
            sub(/^\[projects\./, "", key)
            sub(/\]$/, "", key)
            pending = key
            next
        }
        /^\[/ { pending = ""; next }
        pending != "" && /^path = "/ {
            value = $0
            sub(/^path = "/, "", value)
            sub(/".*$/, "", value)
            if (value != "" && index(value, "<<PLACEHOLDER") == 0) {
                print pending ":" value
            }
            pending = ""
        }
    ' "$CONFIG_DIR/config.toml"
}

PROJECT_PAIRS=$(list_projects)
if [ -z "$PROJECT_PAIRS" ]; then
    echo "no [projects.*] path entries found in $CONFIG_DIR/config.toml" >&2
    exit 2
fi

echo "copying project .multiverse dirs (read-only source, worktrees/ contents listed not copied)..." >&2
: >"$WORK/map/mapping.txt"
for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    src=${pair#*:}
    if [ ! -d "$src" ]; then
        echo "WARNING: $key's configured path does not exist, skipping: $src" >&2
        continue
    fi
    dest="$WORK/projects/$key"
    copy_project "$src" "$dest"
    echo "$src=$dest" >>"$WORK/map/mapping.txt"
done

mkdir -p "$WORK/projects/_unmapped/.multiverse"

echo "copying fleet config.toml (read-only source)..." >&2
cp "$CONFIG_DIR/config.toml" "$WORK/home/.config/multiverse/config.toml"

CFG="$WORK/home/.config/multiverse/config.toml"
STATE_DIR="$WORK/home/.config/multiverse"

python3 -I "$SCRIPT_DIR/rehearsal/rewrite_config.py" \
    "$CONFIG_DIR/config.toml" "$CFG" \
    --mapping "$WORK/map/mapping.txt" --unmapped-dir "$WORK/projects/_unmapped"

cp "$CFG" "$WORK/config-after-rewrite.toml"

PATH_ENV=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin

# Runs one multiverse invocation against the sandbox copies via
# MULTIVERSE_CONFIG/MULTIVERSE_STATE_DIR (never --config: this is the same
# env-driven path the plugin actions use). Prints the exit status; never
# lets a non-zero status trip `set -e`, since several of the commands below
# are expected to report failure data, not abort this script.
run_step() {
    label="$1"
    shift
    out="$WORK/out/$label.out"
    if env -i \
        PATH="$PATH_ENV" \
        MULTIVERSE_CONFIG="$CFG" \
        MULTIVERSE_STATE_DIR="$STATE_DIR" \
        HERDR_BIN_PATH="$FAKE_HERDR" \
        "$RUST_BIN" "$@" >"$out" 2>&1
    then
        echo 0
    else
        echo $?
    fi
}

FAIL=0
note() { printf '%s\n' "$1" >&2; }
ok() { printf 'PASS: %s\n' "$1" >&2; }
bad() { printf 'FAIL: %s\n' "$1" >&2; FAIL=1; }

note "== multiverse config migrate --dry-run =="
rc=$(run_step "migrate-dry-run" config migrate --dry-run)
cat "$WORK/out/migrate-dry-run.out"
if [ "$rc" = "0" ] && cmp -s "$WORK/config-after-rewrite.toml" "$CFG"; then
    ok "dry-run exited 0 and wrote nothing"
else
    bad "dry-run exited $rc or modified the config (see $WORK/out/migrate-dry-run.out)"
fi

note "== multiverse config migrate --backup =="
rc=$(run_step "migrate-backup" config migrate --backup)
cat "$WORK/out/migrate-backup.out"
if [ "$rc" = "0" ]; then
    ok "migrate --backup exited 0"
else
    bad "migrate --backup exited $rc (see $WORK/out/migrate-backup.out)"
fi

# Retired role spellings (mv_config::roles::Role::legacy_config_key),
# checked as a quoted value or a dotted table-path component, skipping
# comment lines -- `council` has no retired spelling, so it is not listed.
RETIRED='orchestrator|explorer|fixer|librarian|reviewer|oracle|designer|observer'
retired_hits=$(grep -vE '^[[:space:]]*#' "$CFG" | grep -nE "\"($RETIRED)\"|\\.($RETIRED)[]. ]" || true)
if [ -z "$retired_hits" ]; then
    ok "zero retired role names remain in the migrated config"
else
    bad "retired role name(s) remain after migrate:"
    printf '%s\n' "$retired_hits"
fi

note "== multiverse check =="
rc=$(run_step "check" check)
cat "$WORK/out/check.out"
if [ "$rc" = "0" ] || [ "$rc" = "3" ]; then
    ok "check exited $rc"
else
    bad "check exited $rc (see $WORK/out/check.out)"
fi

note "== multiverse accounts =="
rc=$(run_step "accounts" accounts)
cat "$WORK/out/accounts.out"
if [ "$rc" = "0" ]; then
    ok "accounts exited 0"
else
    bad "accounts exited $rc (see $WORK/out/accounts.out)"
fi

note "== multiverse tasks (every configured project, named explicitly) =="
# Every project name is passed explicitly rather than running bare `tasks`
# from this script's own cwd: a bare, no-project `tasks` falls back to
# `current_project_root()` (crates/mv-cli/src/commands/mod.rs), which walks
# cwd's real filesystem ancestors for *any* `.multiverse` directory --
# ignoring MULTIVERSE_CONFIG/MULTIVERSE_STATE_DIR entirely. Since this
# script's own cwd is normally nested inside a real project's own
# `.multiverse/worktrees/...` tree, that fallback finds the REAL enclosing
# project instead of the sandbox copy, and without --read-only also
# auto-reconciles (writes) it (crates/mv-cli/src/commands/tasks.rs:911,
# auto_reconcile_project) -- see this task's result.md for the finding.
# Naming every project explicitly resolves each root from the already-
# migrated, already-rewritten sandbox config instead, which is what this
# rehearsal is supposed to exercise.
TASK_PROJECT_NAMES=""
for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    [ -d "$WORK/projects/$key" ] || continue
    TASK_PROJECT_NAMES="$TASK_PROJECT_NAMES $key"
done
# shellcheck disable=SC2086 # intentional word-splitting: one argv entry per project name
rc=$(run_step "tasks" tasks $TASK_PROJECT_NAMES --read-only)
cat "$WORK/out/tasks.out"
if [ "$rc" = "0" ]; then
    ok "tasks exited 0"
else
    bad "tasks exited $rc (see $WORK/out/tasks.out)"
fi

note "== multiverse board --all =="
rc=$(run_step "board" board --all)
cat "$WORK/out/board.out"
if [ "$rc" = "0" ]; then
    ok "board --all exited 0"
else
    bad "board --all exited $rc (see $WORK/out/board.out)"
fi

note "== multiverse health =="
rc=$(run_step "health" health)
cat "$WORK/out/health.out"
if [ "$rc" = "0" ]; then
    ok "health exited 0"
else
    bad "health exited $rc (see $WORK/out/health.out)"
fi

note "== multiverse backlog list (fleet-wide) =="
rc=$(run_step "backlog-list" backlog list)
cat "$WORK/out/backlog-list.out"
if [ "$rc" = "0" ]; then
    ok "backlog list exited 0"
else
    bad "backlog list exited $rc (see $WORK/out/backlog-list.out)"
fi

note "== multiverse workers =="
rc=$(run_step "workers" workers)
cat "$WORK/out/workers.out"
if [ "$rc" = "0" ]; then
    ok "workers exited 0"
else
    bad "workers exited $rc (see $WORK/out/workers.out)"
fi

note "== multiverse sweep --dry-run =="
rc=$(run_step "sweep-dry-run" sweep --dry-run)
cat "$WORK/out/sweep-dry-run.out"
if [ "$rc" = "0" ]; then
    ok "sweep --dry-run exited 0"
else
    bad "sweep --dry-run exited $rc (see $WORK/out/sweep-dry-run.out)"
fi

note "== multiverse worktrees <path>, once per project (no --prune) =="
for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    [ -d "$WORK/projects/$key" ] || continue
    label="worktrees-$key"
    rc=$(run_step "$label" worktrees "$WORK/projects/$key")
    cat "$WORK/out/$label.out"
    if [ "$rc" = "0" ]; then
        ok "$key: worktrees exited 0"
    else
        bad "$key: worktrees exited $rc (see $WORK/out/$label.out)"
    fi
done

note ""
if [ "$FAIL" -eq 0 ]; then
    note "overall: PASS"
else
    note "overall: FAIL"
fi
note "full step output: $WORK/out/*.out (left in place for inspection; re-run to refresh)"
exit "$FAIL"

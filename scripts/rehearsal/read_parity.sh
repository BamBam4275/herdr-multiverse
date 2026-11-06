#!/bin/sh
# Reproduce the Python-vs-Rust read-command parity rehearsal for the
# fleet projects named in a given config file, entirely on disposable
# copies.
#
# Usage:
#   scripts/rehearsal/read_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]
#
#   <projects-root>      directory containing one subdirectory per project
#                        named under a `[projects.<key>]` table in
#                        <config-dir>/config.toml (e.g. demo-app,
#                        api-service) -- the project list is discovered from
#                        that file by discover_projects.py, not hardcoded
#                        here.
#   <config-dir>         the real fleet config+state directory
#                        (normally ~/.config/multiverse)
#   <python-repo>        checkout of the frozen Python reference
#                        (the multiverse package's parent directory)
#   [rust-binary]        defaults to target/release/multiverse under this
#                        script's own repo root
#   [fake-herdr-binary]  defaults to target/release/fake-herdr under this
#                        script's own repo root (build it first: `cargo
#                        build --release -p mv-herdr --locked`)
#
# HARD SAFETY RULE this script exists to uphold: it never writes to
# <config-dir>, never writes to any project's real .multiverse, and never
# points HERDR_BIN_PATH at a real herdr binary or socket. Every path it
# writes to lives under a single mktemp'd directory, removed on exit.
#
# Writes nothing outside that temp directory and its own stdout. Prints a
# PASS/DIFF table (via compare.py) and, for every DIFF, a unified diff of
# the two sides' normalized output.

set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd -P)

PROJECTS_ROOT=${1:?usage: read_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
CONFIG_DIR=${2:?usage: read_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
PYTHON_REPO=${3:?usage: read_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
RUST_BIN=${4:-"$REPO_ROOT/target/release/multiverse"}
FAKE_HERDR=${5:-"$REPO_ROOT/target/release/fake-herdr"}

if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    echo "no config.toml under $CONFIG_DIR" >&2
    exit 2
fi
if [ ! -x "$RUST_BIN" ]; then
    echo "rust binary not found or not executable: $RUST_BIN" >&2
    echo "(build it first: cargo build --release -p mv-cli --locked)" >&2
    exit 2
fi
if [ ! -x "$FAKE_HERDR" ]; then
    echo "fake-herdr binary not found or not executable: $FAKE_HERDR" >&2
    echo "(build it first: cargo build --release -p mv-herdr --locked)" >&2
    exit 2
fi
if [ ! -f "$PYTHON_REPO/multiverse/cli.py" ]; then
    echo "no multiverse/cli.py under $PYTHON_REPO" >&2
    exit 2
fi

# Resolved once against the ambient PATH: the curated PATH used inside
# env -i below (for env parity with the production CLI's own PATH
# handling) lists /usr/bin first, and macOS's /usr/bin/python3 is the
# CommandLineTools stub (3.9, no -P flag) rather than whatever `python3`
# actually means on this machine.
PYTHON3=$(command -v python3) || {
    echo "no python3 on PATH" >&2
    exit 2
}

WORK_RAW=$(mktemp -d "${TMPDIR:-/tmp}/read-parity.XXXXXX")
WORK=$(cd "$WORK_RAW" && pwd -P)
KEEP=${READ_PARITY_KEEP:-0}
cleanup() {
    if [ "$KEEP" = "1" ]; then
        echo "READ_PARITY_KEEP=1: leaving sandbox at $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

echo "sandbox: $WORK" >&2

# configkey:dirname pairs, one per `[projects.<key>]` table in the given
# config file -- discovered rather than hardcoded, so this script never
# carries a fixed list of real project names. Order follows the file.
PROJECT_PAIRS=$(python3 -P "$SCRIPT_DIR/discover_projects.py" "$CONFIG_DIR/config.toml")

mkdir -p "$WORK/python/projects" "$WORK/rust/projects"
mkdir -p "$WORK/python/home/.config/multiverse" "$WORK/rust/home/.config/multiverse"
mkdir -p "$WORK/out" "$WORK/map"

# --- copy each project's .multiverse, minus worktree checkout contents ----
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
    # toplevel (mv-state::worktrees::Worktrees::new does
    # `git rev-parse --show-toplevel`); a fresh throwaway repo satisfies
    # that without copying any real git history.
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

echo "copying project .multiverse dirs (worktrees/ contents listed, not copied)..." >&2
for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    dir=${pair#*:}
    src="$PROJECTS_ROOT/$dir"
    if [ ! -d "$src" ]; then
        echo "WARNING: $src does not exist, skipping" >&2
        continue
    fi
    copy_project "$src" "$WORK/python/projects/$dir"
    copy_project "$src" "$WORK/rust/projects/$dir"
    echo "$src=$WORK/python/projects/$dir" >>"$WORK/map/python.txt"
    echo "$src=$WORK/rust/projects/$dir" >>"$WORK/map/rust.txt"
done

# Safety net for any `[projects.*]` path the mapping above does not name
# (e.g. herdr-multiverse) -- redirected to an empty placeholder rather than
# left pointing at a real checkout. See rewrite_config.py's docstring.
mkdir -p "$WORK/python/projects/_unmapped/.multiverse"
mkdir -p "$WORK/rust/projects/_unmapped/.multiverse"

echo "copying fleet config+state dir..." >&2
rsync -a --exclude='config.toml.bak-*' "$CONFIG_DIR/" "$WORK/python/home/.config/multiverse/"
rsync -a --exclude='config.toml.bak-*' "$CONFIG_DIR/" "$WORK/rust/home/.config/multiverse/"

python3 -P "$SCRIPT_DIR/rewrite_config.py" \
    "$CONFIG_DIR/config.toml" "$WORK/python/home/.config/multiverse/config.toml" \
    --mapping "$WORK/map/python.txt" --unmapped-dir "$WORK/python/projects/_unmapped"

python3 -P "$SCRIPT_DIR/rewrite_config.py" \
    "$CONFIG_DIR/config.toml" "$WORK/rust/home/.config/multiverse/config.toml" \
    --mapping "$WORK/map/rust.txt" --unmapped-dir "$WORK/rust/projects/_unmapped"

echo "migrating the Rust-side config copy (old->new role keys)..." >&2
BASE_ENV="PATH=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin HOME=$WORK/rust/home"
env -i $BASE_ENV "$RUST_BIN" config migrate \
    --config "$WORK/rust/home/.config/multiverse/config.toml" --backup \
    >"$WORK/out/migrate.stdout" 2>"$WORK/out/migrate.stderr" || {
    echo "config migrate failed -- see $WORK/out/migrate.std{out,err}" >&2
    [ "$KEEP" = "1" ] || cat "$WORK/out/migrate.stderr" >&2
}

# KNOWN GAP WORKAROUND, not production behavior: `multiverse config
# migrate` (mv_config::migrate::migrate_role_keys) only rewrites role
# names that appear as TOML table/chain *keys* ([roles.<name>],
# `<name> = [...]` under a `*.chains` table). It does not touch a role
# name appearing as a VALUE inside an array, which is exactly what
# `accounts.<name>.roles_allowed = ["explorer", ...]` is -- confirmed by
# reading crates/mv-config/src/migrate.rs's `migrate_role_keys` (it only
# calls `rename_role_keys_in` on the `roles`/`chains` tables) against the
# real fleet config, which has `roles_allowed = ["explorer", "librarian"]`
# under `[accounts.local]`. Left unpatched, every later Rust invocation in
# this rehearsal fails with "unknown role \"explorer\"" before it reads a
# single project -- see this task's result.md for the finding this is
# standing in for. Patched here, by hand, only so the rest of the read
# matrix below can still be exercised against *this* copy.
sed -i '' '/^roles_allowed = /{
s/"explorer"/"scout"/g
s/"librarian"/"archivist"/g
s/"fixer"/"builder"/g
s/"reviewer"/"critic"/g
s/"oracle"/"seer"/g
s/"designer"/"artisan"/g
s/"observer"/"watcher"/g
s/"orchestrator"/"nexus"/g
}' "$WORK/rust/home/.config/multiverse/config.toml"

# A real, stateful fake-herdr binary, not a nonexistent-path stand-in: Python's
# `accounts`/`review-policy`/`tasks` (cmd_accounts/cmd_review_policy/cmd_tasks,
# via accounts.py's default_open_agent_counts) call `herdr agent list`
# unconditionally to compute live open-seat counts, and raise DispatchError --
# crashing the whole command before it prints anything -- when that binary
# cannot be run at all. A nonexistent path therefore does not measure "does
# this read command behave the same on both sides"; it measures "does Python
# crash when Herdr is unreachable", which it always does, on every single one
# of these commands, independent of anything this rehearsal is supposed to be
# comparing (confirmed by hand: every one of accounts/review-policy/tasks-all/
# tasks-reconcile-* showed as DIFF with an empty Python side and a Traceback
# in its .err file -- see this task's result.md). Pointing both sides at the
# same fake-herdr (MV_FAKE_HERDR_STATE left unset, so only its built-in
# read-only capture answers queries, never a mutation) answers the query with
# real, well-formed, deterministic data instead -- the same reasoning
# write_parity.sh's own copy of this comment gives for doing it there.
export HERDR_BIN_PATH="$FAKE_HERDR"

# Many of these commands exit non-zero on purpose (presence's "unknown" is
# exit 2, etc.) -- that is data, not a script failure, so the exit status
# is captured through an `if` condition. POSIX (and this host's /bin/sh,
# running in its stricter mode) propagates a failing command substitution's
# status into `set -e` even for a plain assignment; only the condition of
# an if/while/until is exempted from that.
run_py() {
    label="$1"
    cwd="$2"
    shift 2
    mkdir -p "$WORK/out/python"
    if (
        cd "$cwd"
        env -i \
            PATH=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin \
            HOME="$WORK/python/home" \
            MULTIVERSE_CONFIG="$WORK/python/home/.config/multiverse/config.toml" \
            MULTIVERSE_STATE_DIR="$WORK/python/home/.config/multiverse" \
            MULTIVERSE_ALLOW_TEMP_PROJECTS=1 \
            HERDR_BIN_PATH="$FAKE_HERDR" \
            PYTHONPATH="$PYTHON_REPO" \
            PYTHONDONTWRITEBYTECODE=1 \
            "$PYTHON3" -P -m multiverse.cli "$@"
    ) >"$WORK/out/python/$label.out" 2>"$WORK/out/python/$label.err"; then
        echo 0
    else
        echo $?
    fi
}

run_rs() {
    label="$1"
    cwd="$2"
    shift 2
    mkdir -p "$WORK/out/rust"
    if (
        cd "$cwd"
        env -i \
            PATH=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin \
            HOME="$WORK/rust/home" \
            MULTIVERSE_CONFIG="$WORK/rust/home/.config/multiverse/config.toml" \
            MULTIVERSE_STATE_DIR="$WORK/rust/home/.config/multiverse" \
            MULTIVERSE_ALLOW_TEMP_PROJECTS=1 \
            HERDR_BIN_PATH="$FAKE_HERDR" \
            "$RUST_BIN" "$@"
    ) >"$WORK/out/rust/$label.out" 2>"$WORK/out/rust/$label.err"; then
        echo 0
    else
        echo $?
    fi
}

MANIFEST="$WORK/manifest.jsonl"
: >"$MANIFEST"

record() {
    label="$1"
    argv="$2"
    py_cwd="$3"
    rs_cwd="$4"
    shift 4
    py_exit=$(run_py "$label" "$py_cwd" "$@")
    rs_exit=$(run_rs "$label" "$rs_cwd" "$@")
    printf '{"label":%s,"argv":%s,"py_out":%s,"py_err":%s,"py_exit":%s,"rs_out":%s,"rs_err":%s,"rs_exit":%s}\n' \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$label")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$argv")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$WORK/out/python/$label.out")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$WORK/out/python/$label.err")" \
        "$py_exit" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$WORK/out/rust/$label.out")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$WORK/out/rust/$label.err")" \
        "$rs_exit" \
        >>"$MANIFEST"
}

echo "running the read-command matrix..." >&2

HUB_PY="$WORK/python/home"
HUB_RS="$WORK/rust/home"

# presence doesn't touch project state at all; run it once, from the hub.
record "presence-json" "presence --json" "$HUB_PY" "$HUB_RS" presence --json
record "accounts" "accounts" "$HUB_PY" "$HUB_RS" accounts
record "review-policy" "review-policy" "$HUB_PY" "$HUB_RS" review-policy
# Neither CLI has a literal `--all` flag on `tasks`; "tasks --all" is read
# here as "tasks with no project argument", which both implementations'
# own --help text documents as the fleet-wide listing (run from outside
# every configured project).
record "tasks-all" "tasks (no project, fleet-wide)" "$HUB_PY" "$HUB_RS" tasks

for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue

    record "tasks-reconcile-$dir" "tasks (default reconcile)" "$py_proj" "$rs_proj" tasks
    record "board-noreconcile-$dir" "board --no-reconcile" "$py_proj" "$rs_proj" board --no-reconcile
    record "backlog-list-$dir" "backlog list" "$py_proj" "$rs_proj" backlog list
    record "inbox-peek-$dir" "activity inbox --peek" "$py_proj" "$rs_proj" activity inbox --peek
    record "worktrees-list-$dir" "worktrees <path>" "$py_proj" "$rs_proj" worktrees .
    record "directives-show-$dir" "directives show" "$py_proj" "$rs_proj" directives show

    # One backlog item id, if this project's routing log recorded any.
    item_id=$(grep -ho 'item=[0-9a-f-]\{1,\}' "$PROJECTS_ROOT/$dir/.multiverse/routing.log" 2>/dev/null | head -1 | cut -d= -f2)
    if [ -n "${item_id:-}" ]; then
        record "backlog-show-$dir" "backlog show $item_id" "$py_proj" "$rs_proj" backlog show "$item_id"
    fi

    # Two recent task ids (one of them likely an old-role-name id, since
    # the live fleet has not migrated yet) for `result`.
    i=0
    for t in $(ls -t "$PROJECTS_ROOT/$dir/.multiverse/tasks" 2>/dev/null | head -2); do
        i=$((i + 1))
        record "result-$dir-$i" "result $t" "$py_proj" "$rs_proj" result "$t"
    done
done

echo "normalizing and diffing..." >&2

RP_PY_ROOTS="$WORK/python/home=<HOME>
$WORK=<SANDBOX_ROOT>"
RP_RS_ROOTS="$WORK/rust/home=<HOME>
$WORK=<SANDBOX_ROOT>"
for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    RP_PY_ROOTS="$RP_PY_ROOTS
$WORK/python/projects/$dir=<PROJECT:$dir>"
    RP_RS_ROOTS="$RP_RS_ROOTS
$WORK/rust/projects/$dir=<PROJECT:$dir>"
done
RP_PY_ROOTS="$RP_PY_ROOTS
$WORK/python/projects/_unmapped=<PROJECT:_unmapped>"
RP_RS_ROOTS="$RP_RS_ROOTS
$WORK/rust/projects/_unmapped=<PROJECT:_unmapped>"

export RP_PY_ROOTS RP_RS_ROOTS
python3 -P "$SCRIPT_DIR/compare.py" "$MANIFEST" | tee "$WORK/report.txt"

echo "full report: $WORK/report.txt (removed on exit unless READ_PARITY_KEEP=1)" >&2

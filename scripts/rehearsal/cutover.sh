#!/bin/sh
# Rehearses the exact command sequence the operator will run at cutover,
# against disposable mktemp copies of the real fleet config+state dir and
# every configured project's `.multiverse` directory -- never the real
# ~/.config/multiverse, never a real project checkout, and never a real
# Herdr (HERDR_BIN_PATH always points at this repo's own fake-herdr,
# wrapped here to record every call it receives).
#
# The sequence, run with no --config/state override so it exercises the
# same default-path resolution the operator's bare commands do (only
# $HOME is redirected, at a copy of the real config+state dir):
#
#   multiverse config migrate --dry-run
#   multiverse config migrate --backup
#   multiverse halt off
#   multiverse start <project>        (once per project named in the config)
#   multiverse sweep --tick
#   multiverse board
#
# Usage:
#   scripts/rehearsal/cutover.sh <config-dir> [rust-binary] [fake-herdr-binary]
#
#   <config-dir>          the real fleet config+state directory (normally
#                         ~/.config/multiverse). Read-only: every byte
#                         under it is only ever rsynced into the sandbox,
#                         never written back.
#   [rust-binary]         defaults to target/release/multiverse under this
#                         script's own repo root (build it first: `cargo
#                         build --release -p mv-cli --locked`)
#   [fake-herdr-binary]   defaults to target/release/fake-herdr under this
#                         script's own repo root (build it first: `cargo
#                         build --release -p mv-herdr --locked`)
#
# Every project copied comes from <config-dir>/config.toml's own
# `[projects.<key>]` `path = "..."` entries -- this script carries no
# hardcoded project name or count, so it rehearses however many projects
# the given config names.
#
# CUTOVER_KEEP=1 leaves the sandbox in place (path printed on exit)
# instead of deleting it.
#
# Exit status: 0 if every assertion below passed, 1 if any failed. A
# command this script drives exiting non-zero is not itself a script
# failure -- some steps (a refused migrate, a config that still fails to
# load) are exactly what this rehearsal exists to catch -- but it is
# always reported as a FAIL line and reflected in the exit status.

set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd -P)

CONFIG_DIR=${1:?usage: cutover.sh <config-dir> [rust-binary] [fake-herdr-binary]}
RUST_BIN=${2:-"$REPO_ROOT/target/release/multiverse"}
FAKE_HERDR=${3:-"$REPO_ROOT/target/release/fake-herdr"}

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

WORK_RAW=$(mktemp -d "${TMPDIR:-/tmp}/mv-cutover.XXXXXX")
WORK=$(cd "$WORK_RAW" && pwd -P)
KEEP=${CUTOVER_KEEP:-0}
# shellcheck disable=SC2329 # invoked only via `trap cleanup EXIT` below
cleanup() {
    if [ "$KEEP" = "1" ]; then
        echo "CUTOVER_KEEP=1: leaving sandbox at $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

echo "sandbox: $WORK" >&2

mkdir -p "$WORK/home/.config/multiverse" "$WORK/projects" "$WORK/out" "$WORK/map"

# --- copy one project's .multiverse, minus worktree checkout contents ----
# Same shape as read_parity.sh/write_parity.sh's own `copy_project`
# (duplicated rather than shared: every rehearsal script in this
# directory is self-contained, matching simulate-plugin-install.sh and
# verify_config_migration.sh).
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
    # `sweep`/`board` reconciliation can reach `multiverse worktrees
    # <path>`, which requires the path to be a real git toplevel
    # (mv-state::worktrees::Worktrees::new does `git rev-parse
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
# line, in file order -- the same narrow, line-oriented contract
# discover_projects.py documents, reimplemented here (rather than shelled
# out to) because this script needs the absolute path to find the real
# project to copy, not discover_projects.py's own dirname-only output.
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

echo "copying fleet config+state dir (includes the real halt marker, if present)..." >&2
rsync -a --exclude='config.toml.bak-*' "$CONFIG_DIR/" "$WORK/home/.config/multiverse/"

python3 -P "$SCRIPT_DIR/rewrite_config.py" \
    "$CONFIG_DIR/config.toml" "$WORK/home/.config/multiverse/config.toml" \
    --mapping "$WORK/map/mapping.txt" --unmapped-dir "$WORK/projects/_unmapped"

CFG="$WORK/home/.config/multiverse/config.toml"
PRISTINE="$WORK/config-after-copy.toml"
cp "$CFG" "$PRISTINE"

# A logging fake-herdr: every call the rehearsal's Herdr runner makes is
# appended to one shared transcript, tagged with whichever project the
# caller names via $CUTOVER_PROJECT (unset = fleet-level, i.e.
# sweep/board), before being forwarded unchanged to the repo's own
# fake-herdr. This is the "Herdr call transcript" the start step below
# asserts against, per project.
TRANSCRIPT="$WORK/herdr-call-transcript.tsv"
: >"$TRANSCRIPT"
WRAPPER="$WORK/fake-herdr-logging"
cat >"$WRAPPER" <<EOF
#!/bin/sh
printf '%s\t%s\n' "\${CUTOVER_PROJECT:-<fleet>}" "\$*" >> "$TRANSCRIPT"
exec "$FAKE_HERDR" "\$@"
EOF
chmod +x "$WRAPPER"

# One shared, disposable state file for the whole rehearsal: every call
# below runs sequentially against it, which is exactly the fixture
# README's documented contract (fixtures/herdr/README.md), and is also
# what lets sweep/board (run last) see every project's agent that start
# opened earlier in the same run.
STATE="$WORK/fake-herdr-state.json"

PATH_ENV=/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin

# Runs one multiverse invocation with no --config/--state override (the
# operator's own bare commands), only $HOME redirected at the sandbox
# copy. Prints the exit status; never lets a non-zero status trip `set
# -e`, since several of the sequence's own steps are expected to report
# failure data, not abort this script.
run_step() {
    label="$1"
    project="$2"
    shift 2
    out="$WORK/out/$label.out"
    if env -i \
        PATH="$PATH_ENV" \
        HOME="$WORK/home" \
        MULTIVERSE_ALLOW_TEMP_PROJECTS=1 \
        HERDR_BIN_PATH="$WRAPPER" \
        MV_FAKE_HERDR_STATE="$STATE" \
        CUTOVER_PROJECT="$project" \
        "$RUST_BIN" "$@" >"$out" 2>&1
    then
        echo 0
    else
        echo $?
    fi
}

FAIL=0
note() { printf '%s\n' "$1"; }
ok() { printf 'PASS: %s\n' "$1"; }
bad() { printf 'FAIL: %s\n' "$1"; FAIL=1; }

note "== step 1: multiverse config migrate --dry-run =="
rc=$(run_step "migrate-dry-run" "" config migrate --dry-run)
cat "$WORK/out/migrate-dry-run.out"
if [ "$rc" = "0" ] && cmp -s "$PRISTINE" "$CFG"; then
    ok "dry-run exited 0 and wrote nothing"
else
    bad "dry-run exited $rc or modified the config (see $WORK/out/migrate-dry-run.out)"
fi

note "== step 2: multiverse config migrate --backup =="
rc=$(run_step "migrate" "" config migrate --backup)
cat "$WORK/out/migrate.out"
if [ "$rc" != "0" ]; then
    bad "migrate exited $rc (see $WORK/out/migrate.out)"
else
    ok "migrate exited 0"
fi

# Retired role spellings (mv_config::roles::Role::legacy_config_key),
# checked as a quoted value or a dotted table-path component, skipping
# comment lines -- `council` has no retired spelling (it kept its name),
# so it is not in this list.
RETIRED='orchestrator|explorer|fixer|librarian|reviewer|oracle|designer|observer'
retired_hits=$(grep -vE '^[[:space:]]*#' "$CFG" | grep -nE "\"($RETIRED)\"|\\.($RETIRED)[]. ]" || true)
if [ -z "$retired_hits" ]; then
    ok "zero retired role names remain in the migrated config"
else
    bad "retired role name(s) remain after migrate:"
    printf '%s\n' "$retired_hits"
fi

rc=$(run_step "check-after-migrate" "" check --read-only --no-reconcile)
cat "$WORK/out/check-after-migrate.out"
if [ "$rc" = "3" ] || grep -q "retired role key" "$WORK/out/check-after-migrate.out"; then
    bad "migrated config does not load cleanly (check exited $rc)"
else
    ok "migrated config loads (check exited $rc)"
fi

note "== step 3: multiverse halt off =="
rc=$(run_step "halt-off" "" halt off)
cat "$WORK/out/halt-off.out"
if [ "$rc" != "0" ]; then
    bad "halt off exited $rc"
else
    ok "halt off exited 0"
fi
if [ -e "$WORK/home/.config/multiverse/halt" ] || [ -e "$WORK/home/.config/multiverse/halt.state" ]; then
    bad "copied halt marker still present after halt off"
else
    ok "copied halt marker cleared"
fi

note "== step 4: multiverse start <project>, once per configured project =="
for pair in $PROJECT_PAIRS; do
    key=${pair%%:*}
    [ -d "$WORK/projects/$key" ] || continue
    label="start-$key"
    rc=$(run_step "$label" "$key" start "$key")
    calls=$(awk -F'\t' -v k="$key" '$1 == k { c++ } END { print c + 0 }' "$TRANSCRIPT")
    handoff=$(grep 'handoff:' "$WORK/out/$label.out" | tail -1 | sed 's/^ *//')
    note "-- $key (exit $rc, $calls Herdr call(s)) --"
    cat "$WORK/out/$label.out"
    if [ "$rc" != "0" ]; then
        bad "$key: start exited $rc"
    fi
    if [ "$calls" -gt 0 ]; then
        ok "$key: $calls Herdr call(s) recorded in the transcript"
    else
        bad "$key: no Herdr calls recorded in the transcript"
    fi
    if [ -n "$handoff" ]; then
        ok "$key: $handoff"
    else
        bad "$key: start printed no handoff line"
    fi
done

note "== step 5: multiverse sweep --tick =="
rc=$(run_step "sweep-tick" "" sweep --tick)
cat "$WORK/out/sweep-tick.out"
if [ "$rc" != "0" ]; then
    bad "sweep --tick exited $rc"
else
    ok "sweep --tick exited 0"
fi

note "== step 6: multiverse board =="
rc=$(run_step "board" "" board)
cat "$WORK/out/board.out"
if [ "$rc" != "0" ]; then
    bad "board exited $rc"
else
    ok "board exited 0"
fi

note ""
if [ "$FAIL" -eq 0 ]; then
    note "overall: PASS"
else
    note "overall: FAIL"
fi
note "full step output: $WORK/out/*.out, Herdr call transcript: $TRANSCRIPT (removed on exit unless CUTOVER_KEEP=1)"
exit "$FAIL"

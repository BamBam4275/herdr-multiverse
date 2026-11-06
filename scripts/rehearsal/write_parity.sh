#!/bin/sh
# Extend read_parity.sh's rehearsal to the commands that WRITE or decide:
# `dispatch --dry-run`, `sweep --dry-run`, `worktrees --prune --dry-run`,
# `tasks --reconcile`, `halt on/off`, and `backlog add/annotate/done` --
# against disposable copies of the fleet projects named in a given config
# file and the fleet config, with HERDR_BIN_PATH pointed at this repo's own
# fake-herdr (mv-herdr's `fake-herdr` binary) so nothing here can ever reach
# a real Herdr socket.
#
# Usage:
#   scripts/rehearsal/write_parity.sh <projects-root> <config-dir> <python-repo> \
#       [rust-binary] [fake-herdr-binary]
#
#   <projects-root>      directory containing one subdirectory per project
#                        named under a `[projects.<key>]` table in
#                        <config-dir>/config.toml (e.g. demo-app,
#                        api-service) -- the project list is discovered
#                        from that file by discover_projects.py, not
#                        hardcoded here.
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
# HARD SAFETY RULE this script exists to uphold: every path it reads from
# outside this repo (<config-dir>, each project's `.multiverse`) is opened
# read-only and only ever copied; every path it writes to lives under a
# single mktemp'd directory, removed on exit; HERDR_BIN_PATH never points
# at anything but the fake-herdr binary named above, so no command run here
# can ever reach a real Herdr socket or a real pane/agent.
#
# What this does NOT prove: `--yes` dispatch, a real `sweep --tick`, or
# `worktrees --prune` against a lane with real uncommitted/unmerged commits
# (every project copy here is a fresh, single-commit placeholder repo, so
# `--prune`'s interesting branches never exist in this sandbox -- see this
# task's result.md for what a deeper worktree fixture would need).
#
# Prints a PASS/DIFF table for every command's stdout/stderr (compare.py,
# the same tool read_parity.sh uses) AND a second PASS/DIFF table for the
# resulting on-disk state (diff_trees.py) -- a write command can match on
# one axis and diverge on the other, and this is the only way to catch a
# silent file-format difference that both sides' stdout agrees to describe
# the same way.

set -eu

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_ROOT=$(cd "$SCRIPT_DIR/../.." && pwd -P)

PROJECTS_ROOT=${1:?usage: write_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
CONFIG_DIR=${2:?usage: write_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
PYTHON_REPO=${3:?usage: write_parity.sh <projects-root> <config-dir> <python-repo> [rust-binary] [fake-herdr-binary]}
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

# Resolved once against the ambient PATH, same reasoning as read_parity.sh:
# the curated PATH used inside `env -i` below lists /usr/bin first, and
# macOS's /usr/bin/python3 is the CommandLineTools stub (3.9, no -P flag).
PYTHON3=$(command -v python3) || {
    echo "no python3 on PATH" >&2
    exit 2
}

WORK_RAW=$(mktemp -d "${TMPDIR:-/tmp}/write-parity.XXXXXX")
WORK=$(cd "$WORK_RAW" && pwd -P)
KEEP=${WRITE_PARITY_KEEP:-0}
cleanup() {
    if [ "$KEEP" = "1" ]; then
        echo "WRITE_PARITY_KEEP=1: leaving sandbox at $WORK" >&2
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

echo "sandbox: $WORK" >&2

# configkey:dirname pairs, one per `[projects.<key>]` table in the given
# config file -- discovered rather than hardcoded, same as read_parity.sh,
# so this script never carries a fixed list of real project names. Order
# follows the file.
PROJECT_PAIRS=$(python3 -P "$SCRIPT_DIR/discover_projects.py" "$CONFIG_DIR/config.toml")

mkdir -p "$WORK/python/projects" "$WORK/rust/projects"
mkdir -p "$WORK/python/home/.config/multiverse" "$WORK/rust/home/.config/multiverse"
mkdir -p "$WORK/out" "$WORK/map"

# --- copy each project's .multiverse, minus worktree checkout contents ----
# Identical to read_parity.sh's own `copy_project` (duplicated rather than
# sourced: this script owns no part of read_parity.sh, and every other
# rehearsal script in this directory -- simulate-plugin-install.sh,
# verify_config_migration.sh -- is likewise self-contained rather than
# sharing a library).
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

# KNOWN GAP WORKAROUND, not production behavior -- same patch
# read_parity.sh carries, for the same reason: `multiverse config migrate`
# (mv_config::migrate::migrate_role_keys) does not rewrite a role name
# appearing as a VALUE inside an array (`accounts.<name>.roles_allowed`),
# only as a table/chain key. Left unpatched, every Rust invocation below
# that reads a seat's `roles_allowed` fails with "unknown role" before it
# does anything else. See read_parity.sh's own copy of this comment, and
# this task's result.md, for the finding this stands in for.
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

# A real, stateful fake-herdr binary (not read_parity.sh's nonexistent-path
# trick): every command this script drives either never calls Herdr at all,
# or only ever issues a read-only query (`agent list`, to decide whether a
# pane is still live) -- see this task's result.md for the per-command
# trace that established this. Giving both sides the *same* fake binary,
# with MV_FAKE_HERDR_STATE left unset, means: (a) a read-only query gets a
# real, well-formed answer instead of a silent empty list, and (b) if
# anything here ever attempts a mutating call this script's design did not
# anticipate, fake-herdr's own `run()` refuses it outright ("set
# MV_FAKE_HERDR_STATE to a disposable file before mutating the fake")
# rather than silently no-op'ing -- a loud failure in the manifest, not a
# quiet gap in the rehearsal's coverage.
export HERDR_BIN_PATH="$FAKE_HERDR"

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

# Split out of read_parity.sh's `record` so id-dependent follow-ups
# (`backlog annotate`/`done`, which need the id each side's own `add`
# minted) can run each side with different argv while still landing in the
# one shared manifest `compare.py` reads.
emit_manifest() {
    label="$1"
    argv="$2"
    py_out="$3"
    py_err="$4"
    py_exit="$5"
    rs_out="$6"
    rs_err="$7"
    rs_exit="$8"
    printf '{"label":%s,"argv":%s,"py_out":%s,"py_err":%s,"py_exit":%s,"rs_out":%s,"rs_err":%s,"rs_exit":%s}\n' \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$label")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$argv")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$py_out")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$py_err")" \
        "$py_exit" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$rs_out")" \
        "$(python3 -P -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$rs_err")" \
        "$rs_exit" \
        >>"$MANIFEST"
}

# Many of these commands exit non-zero on purpose -- that is data, not a
# script failure, so the exit status is captured through an `if` condition
# (see read_parity.sh's own comment on this for the POSIX `set -e` reason).
record() {
    label="$1"
    argv="$2"
    py_cwd="$3"
    rs_cwd="$4"
    shift 4
    py_exit=$(run_py "$label" "$py_cwd" "$@")
    rs_exit=$(run_rs "$label" "$rs_cwd" "$@")
    emit_manifest "$label" "$argv" \
        "$WORK/out/python/$label.out" "$WORK/out/python/$label.err" "$py_exit" \
        "$WORK/out/rust/$label.out" "$WORK/out/rust/$label.err" "$rs_exit"
}

echo "running the write/decide command matrix..." >&2

HUB_PY="$WORK/python/home"
HUB_RS="$WORK/rust/home"

# --- dispatch --dry-run, every project x every role -----------------------
# Paired legacy/current role names (Role::legacy_config_key <-> config_key,
# crates/mv-config/src/roles.rs): the frozen Python reference only knows the
# legacy spelling (`multiverse/roles.py`'s CONFIG_KEYS has no new names at
# all, by design -- it predates the rename), so giving it a new name would
# just measure "does Python recognize vocabulary it was frozen before" on
# every single call, not a real behavioral difference. Each side gets its
# own era-appropriate spelling for what the config's `[projects.*.chains]`
# calls the same role; `compare.py`'s own docstring already documents this
# project's convention of printing the resulting (expected) role-name text
# diff rather than silently translating it away -- annotated in this task's
# result.md, not re-decided here.
ROLE_PAIRS="
scout:explorer
builder:fixer
archivist:librarian
critic:reviewer
seer:oracle
council:council
artisan:designer
watcher:observer
"

for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue

    for rp in $ROLE_PAIRS; do
        rs_role=${rp%%:*}
        py_role=${rp#*:}
        label="dispatch-$dir-$rs_role"
        py_exit=$(run_py "$label" "$py_proj" dispatch . "write_parity.sh rehearsal probe" --dry-run --role "$py_role")
        rs_exit=$(run_rs "$label" "$rs_proj" dispatch . "write_parity.sh rehearsal probe" --dry-run --role "$rs_role")
        emit_manifest "$label" "dispatch . 'probe' --dry-run --role ($py_role/$rs_role)" \
            "$WORK/out/python/$label.out" "$WORK/out/python/$label.err" "$py_exit" \
            "$WORK/out/rust/$label.out" "$WORK/out/rust/$label.err" "$rs_exit"
    done
done

# --- sweep --dry-run (whole fleet, from the hub) ---------------------------
record "sweep-dry-run" "sweep --dry-run" "$HUB_PY" "$HUB_RS" sweep --dry-run

# --- worktrees <project> --prune --dry-run, every project ------------------
for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue
    record "worktrees-prune-$dir" "worktrees . --prune --dry-run" "$py_proj" "$rs_proj" worktrees . --prune --dry-run
done

# --- tasks --reconcile, every project ---------------------------------------
for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue
    record "tasks-reconcile-$dir" "tasks --reconcile" "$py_proj" "$rs_proj" tasks --reconcile
done

# --- backlog add -> annotate -> done, every project -------------------------
# `add`/`annotate` need cwd to equal the project root exactly (see
# `current_project` in both CLIs); `done` is more lenient (`current_project_root`)
# but is run from the same cwd for consistency. Each side mints its own id
# independently, so it is captured from that side's own `add` output and fed
# only to that side's own follow-ups -- there is no reason the two ids
# would (or need to) match.
for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue

    label="backlog-add-$dir"
    py_exit=$(run_py "$label" "$py_proj" backlog add "write_parity.sh rehearsal probe ($dir)" --why "write_parity.sh backlog write-command probe" --role scout)
    rs_exit=$(run_rs "$label" "$rs_proj" backlog add "write_parity.sh rehearsal probe ($dir)" --why "write_parity.sh backlog write-command probe" --role scout)
    emit_manifest "$label" "backlog add 'probe' --why '...' --role scout" \
        "$WORK/out/python/$label.out" "$WORK/out/python/$label.err" "$py_exit" \
        "$WORK/out/rust/$label.out" "$WORK/out/rust/$label.err" "$rs_exit"

    py_id=$(grep -o '[0-9]\{8\}-[a-z][a-z0-9]*-[0-9a-f]\{6\}' "$WORK/out/python/$label.out" | head -1) || true
    rs_id=$(grep -o '[0-9]\{8\}-[a-z][a-z0-9]*-[0-9a-f]\{6\}' "$WORK/out/rust/$label.out" | head -1) || true

    if [ -n "${py_id:-}" ] && [ -n "${rs_id:-}" ]; then
        label="backlog-annotate-$dir"
        py_exit=$(run_py "$label" "$py_proj" backlog annotate "$py_id" --note "write_parity.sh probe annotation")
        rs_exit=$(run_rs "$label" "$rs_proj" backlog annotate "$rs_id" --note "write_parity.sh probe annotation")
        emit_manifest "$label" "backlog annotate <id> --note '...'" \
            "$WORK/out/python/$label.out" "$WORK/out/python/$label.err" "$py_exit" \
            "$WORK/out/rust/$label.out" "$WORK/out/rust/$label.err" "$rs_exit"

        label="backlog-done-$dir"
        py_exit=$(run_py "$label" "$py_proj" backlog done "$py_id" --evidence "write_parity.sh probe complete")
        rs_exit=$(run_rs "$label" "$rs_proj" backlog done "$rs_id" --evidence "write_parity.sh probe complete")
        emit_manifest "$label" "backlog done <id> --evidence '...'" \
            "$WORK/out/python/$label.out" "$WORK/out/python/$label.err" "$py_exit" \
            "$WORK/out/rust/$label.out" "$WORK/out/rust/$label.err" "$rs_exit"
    else
        echo "WARNING: could not capture a backlog id from one or both sides for $dir (py=${py_id:-<none>} rs=${rs_id:-<none>}); skipping annotate/done probes for it -- see $WORK/out/{python,rust}/backlog-add-$dir.out" >&2
    fi
done

# --- halt on / status / off, on the shared copy state dir -------------------
# Run last, deliberately: both CLIs exempt a `--dry-run` dispatch from the
# halt check (`cli.py`'s `cmd_dispatch`: "not args.dry_run"), and Rust's
# dispatch does not consult the halt file at all yet (see result.md), so
# halting first vs last cannot change any dispatch probe above either way
# -- keeping it last just keeps this block's own on/off/on/off history from
# interleaving with anything else in the manifest's reading order.
record "halt-status-before" "halt status" "$HUB_PY" "$HUB_RS" halt status
record "halt-on" "halt on --reason 'write_parity.sh rehearsal'" "$HUB_PY" "$HUB_RS" halt on --reason "write_parity.sh rehearsal"
record "halt-status-while-on" "halt status" "$HUB_PY" "$HUB_RS" halt status
record "halt-off" "halt off" "$HUB_PY" "$HUB_RS" halt off
record "halt-status-after-off" "halt status" "$HUB_PY" "$HUB_RS" halt status

echo "normalizing and diffing stdout/stderr..." >&2

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
python3 -P "$SCRIPT_DIR/compare.py" "$MANIFEST" | tee "$WORK/report-stdout.txt"

echo "diffing on-disk state left behind in each project copy..." >&2
for pair in $PROJECT_PAIRS; do
    dir=${pair#*:}
    py_proj="$WORK/python/projects/$dir"
    rs_proj="$WORK/rust/projects/$dir"
    [ -d "$py_proj" ] || continue
    echo "--- $dir ---"
    # `routing.log.index`/`.fp` are a derived byte-offset index over
    # routing.log, not independent state: once any appended line differs
    # in byte length between the two sides (expected -- see the role-pair
    # comment above), every later offset/hash record in the index differs
    # too, in a cascade that is pure arithmetic, not a second behavioral
    # difference worth re-reporting line by line.
    python3 -P "$SCRIPT_DIR/diff_trees.py" "$py_proj" "$rs_proj" \
        --exclude '.multiverse/routing.log.index'
done | tee "$WORK/report-project-files.txt"

echo "diffing on-disk state left behind in the shared fleet config+state dir..." >&2
# `config.toml`/`config.toml.bak-*` are excluded: they differ by this
# script's own deliberate setup (only the Rust side is migrated, and only
# `config migrate --backup` writes a backup file) -- not a command-driven
# difference, see the KNOWN GAP WORKAROUND comment above.
python3 -P "$SCRIPT_DIR/diff_trees.py" \
    "$WORK/python/home/.config/multiverse" "$WORK/rust/home/.config/multiverse" \
    --exclude 'config.toml' \
    | tee "$WORK/report-hub-files.txt"

echo "full reports: $WORK/report-{stdout,project-files,hub-files}.txt (removed on exit unless WRITE_PARITY_KEEP=1)" >&2

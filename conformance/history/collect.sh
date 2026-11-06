#!/bin/sh
# Snapshots real `.multiverse/` state (and the fleet config-state dir) into a
# local, gitignored corpus so `runner/` can replay it through the Rust
# readers as they land. Read-only against its sources: this script never
# writes into a project's `.multiverse/` or the fleet state dir, and it
# never shells out to `multiverse`/`herdr`. Nothing it produces is
# committed — see .gitignore in this directory.
#
# The corpus stores each project's state under a `multiverse-state/`
# directory, not `.multiverse/` — deliberately not a literal name match.
# This machine's `.git/info/exclude` carries an unanchored `.multiverse/`
# rule (any depth) that exists precisely to stop a task's own `.multiverse/`
# state from ever being committed; committed fixtures built to mirror the
# real layout would be silently swallowed by that same rule; see
# `fixtures/generate.py`'s header and this directory's README for the full
# explanation. `collect.sh` still *reads* from each real project's actual
# `.multiverse/`, unaffected — only the corpus's own directory name differs
# from the thing it's a snapshot of.
#
# Usage:
#   collect.sh [--state-dir DIR] [--out DIR] PROJECT_DIR [PROJECT_DIR ...]
#
# Each PROJECT_DIR must already contain a `.multiverse/` directory; this
# script never initializes one. --state-dir points at the fleet
# config-state directory (e.g. `~/.config/multiverse`, resolved however
# your own config does); it is optional — omit it to collect per-project
# state only. --out defaults to ./corpus (relative to this script).
#
# Only the file families this lane's runner actually reads are copied:
# routing.log (+ its index/fingerprint sidecars), the three activity
# cursor files, task directories (known files verbatim, anything else as a
# zero-byte placeholder so unknown-file tolerance is still exercised
# without copying arbitrary scratch content), the per-project `handoff`
# file, worktree directories (task-id names plus their nested
# `.multiverse/tasks/<id>/` only — never the git checkout itself, which can
# run into the gigabytes), and, from the state dir, the three stores named
# in this lane's brief: breaker-state.json, seat-state.json,
# gauge-state.json. Nothing else is read, copied, or even stat'd.
#
# POSIX sh only. No bashisms, no GNU-only flag reliance.
set -eu

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
OUT="$SCRIPT_DIR/corpus"
STATE_DIR=""

usage() {
    echo "usage: collect.sh [--state-dir DIR] [--out DIR] PROJECT_DIR [PROJECT_DIR ...]" >&2
    exit 2
}

while [ $# -gt 0 ]; do
    case "$1" in
        --state-dir)
            [ $# -ge 2 ] || usage
            STATE_DIR=$2
            shift 2
            ;;
        --out)
            [ $# -ge 2 ] || usage
            OUT=$2
            shift 2
            ;;
        --help|-h)
            usage
            ;;
        --)
            shift
            break
            ;;
        -*)
            echo "collect.sh: unknown option: $1" >&2
            usage
            ;;
        *)
            break
            ;;
    esac
done

[ $# -ge 1 ] || usage

mkdir -p "$OUT"

# Copies $1 (a file) to $2 (a destination path) only if $1 exists, creating
# the destination's parent directory first. Silent no-op when the source is
# absent — most of these files are legitimately optional.
#
# Variable names in every function here are prefixed per-function on
# purpose: POSIX sh functions share one global scope (no `local`), so a
# second function reusing a plain name like `dest` would silently clobber
# its caller's variable of the same name mid-sequence.
copy_file() {
    cf_src=$1
    cf_dest=$2
    if [ -f "$cf_src" ]; then
        mkdir -p "$(dirname -- "$cf_dest")"
        cp -p "$cf_src" "$cf_dest"
    fi
}

collect_project() {
    cp_project_dir=$1
    cp_name=$(basename -- "$cp_project_dir")
    cp_src="$cp_project_dir/.multiverse"
    cp_dest="$OUT/$cp_name/multiverse-state"

    if [ ! -d "$cp_src" ]; then
        echo "collect.sh: skipping $cp_name — no .multiverse directory at $cp_src" >&2
        return 0
    fi

    rm -rf "$cp_dest"
    mkdir -p "$cp_dest/tasks" "$cp_dest/worktrees"

    copy_file "$cp_src/routing.log" "$cp_dest/routing.log"
    copy_file "$cp_src/routing.log.index" "$cp_dest/routing.log.index"
    copy_file "$cp_src/routing.log.index.fp" "$cp_dest/routing.log.index.fp"
    copy_file "$cp_src/activity.cursor" "$cp_dest/activity.cursor"
    copy_file "$cp_src/activity.lastseen" "$cp_dest/activity.lastseen"
    copy_file "$cp_src/activity.lastack" "$cp_dest/activity.lastack"
    copy_file "$cp_src/handoff" "$cp_dest/handoff"

    # Globbing (not `find | while read`) deliberately: these trees run to
    # thousands of task directories on the live fleet (one large project alone
    # has 1,925), and a subshell-per-entry pipeline spends most of its
    # wall time on process spawn rather than I/O. A trailing `/` on the
    # glob matches directories only (POSIX shell behavior, verified
    # against dash); `${path##*/}` is parameter expansion, not a
    # `basename` subprocess.
    if [ -d "$cp_src/tasks" ]; then
        for cp_task_dir in "$cp_src/tasks"/*/; do
            [ -d "$cp_task_dir" ] || continue
            cp_task_dir=${cp_task_dir%/}
            collect_task_dir "$cp_task_dir" "$cp_dest/tasks/${cp_task_dir##*/}"
        done
    fi

    if [ -d "$cp_src/worktrees" ]; then
        for cp_wt_dir in "$cp_src/worktrees"/*/; do
            [ -d "$cp_wt_dir" ] || continue
            cp_wt_dir=${cp_wt_dir%/}
            cp_wt_name=${cp_wt_dir##*/}
            cp_nested="$cp_wt_dir/.multiverse/tasks"
            if [ -d "$cp_nested" ]; then
                for cp_nested_task_dir in "$cp_nested"/*/; do
                    [ -d "$cp_nested_task_dir" ] || continue
                    cp_nested_task_dir=${cp_nested_task_dir%/}
                    collect_task_dir "$cp_nested_task_dir" \
                        "$cp_dest/worktrees/$cp_wt_name/multiverse-state/tasks/${cp_nested_task_dir##*/}"
                done
            else
                mkdir -p "$cp_dest/worktrees/$cp_wt_name"
            fi
        done
    fi

    echo "collect.sh: collected $cp_name"
}

# Copies the sanctioned task-directory files verbatim and records every
# other filename as a zero-byte placeholder of the same name, so the
# runner can still exercise "ignore unknown files" tolerance without this
# corpus carrying arbitrary scratch-file bytes.
collect_task_dir() {
    ctd_task_src=$1
    ctd_task_dest=$2
    mkdir -p "$ctd_task_dest"
    for ctd_f in "$ctd_task_src"/*; do
        [ -f "$ctd_f" ] || continue
        ctd_fname=${ctd_f##*/}
        case "$ctd_fname" in
            brief.md|result.md|status|revise|retire|scratch-pane)
                cp -p "$ctd_f" "$ctd_task_dest/$ctd_fname"
                ;;
            *)
                : > "$ctd_task_dest/$ctd_fname"
                ;;
        esac
    done
}

collect_fleet_state() {
    [ -n "$STATE_DIR" ] || return 0
    if [ ! -d "$STATE_DIR" ]; then
        echo "collect.sh: --state-dir $STATE_DIR does not exist, skipping" >&2
        return 0
    fi
    cfs_dest="$OUT/fleet-state"
    mkdir -p "$cfs_dest"
    copy_file "$STATE_DIR/breaker-state.json" "$cfs_dest/breaker-state.json"
    copy_file "$STATE_DIR/seat-state.json" "$cfs_dest/seat-state.json"
    copy_file "$STATE_DIR/gauge-state.json" "$cfs_dest/gauge-state.json"
    echo "collect.sh: collected fleet-state"
}

for project_dir in "$@"; do
    collect_project "$project_dir"
done
collect_fleet_state

echo "collect.sh: done, corpus at $OUT"

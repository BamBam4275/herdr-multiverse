#!/bin/sh
# Pre-cutover rehearsal: prove the Rust `multiverse` binary drives REAL
# Herdr, not a stub. Every other script under scripts/rehearsal/ points
# HERDR_BIN_PATH at a fake binary on purpose; this one deliberately does
# NOT, because the one thing it exists to show is that a live dispatch
# reaches the real `herdr` on PATH and produces a real pane, for both a
# write-nothing role (Scout) and a role with its own git lane (Builder).
#
# Isolation, so a rehearsal run can never touch anything real:
#   - A fresh `mktemp -d` sandbox holds a brand-new git repo (one commit)
#     as the only registered project.
#   - $HOME and $CLAUDE_CONFIG_DIR are left exactly as the caller's shell
#     already has them — the `claude` seat this script dispatches needs
#     its real login, and there is no sandboxed substitute for that.
#   - `$MULTIVERSE_CONFIG` and `$MULTIVERSE_STATE_DIR` point into the
#     sandbox instead. `dispatch`'s own config loader never reads the
#     global `--config` flag (it calls `resolve_config_path_from_env(None)`
#     directly — see crates/mv-cli/src/commands/dispatch.rs's
#     `load_config`), so `--config` alone would silently fall through to
#     whatever config the caller's environment already resolves. This
#     script sets the env var (what dispatch actually reads) and still
#     passes `--config` too, so the command line matches what an operator
#     would naturally type.
#   - The config this script writes names exactly one project (the
#     sandbox repo) and one account (a `kind = "claude"` seat shaped like
#     config.example.toml's [accounts.claude-work], with
#     `command = "claude"`), on scout and builder chains that both name
#     that one seat.
#   - Never points at ~/.config/multiverse, ~/.config/herdr, or any real
#     project's .multiverse.
#   - The fleet halt marker lives under $MULTIVERSE_STATE_DIR
#     (mv-cli/src/commands/halt.rs's state_dir_from_config), which this
#     script points at a directory that has never existed before — so the
#     operator's real halt (if any) cannot reach this run, and --force is
#     never passed or needed. If dispatch refuses over a halt anyway, that
#     is a real finding, not something to retry with --force.
#
# POSIX sh only. Takes the binary as an argument; bakes in no real path.
#
# Usage:
#   scripts/rehearsal/live_smoke.sh --bin PATH [--keep]
#
#   --bin PATH   The `multiverse` binary to run (e.g.
#                target/release/multiverse from a `cargo build --release
#                -p mv-cli`).
#   --keep       Do not delete the working temp dir on exit; print its
#                path instead. Default is to always delete it. Does not
#                affect the real-Herdr cleanup steps below, which always
#                run when they apply.

set -eu

BIN=""
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --bin) BIN="$2"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$BIN" ]; then
    echo "usage: $0 --bin PATH [--keep]" >&2
    exit 2
fi
if [ ! -x "$BIN" ]; then
    echo "not an executable binary: $BIN" >&2
    exit 2
fi

note() { printf '%s\n' "$1"; }
ok() { printf 'PASS: %s\n' "$1"; }
bad() { printf 'FAIL: %s\n' "$1"; fail=1; }
fail=0

WORK=$(mktemp -d -t mv-live-smoke)
PANES_TO_CLOSE=""

cleanup() {
    for pane in $PANES_TO_CLOSE; do
        note "cleanup: closing pane $pane"
        herdr pane close "$pane" >/dev/null 2>&1 || note "cleanup: could not close pane $pane (report this)"
    done
    if [ "$KEEP" -eq 1 ]; then
        note "kept working directory: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

# --- sandbox project: a throwaway git repo, one commit -----------------
SANDBOX_REPO="$WORK/sandbox-repo"
mkdir -p "$SANDBOX_REPO"
git -C "$SANDBOX_REPO" init -q -b main
export GIT_AUTHOR_NAME=multiverse GIT_AUTHOR_EMAIL=multiverse@localhost
export GIT_COMMITTER_NAME=multiverse GIT_COMMITTER_EMAIL=multiverse@localhost
echo "Throwaway project for scripts/rehearsal/live_smoke.sh. Safe to delete." > "$SANDBOX_REPO/README.md"
git -C "$SANDBOX_REPO" add README.md
git -C "$SANDBOX_REPO" commit -q -m "Initial commit"

# --- isolated config and state; real $HOME/$CLAUDE_CONFIG_DIR ----------
STATE_DIR="$WORK/state"
mkdir -p "$STATE_DIR"

REAL_CLAUDE_CONFIG_DIR="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"

CONFIG="$WORK/config.toml"
cat > "$CONFIG" <<EOF
max_concurrent = 1

[accounts.claude-sandbox]
kind = "claude"
config_dir_var = "CLAUDE_CONFIG_DIR"
config_dir = "$REAL_CLAUDE_CONFIG_DIR"
pool = "claude:sandbox"
available = true
command = "claude"

[roles.scout]
write_scope = "none"
effort = "low"
on_exhausted = "degrade"
skills = []

[roles.scout.models]
claude = "sonnet"

[roles.builder]
write_scope = "declared"
effort = "low"
on_exhausted = "degrade"
skills = []

[roles.builder.models]
claude = "sonnet"

[projects.sandbox]
path = "$SANDBOX_REPO"

[projects.sandbox.chains]
scout = ["claude-sandbox"]
builder = ["claude-sandbox"]
EOF

export MULTIVERSE_CONFIG="$CONFIG"
export MULTIVERSE_STATE_DIR="$STATE_DIR"
# Deliberately NOT setting HERDR_BIN_PATH: see this script's header.
unset HERDR_BIN_PATH 2>/dev/null || true

# --- dispatch one role, confirm the pane, wait, check tasks/result -----
# Prints PASS/FAIL lines for this role and appends its pane (if any) to
# $PANES_TO_CLOSE. On a trust dialog, stops immediately without answering
# it and marks the run failed overall, per this rehearsal's brief.
dispatch_and_check() {
    role="$1"
    task="$2"
    shift 2
    # Remaining args, if any, are extra dispatch flags (e.g. --scope).

    note "== multiverse --config $CONFIG dispatch --yes --role $role $SANDBOX_REPO ... =="
    set +e
    OUTPUT=$("$BIN" --config "$CONFIG" dispatch --yes --role "$role" "$@" "$SANDBOX_REPO" "$task" 2>&1)
    STATUS=$?
    set -e
    printf '%s\n' "$OUTPUT"
    note "exit code: $STATUS"

    if [ "$STATUS" -ne 0 ]; then
        bad "$role dispatch refused (exit $STATUS) — no pane was created; nothing below this line could run for $role"
        return 1
    fi
    ok "$role dispatch exited 0"

    TRUST_PANE=$(printf '%s\n' "$OUTPUT" | sed -n 's/^Confirm the trust dialog on pane \(.*\) before.*$/\1/p')
    if [ -n "$TRUST_PANE" ]; then
        bad "$role seat required trust confirmation on pane $TRUST_PANE — per task brief, not answering it"
        PANES_TO_CLOSE="$PANES_TO_CLOSE $TRUST_PANE"
        return 1
    fi

    TASK_ID=$(printf '%s\n' "$OUTPUT" | sed -n 's/^Dispatched .* (\(.*\))\.$/\1/p')
    if [ -z "$TASK_ID" ]; then
        bad "could not find a task id in $role dispatch's output"
        return 1
    fi
    ok "$role task id: $TASK_ID"

    note "== herdr pane list (looking for $role task $TASK_ID) =="
    PANES=$(herdr pane list 2>&1) || { bad "herdr pane list failed"; printf '%s\n' "$PANES"; return 1; }
    printf '%s\n' "$PANES"

    PANE_ID=$(printf '%s\n' "$PANES" | grep -F "$TASK_ID" | sed -n 's/.*"pane_id":"\([^"]*\)".*/\1/p' | head -n1)
    if [ -z "$PANE_ID" ]; then
        bad "no pane in 'herdr pane list' mentions task id $TASK_ID"
    else
        ok "found pane $PANE_ID for $role task $TASK_ID"
        PANES_TO_CLOSE="$PANES_TO_CLOSE $PANE_ID"
    fi

    note "== multiverse tasks (from the sandbox project, $role task) =="
    (cd "$SANDBOX_REPO" && "$BIN" tasks --task "$TASK_ID") || bad "multiverse tasks failed for $role task $TASK_ID"

    note "== multiverse result $TASK_ID (from the sandbox project) =="
    RESULT=$(cd "$SANDBOX_REPO" && "$BIN" result "$TASK_ID" 2>&1) || true
    printf '%s\n' "$RESULT"
    LAST_TASK_ID="$TASK_ID"
    LAST_RESULT="$RESULT"
    return 0
}

# --- Scout: write-nothing role, result.md/status only ------------------
SCOUT_TASK="Write pong to result.md and set status done."
if dispatch_and_check scout "$SCOUT_TASK"; then
    if printf '%s\n' "$LAST_RESULT" | grep -qi pong; then
        ok "scout result contains pong"
    else
        bad "scout result did not contain pong"
    fi
else
    note "scout step did not complete; skipping its result check"
fi

# --- Builder: declared write scope, needs its own git lane --------------
BUILDER_TASK="Add a file hello.txt containing hi, commit it."
if dispatch_and_check builder "$BUILDER_TASK" --scope hello.txt; then
    BUILDER_TASK_ID="$LAST_TASK_ID"
    note "== checking Builder's lane branch for the commit =="
    BRANCH="multiverse/$BUILDER_TASK_ID"
    LANE="$SANDBOX_REPO/.multiverse/worktrees/$BUILDER_TASK_ID"
    if [ -d "$LANE" ] && git -C "$SANDBOX_REPO" rev-parse --verify -q "$BRANCH" >/dev/null; then
        if git -C "$SANDBOX_REPO" show "$BRANCH:hello.txt" 2>/dev/null | grep -qi hi; then
            ok "lane branch $BRANCH has a committed hello.txt containing hi"
        else
            bad "lane branch $BRANCH exists but hello.txt is missing or does not contain hi"
        fi
    else
        bad "no lane worktree/branch found for builder task $BUILDER_TASK_ID (expected $LANE on $BRANCH)"
    fi
else
    note "builder step did not complete; skipping its lane-branch check"
fi

note ""
if [ "$fail" -eq 0 ]; then
    note "overall: PASS"
else
    note "overall: FAIL"
fi
exit "$fail"

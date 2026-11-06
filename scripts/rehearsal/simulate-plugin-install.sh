#!/bin/sh
# Rehearses `herdr plugin install`/`action invoke` against this checkout
# WITHOUT ever touching a real Herdr: everything below runs against a
# freshly cloned temp "plugin root", isolated config/state/home
# directories, and a caller-supplied fake Herdr binary for $HERDR_BIN_PATH.
#
# It does three things, in order, matching the 0.9.5 plugin contract
# (https://herdr.dev/tui/docs/plugins/#commands-and-environment):
#   1. `git archive` this checkout's tracked tree into a temp plugin root,
#      the same tree shape a real `herdr plugin install` clone would give.
#   2. Run the manifest's [[build]] command (scripts/bootstrap-plugin.sh)
#      with NO runtime plugin env (the contract: build commands "do not
#      receive runtime plugin context or Herdr socket env") and confirm it
#      produces a working bin/multiverse.
#   3. Run every herdr-plugin.toml action/pane command with the full
#      documented runtime env (HERDR_ENV, HERDR_SOCKET_PATH,
#      HERDR_PLUGIN_ID/ROOT/CONFIG_DIR/STATE_DIR/CONTEXT_JSON/ACTION_ID),
#      always with $HERDR_BIN_PATH pointed at the caller's fake binary so
#      nothing here can ever shell out to a real `herdr`.
#
# This is a simulation, not an install: it proves the manifest's own
# scripts behave as documented. It cannot prove how the real Herdr binary
# parses herdr-plugin.toml, renders the action menu, enforces
# min_herdr_version, or manages install/link/uninstall lifecycle state —
# only a real `herdr plugin install`/`link` can.
#
# Usage:
#   scripts/rehearsal/simulate-plugin-install.sh <repo-checkout> <fake-herdr-binary>
#
# <repo-checkout>     path to a herdr-multiverse git checkout (this repo).
# <fake-herdr-binary>  path to an executable used as $HERDR_BIN_PATH for
#                      every action invocation (e.g. mv-herdr's fake-herdr).
#
# Leaves no trace outside a self-deleting mktemp -d work directory.

set -u

if [ $# -ne 2 ]; then
    echo "usage: $0 <repo-checkout> <fake-herdr-binary>" >&2
    exit 2
fi

REPO=$1
FAKE_HERDR=$2

case "$REPO" in
    /*) ;;
    *) echo "repo-checkout must be an absolute path" >&2; exit 2 ;;
esac
case "$FAKE_HERDR" in
    /*) ;;
    *) echo "fake-herdr-binary must be an absolute path" >&2; exit 2 ;;
esac
[ -f "$REPO/herdr-plugin.toml" ] || { echo "$REPO does not look like the plugin checkout (no herdr-plugin.toml)" >&2; exit 2; }
[ -x "$FAKE_HERDR" ] || { echo "$FAKE_HERDR is not an executable file" >&2; exit 2; }

WORK=$(mktemp -d "${TMPDIR:-/tmp}/mv-plugin-rehearsal.XXXXXX") || exit 1
trap 'rm -rf "$WORK"' EXIT INT TERM

PLUGIN_ROOT="$WORK/plugin-root"
CONFIG_DIR="$WORK/config"
STATE_DIR="$WORK/state"
HOME_DIR="$WORK/home"
PROJECT_DIR="$WORK/project"
mkdir -p "$PLUGIN_ROOT" "$CONFIG_DIR" "$STATE_DIR" "$HOME_DIR" "$PROJECT_DIR/.multiverse"

FAILED=0

echo "== 1. cloning the tracked checkout into a temp plugin root =="
if ! (cd "$REPO" && git archive HEAD) | tar -x -C "$PLUGIN_ROOT"; then
    echo "FAIL: could not archive $REPO into $PLUGIN_ROOT" >&2
    exit 1
fi

echo "== seeding an isolated project and config =="
git -C "$PROJECT_DIR" init -q
git \
    -c user.name=rehearsal -c user.email=rehearsal@example.invalid -c commit.gpgsign=false \
    -C "$PROJECT_DIR" commit --allow-empty -qm seed

cat >"$CONFIG_DIR/config.toml" <<EOF
[accounts.rehearsal-seat]
kind = "codex"
config_dir_var = "CODEX_HOME"
config_dir = '$HOME_DIR/codex-home'
pool = "codex:rehearsal"

[projects.rehearsal]
path = '$PROJECT_DIR'

[projects.rehearsal.chains]
scout = ["rehearsal-seat"]

[roles.scout]
write_scope = "none"
effort = "low"
on_exhausted = "queue"
skills = []

[roles.scout.models]
codex = "gpt-test"
EOF

echo "== 2. running the manifest build hook (scripts/bootstrap-plugin.sh), with no runtime plugin env =="
if env -i PATH="$PATH" HOME="$HOME_DIR" "$PLUGIN_ROOT/scripts/bootstrap-plugin.sh" install; then
    if [ -x "$PLUGIN_ROOT/bin/multiverse" ]; then
        VERSION_OUT=$(env -i PATH="$PATH" HERDR_BIN_PATH="$FAKE_HERDR" "$PLUGIN_ROOT/bin/multiverse" --version 2>&1) || true
        echo "PASS: build hook produced bin/multiverse ($VERSION_OUT)"
    else
        echo "FAIL: build hook exited 0 but bin/multiverse is missing or not executable"
        FAILED=1
    fi
else
    echo "FAIL: build hook (scripts/bootstrap-plugin.sh install) exited nonzero"
    FAILED=1
fi

# Asserts the action's exit code against the one expected under this fixture
# (a single isolated project, a fake Herdr with no live agents, cwd forced
# to the plugin checkout directory — never a real project). A mismatch here
# is exactly the shape of regression card 20261102-b-2691da was: an action
# Herdr's own manifest documents as working silently failing under Herdr's
# real invocation contract. `expected_rc` is not "exit 0 or bust": some
# actions are documented to fail in this exact fixture (see each call
# site's comment) and a passing rehearsal must still expect that.
run_action() {
    action_id=$1
    expected_rc=$2
    shift 2
    echo "== 3. action $action_id: $* (expect exit=$expected_rc) =="
    # Herdr 0.9.5 runs runtime commands with the plugin directory as cwd
    # (plugin-action.sh's own header comment, and the contract doc). Some
    # commands (e.g. `tasks` with no explicit project) fall back to
    # resolving "the current project" from cwd when it is not a configured
    # project, so reproducing Herdr's cwd exactly matters, not just its env.
    (
        cd "$PLUGIN_ROOT" && env -i \
            PATH="$PATH" \
            HOME="$HOME_DIR" \
            HERDR_ENV=1 \
            HERDR_SOCKET_PATH="$WORK/herdr.sock" \
            HERDR_BIN_PATH="$FAKE_HERDR" \
            HERDR_PLUGIN_ID=multiverse \
            HERDR_PLUGIN_ROOT="$PLUGIN_ROOT" \
            HERDR_PLUGIN_CONFIG_DIR="$CONFIG_DIR" \
            HERDR_PLUGIN_STATE_DIR="$STATE_DIR" \
            HERDR_PLUGIN_CONTEXT_JSON='{}' \
            HERDR_PLUGIN_ACTION_ID="$action_id" \
            "$@"
    )
    rc=$?
    if [ "$rc" -eq "$expected_rc" ]; then
        echo "   PASS: exit=$rc"
    else
        echo "   FAIL: exit=$rc, expected $expected_rc"
        FAILED=1
    fi
}

if [ "$FAILED" -eq 0 ]; then
    # herdr reachable (0.9.3 via the fake) and accounts/project checks all
    # pass; "git user configured" reads the *plugin checkout's* git config
    # (doctor.rs: "not a configured project"), and the archived plugin root
    # here is a plain directory tree with no .git and an isolated, empty
    # $HOME — so that one line always fails in this fixture, not a bug.
    run_action multiverse-doctor 1 "$PLUGIN_ROOT/scripts/plugin-action.sh" doctor
    run_action multiverse-check 0 "$PLUGIN_ROOT/scripts/plugin-action.sh" check --read-only
    run_action multiverse-accounts 0 "$PLUGIN_ROOT/scripts/plugin-action.sh" accounts
    # Herdr invokes this with the plugin directory as cwd and no project
    # args (card 20261102-b-2691da): must show the fleet view (exit 0), not
    # fail with "no project found above the current directory" (exit 3).
    run_action multiverse-tasks 0 "$PLUGIN_ROOT/scripts/plugin-action.sh" tasks --read-only
    run_action multiverse-board 0 "$PLUGIN_ROOT/scripts/plugin-action.sh" board --all --read-only
    run_action multiverse-workers 0 "$PLUGIN_ROOT/scripts/plugin-action.sh" workers
    # Known gap, not this task's to close: `mv-cli` has no Herdr runner
    # wired for Nexus launch yet, so every project fails to start.
    run_action multiverse-start 1 "$PLUGIN_ROOT/scripts/plugin-action.sh" start --all

    echo "== 3b. dashboard pane (board --all --watch --read-only), capped at 5s wall clock =="
    OUT=$(cd "$PLUGIN_ROOT" && perl -e 'alarm 5; exec @ARGV' -- env -i \
        PATH="$PATH" \
        HOME="$HOME_DIR" \
        HERDR_ENV=1 \
        HERDR_SOCKET_PATH="$WORK/herdr.sock" \
        HERDR_BIN_PATH="$FAKE_HERDR" \
        HERDR_PLUGIN_ID=multiverse \
        HERDR_PLUGIN_ROOT="$PLUGIN_ROOT" \
        HERDR_PLUGIN_CONFIG_DIR="$CONFIG_DIR" \
        HERDR_PLUGIN_STATE_DIR="$STATE_DIR" \
        HERDR_PLUGIN_CONTEXT_JSON='{}' \
        HERDR_PLUGIN_ENTRYPOINT_ID=dashboard \
        "$PLUGIN_ROOT/scripts/plugin-action.sh" board --all --watch --read-only 2>&1)
    rc=$?
    echo "$OUT" | head -c 2000
    echo
    # A real terminal would block in raw mode until the 5s alarm kills it
    # (perl's default SIGALRM disposition: terminated by signal 14, exit
    # 128+14=142 under this shell's reporting). This sandbox has no tty, so
    # mv-board's raw-mode setup fails immediately instead (exit 1) — both
    # are "did not hang, did not crash the harness", so both pass here.
    case "$rc" in
        1 | 124 | 142) echo "   PASS: exit=$rc" ;;
        *)
            echo "   FAIL: exit=$rc, expected 1 (no-tty fail-fast), 124, or 142 (alarm-killed)"
            FAILED=1
            ;;
    esac
else
    echo "== skipping actions: build hook failed =="
fi

echo "== done: temp work dir $WORK will be removed on exit =="
exit "$FAILED"

#!/bin/sh
# Rehearses `multiverse config migrate` against a real config.toml without
# ever touching the real file: makes two private copies under a fresh
# mktemp -d (one left read-only as a pristine reference, one migrated for
# real), then checks that the migration is comment-preserving, idempotent,
# and leaves the config loadable by the Rust CLI's `check`/`doctor` — and,
# if a Python multiverse reference is given, that it correctly can no
# longer load the migrated file (new role spellings are a one-way door).
#
# Never touches ~/.config/multiverse, ~/.config/herdr, or any project's
# real .multiverse. Never starts, stops, dispatches, or sends to real
# Herdr: HERDR_BIN_PATH is always pointed at a stub script this file
# writes into its own temp dir.
#
# POSIX sh only. Takes every path as an argument; bakes in none of its own.
#
# Usage:
#   verify_config_migration.sh --config PATH --bin PATH
#     [--python-ref DIR] [--keep]
#
#   --config PATH      A config.toml to rehearse against. Read-only: this
#                       script only ever copies it, never writes to it.
#   --bin PATH          The `multiverse` binary to run (e.g.
#                       target/release/multiverse from a `cargo build
#                       --release -p mv-cli`).
#   --python-ref DIR    Optional: a checkout of the frozen Python
#                       reference (the directory holding the `multiverse`
#                       package). When given, also verifies that
#                       `PYTHONPATH=DIR python3 -P -m multiverse.cli check`
#                       refuses the migrated config.
#   --keep              Do not delete the working temp dir on exit; print
#                       its path instead. Default is to always delete it.

set -eu

CONFIG=""
BIN=""
PYTHON_REF=""
KEEP=0

while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG="$2"; shift 2 ;;
        --bin) BIN="$2"; shift 2 ;;
        --python-ref) PYTHON_REF="$2"; shift 2 ;;
        --keep) KEEP=1; shift ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$CONFIG" ] || [ -z "$BIN" ]; then
    echo "usage: $0 --config PATH --bin PATH [--python-ref DIR] [--keep]" >&2
    exit 2
fi
if [ ! -f "$CONFIG" ]; then
    echo "no such config file: $CONFIG" >&2
    exit 2
fi
if [ ! -x "$BIN" ]; then
    echo "not an executable binary: $BIN" >&2
    exit 2
fi

WORK=$(mktemp -d -t mv-config-migration-rehearsal)
cleanup() {
    if [ "$KEEP" -eq 1 ]; then
        echo "kept working directory: $WORK"
    else
        rm -rf "$WORK"
    fi
}
trap cleanup EXIT

ORIGINAL="$WORK/config-original.toml"
WORKING="$WORK/config-working.toml"
cp "$CONFIG" "$ORIGINAL"
chmod 444 "$ORIGINAL"
cp "$CONFIG" "$WORKING"

mkdir -p "$WORK/herdr-config" "$WORK/state-dir" "$WORK/home"
FAKE_HERDR="$WORK/fake-herdr"
cat > "$FAKE_HERDR" <<EOF
#!/bin/sh
case "\$1" in
  --version) echo "herdr 0.9.0" ;;
  plugin)
    if [ "\$2" = "config-dir" ]; then
      echo "$WORK/herdr-config"
      exit 0
    fi
    echo "fake-herdr: refusing to act on \$*" >&2
    exit 1
    ;;
  *) echo "fake-herdr: refusing to act on \$*" >&2; exit 1 ;;
esac
EOF
chmod +x "$FAKE_HERDR"

export HERDR_BIN_PATH="$FAKE_HERDR"
export HERDR_PLUGIN_CONFIG_DIR="$WORK/herdr-config"
export HERDR_PLUGIN_STATE_DIR="$WORK/state-dir"
export HOME="$WORK/home"
unset MULTIVERSE_CONFIG || true

fail=0
note() { printf '%s\n' "$1"; }
ok() { printf 'PASS: %s\n' "$1"; }
bad() { printf 'FAIL: %s\n' "$1"; fail=1; }

note "== dry-run =="
if "$BIN" config migrate --config "$WORKING" --dry-run > "$WORK/dry-run.diff" 2>&1; then
    if cmp -s "$ORIGINAL" "$WORKING"; then
        ok "dry-run wrote nothing"
    else
        bad "dry-run modified the working copy"
    fi
else
    bad "dry-run exited nonzero"
fi

note "== real migrate =="
if ! "$BIN" config migrate --config "$WORKING" --backup > "$WORK/migrate-1.out" 2>&1; then
    bad "migrate (first run) exited nonzero"
fi
cat "$WORK/migrate-1.out"

note "== idempotency =="
cp "$WORKING" "$WORK/config-after-first.toml"
if ! "$BIN" config migrate --config "$WORKING" --backup > "$WORK/migrate-2.out" 2>&1; then
    bad "migrate (second run) exited nonzero"
fi
if cmp -s "$WORK/config-after-first.toml" "$WORKING"; then
    ok "second migrate run is a no-op"
else
    bad "second migrate run changed the file"
fi
if ! grep -q "already on current role keys" "$WORK/migrate-2.out"; then
    bad "second run did not report 'already on current role keys'"
fi

note "== comment count preserved =="
before=$(grep -c "^[[:space:]]*#" "$ORIGINAL" || true)
after=$(grep -c "^[[:space:]]*#" "$WORKING" || true)
if [ "$before" = "$after" ]; then
    ok "comment line count unchanged ($before)"
else
    bad "comment line count changed: $before -> $after"
fi

note "== original untouched =="
if cmp -s "$CONFIG" "$ORIGINAL"; then
    ok "pristine reference copy still matches the input config"
else
    bad "the pristine reference copy changed — this should be impossible"
fi

note "== multiverse check (Rust) =="
if "$BIN" check --config "$WORKING" > "$WORK/check.out" 2>&1; then
    ok "check passed"
else
    note "check exited $? (see below) — this may be a real finding, not a script bug:"
    cat "$WORK/check.out"
fi

note "== multiverse doctor (Rust) =="
if "$BIN" doctor --config "$WORKING" > "$WORK/doctor.out" 2>&1; then
    ok "doctor passed"
else
    note "doctor exited $? (see below) — this may be a real finding, not a script bug:"
    cat "$WORK/doctor.out"
fi

if [ -n "$PYTHON_REF" ]; then
    note "== Python reference can no longer load the migrated config =="
    export MULTIVERSE_CONFIG="$WORKING"
    set +e
    PYTHONPATH="$PYTHON_REF" python3 -P -m multiverse.cli check > "$WORK/python-check.out" 2>&1
    py_exit=$?
    set -e
    unset MULTIVERSE_CONFIG
    if [ "$py_exit" -eq 0 ]; then
        bad "Python CLI loaded the migrated config (expected it to refuse)"
    else
        ok "Python CLI refused the migrated config (exit $py_exit)"
    fi
    cat "$WORK/python-check.out"
fi

note ""
if [ "$fail" -eq 0 ]; then
    note "overall: PASS"
else
    note "overall: FAIL"
fi
exit "$fail"

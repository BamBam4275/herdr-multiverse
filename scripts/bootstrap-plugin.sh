#!/bin/sh
# The plugin's [[build]] entry (herdr-plugin.toml). Runs once, on
# `herdr plugin install`, after the clone and before registration; a nonzero
# exit here aborts the install. `herdr plugin link` never runs [[build]]:
# plugin-action.sh calls this script itself, with "action", as a last-mile
# fallback on a linked checkout's first action.
#
# Order of attempts:
#   1. bin/multiverse already present and its --version matches this
#      manifest's version (a previous download, or a dev `cargo build`) ->
#      no-op, success.
#   2. Download the checksum-verified release archive for this platform from
#      the tagged GitHub Release, verify it against SHA256SUMS, extract, and
#      install it to bin/multiverse.
#   3. Else, if cargo is on PATH, build from source (scripts/build.sh) and
#      install the result to bin/multiverse.
#   4. Else, fail with a clear message: install rustc, or use a tagged
#      release.
#
# POSIX sh only. Never executes anything from a downloaded archive before its
# checksum is verified.
# MULTIVERSE_RELEASE_BASE_URL overrides the versioned asset directory for
# offline mirrors and local release tests (see scripts/tests/README.md).
#
# Provenance: GitHub Releases attach a SLSA build provenance attestation
# (see .github/workflows/release.yml). SHA256SUMS is same-origin as the
# tarball, so a compromised GitHub release pair would still checksum-verify.
# When `gh` is on PATH and MULTIVERSE_RELEASE_BASE_URL is unset (the default
# GitHub download), this script runs `gh attestation verify` on the archive
# after the checksum check. Checksums already passed, so a missing or
# unusable `gh` (too old, unauthenticated, offline, or a release with no
# attestation) prints a warning and keeps the archive. The release binary
# is refused only when `gh` positively reports that the artifact does not
# match its attestation; the cargo source fallback still runs. Missing
# `gh` is not a failure. A custom MULTIVERSE_RELEASE_BASE_URL (offline
# mirror / tests) skips attestation because those assets have no GitHub
# attestation.
set -eu

CALLER=${1:-install}
if [ "$CALLER" = "action" ]; then
    RETRY_HINT="retry the action"
else
    RETRY_HINT="re-run 'herdr plugin install'"
fi

SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PLUGIN_ROOT=$(CDPATH='' cd -- "$SCRIPT_DIR/.." && pwd)
MANIFEST="$PLUGIN_ROOT/herdr-plugin.toml"
BIN_DIR="$PLUGIN_ROOT/bin"
BIN="$BIN_DIR/multiverse"
REPO="BamBam4275/herdr-multiverse"

if [ ! -f "$MANIFEST" ]; then
    echo "bootstrap-plugin: $MANIFEST not found — run this script from inside the plugin checkout" >&2
    exit 1
fi

VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)"[[:space:]]*$/\1/p' "$MANIFEST" | head -n1)
if [ -z "$VERSION" ]; then
    echo "bootstrap-plugin: could not read 'version' from $MANIFEST" >&2
    exit 1
fi

# Step 1: an existing binary of the right version is already enough.
if [ -x "$BIN" ]; then
    CURRENT=$("$BIN" --version 2>/dev/null || true)
    if [ "$CURRENT" = "multiverse $VERSION" ]; then
        echo "bootstrap-plugin: bin/multiverse already at $VERSION, nothing to do"
        exit 0
    fi
fi

# Map uname -s/-m to the four published release triples. Reject anything
# else rather than guessing.
OS=$(uname -s)
ARCH=$(uname -m)
case "$OS" in
Darwin)
    case "$ARCH" in
    arm64) TRIPLE="aarch64-apple-darwin" ;;
    x86_64) TRIPLE="x86_64-apple-darwin" ;;
    *) TRIPLE="" ;;
    esac
    ;;
Linux)
    case "$ARCH" in
    aarch64) TRIPLE="aarch64-unknown-linux-gnu" ;;
    x86_64) TRIPLE="x86_64-unknown-linux-gnu" ;;
    *) TRIPLE="" ;;
    esac
    ;;
*)
    TRIPLE=""
    ;;
esac

DOWNLOAD_OK=0
if [ -n "$TRIPLE" ]; then
    DOWNLOADER=""
    if command -v curl >/dev/null 2>&1; then
        DOWNLOADER="curl"
    elif command -v wget >/dev/null 2>&1; then
        DOWNLOADER="wget"
    fi

    if [ -n "$DOWNLOADER" ]; then
        TMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/multiverse-bootstrap.XXXXXX")
        trap 'rm -rf "$TMP_DIR"' EXIT

        ARCHIVE="multiverse-$TRIPLE.tar.gz"
        BASE_URL="${MULTIVERSE_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download/v$VERSION}"
        BASE_URL="${BASE_URL%/}"
        ARCHIVE_URL="$BASE_URL/$ARCHIVE"
        SUMS_URL="$BASE_URL/SHA256SUMS"

        fetch() {
            # fetch <url> <dest>
            if [ "$DOWNLOADER" = "curl" ]; then
                curl --fail --location --silent --show-error --output "$2" "$1"
            else
                wget --quiet --output-document="$2" "$1"
            fi
        }

        if fetch "$ARCHIVE_URL" "$TMP_DIR/$ARCHIVE" 2>/dev/null \
            && fetch "$SUMS_URL" "$TMP_DIR/SHA256SUMS" 2>/dev/null; then
            EXPECTED=$(awk -v f="$ARCHIVE" '{ name = $2; sub(/^\*/, "", name); if (name == f) print $1 }' "$TMP_DIR/SHA256SUMS" 2>/dev/null | head -n1)
            if [ -n "$EXPECTED" ]; then
                if command -v sha256sum >/dev/null 2>&1; then
                    ACTUAL=$(sha256sum "$TMP_DIR/$ARCHIVE" | awk '{print $1}')
                elif command -v shasum >/dev/null 2>&1; then
                    ACTUAL=$(shasum -a 256 "$TMP_DIR/$ARCHIVE" | awk '{print $1}')
                else
                    ACTUAL=""
                fi

                if [ -n "$ACTUAL" ] && [ "$ACTUAL" = "$EXPECTED" ]; then
                    ATTEST_OK=1
                    if [ -z "${MULTIVERSE_RELEASE_BASE_URL:-}" ] && command -v gh >/dev/null 2>&1; then
                        ATTEST_LOG="$TMP_DIR/attestation.log"
                        if ! gh attestation verify --help >/dev/null 2>&1; then
                            echo "bootstrap-plugin: gh is too old for provenance attestation; checksum already verified — continuing with $ARCHIVE" >&2
                        else
                            ATTEST_STATUS=0
                            gh attestation verify "$TMP_DIR/$ARCHIVE" --repo "$REPO" >"$ATTEST_LOG" 2>&1 || ATTEST_STATUS=$?
                            if [ "$ATTEST_STATUS" -eq 0 ]; then
                                :
                            elif grep -q -e 'Verification failed' -e 'does not match' "$ATTEST_LOG"; then
                                echo "bootstrap-plugin: GitHub provenance attestation does not match for $ARCHIVE — refusing release binary" >&2
                                ATTEST_OK=0
                            elif [ "$ATTEST_STATUS" -eq 4 ] || grep -qi -e 'authentication required' -e 'gh auth login' -e 'not logged in' "$ATTEST_LOG"; then
                                echo "bootstrap-plugin: gh is not authenticated; checksum already verified — continuing with $ARCHIVE" >&2
                            elif grep -qi -e 'unknown command' -e 'not a gh command' "$ATTEST_LOG"; then
                                echo "bootstrap-plugin: gh is too old for provenance attestation; checksum already verified — continuing with $ARCHIVE" >&2
                            elif grep -qi -e 'no attestations found' -e 'HTTP 404' "$ATTEST_LOG"; then
                                echo "bootstrap-plugin: release has no provenance attestation; checksum already verified — continuing with $ARCHIVE" >&2
                            elif grep -qi -e 'failed to fetch' -e 'connection refused' -e 'network' -e 'timeout' -e 'Could not resolve' -e 'Temporary failure in name resolution' -e 'offline' "$ATTEST_LOG"; then
                                echo "bootstrap-plugin: could not reach GitHub to verify provenance; checksum already verified — continuing with $ARCHIVE" >&2
                            else
                                echo "bootstrap-plugin: provenance attestation unusable; checksum already verified — continuing with $ARCHIVE" >&2
                            fi
                        fi
                    fi
                    if [ "$ATTEST_OK" = "1" ]; then
                        EXTRACT_DIR="$TMP_DIR/extracted"
                        mkdir -p "$EXTRACT_DIR"
                        if tar -xzf "$TMP_DIR/$ARCHIVE" -C "$EXTRACT_DIR" && [ -f "$EXTRACT_DIR/multiverse" ]; then
                            mkdir -p "$BIN_DIR"
                            chmod +x "$EXTRACT_DIR/multiverse"
                            mv -f "$EXTRACT_DIR/multiverse" "$BIN"
                            DOWNLOAD_OK=1
                        fi
                    fi
                elif [ -n "$ACTUAL" ]; then
                    echo "bootstrap-plugin: checksum mismatch for $ARCHIVE — refusing release binary" >&2
                fi
            fi
        fi

        rm -rf "$TMP_DIR"
        trap - EXIT
    fi
fi

if [ "$DOWNLOAD_OK" = "1" ]; then
    echo "bootstrap-plugin: installed bin/multiverse $VERSION ($TRIPLE) from release"
    exit 0
fi

# Step 3: build from source.
if command -v cargo >/dev/null 2>&1; then
    if "$SCRIPT_DIR/build.sh"; then
        BUILT="$PLUGIN_ROOT/target/release/multiverse"
        if [ -f "$BUILT" ]; then
            mkdir -p "$BIN_DIR"
            cp -f "$BUILT" "$BIN"
            chmod +x "$BIN"
            echo "bootstrap-plugin: built bin/multiverse $VERSION from source with cargo"
            exit 0
        fi
        echo "bootstrap-plugin: cargo build succeeded but $BUILT is missing — ${RETRY_HINT}" >&2
        exit 1
    fi
    echo "bootstrap-plugin: 'cargo build' failed — ${RETRY_HINT}" >&2
    exit 1
fi

echo "bootstrap-plugin: no release binary available for this platform and no cargo found on PATH — install rustc (https://rustup.rs) or use a tagged release, then ${RETRY_HINT}" >&2
exit 1

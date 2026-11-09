#!/bin/sh
# Reproducible Linux demo: discovery, confirmation, then a traced hit.
#
# Prerequisites: Linux aarch64, cc, git, sha256sum, kioku on PATH
# (or KIOKU_BIN), and KIOKU_STRACE pointing at the pinned Debian strace 6.1
# binary described in README.md, section "Pinned tracer".
#
# The static helper burns CPU and reads input.txt. The CPU work stands in for a
# real deterministic checker so the hit's saving is visible; it is not a
# benchmark of cargo test. Every traced command and tool run uses a synthetic
# environment. A fresh directory is created with mktemp and removed on exit;
# no caller-supplied path is ever deleted.
set -eu

fail() {
    printf 'demo: %s\n' "$1" >&2
    exit 1
}

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
SENTINEL=kioku-demo-synthetic-secret-5f2c9a

TMP_PARENT=${TMPDIR:-/tmp}
[ -d "$TMP_PARENT" ] || fail "TMPDIR is not a directory"
TMP_PARENT=$(CDPATH= cd -P "$TMP_PARENT" && pwd) || fail "cannot resolve TMPDIR"
DEMO_DIR=$(mktemp -d "$TMP_PARENT/kioku-demo.XXXXXXXXXX") \
    || fail "cannot create a demo directory under TMPDIR"

cleanup() {
    rm -rf -- "$DEMO_DIR" || :
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

[ "$(uname -s)" = Linux ] || fail "traced reuse requires Linux"
[ "$(uname -m)" = aarch64 ] || fail "this demo is captured on Linux aarch64"
ARCH=aarch64
PINNED=0e69fa123b3d9732645ce1ac8c89819d100f83cdc9e52a80565d9ad8e7d8cf28

CC=$(command -v cc) || fail "cc is required to build the static helper"
GIT=$(command -v git) || fail "git is required"
command -v sha256sum >/dev/null || fail "sha256sum is required"

KIOKU_BIN=${KIOKU_BIN:-kioku}
case $KIOKU_BIN in
    */*) KIOKU_PATH=$KIOKU_BIN ;;
    *) KIOKU_PATH=$(command -v "$KIOKU_BIN" 2>/dev/null || true) ;;
esac
[ -n "$KIOKU_PATH" ] && [ -x "$KIOKU_PATH" ] || fail "kioku not found; set KIOKU_BIN"
KIOKU_PATH=$(CDPATH= cd -- "$(dirname -- "$KIOKU_PATH")" && pwd)/$(basename -- "$KIOKU_PATH")
KIOKU_DIR_NAME=$(dirname -- "$KIOKU_PATH")

[ -n "${KIOKU_STRACE:-}" ] || fail "set KIOKU_STRACE to the pinned strace binary (README: Pinned tracer)"
[ -x "$KIOKU_STRACE" ] || fail "KIOKU_STRACE is not an executable file"
case $KIOKU_STRACE in /*) ;; *) fail "KIOKU_STRACE must be an absolute path" ;; esac
ACTUAL=$(sha256sum -- "$KIOKU_STRACE" | cut -d' ' -f1)
[ "$ACTUAL" = "$PINNED" ] || fail "KIOKU_STRACE has sha256 $ACTUAL, expected pinned $PINNED"

SYNTH_PATH="$KIOKU_DIR_NAME:$(dirname -- "$GIT"):$(dirname -- "$CC"):/usr/local/bin:/usr/bin:/bin"
DEMO_HOME="$DEMO_DIR/home"
DEMO_TMP="$DEMO_DIR/tmp"
DEMO_CACHE="$DEMO_DIR/cache"

run_env() {
    env -i \
        PATH="$SYNTH_PATH" \
        HOME="$DEMO_HOME" \
        TMPDIR="$DEMO_TMP" \
        LANG=C \
        LC_ALL=C \
        GIT_CONFIG_NOSYSTEM=1 \
        KIOKU_DIR="$DEMO_CACHE" \
        KIOKU_STRACE="$KIOKU_STRACE" \
        KIOKU_SENTINEL_SECRET="$SENTINEL" \
        "$@"
}

run_checked() {
    demo_pattern=$1
    shift
    printf '$ %s\n' "$*"
    if ! demo_output=$(run_env "$@" 2>&1 </dev/null); then
        printf '%s\n' "$demo_output"
        fail "command failed"
    fi
    printf '%s\n' "$demo_output"
    printf '%s\n' "$demo_output" | grep -q -F -- "$demo_pattern" \
        || fail "expected output containing: $demo_pattern"
}

mkdir -p "$DEMO_HOME" "$DEMO_TMP" "$DEMO_CACHE" "$DEMO_DIR/repo"
cd "$DEMO_DIR/repo"
printf 'demo input\n' > input.txt
cp "$ROOT/docs/demo-helper-$ARCH.S" check.S
run_env "$CC" -nostdlib -static check.S -o check
run_env "$GIT" init -q
printf 'tracer sha256: %s\n' "$PINNED"
run_checked 'kioku ' kioku --version
run_checked 'executed (exit 0' kioku run --trust-trace -- ./check input.txt
run_checked 'executed (exit 0' kioku run --trust-trace -- ./check input.txt
run_checked 'cached (pass,' kioku run --trust-trace -- ./check input.txt
run_checked 'counters: 1 hits (1 measured)' kioku status
printf '%s\n' "$demo_output" | grep -Eq 'saved: [1-9][0-9]* ms over 1 measured hit' \
    || fail "expected a positive measured saving"

if grep -R -F -q -- "$SENTINEL" "$DEMO_DIR"; then
    fail "synthetic environment secret leaked into the demo directory"
fi
printf 'synthetic environment secret: absent from the demo directory\n'

# The EXIT trap removes only $DEMO_DIR, the directory mktemp created above.

#!/bin/sh
# Security and replay checks for docs/demo.sh.
#
# Requires the same prerequisites as the demo: Linux aarch64, cc, git, a
# release kioku (KIOKU_BIN) and the pinned tracer (KIOKU_STRACE).
#
# It proves that the demo:
#   - never reads or deletes a caller-supplied KIOKU_DEMO_DIR;
#   - removes only the directory it created, even when TMPDIR is a symlink;
#   - replays the committed capture under an outer `env -i`.
set -eu

fail() {
    printf 'test-demo: %s\n' "$1" >&2
    exit 1
}

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
[ -n "${KIOKU_BIN:-}" ] || fail "set KIOKU_BIN to the kioku binary"
[ -x "$KIOKU_BIN" ] || fail "KIOKU_BIN is not executable"
case $KIOKU_BIN in
    /*) ;;
    *) KIOKU_BIN=$(CDPATH= cd -- "$(dirname -- "$KIOKU_BIN")" && pwd)/$(basename -- "$KIOKU_BIN") ;;
esac
[ -n "${KIOKU_STRACE:-}" ] || fail "set KIOKU_STRACE to the pinned tracer"
[ -x "$KIOKU_STRACE" ] || fail "KIOKU_STRACE is not executable"
case $KIOKU_STRACE in /*) ;; *) fail "KIOKU_STRACE must be absolute" ;; esac
KIOKU_DIR=$(dirname -- "$KIOKU_BIN")

# The script must not honor a caller path at all: no deletion code can run on it.
if grep -q 'KIOKU_DEMO_DIR' "$ROOT/docs/demo.sh"; then
    fail "docs/demo.sh still references KIOKU_DEMO_DIR"
fi

WORK=$(mktemp -d "${TMPDIR:-/tmp}/kioku-demo-test.XXXXXXXXXX") \
    || fail "cannot create a test directory"
trap 'rm -rf -- "$WORK" || :' EXIT
trap 'exit 1' HUP INT TERM

mkdir -p "$WORK/home" "$WORK/provided"
printf 'keep\n' > "$WORK/home/keep"
printf 'keep\n' > "$WORK/provided/keep"
ln -s "$WORK/home" "$WORK/tmp-link"

env -i \
    PATH="$KIOKU_DIR:${PATH:-/usr/local/bin:/usr/bin:/bin}" \
    HOME="$WORK/home" \
    TMPDIR="$WORK/tmp-link" \
    KIOKU_BIN="$KIOKU_BIN" \
    KIOKU_STRACE="$KIOKU_STRACE" \
    KIOKU_DEMO_DIR="$WORK/provided" \
    sh "$ROOT/docs/demo.sh" > "$WORK/out" 2>&1 \
    || { cat "$WORK/out"; fail "demo failed under env -i"; }

[ -f "$WORK/provided/keep" ] || fail "caller-supplied path was modified"
[ -f "$WORK/home/keep" ] || fail "outer HOME or symlink target was modified"
if find "$WORK/home" -maxdepth 1 -name 'kioku-demo.*' | grep -q .; then
    fail "demo directory was not removed"
fi
grep -q 'synthetic environment secret: absent' "$WORK/out" \
    || fail "sentinel check missing"

normalize() {
    sed -E 's#[^ ]*/kioku-demo\.[A-Za-z0-9]+#<demo>#g; s/[0-9]+ ms/N ms/g'
}
normalize < "$WORK/out" > "$WORK/got"
normalize < "$ROOT/docs/demo-session.txt" > "$WORK/want"
diff -u "$WORK/want" "$WORK/got" || fail "capture does not replay"

printf 'test-demo: security checks and env -i replay OK\n'

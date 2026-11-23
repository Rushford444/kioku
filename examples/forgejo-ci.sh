#!/usr/bin/env bash
set -u
# The runner is provisioned offline with Rust, nextest, cargo-deny and dependencies.
export CARGO_NET_OFFLINE=true
full() {
  printf 'Full suite: %s\n' "$1" >&2
  status=0
  cargo build --locked || status=1
  cargo fmt --check || status=1
  cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic || status=1
  cargo clippy --all-targets -- -D warnings -W clippy::pedantic || status=1
  cargo nextest run --all-features --no-fail-fast || status=1
  cargo nextest run --no-fail-fast || status=1
  cargo test --doc --all-features || status=1
  cargo deny check || status=1
  return "$status"
}
if [ "${FORGEJO_EVENT_NAME:-}" != push ] || [[ "${FORGEJO_REF:-}" != refs/heads/* ]] || [ "${FORGEJO_REF:-}" = refs/heads/main ]; then
  full 'main, merge request, or unknown event/ref'
  exit $?
fi
if [ -z "${KIOKU_SELECT_STORE:-}" ] || [ ! -d "$KIOKU_SELECT_STORE" ] || ! git rev-parse --verify 'refs/heads/main^{commit}' >/dev/null 2>&1; then
  full 'local main base or prepared store unavailable'
  exit $?
fi
if ! cargo build --locked; then
  full 'local kioku build failed'
  exit $?
fi
if [ ! -x target/debug/kioku ]; then
  full 'local kioku binary unavailable'
  exit $?
fi
# Kioku itself validates the base/store and falls back to all on incomplete
# evidence. Never retry after invocation: a child may already have run.
set +e
target/debug/kioku --json test --repo . --store "$KIOKU_SELECT_STORE" --base main
rc=$?
set -e
case "$rc" in
  0) printf 'CI: partial result; full CI not established\n' >&2; exit 3 ;;
  5) full 'selection reported full-green; run the remaining CI gates too'; exit $? ;;
  *) exit "$rc" ;;
esac

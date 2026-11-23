# Contributing

Thanks for taking the time.

## Setup

```sh
git clone https://github.com/Rushford444/kioku.git
cd kioku
cargo build
```

Rust stable, edition 2024. The MSRV is declared in `Cargo.toml`.

## Before opening a pull request

```sh
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic
cargo clippy --all-targets -- -D warnings -W clippy::pedantic
cargo nextest run --all-features
cargo test --doc
cargo deny check
```

For a change in the fingerprint, the key or the profiles, also run:

```sh
cargo mutants
```

Report survivors with a written justification.

## Rules that matter here

- **No false green.** A skip must never hide a command whose result would
  have changed. When a doubt exists, run the command.
- Failures are never memorized.
- Nothing ambiguous is cached: a parse error, an unreadable file, a changed
  source state during the run, a concurrent write.
- No `unwrap()` or `expect()` outside tests, no `unsafe`, typed errors in the
  library (`thiserror`), `anyhow` only in the binary.
- Public API changes need a decision recorded in the changelog.
- Legacy fingerprint and key tests do not establish portability of traced
  entries. Traced entries are local and bound to the canonical repository.

## Tests

- Fingerprint property tests (`proptest`) cover AST and byte modes. These
  do not authorize AST-based traced reuse: traced reads use bytes.
- Integration tests (`assert_cmd`, `tempfile`) cover execution, tracing,
  invalidation, locking and failures. Traced reuse is opt-in, local, and
  includes the full environment; consult the README for its assumptions.

Platform-specific test cases and helpers are gated individually where needed.
Windows does not exercise Unix-only shell cases. A cross-platform pass does not
replace Linux/aarch64 tracing evidence.

## Pinned tracer tests (Linux/aarch64)

Tracer-dependent tests are explicitly ignored by default. The default
`cargo nextest run` does not establish tracing coverage. For the tracing gate,
obtain the Debian bookworm arm64 `strace` 6.1-0.1 package, verify both the
archive and extracted executable, and run all tests including ignored tests:

```sh
mkdir -p target/tracer
curl -fL https://deb.debian.org/debian/pool/main/s/strace/strace_6.1-0.1_arm64.deb -o target/tracer/strace_6.1-0.1_arm64.deb
printf '%s  %s\n' 751817724af91cb95df566a21a154f9a7dcdd938e700e5cb9a5a47eb19752ea2 target/tracer/strace_6.1-0.1_arm64.deb | sha256sum -c -
dpkg-deb -x target/tracer/strace_6.1-0.1_arm64.deb target/tracer/extracted
printf '%s  %s\n' 0e69fa123b3d9732645ce1ac8c89819d100f83cdc9e52a80565d9ad8e7d8cf28 target/tracer/extracted/usr/bin/strace | sha256sum -c -
mkdir -p target/tracer/helpers
cc -nostdlib -static tests/false_green_zoo/read_aarch64.S -o target/tracer/helpers/read
cc -nostdlib -static tests/false_green_zoo/exec_aarch64.S -o target/tracer/helpers/exec
cc -nostdlib -static tests/false_green_zoo/operations_aarch64.S -o target/tracer/helpers/operations
TRACE_TMP="$(mktemp -d /tmp/kioku-trace-tests.XXXXXXXXXX)"
TMPDIR="$TRACE_TMP" \
KIOKU_STRACE="$PWD/target/tracer/extracted/usr/bin/strace" \
KIOKU_TRACE_TEST_HELPER="$PWD/target/tracer/helpers/read" \
KIOKU_TRACE_EXEC_HELPER="$PWD/target/tracer/helpers/exec" \
KIOKU_TRACE_OPS_HELPER="$PWD/target/tracer/helpers/operations" \
cargo nextest run --run-ignored all --no-fail-fast --test-threads 1
```

The temporary directory is retained for inspection; test fixtures may need permission repair before removal.

Stop if either checksum fails. Downloading requires network access; normal
kioku operation does not. The tracing gate requires a Linux/aarch64 host that
permits tracing, and the approved executable. Other platforms do not provide
this tracing evidence.

## Packaging

```sh
scripts/package.sh dist
```

Builds the release binary and writes the archives locally. Cargo defaults to
offline mode; dependencies and target toolchains must already be available.
Set `CARGO_NET_OFFLINE=false` explicitly if network access is authorized. Each
archive has its own `.sha256` file; existing output files are not overwritten.
Aggregate and verify checksums separately for a multi-target release.

Inspect the crate list with `cargo package --offline --locked --list` and audit
both crate and binary archive contents, links, licenses and secrets on the final
candidate before any distribution. Packaging is not publication approval.
Nothing is published: no release, no registry, no identity.

## Commit messages

Atomic commits, one subject per commit: `feat:`, `fix:`, `test:`, `docs:`,
`chore:`.

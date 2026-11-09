# Evidence for traced reuse

This document lists what a reader can replay to check the treated behaviour of
traced reuse. It makes no timing or speedup claim: the figures that once backed
such a claim came from unpublished repositories and an unpublished helper, so
they have been removed instead of being replaced by a benchmark.

## What a reader can check

- **Supported profile.** Linux aarch64 or x86_64, a static little-endian ELF
  child without an interpreter, and the pinned Debian strace 6.1 binary for that
  architecture. The README gives both SHA-256 values ("Pinned tracer") and the
  CI workflow downloads and verifies them.
- **Dynamic children are denied, not silently executed twice.** The test
  `traced_dynamic_binary_reports_executable_format` in `tests/cli.rs` runs
  `/usr/bin/true` with `--trust-trace` and asserts `decision: executed`,
  `denial_reason: executable-format`, `trace_state: denied` and
  `cacheable: false`. It is ignored by default; run it with the tracer set:

  ```sh
  KIOKU_STRACE=<pinned-strace> \
    cargo nextest run --run-ignored all -E 'test(traced_dynamic_binary_reports_executable_format)'
  ```

- **Traced test suite.** `tests/false_green_zoo.rs` defines the traced scenarios
  (input changes, symlinks, directory membership, child processes, network,
  clock, randomness, stdin, writable shared mappings, and the rest) and
  `tests/false_green_zoo/cases.md` maps them to the plan. They need Linux
  aarch64, the pinned tracer, ptrace and `cc` for the static helpers; the
  CONTRIBUTING section "Pinned tracer tests" gives the exact setup and this
  command:

  ```sh
  cargo nextest run --run-ignored all --no-fail-fast --test-threads 1
  ```

  These tests show that the listed cases are handled. They are not a proof that
  every influence is captured, and no pass count here is offered as one.
- **Demo.** `docs/demo.sh` performs discovery, confirmation and one traced hit
  with a static helper, then prints `status`. `scripts/test-demo.sh` replays the
  committed capture `docs/demo-session.txt` under an outer `env -i`, checks that
  only the created temporary directory is removed and diffs the output with the
  millisecond values normalised, so the capture itself is reproducible. The
  helper burns CPU on purpose; the demo is not a benchmark of `cargo test`, and
  the durations in the capture vary with the machine.

## x86_64

On an aarch64 host, the x86_64 trace grammar (the legacy `open`, `stat`,
`lstat`, `access`, `readlink` and `execve` syscalls) is covered by unit tests
that parse synthetic traces; no real x86_64 child runs there.

The CI job `traced-tests-x86_64` runs on an x86_64 Linux runner, downloads the
pinned Debian amd64 strace 6.1 package, verifies both SHA-256 values, builds
`tests/false_green_zoo/read_x86_64.S` as a static helper and runs the ignored
test `x86_64_static_helper_is_traced` through `cargo test --test trace`. That is
one traced child in one configuration, not the traced suite that
`traced-tests` runs on aarch64, and not a replay of a real workload. No x86_64
hit-rate or timing claim is made here.

## Not claimed

- Reuse does not speed up `cargo test`, CI caches or ordinary builds: dynamic
  commands are ineligible and execute every time.
- A hit returns only a successful exit status; it replays no output and
  recreates no artifact.
- No measurement published earlier is restated here. Reuse pays only when the
  command's own work exceeds the hit overhead, which depends on the repository
  and the machine; measure it on your own workload.

# Replay measurements for kioku v0.3.0

These measurements were taken with v0.3.0; v0.3.1 changes packaging and
documentation only.

Measured on 2026-11-06 during v0.3 development. This document separates
mechanism checks from acceleration claims. No number here is rounded up, and
no cell claims a speedup that the command did not produce.

## Environment

- Host: Linux 6.8.0-117-generic, aarch64 (Ubuntu container, no privilege, no
  container runtime, no native x86_64).
- Rust: 1.99.1.
- Tracer: Debian arm64 strace 6.1, pinned in a repository-local proof copy;
  SHA-256 `0e69fa123b3d9732645ce1ac8c89819d100f83cdc9e52a80565d9ad8e7d8cf28`.
- Child: static aarch64 helper `/tmp/kio/read` that opens and reads one file.
  It has no interpreter and no dynamic libraries.
- kioku: debug build for the structural replay below; release build for the
  cost measurement in "Hit cost". Both use trace schema `trace-v2`.

## Real-repository replay

Three repositories, 20 first-parent commits each. For every commit, a fresh
`git clone --shared --no-checkout` was created, that commit was checked out,
and the same static helper ran on one file from the repository. Each commit
got three invocations:

1. discovery run;
2. confirmation run;
3. `kioku verify --trust-trace --verify-rate 1`, which asks for a hit and then
   executes the command anyway to detect a false green.

| Repository | HEAD commit | Commits | Third-run hits | False greens | Discovery median (ms) | Confirmation median (ms) |
|---|---|---:|---:|---:|---:|---:|
| private Rust CLI repository | not published | 20 | 20/20 | 0 | 239.0 | 234.5 |
| private Next.js application repository (read-only) | not published | 20 | 20/20 | 0 | 84.0 | 85.0 |
| local fixture repository | not published | 20 | 20/20 | 0 | 83.0 | 83.0 |
| **Total** | | **60** | **60/60** | **0** | | |

Per-invocation hit rate is 60/180 = **33.3 %**, because the first two
invocations of each commit are discovery and confirmation, not hits. The
third-invocation hit rate is **100 % (60/60)** for this fixed workload.

`false green` means the verifier would have skipped the command but the actual
execution disagreed with the recorded status. All 60 verifier runs reported
`false_green: false`; no skipped command had a different exit status.

These runs prove that discovery, confirmation and verification work on three
real repositories and that every eligible commit became a hit on its third
invocation. They do **not** prove acceleration: the helper reads a small file,
so almost all of the 83–239 ms is tracer startup and kioku's own key and store
work. That is why the next section measures the cost separately.

## Hit cost

The command is only one part of a hit. Measured on a fresh clone of the private
Rust CLI repository, release build, 10 discovery runs and 20 hits of the same
helper:

| Path | Median wall time | Mean | p95 |
|---|---:|---:|---:|
| Discovery / execution | 50.9 ms | 51.5 ms | |
| Hit | 48.4 ms | 48.6 ms | 52.0 ms |

The hit is **not faster** than this trivial command: 48.4 ms versus 50.9 ms,
about 1.1× on the median. The cache hit still resolves the repository, reads
the authenticated state, and starts the process; for a helper whose own work
is near zero, those costs dominate.

Consequence for users: reuse can only pay off when the command's own work is
larger than roughly 50 ms. A `cargo test` or a browser suite is far above that
threshold, but such commands are dynamically linked and are currently denied
as `executable-format`. A static checker that takes seconds is the intended
profitable case. The README states this limit instead of promising a speedup
on every command.

### Positive control with real work

To show that the mechanism can accelerate when the command does enough work,
the same static reader was given a 400-million-iteration CPU loop before it
reads the input. This is still a static aarch64 ELF child, so it is eligible.
Measured on the release build:

| Run | Wall time |
|---|---:|
| First learning run | 405.1 ms |
| Second learning run | 476.0 ms |
| Five hits, median | 39.7 ms |

The hits were 31.9, 39.7, 37.3, 44.9 and 48.1 ms. The median hit is about
**10–12× faster** than the learning runs (405/476 ms versus 39.7 ms). This is a
deliberately artificial positive control; it does not turn a real dynamic
`cargo test` into an eligible command. It does show that the ~40–50 ms hit
overhead is small compared with real work, and that the earlier trivial-helper
result was a workload limit, not a broken cache.

## Real workloads and denial reasons

Dynamic commands were measured to confirm that kioku refuses them explicitly
instead of silently falling back:

| Repository | Command | Result | `denial_reason` | False green |
|---|---|---|---|---|
| private Next.js repository | `cargo test --quiet --no-run` | executed, exit 0 | `executable-format` | false |
| private Next.js repository | `pnpm exec vitest --version` | executed, exit 0 | `executable-format` | false |
| local fixture repository | `node -e 'process.exit(0)'` | executed, exit 0 | `executable-format` | false |

A full `pnpm exec vitest run` in the private Next.js repository was started with
a 180 s limit. It did not finish inside the limit and was stopped; its result is
not used as a measurement. The short `vitest --version` command above is the
recorded dynamic-workload check. No hit is claimed for either command.

## x86_64 status

The x86_64 parser, denial codes and static-ELF check are unit-tested on the
aarch64 host with synthetic traces and a cross-compiled x86_64 helper
(`tests/false_green_zoo/read_x86_64.S`). An end-to-end x86_64 replay was **not
executed locally**:

- no Docker, podman, nerdctl or other container runtime is available;
- the process has no root and user namespaces are disabled;
- the host has no native x86_64 CPU;
- `qemu-user` cannot emulate `ptrace` for the guest; the measurement showed
  `ptrace not implemented in qemu-user for the guest`.

The CI job `traced-tests-x86_64` downloads the pinned Debian amd64 strace 6.1
package, verifies both the `.deb` and the binary SHA-256, builds the static
x86_64 helper and runs the trace unit tests plus the ignored helper test. It
does not run the three-repository replay. Therefore this document makes no
x86_64 hit-rate or timing claim; the first real x86_64 end-to-end numbers must
come from that CI job or a native x86_64 host.

## Reproduce

The structural replay used the debug binary and this shape:

```sh
git clone --shared --no-checkout <repo> <tmp>
git -C <tmp> checkout <first-parent-commit>
KIOKU_STRACE=<pinned-strace> kioku run --json --trust-trace -- /tmp/kio/read <file>
KIOKU_STRACE=<pinned-strace> kioku run --json --trust-trace -- /tmp/kio/read <file>
KIOKU_STRACE=<pinned-strace> kioku verify --json --trust-trace --verify-rate 1 -- /tmp/kio/read <file>
```

The result is read from stderr JSON: `decision`, `trace_state`, `cacheable`,
`denial_reason`, `false_green` and `duration_ms`. The cost measurement wraps
the release binary with `time.perf_counter()` and reports wall time, not the
`duration_ms` field (a hit reports `0` by design because no child ran during
the decision).

## Limits

- One host, one tracer version, one helper, aarch64 only for end-to-end data.
- The three repositories are small to medium; a very large repository may
  increase key and state costs.
- The helper is deliberately trivial, so the numbers above measure overhead,
  not the value of skipping real work.
- No claim is made about macOS or Windows: they execute only, and their CI
  results are separate.

# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-11-18

The validated scope of this release is **Rust test selection with Cargo and
nextest**, its CLI and its journal. JS/TS selection, shared Cargo or JS build
caches and the D1 decision model are outside it. The exit-status reuse feature
from v0.3 is unchanged.

### Added

- `kioku test`: run only the tests that depend on what changed. It compares the
  working tree with the last known green state of the base branch, walks a
  file → file dependency graph, and runs the tests that reach a changed file.
  Every test that is not run is journaled with its reason. Exit codes: `0`
  partial green, `1` red, `3` incomplete or selection error (the full suite was
  run), `5` full green. A partial run is never reported as a full green.
- `kioku select base`: inventory a clean, green base commit — tests, JUnit
  results and the file → file graph — into a sealed store.
- `kioku select learn`: add the edges the static graph misses, from real
  per-test coverage (`--coverage-json FILE`), or collect that coverage with
  `--run`.
- `kioku select plan`: inspect a selection plan without running anything.
- Selection journal (`kioku/select/v1`): every run and skip decision with its
  reason, so a wrong skip is auditable rather than invisible.
- Sealed store: base, graph and learned files carry content digests. A tampered,
  truncated or foreign store is refused and the run widens to the full suite.
- `--d1 FILE`: an optional JSON proposal (`{"add": [...], "skip": [...]}`) that
  can only **add** tests. Proposed skips are ignored. This is not a trained
  model.
- `--native-exit-code`: return the last nextest nonzero code. Both full and
  partial green return `0`, so inspect `status` and `scope` instead.
- `--runner vitest`: a conservative **full-suite fallback**, not a selection. It
  runs the whole Vitest suite and still returns `3`, because no base preparation
  exists for it.
- `--journal PATH`, `--store DIR`, `--base REF`, `--repo DIR`, `-E/--filter` and
  `--features` for the selection commands.
- `docs/AGENTS-KIOKU.md`: the agent-facing contract for the selection commands.

### Changed

- The CLI now has eight commands: `run`, `verify`, `explain`, `status`, `clean`,
  `quarantine`, `test`, `select`. The first six behave as in v0.3.1.
- The former cooperative-CI daemon, `kioku daemon`, `kioku ci` and the
  `legacy-daemon` feature are removed.
- `Cargo.toml` declares `0.4.0` and keeps `publish = false`.

### Fixed

- `kioku test --runner vitest`: a full-suite fallback killed by a signal (OOM,
  `kill`, pool death) now exits with `128 + signal` (143 for SIGTERM) on Unix
  instead of `3` (incomplete), so it reads as a failure, not as an incomplete
  success.
- The Linux-only descriptor-anchored capture module no longer fails the
  `clippy::pedantic` gate on macOS and Windows, where only its `capture` stub is
  reachable. The allowance is scoped to non-Linux builds; the Linux build is
  unchanged.
- `tests/v2_real_libtest.rs` resolves `rustc` from `PATH` instead of a fixed
  absolute path, so the suite runs outside the machine it was written on.
- `scripts/lot9c_bench.py` takes its heavy-build lock from `KIOKU_HEAVY_LOCK`
  (default: `target/.heavy.lock`) and its Cargo bin directory from `CARGO_HOME`,
  instead of hard-coded absolute paths.

### Limits

- **No timing measurement is published for the selection path.** The release
  campaign recorded one observation on one repository: a base of 853 test
  identities, one new test added, 70 run decisions and 784 skip decisions. That
  is a scenario, not a benchmark, and no speedup is claimed.
- **The base graph is the only graph v0.4.0 builds.** The design argument is
  written at the top of `src/select/mod.rs`. A later investigation found a real
  repository where an edge present only in the working tree caused two tests to
  be skipped; the design was changed after this release to build and union a
  HEAD graph. v0.4.0 does not contain that fix. Keep the native suite as the
  gate.
- **`--coverage-json` is the caller's responsibility.** Synthesised coverage
  produces a wrong graph and therefore wrong skips.
- **No JS/TS selection and no shared build caches.** `--runner vitest` is a
  fallback.
- **Traced reuse is unchanged from v0.3**: Linux aarch64 and x86_64 only, static
  little-endian ELF children only, ~40–50 ms of overhead per hit, and no
  end-to-end x86_64 replay was run.

## [0.3.1] - 2026-11-07

### Changed

- Publish a macOS Intel (`darwin-x86_64`) archive next to the Apple silicon
  one; both macOS archives run execute-only. The install table now lists the
  exact asset name for every platform and architecture.
- Release archives contain every document the README links: `CONTRIBUTING.md`,
  `docs/demo-session.txt`, the banners and the French guides. Packaging fails
  when a relative link in the archive has no target.
- Document the pinned strace 6.1 download, package hash and binary hash used by
  CI, and point the `tracer-untrusted` denial at that section.
- Build release binaries stripped.

### Fixed

- Replace the "assets are planned" note with the list of published assets.
- Regenerate `docs/demo-session.txt` from v0.3.1 with the current `status` output.
- Remove a private repository name and internal process vocabulary from
  `docs/REPLAY-V03.md`.

## [0.3.0] - 2026-11-06

### Added

- Trace Linux x86_64 static ELF children, including the legacy
  `open/stat/lstat/access/readlink/execve` syscall grammar.
- Resolve relative paths, `..`, absolute symlinks and bare PATH names before
  tracing, and include the resolved executable in the command key so changing
  PATH cannot reuse another program's status.
- Emit explicit `denial_reason` codes and human messages for every refusal;
  `kioku explain` now prints key, mode, dependencies and the denial.
- Publish a measured replay report in `docs/REPLAY-V03.md`, including three
  real repositories, zero false greens, hit cost and x86_64 limits.

### Changed

- Release archives contain only the binary and the listed user documents;
  packaging fails when one is missing.
- Document that only static ELF children are traced and that a release-build
  hit costs about 48 ms, so reuse pays only for commands slower than that.

### Fixed

- Read the child status and output without misreading a partial pipe, and deny
  dynamic executables as `executable-format` instead of reporting an unknown
  child status or silently retrying.
- Report `status` and `clean` outside a Git repository as an empty success
  instead of an error.
- Stop repeating the `git` prefix in Git error messages (`git git ...`).
- Make the FIFO cleanup test independent of unrelated temporary directories.

## [0.2.3] - 2026-11-06

### Changed

- Publish platform archives and checksums with GitHub Releases.
- Add English and French guides and agent-oriented documentation.

### Fixed

- Fix the rustup proxy test to compare against the canonical temporary path on macOS.

## [0.2.2] - 2026-11-06

### Fixed

- Probe Go with `go version` and isolate tool probes from host installations.
- Resolve test executables portably instead of relying on fixed host paths.
- Restrict Unix policy tests to Unix and cover execute-only behavior on Windows.
- Canonicalize the temporary path prefix in 40/41-symlink boundary tests.
- Validate invalid globs before accessing the repository.

## [0.2.1] - 2026-11-06

### Changed

- Document CLI output channels, UTF-8 arguments, traced-hit overhead, and tracer test setup.

### Fixed

- Make tracer-dependent tests explicitly ignored by default and correct platform lint configuration.
- Fix Windows ZIP packaging fallback.

## [0.2.0] - 2026-11-06

### Changed

- v0.2 is designed as an exit-status-only cache: a hit does not replay output
  or restore artifacts. Default execution does not reuse results; traced reuse
  requires explicit opt-in and caller assumptions. The implementation and
  release evidence are specific to the tested platform and prerequisites.

## [0.1.0] - 2026-11-05

### Added

- `kioku run`: run a command, or skip it when the same content already passed
  it. Only successes are memorized.
- `kioku verify`: shadow mode. The command always runs, kioku logs
  `would_skip` and `actual_exit` in `<git-dir>/kioku/shadow.jsonl`, and reports
  a false green as an error.
- `kioku explain`: hit or miss, with the closest entry for the same command.
- `kioku status`: entries, store size, accumulated savings.
- `kioku clean`: drop expired entries, or all of them.
- Content addressed key: kioku version, exact argv, relative working
  directory, declared environment variables, lockfile digests, tool versions,
  input digests.
- Two fingerprint modes: `bytes` (the default for all commands) and `ast`
  (explicit opt-in under a caller-asserted trivia-insensitivity contract).
- Store under the real git directory, atomic writes, file lock, `KIOKU_DIR`
  override, `KIOKU_DISABLE=1` escape hatch.
- `--json` (schema `kioku/v1`), `--quiet`, documented exit codes.

[0.4.0]: https://github.com/Rushford444/kioku/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/Rushford444/kioku/releases/tag/v0.3.1
[0.3.0]: https://github.com/Rushford444/kioku/compare/v0.2.3...v0.3.0
[0.2.3]: https://github.com/Rushford444/kioku/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/Rushford444/kioku/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/Rushford444/kioku/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Rushford444/kioku/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Rushford444/kioku/releases/tag/v0.1.0

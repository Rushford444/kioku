# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.0] - 2026-11-08

### Added

- First public release. kioku runs a command and can reuse its successful exit
  status when an explicitly trusted trace shows that the observed inputs are
  unchanged. The default executes every time; traced reuse is an explicit
  opt-in. A hit returns only a successful exit status, never output or
  artifacts.

- Commands: `run`, `verify` (shadow comparison), `explain`, `status`, `clean`
  and `quarantine reset`, with human output, `kioku/v1` JSON and documented
  exit codes.

- Content-addressed keys over the exact argv, the relative working directory,
  the complete initial environment digest and the files the child observes.
  The store lives under the real Git directory with atomic writes and a file
  lock; `KIOKU_DIR` overrides its location and `KIOKU_DISABLE=1` executes
  without reuse. A store inside the work tree turns the cache off: kioku runs
  the command, learns nothing, reuses nothing and writes nothing there.

- Opt-in traced reuse on Linux aarch64 and x86_64 with the pinned Debian
  strace 6.1 binary and a static little-endian ELF child without an
  interpreter. Scripts, dynamically linked commands and unsupported
  observations execute with a typed denial reason instead of a silent
  fallback.

- English and French guides, a replayable demo, and the traced-reuse
  assumptions the caller must accept before opting in.

### Changed

- The `kioku/v1` events name the reason a cache is off. `run`, `verify` and
  `explain` report `reason` and `denial_reason` `store-in-work-tree` when the
  store lives inside the work tree; `status` and `clean` report `reason`
  (`null` otherwise) and say the same in their human output. Outside a Git
  repository, `--trust-trace` now reports `repository-unavailable` instead of
  `trace-unavailable`: the repository is resolved before the tracer.

- An incoherent `measured_hits` count, more measured hits than hits, is
  reported as unknown (`null`, "measurement unknown") instead of a smaller
  invented number. A status without any hit reads "0 hits (none)".

- The library API gains `Status::denial_reason` and
  `OutcomeClean::denial_reason`, which carry that reason.

[0.1.0]: https://github.com/Rushford444/kioku/releases/tag/v0.1.0

# Contributing agents

Kioku is a Rust CLI and library. Read [CONTRIBUTING.md](CONTRIBUTING.md),
[SECURITY.md](SECURITY.md), and the trace assumptions in [README.md](README.md)
before changing behavior. Keep trace reuse opt-in and fail closed when observations
are unsupported or incomplete. Do not expose secrets or add network access.

Build and check with Rust stable (MSRV 1.89):

```sh
cargo build --locked
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings -W clippy::pedantic
cargo nextest run --all-features --no-fail-fast
cargo test --doc --all-features
cargo deny check
```

Add focused regression tests for behavior changes. Do not claim traced coverage
without its Linux aarch64 or x86_64 prerequisites. Avoid panics on user input,
unsafe Rust, and printing environment values. Preserve JSON `kioku/v1`, stream and exit-code
contracts or document and test any intentional change.

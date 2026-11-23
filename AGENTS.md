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
cargo clippy --all-targets -- -D warnings -W clippy::pedantic
cargo nextest run --all-features --no-fail-fast
cargo test --doc --all-features
cargo deny check
```

The cooperative-CI daemon and its client commands have been removed. Tests that must
not read outside the repository can name an operator stop file in
`KIOKU_TEST_OPERATOR_STOP_FILE`; unset means no check.

Add focused regression tests for behavior changes. Do not claim traced coverage
without its Linux/aarch64 prerequisites. Avoid panics on user input, unsafe Rust,
and printing environment values. Preserve JSON `kioku/v1`, stream and exit-code
contracts or document and test any intentional change.

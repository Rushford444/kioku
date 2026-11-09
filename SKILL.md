---
name: kioku
description: Use when running a command and assessing whether an exit-status cache is appropriate.
---

# kioku

The default behavior executes the command. Traced reuse is an explicit
opt-in with caller assumptions; consult the installed command's help and
[safety assumptions](README.md#traced-reuse-assumptions). It is Linux-specific and fails closed when
tracing or an observed operation is unsupported. Other platforms execute.

Traced reuse always includes the complete initial environment and the files the
child observes; no option narrows them.

A hit skips the command and reuses only its successful exit status. It does
not replay stdout or stderr and does not create build artifacts. Do not rely on
hits for commands whose outputs or side effects are needed. Stable-input and
atime assumptions are the caller's responsibility.

```sh
kioku run -- cargo test
kioku verify -- cargo test
kioku explain -- cargo test
kioku status
kioku clean --older-than 7d
```

`verify` executes the child for comparison. `KIOKU_DISABLE=1` requests
execution without reuse. The local store may contain paths and command
arguments; do not pass credentials in argv. See [SECURITY.md](SECURITY.md).

Child exit codes are propagated, including 1, 2, 3, 126 and 127. A process
termination without an exit code maps to 1. Kioku also uses 2 for usage errors,
3 for internal errors, 126 for non-executable commands and 127 for commands not
found. These numbers alone do not distinguish a kioku error from a child exit.
Use `--json` for machine-readable `kioku/v1` output. For `run` and `verify`,
kioku events go to stderr and child stdout remains separate; other commands
write JSON to stdout and errors to stderr. Check `--help` for exact syntax.

The Cargo package is `kdw-kioku`; it is not published on crates.io. Git credentials
must be preconfigured while the repository is private. Install noninteractively with `GIT_TERMINAL_PROMPT=0 cargo install --git https://github.com/Rushford444/kioku --tag v0.1.0 --locked`.
Use `CI=true kioku --json run -- cargo test` in CI; this remains execute-only by default.

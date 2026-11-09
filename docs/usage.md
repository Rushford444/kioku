# Usage

Run kioku from a Git repository. The default executes the child every time;
only successful exit statuses can be reused and only after explicit opt-in.
A hit does not replay output or restore files.

```sh
kioku run -- cargo test
kioku verify -- cargo test
kioku explain -- cargo test
kioku status
kioku clean --older-than 7d
```

`verify` always runs and compares the result to the cache decision. `explain`
reports the decision without running the child. `clean --all` removes all local
entries; `quarantine reset -- COMMAND` clears the exact command's quarantine
and restarts learning. `run --require-output` forces execution. Use `--quiet`
to reduce human messages.

Opt in only if you accept every [traced-reuse assumption](../README.md#traced-reuse-assumptions):

```sh
kioku run --trust-trace --verify-rate 1 -- /absolute/path/to/deterministic-check
```

`--verify-rate` is between 0 and 1; 1 checks every eligible hit. Tracing is
restricted to Linux aarch64 and x86_64, the pinned Debian strace for that
architecture and static little-endian ELF children without an interpreter.
Dynamic commands execute with `denial_reason: "executable-format"`, never a
silent fallback. Traced reuse always includes the complete initial environment;
no extra option is needed. See [configuration](configuration.md) for modes and
variables. Do not expect caching for ordinary dynamically linked commands.

## CI and agents

No prompt is required. In CI, run the command directly or use execute-only mode;
do not rely on a hit to create artifacts. Example:

```sh
kioku run -- cargo test --locked
```

Instructions for `AGENTS.md` or `CLAUDE.md`:

```text
Use kioku run -- COMMAND for checks whose exit status is sufficient.
The default executes. Never depend on a cache hit for output or files.
Opt in to tracing only after checking the documented assumptions.
```

## JSON and exit codes

```sh
kioku --json status
kioku --json run -- cargo test
```

Kioku's JSON schema is `kioku/v1`. For `run` and `verify`, JSON goes to stderr;
the child keeps its inherited stdout and stderr. Redirecting stderr does not
separate child stderr from kioku's JSON. Other commands emit JSON on stdout;
errors go to stderr. Inspect `cacheable` and `denial_reason`, not merely
`trace_state`. Kioku propagates the child exit code, including 1, 2, 3, 126
and 127. A child termination without a code maps to 1. Kioku also uses 2 for
usage errors, 3 for internal errors, 126 for launch failure and 127 for a
command not found. These values overlap: the code alone does not identify its
origin. Arguments must be UTF-8.

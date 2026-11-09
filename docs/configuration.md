# Configuration reference

`kioku.toml` is read from the Git repository root. It is optional. It selects a
fingerprint mode for an exact command argument vector, not a shell expression:

```toml
# Each argv item is one argument, including the executable.
[[commands]]
argv = ["cargo", "test", "--locked"]
mode = "bytes" # "bytes" or "ast"

[[commands]]
argv = ["cargo", "clippy"]
mode = "ast"
```

Arguments and order must match exactly. Unknown fields, invalid modes and
duplicate matching commands are errors. `--mode` can select a mode on the
command line. Traced reuse always uses bytes even if AST is requested; AST
analysis by `explain` is hypothetical. Configuration alone never enables reuse.
The traced manifest lists the files the child observes, and the complete initial
environment is included as a digest. No option narrows either. See
[usage](usage.md) and the [traced-reuse contract](../README.md#traced-reuse-assumptions).

The AST diagnostic fingerprints every Git-visible file for `explain`, and before
and after `run` while caching is enabled. It reads regular files in that list,
including untracked files, but not ignored ones; a large tree costs I/O, and a
regular file held under a write lease or on a slow filesystem can block it.
Ignore expensive or blocking files when they do not affect the command.
`KIOKU_DISABLE=1` skips the diagnostic for `run`; `explain` still computes it.

Environment variables:

- `KIOKU_DIR`: override the local store location. Protect it as sensitive. A
  store inside the work tree turns the cache off: kioku runs the command,
  learns nothing, reuses nothing and writes nothing there. A store left there
  by an earlier version is not removed by `kioku clean`.
- `KIOKU_DISABLE=1`: execute without reuse.
- `KIOKU_STRACE`: absolute path to the approved Linux aarch64 or x86_64 tracer.
- `NO_COLOR`: disable color when applicable.

No configuration can make tracing safe for commands whose output or side effects
are required. Use `--require-output` or run them directly.

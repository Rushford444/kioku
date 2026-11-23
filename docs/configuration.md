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
`--inputs` does not restrict the traced manifest; `--env` does not restrict
traced environment identity. The complete initial environment is included as a
digest. See [usage](usage.md) and the [traced-reuse contract](../README.md#traced-reuse-assumptions).

Environment variables:

- `KIOKU_DIR`: override the local store location. Protect it as sensitive.
- `KIOKU_DISABLE=1`: execute without reuse.
- `KIOKU_STRACE`: absolute path to the approved Linux aarch64 or x86_64 tracer.

No configuration can make tracing safe for commands whose output or side effects
are required. Use `--require-output` or run them directly.

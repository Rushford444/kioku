# Security

## Reporting a vulnerability

Open a private security advisory or contact the maintainer directly. Include
the version, platform, reproduction steps and impact. Do not publish a report
before a fix is available.

## Trust boundary

A cached result is an exit status only. Hits do not restore output or files.
The default is execute-only; traced reuse requires explicit caller opt-in
and the [traced-reuse assumptions](README.md#traced-reuse-assumptions). Unknown observations, unavailable
tracing or incomplete evidence must prevent reuse. Linux tracing is not a
promise of traced caching on other operating systems. Commands themselves can
access the network, alter files, or expose data; kioku is not a sandbox.

Treat the store as sensitive. Trace-v2 identity uses digests for command
arguments and environment values; its manifest contains paths and digests.
Entries written by earlier builds may contain command arguments and paths.
Never put credentials in command arguments. The tracer must
never emit or persist exec environment values or raw trace records. Diagnostic
outputs must not contain secrets.

## Synthetic sentinel test

Before trusting a tracer implementation, run its security tests with an empty,
explicitly constructed environment containing a synthetic sentinel. Check
that the sentinel is absent from every persisted artifact and diagnostic,
without printing the sentinel or raw trace. A passing sentinel test covers only
the exercised paths; it is not proof that all possible secret-bearing syscall
arguments are safe. The supported tracer and its SHA-256 are identified in the [README](README.md#traced-reuse-assumptions).

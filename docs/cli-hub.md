# CLI-Hub entry

| Field | Value |
|---|---|
| Name | `kioku` |
| Description | Local cache of successful command exit statuses, with traced reuse opt-in |
| Install | `git clone https://github.com/Rushford444/kioku.git && cd kioku && cargo install --path . --locked` |
| Category | Developer tools / build and test |
| License | MIT |

The default executes commands. Traced reuse requires explicit opt-in and
caller assumptions; unsupported observations or platforms must execute instead.
Tracing is Linux-specific. A hit reuses only the exit status, not output or
artifacts. Consult `kioku --help`, and the README before use.

Child exit codes are propagated, including 1, 2, 3, 126 and 127. On Unix a
child killed by a signal is reported as 128 plus the signal number; termination
without a code maps to 1 only on a platform that reports none. Kioku also uses
2 for usage errors, 3 for internal errors, 126 for launch failure and 127 for a
missing command. These values therefore do not uniquely identify their origin.

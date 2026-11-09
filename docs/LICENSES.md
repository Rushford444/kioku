# Dependency licenses

> **Historical evidence only:** these results predate the current uncommitted code and dependency changes. They are not evidence for the current build. Remeasure after code freeze.

Measured with `cargo deny check` on the locked dependency graph
(105 crates). The licence of a crate can be a choice
(`OR`) or a combination (`AND`): cargo-deny accepts a crate when one of the
allowed branches applies, which is why a `MIT OR Apache-2.0` crate is listed
under its whole expression.

| licence expression | crates |
|---|---|
| `MIT OR Apache-2.0` | 70 |
| `MIT` | 9 |
| `Apache-2.0 OR MIT` | 7 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 4 |
| `Unlicense OR MIT` | 3 |
| `MIT/Apache-2.0` | 3 |
| `MIT OR Apache-2.0 OR LGPL-2.1-or-later` | 2 |
| `BSD-2-Clause OR Apache-2.0 OR MIT` | 2 |
| `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception` | 1 |
| `CC0-1.0 OR MIT-0 OR Apache-2.0` | 1 |
| `Apache-2.0 / MIT` | 1 |
| `Zlib` | 1 |
| `(MIT OR Apache-2.0) AND Unicode-3.0` | 1 |

The allow list lives in `deny.toml`. Only permissive licences are allowed;
`cargo deny check` passes with no warning:

```text
advisories ok, bans ok, licenses ok, sources ok
```

## Crates whose expression is not a single permissive licence

- `(MIT OR Apache-2.0) AND Unicode-3.0`
  - unicode-ident 1.0.27
- `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT`
  - linux-raw-sys 0.12.1
  - rustix 1.1.6
  - wasip2 1.0.1+wasi-0.2.4
  - wit-bindgen 0.46.0
- `BSD-2-Clause OR Apache-2.0 OR MIT`
  - zerocopy 0.8.63
  - zerocopy-derive 0.8.63
- `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception`
  - blake3 1.8.7
- `CC0-1.0 OR MIT-0 OR Apache-2.0`
  - constant_time_eq 0.4.2
- `MIT OR Apache-2.0 OR LGPL-2.1-or-later`
  - r-efi 5.3.0
  - r-efi 6.0.0
- `Unlicense OR MIT`
  - aho-corasick 1.1.5
  - globset 0.4.19
  - memchr 2.8.3
- `Zlib`
  - foldhash 0.2.0

<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Contributing to eon-office-core

The organisation-wide guide —
[`.github/CONTRIBUTING.md`](https://github.com/EON-Extensible-Open-Network/.github/blob/main/CONTRIBUTING.md)
— applies: the Code of Conduct, the DCO (`git commit -s`, no CLA), Conventional Commits,
trunk-based branches. This file adds what is particular to the core.

## Before writing code

- **Does it belong in the core?** If it knows what a note, a cell, a meeting or an invoice
  is, it does not (madde 43c). The core gets a feature when two or more applications need
  the same mechanism and neither should own it.
- **Does it change the protocol?** Then it starts as a pull request against
  [`eon-office-spec`](https://github.com/EON-Extensible-Open-Network/eon-office-spec), and
  this repository follows the spec — not the other way round.
- **Does it add a dependency?** Say why in the pull request and in a comment beside the
  line in `Cargo.toml`. A dependency is a supply-chain commitment (madde 37). The core
  has five today, on purpose.

## Licence of contributions

By contributing you license your work under `GPL-3.0-or-later` **with** the EON Module ABI
Exception 1.0 (`LICENSE-EXCEPTION.md`). If you are not willing to grant the exception, say
so in the pull request; it will not be merged here, and that is not a judgement of the work.

## Checks that must pass

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI additionally runs `cargo deny check`, `reuse lint`, the DCO check, and the invariants
job: no telemetry, no GUI dependency, no listening address that is not loopback.

## Tests

`tests/protocol.rs` starts the real binary and talks to it over a socket **without** the
SDK. Keep it that way: those tests are the evidence that the wire format, not a Rust
library, is the contract. A new method gets a test there, including the refusal path —
what happens without the capability, without the owner, with bad input.

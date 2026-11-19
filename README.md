<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# eon-office-core

**EON Office's shared foundation.** A small local service that every EON Office application
connects to, so that EON Notes, EON Writer, EON Tasks and anything a third party writes can
share one identity, one notification list, one search, one set of preferences and one
permission system, without depending on each other's code.

It is not an application: it has no window and never will. It is not a monolith either.
Each application is its own program in its own repository, and it talks to the core over a
small, documented, versioned protocol:
[`eon-office-spec/docs/protocol.md`](https://github.com/EON-Extensible-Open-Network/eon-office-spec/blob/main/docs/protocol.md).

> **Status: `0.1.0`, protocol `v0`, unstable.** Every EON contract starts at `v0`, and `v0`
> may break without a migration path until `v1` (plan, "Kapsam notu"). Build on it to learn,
> not to ship. The decision to start EON Office at all is
> [madde 43](https://github.com/EON-Extensible-Open-Network/eon-docs/blob/main/plan/eon-plan.md)
> of the project plan, with what it costs.

## What it does

| | |
|---|---|
| **Applications** | Install, upgrade (never downgrade), uninstall. Launch, stop, and supervise: a crashing application is restarted up to three times a minute and then left alone. |
| **Capabilities** | An application asks in its manifest; the owner grants, one by one, and can revoke at any time. Checked on every call against the registry as it is *now*. |
| **Identity and directory** | The owner; people; organisations, workspaces and teams; roles held *in a scope* (guest, member, admin, owner), reaching downwards; one `access.check` every application uses. |
| **Storage** | Each application's own JSON document store, with revisions that refuse a stale write. |
| **Sync** | Offline-first. Export changes after a hybrid-logical-clock stamp; import them elsewhere; the later edit wins and the losing one is **kept**, not discarded. Transport-free: a file, a USB stick or a server can carry it. |
| **Events** | Applications publish the events their manifest declares; others subscribe if granted. A slow subscriber loses its own events, never blocks anyone. |
| **Calls** | An application calls methods another application *provides*, through the core. If the provider is not running, or dies mid-call, the caller gets `app_unavailable` at once. |
| **Notifications** | One list for the suite, rate-limited per application. |
| **Search** | One index. Turkish-aware folding (`İ ı I i`, madde 40). An application sees its own entries and what others chose to share. |
| **Jobs** | Wake-ups at a time or every so often. Missed runs coalesce into one. |
| **Settings, preferences, themes** | Per-application settings; the person's language, theme, motion, text size and contrast, pushed to every application on change; declarative themes refused below WCAG AA. |
| **Audit** | An append-only hash chain of every change of authority and every refusal — and never of what anyone read or wrote. |
| **Backup and restore** | Online backup while running; restore that keeps the previous database beside the new one; migrations that refuse a database from a newer core. |

What it deliberately does **not** do, and why, is in [`docs/architecture.md`](docs/architecture.md).

## Run it

```
cargo run --release -- serve
```

It listens on a free loopback port and writes `runtime/endpoint.json` into its home
directory: `EON_OFFICE_HOME`, else `%LOCALAPPDATA%\EON\Office` on Windows,
`~/Library/Application Support/EON Office` on macOS, `$XDG_DATA_HOME/eon-office` elsewhere.
An application finds the core there. The owner's token is `runtime/owner.token`; each
installed application's token is `apps/<id>/token`.

```
eon-office-core serve   [--home DIR] [--listen 127.0.0.1:0]
eon-office-core backup  --out FILE [--home DIR]
eon-office-core restore --from FILE [--home DIR]     # the core must be stopped
eon-office-core verify-audit [--home DIR]
eon-office-core home | version
```

It refuses to listen anywhere but loopback. A network service for many people is a
different program with different obligations (madde 30), and will be `eon-office-server`.

## Write an application for it

Use [`eon-office-sdk`](https://github.com/EON-Extensible-Open-Network/eon-office-sdk) (MIT),
whose `hello` example is the reference integration. Or speak the protocol directly: it is
JSON-RPC 2.0, one object per line, over TCP — this repository's integration tests do exactly
that with nothing but a socket.

## Develop

Toolchain pinned in `rust-toolchain.toml`.

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test            # unit tests, then tests/protocol.rs against the real binary
cargo deny check      # licences and advisories; runs in CI
```

`unwrap`, `expect` and `panic!` are denied: the core is shared by every application on the
machine, and a panic in it is all of them losing their storage at once.

## Licence

`GPL-3.0-or-later` with the [EON Module ABI Exception](LICENSE-EXCEPTION.md). Applications
that only talk to the core over the published protocol are separate programs and may be
licensed however their authors choose; the exception is there for the day modules run
inside the core, and is granted now because the DCO makes adding it later impossible.
Contributions: [`CONTRIBUTING.md`](CONTRIBUTING.md). Security: [`SECURITY.md`](SECURITY.md).

<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# eon-office-core — architecture

The contract is `eon-office-spec`: the protocol, the manifest schema, the capability list.
This document is about *how this implementation keeps that contract*, and the decisions a
reader of the code would otherwise have to reverse-engineer.

## Shape

```
  EON Notes        EON Tasks        a third party's app        the owner's shell
     │ (process)       │ (process)          │ (process)               │
     └──────────── TCP loopback, JSON-RPC 2.0, one object per line ───┘
                                   │
                          ┌────────┴────────┐
                          │ eon-office-core │  one process per person per machine
                          │                 │
                          │ server  ── sessions, method table, events, calls,
                          │            supervisor, job ticker
                          │ apps · directory · storage · search · notify ·
                          │ schedule · settings · audit · db · hlc · text
                          └────────┬────────┘
                                   │
                            core.sqlite (WAL)
```

## Decisions

**A service, not a library.** EON Stream's core is a library linked into one program. EON
Office's foundation is shared by many programs at once, and *a failure in one application
must not bring down the ecosystem*. Linking a shared core into every application would put
every application's memory in the same blast radius as the data. A separate process with a
socket is the cheapest boundary that actually holds: a crashed application closes a
connection, nothing more.

**Loopback TCP rather than named pipes / Unix sockets.** One code path on Windows, macOS and
Linux in the standard library, no dependency. The cost is that any local process can open
the port; authentication is therefore by token (below), and the threat model is honest
about what that buys.

**Threads, not async.** Two threads per connection (reader, writer) and a handful of
service threads. The core serves one person's applications, not ten thousand clients; a
runtime would be a large dependency for no measured benefit. `MAX_SESSIONS` caps it.

**One SQLite file, one connection behind a mutex.** Every write is small. Serialising
through one connection removes a class of locking bugs, and WAL lets `backup` and
`verify-audit` read concurrently from a second connection.

**Authority is read fresh on every call.** `Ctx::require` loads the application's grants
from the database each time. Revocation is therefore immediate, at the price of one
indexed read per call — well below what the socket costs.

**Bounded queues everywhere a peer is involved.** Each connection's outgoing queue holds
1,024 messages. Events and jobs use `try_send`: if an application stops reading, *it*
loses events (counted in `dropped`), and the publisher never waits.

**Calls run on their own thread.** `apps.call` waits for the provider on a fresh thread so
the caller's connection keeps reading. Two applications calling each other therefore
cannot deadlock, and an `invoke` arriving at an application that is itself waiting on a
call is delivered.

**Process supervision by polling `try_wait`.** The supervisor needs to be able to kill a
child it is also waiting on; `std::process::Child::wait` consumes the handle, so a 100 ms
`try_wait` loop under a mutex is the std-only way to do both. Restart policy: `never`
(default) or `on-failure`, at most three restarts in sixty seconds with 1 s / 2 s / 4 s
back-off, then `failed`.

**Hybrid logical clocks for sync, last-writer-wins at document granularity, losers kept.**
Field-level CRDT merging would be better for some applications and wrong for others (a
spreadsheet cell is not a paragraph). The core does the part every application needs —
a total order everyone agrees on and no silent loss — and leaves semantic merging to the
application, which gets the losing version from `storage.conflicts`.

## What the core does not do, on purpose

| Not here | Where it belongs |
|---|---|
| Anything about notes, cells, mail, events, tasks | The application (madde 43c) |
| Listening on a network interface; serving many people | `eon-office-server`, behind madde 30's gate |
| Running application code (plugins, scripts) | Not before madde 4's sandbox design exists |
| Encrypting the database itself | The operating system's disk encryption, for now — see `SECURITY.md` |
| Telemetry of any kind | Nowhere (madde 36) |
| A user interface | Applications; the core never takes a GUI dependency |

## Data layout

```
<home>/
  core.sqlite            all state (schema in src/db.rs, version in PRAGMA user_version)
  runtime/endpoint.json  address, pid, api, version, node -- present while running
  runtime/owner.token    the owner's token (0600 on Unix)
  apps/<id>/token        each application's token, written on first install
```

## Error handling

Every failure is an `Error { kind, message, detail }`. `kind` maps to a JSON-RPC number and a
stable string code (`forbidden`, `conflict`, `app_unavailable`…). Database and I/O errors
are logged to stderr with their detail and reported to the application only as `internal`,
because their text can name other applications' data.

`clippy::unwrap_used`, `expect_used` and `panic` are denied. Mutex poisoning is tolerated
(`lock()`), so a panic in one connection's thread — which the lints make unlikely —
does not wedge every other connection.

## Versioning

- **Protocol** (`api`): an integer, `0` today. A client may send `api` in `session.hello`;
  a mismatch is `incompatible`. Manifests state `api.min..=api.max`.
- **Manifest format** (`manifest`): `0`.
- **Database schema**: ordered migrations; a newer database is refused.
- **This crate**: semver, `0.x` while the protocol is `v0`.

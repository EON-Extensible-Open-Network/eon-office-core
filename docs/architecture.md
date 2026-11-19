<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# eon-office-core — architecture

The contract is `eon-office-spec`: the shared-store layout, the manifest schema, the
capability list, the link format. This document is about *how this implementation keeps
that contract*, and the decisions a reader of the code would otherwise reverse-engineer.

## Shape

```
  EON Notes (process)     EON Tasks (process)     EON Launcher (optional hub)
  └ eon-office-core       └ eon-office-core       └ eon-office-core
        │      │                 │      │                  │
        │      └──── <home>/shared.sqlite ────────────────┘
        │                        │        registry · prefs · themes · search ·
        │                        │        activity · public · audit
  apps/org.eon.office.notes/     apps/org.eon.office.tasks/
        data.sqlite                    data.sqlite
```

No process is required to be running for any other to work. An application that finds
nothing else installed still has everything it needs.

## Decisions

**A library, not a service.** `0.1.0` was a service every application connected to over
loopback JSON-RPC, with tokens and a supervisor. Even Notes could not start without it.
That is the model of KDE Akonadi and GNOME Evolution Data Server, and it carries their
cost: a daemon's lifecycle, its versioning against every client, and one more thing that
can be broken when a person just wants to write a note. The one thing it bought — a
central permission check — was never a boundary against a same-user program reading the
database file, which `0.1.0`'s own `SECURITY.md` conceded. The design note in `eon-docs`
compares four approaches; this one matches EON Stream.

**Two files, not one.** An application's documents are its own file: a corrupted or
locked document store affects one application, a backup of one application is one file,
and removing an application is removing a folder. What applications genuinely share — the
registry, preferences, the search index, recent activity, published summaries, the audit
chain — is one small file every application opens.

**SQLite between processes.** WAL mode lets readers and a writer proceed together; a
5-second busy timeout makes writers wait for each other instead of failing. The trap is a
*deferred* transaction that reads and then writes: when another process wrote in between,
SQLite cannot upgrade it and fails at once, whatever the timeout. Every read-then-write on
the shared file therefore uses `db::immediate`. `tests/library.rs` runs two writers against
one file; switching registration back to a deferred transaction fails it every time.

**Migrations under the write lock.** Two applications starting at the same moment both see
an old schema; the first to take the lock migrates, the second re-reads the version inside
the lock and finds the work done.

**A newer shared file does not stop an application.** If `shared.sqlite` was written by a
newer EON Office, the application still opens and keeps working with its own documents;
search, activity and preferences answer `incompatible`. A person's notes are never held
hostage by a version mismatch in a shared feature.

**Hybrid logical clocks for sync, last-writer-wins at document granularity, losers kept.**
Field-level merging would be right for some applications and wrong for others (a
spreadsheet cell is not a paragraph). The library does what every application needs — a
total order everyone agrees on and no silent loss — and leaves semantic merging to the
application, which gets the losing version from `conflicts`.

**Capabilities are checked, and described honestly.** Each call that touches something
shared checks the manifest's declaration minus the person's revocations, read fresh from
the registry so a revocation applies at once. It stops a buggy application and makes every
application's reach visible before it is installed. It does not stop code that opens the
file itself; nothing in a same-user desktop process could.

**Links start programs, nothing more.** `links::open` looks up the registered folder,
starts the program with the link as its only argument, and returns. An application that
is already open receives it through its own single-instance handling.

## What the library does not do, on purpose

| Not here | Where it belongs |
|---|---|
| Anything about notes, tasks, contacts, events | The application (madde 43c) |
| Notifications | The operating system's notification centre, through the application |
| Reminders and timers | The application, while it runs; it catches up at start |
| Serving many people over a network | `eon-office-server`, behind madde 30's gate |
| Running application code (plugins, scripts) | Not before madde 4's sandbox design exists |
| Encrypting the files | `docs/encryption-at-rest.md`; the OS's disk encryption for now |
| Telemetry of any kind | Nowhere (madde 36) |
| A user interface | Applications; the library never takes a GUI dependency |

## Error handling

Every failure is an `Error { kind, message, detail }` with a stable string code
(`forbidden`, `conflict`, `incompatible`…). Database and I/O errors are reported as
`internal` without their text, because it can name tables and values.

`clippy::unwrap_used`, `expect_used` and `panic` are denied.

## Versioning

- **Manifest format** (`manifest`): `0`. The `api` range is kept for the format and means
  the shared-store version.
- **Schemas**: `shared.sqlite` and `data.sqlite` each have ordered migrations; a newer file
  is refused.
- **This crate**: semver, `0.x` while the formats are `v0`.

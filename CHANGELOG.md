<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Changelog

## Unreleased

- A relative `entry.command` is found in the install folder with or without the
  platform's executable suffix, so one manifest serves every platform.

## 0.1.0 — first version

The first foundation for EON Office (plan, madde 43). Protocol `v0`, unstable.

- **Transport:** JSON-RPC 2.0, one object per line, over loopback TCP; 1 MiB per message;
  bounded per-connection queues; `runtime/endpoint.json` for discovery; one core per home.
- **Applications:** manifests (format `0`), install and upgrade without downgrade,
  uninstall that removes the application's data, owner-granted capabilities checked on
  every call, launch and stop, supervision with `on-failure` restarts (three a minute).
- **Directory:** the owner, people, organisations, workspaces, teams, scoped roles that
  reach downwards, and `access.check` with the role and scope that decided it.
- **Storage and sync:** per-application documents with revision checks; hybrid logical
  clock stamps; export and import between devices; later edit wins, losing edit kept in
  `storage.conflicts`.
- **Events and calls:** declared events, subscriptions by pattern, calls to provided
  methods through the core with timeouts and immediate failure when the provider dies.
- **Notifications** (30 a minute per application), **search** with Turkish-aware folding
  and private/shared entries, **jobs** that coalesce missed runs, **settings**,
  **preferences** pushed to every application, **themes** checked against WCAG AA.
- **Audit:** hash-chained log of changes of authority and refusals; `verify-audit`.
- **Backup and restore:** online backup, integrity-checked restore that keeps the
  previous database, migrations that refuse a newer schema.
- Tests: 46 unit tests and 16 protocol tests against the real binary.

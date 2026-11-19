<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# Security

## Reporting

Do not open a public issue. Use GitHub private vulnerability reporting on this repository
(**Security → Report a vulnerability**). The organisation-wide policy — response targets,
disclosure, credit — is in
[`.github/SECURITY.md`](https://github.com/EON-Extensible-Open-Network/.github/blob/main/SECURITY.md)
and applies here unchanged.

Only the latest `0.x` release is supported while the protocol is `v0`.

## What the core defends, and what it does not

Written down so nobody has to infer it from the code — madde 37's rule that a defence with
no named adversary is a guess.

| Adversary | Goal | Defence | Limit, stated plainly |
|---|---|---|---|
| A buggy or over-reaching application | Read another app's data, act beyond what the person allowed | Per-app storage keyed by the session, not the request; capabilities granted by the owner only, checked on every call; `events.publish` limited to declared events; refusals audited | — |
| A third-party application the person installed | Same, deliberately | Same. A grant can only cover what the manifest requested, so its power is visible before it is granted | The person can still grant too much. The core shows; it cannot judge |
| A crashing or hanging application | Take the suite down with it | Separate processes; bounded per-connection queues; calls time out; a dead provider fails pending calls at once; supervisor gives up after three restarts | — |
| A web page in the person's browser | Talk to the core | No HTTP: a browser cannot speak line-delimited JSON-RPC over raw TCP. Tokens are required before anything but `core.info` | — |
| Another user on the same machine | Read the person's data | Home in the per-user profile; tokens `0600` on Unix; on Windows, the profile's ACL | No database encryption yet — see below |
| Malware running as the same user | Anything | **None.** It can read the token files and the database like the person can | Same-user malware is outside every desktop application's boundary; claiming otherwise would be false |
| Someone who edits the database | Hide what happened | The audit log is a hash chain; `verify-audit` names the first broken entry | Tamper-*evident*, not tamper-proof: a full rewrite of the chain is undetectable unless its head hash was kept elsewhere |
| A sync peer | Push a far-future clock to win every merge | Stamps more than a day ahead are refused; losers are kept, so nothing is lost even when the wrong side wins | — |

### Known gaps, tracked, not hidden

- **No encryption at rest.** The database is plain SQLite. Until there is a design for key
  custody that does not reduce to "a key file beside the data", the honest answer is the
  operating system's disk encryption (BitLocker, FileVault, LUKS).
- **No signature check on applications.** The registry trusts whatever the owner installs.
  The signing chain EON Stream already uses (madde 34) is the intended mechanism; it
  arrives with `eon-office-marketplace`, and — as in EON Stream — no key will exist until
  there is a legal entity to hold it (madde 30).
- **Token files are the authentication.** An application started by the core receives its
  token in an environment variable; one started by the person reads `apps/<id>/token`.
  Both are as safe as the user's profile directory.

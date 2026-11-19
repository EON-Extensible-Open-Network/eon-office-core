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

Only the latest `0.x` release is supported while the formats are `v0`.

## What the library defends, and what it does not

Written down so nobody has to infer it from the code — madde 37's rule that a defence with
no named adversary is a guess.

| Adversary | Goal | Defence | Limit, stated plainly |
|---|---|---|---|
| A buggy EON Office application | Read or damage another application's data | Each application's documents are a separate file only it opens; the library never takes an application id from anywhere but the manifest it was started with; an import naming another application is refused | — |
| An application doing more than it said | Index, publish or read published data it never declared | Capabilities declared in the manifest, shown at install, revocable by the person, checked on every call; changes are audited | A program running as the person can open the files without this library. Capabilities are transparency, not a sandbox; a real boundary arrives with sandboxed third-party code (madde 4) |
| Two applications at once | Corrupt the shared file | SQLite WAL, a 5-second busy timeout, and every read-then-write in a write-locked transaction; tested with two writers | — |
| Another user on the same machine | Read the person's data | The home is in the per-user profile | No encryption at rest yet — see below |
| Malware running as the same user | Anything | **None.** | Outside every desktop application's boundary; claiming otherwise would be false |
| Someone who edits the shared file | Hide a capability change | The audit log is a hash chain; `Shared::audit_verify` names the first broken entry | Tamper-*evident*, not tamper-proof |
| A sync peer | Push a far-future clock to win every merge | Stamps more than a day ahead are refused; losers are kept | — |
| A link from elsewhere | Make an application do something | `links::parse` accepts only `eon-office://<valid id>/<item>`; the target treats the item as an id to show, never as a command | — |

### Known gaps, tracked, not hidden

- **No encryption at rest.** See [`docs/encryption-at-rest.md`](docs/encryption-at-rest.md);
  until then, the operating system's disk encryption.
- **No signature check on applications.** The marketplace's signing format exists
  (madde 34); no key exists until there is a legal entity to hold it (madde 30).

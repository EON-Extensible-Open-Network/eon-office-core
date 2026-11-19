<!--
SPDX-License-Identifier: GPL-3.0-or-later
SPDX-FileCopyrightText: 2026 EON contributors
-->

# eon-office-core

**The kernel of EON Office's code.** A Rust library every EON Office application links —
EON Notes, EON Tasks, EON Contacts, EON Calendar — the way every EON Stream build links
[`eon-stream-core`](https://github.com/EON-Extensible-Open-Network/eon-stream-core). It is not
a program: nothing runs in the background, nothing has to be started first.

Each application stands on its own. Download EON Notes, run it, done. When other EON Office
applications are installed too, they find each other through a few files in one well-known
folder, described in
[`eon-office-spec/docs/shared-store.md`](https://github.com/EON-Extensible-Open-Network/eon-office-spec/blob/main/docs/shared-store.md).

> **Status: `0.2.0`, formats `v0`, unstable.** `0.1.0` was a local service every application
> connected to; the owner rejected that design, and
> [`eon-docs/office/design/2026-11-19-library-core.md`](https://github.com/EON-Extensible-Open-Network/eon-docs/blob/main/office/design/2026-11-19-library-core.md)
> records the four approaches compared and what the change costs. The decision to build EON
> Office at all is [madde 43](https://github.com/EON-Extensible-Open-Network/eon-docs/blob/main/plan/eon-plan.md).

## Use it

```rust
use eon_office_core::{manifest::Manifest, Home};

let home = Home::open(&Home::default_location().unwrap())?;
let mut app = home.app(&Manifest::from_json(manifest_json)?, Some(exe_folder))?;
app.put("notes", "n1", &serde_json::json!({ "title": "Merhaba" }), Some(0))?;
app.index("n1", "Merhaba", "…", false)?;          // into the shared search, private
app.activity("n1", "Merhaba", "created")?;        // the person's "recent" list
```

| | |
|---|---|
| **Home** | `EON_OFFICE_HOME`, else `%LOCALAPPDATA%\EON\Office`, `~/Library/Application Support/EON Office`, `$XDG_DATA_HOME/eon-office`. `shared.sqlite` and `apps/<id>/data.sqlite` live there. |
| **Registration** | `Home::app` registers the application (manifest, folder) so the hub and links can find it. An older copy never downgrades the registry. |
| **Storage** | The application's own JSON documents, revisions that refuse a stale write. |
| **Sync** | Offline-first. Export changes after a hybrid-logical-clock stamp, import them on another device; the later edit wins and the losing one is **kept**. Transport-free. |
| **Search** | One shared index with Turkish-aware folding (`İ ı I i`, madde 40). An application sees its own entries and what others shared; the person's hub sees all. |
| **Activity** | Ids and titles for a "recent" list across applications — never contents. The last 500 are kept. |
| **Published data** | Summaries one application offers others (Contacts: names and addresses). Reading needs `call:<that app>`. |
| **Links** | `eon-office://<app>/<item>`: `links::open` starts the registered application with the link. |
| **Preferences, themes** | The person's language, theme, motion, text size, contrast; a version number applications poll to notice a change; themes refused below WCAG AA. |
| **Capabilities** | Declared in the manifest, revocable by the person, checked by the library. Transparency and a guard against mistakes — see `SECURITY.md` for what they are not. |
| **Audit** | A hash chain of registrations and capability changes. |
| **Backup** | Online backup of either file; restore that keeps the previous file beside it. |

Notifications are the operating system's, and reminders are each application's own; the
library has nothing to add there.

## Develop

Toolchain pinned in `rust-toolchain.toml`.

```
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test                                    # unit tests and tests/library.rs
EON_OFFICE_SPEC_DIR=../eon-office-spec cargo test --test spec_examples -- --ignored
cargo deny check                              # licences and advisories; runs in CI
```

`unwrap`, `expect` and `panic!` are denied: this code runs inside every application, and a
panic in it is that application losing the person's work.

## Licence

`GPL-3.0-or-later` with the [EON Module ABI Exception](LICENSE-EXCEPTION.md). Linking a GPL
library makes the application GPL too; a third party who wants otherwise can implement the
spec'd file formats themselves. Whether this library should carry a weaker copyleft is open
under madde 33. Contributions: [`CONTRIBUTING.md`](CONTRIBUTING.md). Security:
[`SECURITY.md`](SECURITY.md).

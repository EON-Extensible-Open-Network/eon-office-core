// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! # eon-office-core
//!
//! The kernel of EON Office's code (plan, madde 43): a library every EON
//! Office application links, as every EON Stream build links
//! `eon-stream-core`. It is not a program and runs nothing in the
//! background. Each application stands alone; what they share lives in
//! files this library reads and writes (`eon-office-spec/docs/shared-store.md`):
//!
//! | Module      | What it gives an application                                    |
//! |-------------|-----------------------------------------------------------------|
//! | `home`      | the home folder, registration, an application's context, the hub's view |
//! | `storage`   | its own document store, revisions, offline sync with conflicts  |
//! | `search`    | a shared index that only shows what its owners shared           |
//! | `settings`  | its own settings; the person's preferences; checked themes      |
//! | `links`     | `eon-office://` links that open another application             |
//! | `audit`     | a tamper-evident log of every change of authority               |
//! | `directory` | people, organisations, workspaces, teams, scoped roles          |
//! | `db`        | the two schemas, migrations, online backup, restore             |
//! | `text`      | Turkish-aware folding for search and sorting                    |
//!
//! Nothing in this crate knows what a note, a task or an e-mail is. Anything
//! that would need to is application code and belongs in that application's
//! repository (madde 43c).

pub mod audit;
pub mod db;
pub mod directory;
pub mod error;
pub mod hlc;
pub mod home;
pub mod links;
pub mod manifest;
pub mod search;
pub mod settings;
pub mod storage;
pub mod text;
pub mod util;

pub use error::{Error, Kind, Result};
pub use home::{AppContext, Home, Shared};

use std::path::PathBuf;

/// Where EON Office keeps its files when nothing else is said:
/// `EON_OFFICE_HOME` if set, otherwise the platform's per-user data folder.
///
/// This differs on purpose from EON Stream, which keeps everything beside its
/// executable: EON Stream is one program, while independently installed EON
/// Office applications need one well-known place to meet.
#[must_use]
pub fn default_home() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("EON_OFFICE_HOME").filter(|h| !h.is_empty()) {
        return Some(PathBuf::from(home));
    }
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|base| PathBuf::from(base).join("EON").join("Office"))
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME")
            .map(|base| PathBuf::from(base).join("Library/Application Support/EON Office"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
            .map(|base| base.join("eon-office"))
    }
}

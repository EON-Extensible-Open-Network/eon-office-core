// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! # eon-office-core
//!
//! The shared foundation of EON Office (plan, madde 43). It is a local
//! service, not an application: it has no window and never will (the same
//! rule as `eon-stream-core`). Applications -- EON Notes, EON Writer, a
//! third party's tool -- are separate processes that connect to it over the
//! protocol in `eon-office-spec/docs/protocol.md` and get, through one
//! documented door:
//!
//! | Module      | What it gives an application                                  |
//! |-------------|---------------------------------------------------------------|
//! | `apps`      | install, upgrade, grants, launch, stop, crash supervision      |
//! | `directory` | people, organisations, workspaces, teams, scoped roles, checks |
//! | `storage`   | its own document store, revisions, offline sync with conflicts |
//! | `server`    | events between applications, calls between applications       |
//! | `notify`    | one notification list for the whole suite                      |
//! | `search`    | a shared index that only shows what its owners shared          |
//! | `schedule`  | background wake-ups                                            |
//! | `settings`  | its own settings; the person's preferences; checked themes     |
//! | `audit`     | a tamper-evident log of every change of authority              |
//! | `db`        | migrations, online backup, restore                             |
//!
//! Nothing in this crate knows what a note, a cell or an e-mail is. Anything
//! that would need to is application code and belongs in that application's
//! repository (madde 43c).

pub mod apps;
pub mod audit;
pub mod db;
pub mod directory;
pub mod error;
pub mod hlc;
pub mod manifest;
pub mod notify;
pub mod schedule;
pub mod search;
pub mod server;
pub mod settings;
pub mod storage;
pub mod text;
pub mod util;

pub use error::{Error, Kind, Result};
pub use server::{serve, Core};

use std::path::PathBuf;

/// Where the core keeps its data when nothing else is said: `EON_OFFICE_HOME`
/// if set, otherwise the platform's per-user data directory.
///
/// This differs on purpose from EON Stream, which keeps everything beside its
/// executable: EON Stream is one program, while this is a meeting point for
/// many, and independently installed applications need one well-known place
/// to find it (`runtime/endpoint.json`).
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

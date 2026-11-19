// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! `eon-office://<app-id>/<item>[?query]` links: how one application asks
//! another to show or make something.
//!
//! A link is delivered by starting the target application from the folder
//! it registered, with the link as its only argument. An application that is
//! already open receives it from its second start (single instance) and
//! brings its window forward, so a link never opens a second window.

use crate::error::{Error, Result};
use crate::manifest::Manifest;
use crate::util::check_app_id;
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SCHEME: &str = "eon-office://";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub app: String,
    pub item: String,
    pub query: String,
}

/// Read a link. `None` for anything that is not a well-formed EON Office link.
#[must_use]
pub fn parse(text: &str) -> Option<Link> {
    let rest = text.trim().strip_prefix(SCHEME)?;
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (app, item) = path.split_once('/').unwrap_or((path, ""));
    check_app_id(app).ok()?;
    if item.chars().any(|c| c.is_whitespace() || c.is_control()) || item.len() > 512 {
        return None;
    }
    Some(Link {
        app: app.to_owned(),
        item: item.trim_end_matches('/').to_owned(),
        query: query.to_owned(),
    })
}

/// The program a registered application starts with. A relative
/// `entry.command` names a file in its folder, written without the
/// platform's executable suffix so one manifest serves every platform.
fn program(folder: &Path, command: &str) -> PathBuf {
    if Path::new(command).is_relative() {
        for name in [
            command.to_owned(),
            format!("{command}{}", std::env::consts::EXE_SUFFIX),
        ] {
            let candidate = folder.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
        return folder.join(format!("{command}{}", std::env::consts::EXE_SUFFIX));
    }
    PathBuf::from(command)
}

/// Where the application `app` registered its program, if it did.
pub fn program_of(home: &Path, app: &str) -> Result<Option<PathBuf>> {
    let Some(shared) = shared_if_present(home)? else {
        return Ok(None);
    };
    let row: Option<(String, Option<String>)> = shared
        .query_row(
            "SELECT manifest, path FROM registry WHERE id = ?1",
            params![app],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((manifest, Some(folder))) = row else {
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_str(&manifest)?;
    Ok(manifest
        .entry
        .map(|e| program(Path::new(&folder), &e.command)))
}

fn shared_if_present(home: &Path) -> Result<Option<rusqlite::Connection>> {
    let path = home.join("shared.sqlite");
    if !path.is_file() {
        return Ok(None);
    }
    crate::db::open(&path, crate::db::Schema::Shared).map(Some)
}

/// Deliver `link`: start its application with it.
/// `{opened: true}`, or `{opened: false, reason: "not_installed" | "missing_file"}`.
pub fn open(home: &Path, link: &str) -> Result<Value> {
    let parsed = parse(link).ok_or_else(|| Error::params("not an eon-office:// link"))?;
    let Some(program) = program_of(home, &parsed.app)? else {
        return Ok(json!({ "opened": false, "reason": "not_installed" }));
    };
    if !program.is_file() {
        return Ok(json!({ "opened": false, "reason": "missing_file" }));
    }
    let mut command = std::process::Command::new(&program);
    command.arg(link.trim()).env("EON_OFFICE_HOME", home);
    if let Some(folder) = program.parent() {
        command.current_dir(folder);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.spawn()?;
    Ok(json!({ "opened": true }))
}

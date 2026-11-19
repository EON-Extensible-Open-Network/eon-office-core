// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The application registry: what is installed, what it was granted, and the
//! token it proves itself with.
//!
//! Installing grants nothing unless the owner says so in the same breath:
//! every requested capability starts *pending*. A grant can only cover
//! something the manifest asked for, so an application's power is always
//! visible in its manifest. Upgrades never go backwards in version (madde
//! 39), keep the application's data and token, and keep only the grants the
//! new version still asks for.

use crate::error::{Error, Kind, Result};
use crate::manifest::{Capability, Manifest, Version};
use crate::util::{now_ms, random_hex, same_secret, sha256_hex};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    pub path: Option<String>,
    pub granted: BTreeSet<Capability>,
}

impl Installed {
    #[must_use]
    pub fn has(&self, capability: &Capability) -> bool {
        self.granted.contains(capability)
    }

    #[must_use]
    pub fn pending(&self) -> Vec<String> {
        self.manifest
            .requested()
            .into_iter()
            .filter(|c| !self.granted.contains(c))
            .map(|c| c.as_string())
            .collect()
    }

    #[must_use]
    pub fn granted_strings(&self) -> Vec<String> {
        self.granted.iter().map(Capability::as_string).collect()
    }
}

fn now() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

pub fn get(conn: &Connection, id: &str) -> Result<Option<Installed>> {
    let row: Option<(String, Option<String>, String)> = conn
        .query_row(
            "SELECT manifest, path, granted FROM apps WHERE id = ?1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((manifest, path, granted)) = row else {
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_str(&manifest)?;
    let granted: Vec<String> = serde_json::from_str(&granted)?;
    let granted = granted
        .iter()
        .filter_map(|c| Capability::parse(c).ok())
        .collect();
    Ok(Some(Installed {
        manifest,
        path,
        granted,
    }))
}

pub fn ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT id FROM apps ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<std::result::Result<_, _>>()?)
}

fn parse_grants(manifest: &Manifest, grant: &[String]) -> Result<BTreeSet<Capability>> {
    let requested: BTreeSet<Capability> = manifest.requested().into_iter().collect();
    let mut out = BTreeSet::new();
    for text in grant {
        let capability = Capability::parse(text)?;
        if !requested.contains(&capability) {
            return Err(Error::params(format!(
                "`{text}` is not in the manifest's capabilities; only requested capabilities can be granted"
            )));
        }
        out.insert(capability);
    }
    Ok(out)
}

fn store_grants(conn: &Connection, id: &str, granted: &BTreeSet<Capability>) -> Result<()> {
    let list: Vec<String> = granted.iter().map(Capability::as_string).collect();
    conn.execute(
        "UPDATE apps SET granted = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, serde_json::to_string(&list)?, now()],
    )?;
    Ok(())
}

pub struct InstallOutcome {
    pub installed: Installed,
    /// The new token, on a first install only. An upgrade keeps the old one.
    pub token: Option<String>,
    pub upgraded_from: Option<String>,
}

pub fn install(
    conn: &Connection,
    manifest: Manifest,
    path: Option<&str>,
    grant: &[String],
) -> Result<InstallOutcome> {
    manifest.validate()?;
    let extra = parse_grants(&manifest, grant)?;
    let text = serde_json::to_string(&manifest)?;
    match get(conn, &manifest.id)? {
        None => {
            let token = random_hex(32)?;
            conn.execute(
                "INSERT INTO apps (id, manifest, path, token_hash, granted, installed_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '[]', ?5, ?5)",
                params![manifest.id, text, path, sha256_hex(token.as_bytes()), now()],
            )?;
            store_grants(conn, &manifest.id, &extra)?;
            Ok(InstallOutcome {
                installed: Installed {
                    manifest,
                    path: path.map(str::to_owned),
                    granted: extra,
                },
                token: Some(token),
                upgraded_from: None,
            })
        }
        Some(old) => {
            if Version::parse(&manifest.version)? < Version::parse(&old.manifest.version)? {
                return Err(Error::new(
                    Kind::Incompatible,
                    format!(
                        "{} is installed; installing the older {} is refused",
                        old.manifest.version, manifest.version
                    ),
                ));
            }
            let requested: BTreeSet<Capability> = manifest.requested().into_iter().collect();
            let granted: BTreeSet<Capability> = old
                .granted
                .intersection(&requested)
                .cloned()
                .chain(extra)
                .collect();
            conn.execute(
                "UPDATE apps SET manifest = ?2, path = COALESCE(?3, path), updated_at = ?4 WHERE id = ?1",
                params![manifest.id, text, path, now()],
            )?;
            store_grants(conn, &manifest.id, &granted)?;
            let from = old.manifest.version.clone();
            let path = path.map(str::to_owned).or(old.path);
            Ok(InstallOutcome {
                installed: Installed {
                    manifest,
                    path,
                    granted,
                },
                token: None,
                upgraded_from: Some(from),
            })
        }
    }
}

pub fn grant(conn: &Connection, id: &str, capabilities: &[String]) -> Result<Installed> {
    let mut app = get(conn, id)?.ok_or_else(|| Error::not_found("no such application"))?;
    let extra = parse_grants(&app.manifest, capabilities)?;
    app.granted.extend(extra);
    store_grants(conn, id, &app.granted)?;
    Ok(app)
}

pub fn revoke(conn: &Connection, id: &str, capabilities: &[String]) -> Result<Installed> {
    let mut app = get(conn, id)?.ok_or_else(|| Error::not_found("no such application"))?;
    for text in capabilities {
        app.granted.remove(&Capability::parse(text)?);
    }
    store_grants(conn, id, &app.granted)?;
    Ok(app)
}

pub fn verify_token(conn: &Connection, id: &str, token: &str) -> Result<bool> {
    let hash: Option<String> = conn
        .query_row(
            "SELECT token_hash FROM apps WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(hash.is_some_and(|h| same_secret(&h, &sha256_hex(token.as_bytes()))))
}

/// Remove the application and everything the core holds for it.
pub fn uninstall(conn: &Connection, id: &str) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    if tx.execute("DELETE FROM apps WHERE id = ?1", params![id])? == 0 {
        return Err(Error::not_found("no such application"));
    }
    crate::storage::forget_app(&tx, id)?;
    crate::search::forget_app(&tx, id)?;
    crate::notify::forget_app(&tx, id)?;
    crate::schedule::forget_app(&tx, id)?;
    tx.execute(
        "DELETE FROM settings WHERE scope = ?1",
        params![format!("app:{id}")],
    )?;
    tx.commit()?;
    Ok(())
}

/// The registry entry as the owner sees it.
#[must_use]
pub fn describe(app: &Installed) -> Value {
    json!({
        "id": app.manifest.id,
        "name": app.manifest.name,
        "version": app.manifest.version,
        "description": app.manifest.description,
        "launchable": app.manifest.entry.is_some(),
        "granted": app.granted_strings(),
        "pending": app.pending(),
        "provides": app.manifest.provides,
        "publishes": app.manifest.publishes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn manifest(version: &str, caps: &[&str]) -> Result<Manifest> {
        Manifest::from_json(json!({
            "manifest": 0, "id": "org.example.notes", "name": "Notes",
            "version": version, "api": { "min": 0, "max": 0 }, "capabilities": caps,
        }))
    }

    #[test]
    fn nothing_is_granted_unless_asked_for_and_requested() -> Result<()> {
        let conn = open_in_memory()?;
        let out = install(
            &conn,
            manifest("1.0.0", &["storage", "notifications"])?,
            None,
            &[],
        )?;
        assert!(out.installed.granted.is_empty());
        assert_eq!(out.installed.pending().len(), 2);
        assert!(grant(&conn, "org.example.notes", &["schedule".into()]).is_err());
        let app = grant(&conn, "org.example.notes", &["storage".into()])?;
        assert!(app.has(&Capability::Storage));
        let token = out.token.unwrap_or_default();
        assert!(verify_token(&conn, "org.example.notes", &token)?);
        assert!(!verify_token(&conn, "org.example.notes", "nope")?);
        Ok(())
    }

    #[test]
    fn upgrades_keep_still_requested_grants_and_never_go_back() -> Result<()> {
        let conn = open_in_memory()?;
        install(
            &conn,
            manifest("1.0.0", &["storage", "notifications"])?,
            None,
            &["storage".into(), "notifications".into()],
        )?;
        let out = install(
            &conn,
            manifest("1.1.0", &["storage", "schedule"])?,
            None,
            &[],
        )?;
        assert!(out.token.is_none(), "an upgrade keeps the token");
        assert_eq!(out.installed.granted_strings(), vec!["storage"]);
        assert_eq!(out.installed.pending(), vec!["schedule"]);
        let down = install(&conn, manifest("1.0.5", &["storage"])?, None, &[])
            .err()
            .map(|e| e.kind);
        assert_eq!(down, Some(Kind::Incompatible));
        Ok(())
    }

    #[test]
    fn uninstall_forgets_the_application_s_data() -> Result<()> {
        let conn = open_in_memory()?;
        install(
            &conn,
            manifest("1.0.0", &["storage"])?,
            None,
            &["storage".into()],
        )?;
        let mut clock = crate::hlc::Clock::new("01".into());
        crate::storage::put(
            &conn,
            &mut clock,
            "org.example.notes",
            "c",
            "d",
            &json!(1),
            None,
        )?;
        uninstall(&conn, "org.example.notes")?;
        let left: i64 = conn.query_row("SELECT COUNT(*) FROM docs", [], |r| r.get(0))?;
        assert_eq!(left, 0);
        Ok(())
    }
}

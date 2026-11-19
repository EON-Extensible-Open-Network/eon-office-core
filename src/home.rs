// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The EON Office home folder, and what an application holds while it runs.
//!
//! ```text
//! <home>/shared.sqlite           registry, preferences, themes, search index,
//!                                activity, published data, audit
//! <home>/apps/<id>/data.sqlite   one application's documents
//! ```
//!
//! An application opens the home, then [`Home::app`] with its own manifest:
//! that registers it (so the hub and other applications can find it) and
//! opens its document store. Everything after goes through the returned
//! [`AppContext`]. The person's hub uses [`Shared`].
//!
//! **Capabilities.** An application's calls are checked against what its
//! manifest declares, minus what the person revoked. That is transparency and
//! a guard against mistakes; it is *not* a boundary against a program running
//! as the same user, which could open the files itself
//! (`eon-office-spec/docs/capabilities.md`).
//!
//! **A newer shared file.** If `shared.sqlite` was written by a newer EON
//! Office, the application still opens and keeps its own documents; only the
//! shared features answer `incompatible`.

use crate::db::{self, Schema};
use crate::error::{Error, Kind, Result};
use crate::hlc::Clock;
use crate::manifest::{Capability, Manifest, Version};
use crate::util::{check_app_id, now_ms, random_hex};
use crate::{audit, search, settings, storage};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Activity rows kept: enough for a "recent" list, never a history of a
/// person's work.
pub const ACTIVITY_KEPT: i64 = 500;
const MAX_PUBLIC_BYTES: usize = 16 * 1024;

fn now() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

/// The home folder.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

impl Home {
    /// Use `root` as the home, creating it if needed.
    pub fn open(root: &Path) -> Result<Home> {
        std::fs::create_dir_all(root.join("apps"))?;
        Ok(Home {
            root: root.to_path_buf(),
        })
    }

    /// `EON_OFFICE_HOME` if set, otherwise the platform's per-user data folder.
    #[must_use]
    pub fn default_location() -> Option<PathBuf> {
        crate::default_home()
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn shared_path(&self) -> PathBuf {
        self.root.join("shared.sqlite")
    }

    /// The folder an application keeps its files in.
    #[must_use]
    pub fn app_dir(&self, id: &str) -> PathBuf {
        self.root.join("apps").join(id)
    }

    /// The person's view of the shared file: what the hub uses.
    pub fn shared(&self) -> Result<Shared> {
        Ok(Shared {
            conn: db::open(&self.shared_path(), Schema::Shared)?,
        })
    }

    /// Start an application: register it and open its document store.
    /// `installed_at` is the folder its program lives in, so a link can
    /// start it; `None` leaves what is registered unchanged.
    pub fn app(&self, manifest: &Manifest, installed_at: Option<&Path>) -> Result<AppContext> {
        manifest.validate()?;
        check_app_id(&manifest.id)?;
        let dir = self.app_dir(&manifest.id);
        std::fs::create_dir_all(&dir)?;
        let data = db::open(&dir.join("data.sqlite"), Schema::Data)?;
        let stored: Option<String> = data
            .query_row("SELECT value FROM meta WHERE key = 'node'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let node = if let Some(node) = stored {
            node
        } else {
            let node = random_hex(8)?;
            data.execute(
                "INSERT INTO meta (key, value) VALUES ('node', ?1)",
                params![node],
            )?;
            node
        };
        let mut clock = Clock::new(node.clone());
        let latest: Option<String> =
            data.query_row("SELECT MAX(hlc) FROM docs", [], |r| r.get(0))?;
        if let Some(latest) = latest {
            clock.observe(&latest, now_ms())?;
        }
        // A shared file from a newer EON Office leaves the application
        // working with its own documents; any other failure is a failure.
        let shared = match db::open(&self.shared_path(), Schema::Shared) {
            Ok(conn) => {
                register(&conn, manifest, installed_at)?;
                Ok(conn)
            }
            Err(e) if e.kind == Kind::Incompatible => Err(e),
            Err(e) => return Err(e),
        };
        Ok(AppContext {
            id: manifest.id.clone(),
            declared: manifest.requested(),
            data,
            shared,
            clock,
            node,
        })
    }
}

fn register(conn: &Connection, manifest: &Manifest, installed_at: Option<&Path>) -> Result<()> {
    let tx = db::immediate(conn)?;
    let existing: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT manifest, path FROM registry WHERE id = ?1",
            params![manifest.id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let body = serde_json::to_string(manifest)?;
    let path = installed_at.map(|p| p.to_string_lossy().into_owned());
    let action = match existing {
        None => {
            tx.execute(
                "INSERT INTO registry (id, manifest, path, registered_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
                params![manifest.id, body, path, now()],
            )?;
            Some("app.registered")
        }
        Some((old, old_path)) => {
            let old: Manifest = serde_json::from_str(&old)?;
            let (new_v, old_v) = (
                Version::parse(&manifest.version)?,
                Version::parse(&old.version)?,
            );
            if new_v < old_v {
                // An older copy started: it runs, but the registry keeps
                // pointing at the newest one.
                None
            } else if new_v > old_v || old != *manifest || (path.is_some() && path != old_path) {
                tx.execute(
                    "UPDATE registry SET manifest = ?2, path = COALESCE(?3, path), updated_at = ?4 WHERE id = ?1",
                    params![manifest.id, body, path, now()],
                )?;
                (new_v > old_v).then_some("app.upgraded")
            } else {
                None
            }
        }
    };
    tx.commit()?;
    if let Some(action) = action {
        audit::record(
            conn,
            &format!("app:{}", manifest.id),
            action,
            &manifest.id,
            &json!({ "version": manifest.version }),
        )?;
    }
    Ok(())
}

fn revoked(conn: &Connection, app: &str) -> Result<Vec<String>> {
    let text: Option<String> = conn
        .query_row(
            "SELECT revoked FROM registry WHERE id = ?1",
            params![app],
            |r| r.get(0),
        )
        .optional()?;
    Ok(serde_json::from_str(text.as_deref().unwrap_or("[]")).unwrap_or_default())
}

/// What a running application holds.
#[derive(Debug)]
pub struct AppContext {
    id: String,
    declared: Vec<Capability>,
    data: Connection,
    shared: Result<Connection>,
    clock: Clock,
    node: String,
}

impl AppContext {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// This device's identity in sync exchanges.
    #[must_use]
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Is the shared file usable (not written by a newer EON Office)?
    #[must_use]
    pub fn shared_available(&self) -> bool {
        self.shared.is_ok()
    }

    fn shared(&self) -> Result<&Connection> {
        self.shared.as_ref().map_err(Clone::clone)
    }

    /// Declared in the manifest and not revoked by the person.
    #[must_use]
    pub fn allows(&self, capability: &Capability) -> bool {
        self.declared.contains(capability)
            && self.shared.as_ref().map_or(true, |conn| {
                revoked(conn, &self.id).is_ok_and(|r| !r.contains(&capability.as_string()))
            })
    }

    fn need(&self, capability: &Capability) -> Result<()> {
        if self.allows(capability) {
            Ok(())
        } else {
            Err(Error::missing(&capability.as_string()))
        }
    }

    // ---- documents --------------------------------------------------------

    pub fn put(
        &mut self,
        collection: &str,
        id: &str,
        doc: &Value,
        rev: Option<u64>,
    ) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::put(
            &self.data,
            &mut self.clock,
            &self.id,
            collection,
            id,
            doc,
            rev,
        )
    }

    /// The document with its `rev`, or `Null` when there is none.
    pub fn get(&self, collection: &str, id: &str) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::get(&self.data, &self.id, collection, id)
    }

    pub fn delete(&mut self, collection: &str, id: &str, rev: Option<u64>) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::delete(&self.data, &mut self.clock, &self.id, collection, id, rev)
    }

    pub fn list(&self, collection: &str, after: Option<&str>, limit: i64) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::list(&self.data, &self.id, collection, after, limit)
    }

    pub fn conflicts(&self, limit: i64) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::conflicts(&self.data, Some(&self.id), limit)
    }

    pub fn resolve(&self, conflict: i64) -> Result<()> {
        self.need(&Capability::Storage)?;
        storage::resolve(&self.data, Some(&self.id), conflict)
    }

    /// Changes after `since`, to carry to another device.
    pub fn export(&self, since: Option<&str>, limit: i64) -> Result<Value> {
        self.need(&Capability::Storage)?;
        storage::export(&self.data, &self.node, since, limit)
    }

    /// Merge changes from another device. Changes naming another application
    /// are refused as a whole.
    pub fn import(&mut self, changes: &[Value]) -> Result<Value> {
        self.need(&Capability::Storage)?;
        if let Some(foreign) = changes
            .iter()
            .find(|c| c.get("app").and_then(Value::as_str) != Some(self.id.as_str()))
        {
            return Err(Error::params(format!(
                "a change for `{}` cannot be imported by `{}`",
                foreign.get("app").and_then(Value::as_str).unwrap_or("?"),
                self.id
            )));
        }
        let (mut out, touched) = storage::import(&self.data, &mut self.clock, changes)?;
        out["touched"] = touched
            .into_iter()
            .map(|(_, collection, id)| json!({ "collection": collection, "id": id }))
            .collect();
        Ok(out)
    }

    /// The application's own settings.
    pub fn settings_get(&self, key: &str) -> Result<Value> {
        settings::get(&self.data, &self.id, key)
    }

    pub fn settings_set(&self, key: &str, value: &Value) -> Result<()> {
        settings::set(&self.data, &self.id, key, value)
    }

    /// A consistent copy of this application's documents.
    pub fn backup(&self, target: &Path) -> Result<()> {
        db::backup(&self.data, target, Schema::Data)
    }

    // ---- shared -----------------------------------------------------------

    /// Put an item in the search index. `shared`: other applications may find
    /// it too; otherwise only this application and the person's hub.
    pub fn index(&self, id: &str, title: &str, text: &str, shared: bool) -> Result<()> {
        self.need(&Capability::SearchIndex)?;
        let visibility = if shared {
            search::Visibility::Shared
        } else {
            search::Visibility::Private
        };
        search::index(self.shared()?, &self.id, id, title, text, visibility, None)
    }

    pub fn unindex(&self, id: &str) -> Result<()> {
        self.need(&Capability::SearchIndex)?;
        search::remove(self.shared()?, &self.id, id).map(|_| ())
    }

    /// This application's items and every application's shared ones.
    pub fn search(&self, query: &str, limit: i64) -> Result<Value> {
        self.need(&Capability::SearchQuery)?;
        search::query(self.shared()?, Some(&self.id), query, limit)
    }

    /// Say that the person did something to an item, for the "recent" list:
    /// its id and title, never its contents.
    pub fn activity(&self, item: &str, title: &str, kind: &str) -> Result<()> {
        self.need(&Capability::EventsPublish)?;
        crate::util::check_name("item", item)?;
        crate::util::check_name("kind", kind)?;
        let title: String = title.chars().take(300).collect();
        let conn = self.shared()?;
        let tx = db::immediate(conn)?;
        tx.execute(
            "INSERT INTO activity (at, app, item, title, kind) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), self.id, item, title, kind],
        )?;
        tx.execute(
            "DELETE FROM activity WHERE seq <= (SELECT MAX(seq) FROM activity) - ?1",
            params![ACTIVITY_KEPT],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Publish a summary other applications may read (a contact's name and
    /// addresses, say). Summaries only: a published value is readable by
    /// every application that declares `call:<this id>`.
    pub fn publish(&self, key: &str, value: &Value) -> Result<()> {
        self.need(&Capability::EventsPublish)?;
        crate::util::check_name("key", key)?;
        let body = value.to_string();
        if body.len() > MAX_PUBLIC_BYTES {
            return Err(Error::new(
                Kind::Limit,
                "a published value is at most 16 KiB",
            ));
        }
        self.shared()?.execute(
            "INSERT INTO public (app, key, value, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (app, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![self.id, key, body, now()],
        )?;
        Ok(())
    }

    pub fn unpublish(&self, key: &str) -> Result<()> {
        self.need(&Capability::EventsPublish)?;
        self.shared()?.execute(
            "DELETE FROM public WHERE app = ?1 AND key = ?2",
            params![self.id, key],
        )?;
        Ok(())
    }

    /// What `app` published, by key. Needs `call:<app>`.
    pub fn public_of(&self, app: &str) -> Result<Vec<(String, Value)>> {
        check_app_id(app)?;
        if app != self.id {
            self.need(&Capability::Call(app.to_owned()))?;
        }
        let conn = self.shared()?;
        let mut stmt = conn.prepare("SELECT key, value FROM public WHERE app = ?1 ORDER BY key")?;
        let rows = stmt.query_map(params![app], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (key, value) = row?;
            out.push((key, serde_json::from_str(&value)?));
        }
        Ok(out)
    }

    /// The person's preferences (language, theme, text size, motion, contrast).
    pub fn prefs(&self) -> Result<Value> {
        settings::prefs(self.shared()?)
    }

    /// Changes whenever a preference changes; poll it to notice.
    pub fn prefs_version(&self) -> Result<i64> {
        prefs_version(self.shared()?)
    }

    pub fn themes(&self) -> Result<Value> {
        settings::themes(self.shared()?)
    }
}

fn prefs_version(conn: &Connection) -> Result<i64> {
    let v: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'prefs_version'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.and_then(|v| v.parse().ok()).unwrap_or(0))
}

/// The person's view of the shared file: the registry, preferences, search
/// over everything, the recent list, the audit log. What the hub uses.
#[derive(Debug)]
pub struct Shared {
    conn: Connection,
}

impl Shared {
    /// Every registered application: id, name, version, path, manifest, revoked.
    pub fn apps(&self) -> Result<Vec<Value>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, manifest, path, revoked, registered_at, updated_at FROM registry ORDER BY id")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, manifest, path, revoked, registered_at, updated_at) = row?;
            let manifest: Value = serde_json::from_str(&manifest)?;
            out.push(json!({
                "id": id, "name": manifest["name"], "version": manifest["version"],
                "path": path, "manifest": manifest,
                "revoked": serde_json::from_str::<Value>(&revoked)?,
                "registered_at": registered_at, "updated_at": updated_at,
            }));
        }
        Ok(out)
    }

    fn set_revoked(&self, app: &str, capability: &str, revoke: bool) -> Result<()> {
        Capability::parse(capability)?;
        let tx = db::immediate(&self.conn)?;
        let mut list = revoked(&tx, app)?;
        let exists: bool = tx
            .query_row("SELECT 1 FROM registry WHERE id = ?1", params![app], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        if !exists {
            return Err(Error::not_found("no such application"));
        }
        list.retain(|c| c != capability);
        if revoke {
            list.push(capability.to_owned());
        }
        list.sort();
        tx.execute(
            "UPDATE registry SET revoked = ?2 WHERE id = ?1",
            params![app, serde_json::to_string(&list)?],
        )?;
        tx.commit()?;
        let action = if revoke {
            "capability.revoked"
        } else {
            "capability.granted"
        };
        audit::record(
            &self.conn,
            "owner",
            action,
            app,
            &json!({ "capability": capability }),
        )
    }

    /// Take a capability away from an application.
    pub fn revoke(&self, app: &str, capability: &str) -> Result<()> {
        self.set_revoked(app, capability, true)
    }

    /// Give back a capability the application declares.
    pub fn grant(&self, app: &str, capability: &str) -> Result<()> {
        self.set_revoked(app, capability, false)
    }

    /// Forget an application in the shared file: registry, index, activity,
    /// published data. Its own folder is the caller's to remove.
    pub fn unregister(&self, app: &str) -> Result<()> {
        let tx = db::immediate(&self.conn)?;
        for sql in [
            "DELETE FROM registry WHERE id = ?1",
            "DELETE FROM search_terms WHERE app = ?1",
            "DELETE FROM search_docs WHERE app = ?1",
            "DELETE FROM activity WHERE app = ?1",
            "DELETE FROM public WHERE app = ?1",
        ] {
            tx.execute(sql, params![app])?;
        }
        tx.commit()?;
        audit::record(&self.conn, "owner", "app.unregistered", app, &json!({}))
    }

    pub fn prefs(&self) -> Result<Value> {
        settings::prefs(&self.conn)
    }

    pub fn set_pref(&self, key: &str, value: &Value) -> Result<()> {
        let tx = db::immediate(&self.conn)?;
        settings::set_pref(&tx, key, value)?;
        let next = prefs_version(&tx)? + 1;
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('prefs_version', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![next.to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn prefs_version(&self) -> Result<i64> {
        prefs_version(&self.conn)
    }

    pub fn themes(&self) -> Result<Value> {
        settings::themes(&self.conn)
    }

    pub fn put_theme(&self, theme: &Value) -> Result<Value> {
        settings::put_theme(&self.conn, theme)
    }

    /// The most recent activity, newest first.
    pub fn recent(&self, limit: i64) -> Result<Value> {
        let mut stmt = self.conn.prepare(
            "SELECT at, app, item, title, kind FROM activity ORDER BY seq DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit.clamp(1, ACTIVITY_KEPT)], |r| {
            Ok(json!({
                "at": r.get::<_, i64>(0)?, "app": r.get::<_, String>(1)?,
                "item": r.get::<_, String>(2)?, "title": r.get::<_, String>(3)?,
                "kind": r.get::<_, String>(4)?,
            }))
        })?;
        Ok(json!({ "items": rows.collect::<std::result::Result<Vec<_>, _>>()? }))
    }

    /// Search as `viewer` (an application id), or as the person (`None`).
    pub fn query(&self, viewer: Option<&str>, query: &str, limit: i64) -> Result<Value> {
        search::query(&self.conn, viewer, query, limit)
    }

    pub fn audit_list(&self, after: i64, limit: i64) -> Result<Value> {
        audit::list(&self.conn, after, limit)
    }

    pub fn audit_verify(&self) -> Result<Value> {
        audit::verify(&self.conn)
    }

    /// A consistent copy of the shared file.
    pub fn backup(&self, target: &Path) -> Result<()> {
        db::backup(&self.conn, target, Schema::Shared)
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Documents, and keeping two devices' documents in step.
//!
//! Each application gets its own document store: JSON documents in named
//! collections. An application can never name another application's
//! documents -- the application id is taken from the session, not from the
//! request. Sharing data between applications happens by *asking*: one
//! application calls a method another provides (`apps.call`), and the owner
//! of the data decides what to answer. That is what keeps an application's
//! storage layout its own business, free to change between versions.
//!
//! **Writes** carry an optional expected revision. A write against a stale
//! revision is refused with the current one, so two windows of the same
//! application cannot silently overwrite each other.
//!
//! **Sync** is offline-first and transport-free: `export` returns every
//! change after a clock stamp, `import` merges changes from elsewhere. The
//! later stamp wins, and the losing version is *kept* in the conflict list
//! instead of discarded -- no edit disappears without a record. How changes
//! travel (a file, a USB stick, a self-hosted server) is the transport's
//! business; the merge is the same.

use crate::error::{Error, Kind, Result};
use crate::hlc::{self, Clock};
use crate::util::{check_name, now_ms};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

pub const MAX_DOC_BYTES: usize = 512 * 1024;

fn now() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

struct Current {
    rev: i64,
    hlc: String,
    deleted: bool,
    body: Option<String>,
}

fn current(conn: &Connection, app: &str, collection: &str, id: &str) -> Result<Option<Current>> {
    Ok(conn
        .query_row(
            "SELECT rev, hlc, deleted, body FROM docs WHERE app = ?1 AND collection = ?2 AND id = ?3",
            params![app, collection, id],
            |r| {
                Ok(Current {
                    rev: r.get(0)?,
                    hlc: r.get(1)?,
                    deleted: r.get::<_, i64>(2)? != 0,
                    body: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn check_rev(found: Option<&Current>, expected: Option<u64>) -> Result<i64> {
    let live_rev = found.filter(|c| !c.deleted).map_or(0, |c| c.rev);
    if let Some(expected) = expected {
        let expected = i64::try_from(expected).unwrap_or(i64::MAX);
        if expected != live_rev {
            return Err(
                Error::new(Kind::Conflict, "the document changed since it was read")
                    .with(json!({ "rev": live_rev })),
            );
        }
    }
    // Revisions keep counting through a delete, so a recreated document never
    // reuses a revision a stale reader might still hold.
    Ok(found.map_or(0, |c| c.rev) + 1)
}

/// Write a document. `expected_rev`: `0` means "must not exist yet".
pub fn put(
    conn: &Connection,
    clock: &mut Clock,
    app: &str,
    collection: &str,
    id: &str,
    doc: &Value,
    expected_rev: Option<u64>,
) -> Result<Value> {
    check_name("collection", collection)?;
    check_name("id", id)?;
    if doc.is_null() {
        return Err(Error::params(
            "`doc` is required; use storage.delete to remove",
        ));
    }
    let body = doc.to_string();
    if body.len() > MAX_DOC_BYTES {
        return Err(Error::new(Kind::Limit, "a document is at most 512 KiB"));
    }
    let found = current(conn, app, collection, id)?;
    let rev = check_rev(found.as_ref(), expected_rev)?;
    let stamp = clock.tick(now_ms());
    conn.execute(
        "INSERT INTO docs (app, collection, id, body, rev, hlc, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
         ON CONFLICT (app, collection, id) DO UPDATE
         SET body = excluded.body, rev = excluded.rev, hlc = excluded.hlc, deleted = 0",
        params![app, collection, id, body, rev, stamp],
    )?;
    Ok(json!({ "rev": rev, "hlc": stamp }))
}

pub fn get(conn: &Connection, app: &str, collection: &str, id: &str) -> Result<Value> {
    check_name("collection", collection)?;
    check_name("id", id)?;
    Ok(match current(conn, app, collection, id)? {
        Some(c) if !c.deleted => json!({
            "doc": serde_json::from_str::<Value>(c.body.as_deref().unwrap_or("null"))?,
            "rev": c.rev,
            "hlc": c.hlc,
        }),
        _ => Value::Null,
    })
}

/// Delete a document. The row stays as a tombstone so the deletion syncs.
pub fn delete(
    conn: &Connection,
    clock: &mut Clock,
    app: &str,
    collection: &str,
    id: &str,
    expected_rev: Option<u64>,
) -> Result<Value> {
    check_name("collection", collection)?;
    check_name("id", id)?;
    let found = current(conn, app, collection, id)?;
    if found.as_ref().is_none_or(|c| c.deleted) {
        return Err(Error::not_found("no such document"));
    }
    let rev = check_rev(found.as_ref(), expected_rev)?;
    let stamp = clock.tick(now_ms());
    conn.execute(
        "UPDATE docs SET body = NULL, deleted = 1, rev = ?4, hlc = ?5
         WHERE app = ?1 AND collection = ?2 AND id = ?3",
        params![app, collection, id, rev, stamp],
    )?;
    Ok(json!({ "rev": rev, "hlc": stamp }))
}

/// Documents in a collection, by id, paged with `after`.
pub fn list(
    conn: &Connection,
    app: &str,
    collection: &str,
    after: Option<&str>,
    limit: i64,
) -> Result<Value> {
    check_name("collection", collection)?;
    let mut stmt = conn.prepare(
        "SELECT id, body, rev, hlc FROM docs
         WHERE app = ?1 AND collection = ?2 AND deleted = 0 AND id > ?3
         ORDER BY id LIMIT ?4",
    )?;
    let rows = stmt.query_map(params![app, collection, after.unwrap_or(""), limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (id, body, rev, hlc) = row?;
        items.push(json!({ "id": id, "doc": serde_json::from_str::<Value>(&body)?, "rev": rev, "hlc": hlc }));
    }
    Ok(json!({ "items": items }))
}

// ---- sync ----------------------------------------------------------------

/// Every change stamped after `since` (all of them when `None`), oldest
/// first, at most `limit`. `until` is the stamp to pass as `since` next time.
pub fn export(conn: &Connection, node: &str, since: Option<&str>, limit: i64) -> Result<Value> {
    if let Some(since) = since {
        hlc::parse(since)?;
    }
    let mut stmt = conn.prepare(
        "SELECT app, collection, id, body, deleted, hlc FROM docs
         WHERE hlc > ?1 ORDER BY hlc LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![since.unwrap_or(""), limit], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)? != 0,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut changes = Vec::new();
    let mut until = since.map(str::to_owned);
    for row in rows {
        let (app, collection, id, body, deleted, stamp) = row?;
        let doc = match body {
            Some(text) if !deleted => serde_json::from_str::<Value>(&text)?,
            _ => Value::Null,
        };
        until = Some(stamp.clone());
        changes.push(json!({
            "app": app, "collection": collection, "id": id,
            "doc": doc, "deleted": deleted, "hlc": stamp,
        }));
    }
    let complete = (changes.len() as i64) < limit;
    Ok(json!({ "node": node, "changes": changes, "until": until, "complete": complete }))
}

/// A document an import changed: application, collection, id.
pub type Touched = (String, String, String);

/// Merge changes from another device. Returns what happened to each, and the
/// documents that changed, so their applications can be told.
pub fn import(
    conn: &Connection,
    clock: &mut Clock,
    changes: &[Value],
) -> Result<(Value, Vec<Touched>)> {
    let wall = now_ms();
    let tx = conn.unchecked_transaction()?;
    let (mut applied, mut stale, mut conflicts) = (0u64, 0u64, 0u64);
    let mut touched = Vec::new();
    for change in changes {
        let field = |key: &str| {
            change
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| Error::params(format!("every change needs `{key}`")))
        };
        let (app, collection, id, stamp) = (
            field("app")?,
            field("collection")?,
            field("id")?,
            field("hlc")?,
        );
        crate::util::check_app_id(app)?;
        check_name("collection", collection)?;
        check_name("id", id)?;
        hlc::parse(stamp)?;
        let deleted = change
            .get("deleted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let body = if deleted {
            None
        } else {
            let doc = change
                .get("doc")
                .filter(|d| !d.is_null())
                .ok_or_else(|| Error::params("a change that is not a deletion needs `doc`"))?;
            let text = doc.to_string();
            if text.len() > MAX_DOC_BYTES {
                return Err(Error::new(Kind::Limit, "a document is at most 512 KiB"));
            }
            Some(text)
        };
        clock.observe(stamp, wall)?;

        let found = current(&tx, app, collection, id)?;
        match &found {
            Some(local) if local.hlc.as_str() == stamp => {
                stale += 1; // already have exactly this change
                continue;
            }
            Some(local) if local.hlc.as_str() > stamp => {
                // Ours is later: theirs loses, and is kept.
                record_conflict(
                    &tx,
                    app,
                    collection,
                    id,
                    body.as_deref(),
                    deleted,
                    stamp,
                    &local.hlc,
                )?;
                conflicts += 1;
                continue;
            }
            _ => {}
        }
        if let Some(local) = &found {
            // Theirs is later: ours loses, and is kept -- unless it was already
            // deleted, which loses nothing.
            if !local.deleted {
                record_conflict(
                    &tx,
                    app,
                    collection,
                    id,
                    local.body.as_deref(),
                    false,
                    &local.hlc,
                    stamp,
                )?;
                conflicts += 1;
            }
        }
        let rev = found.as_ref().map_or(0, |c| c.rev) + 1;
        tx.execute(
            "INSERT INTO docs (app, collection, id, body, rev, hlc, deleted) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (app, collection, id) DO UPDATE
             SET body = excluded.body, rev = excluded.rev, hlc = excluded.hlc, deleted = excluded.deleted",
            params![app, collection, id, body, rev, stamp, i64::from(deleted)],
        )?;
        applied += 1;
        touched.push((app.to_owned(), collection.to_owned(), id.to_owned()));
    }
    tx.commit()?;
    Ok((
        json!({ "applied": applied, "unchanged": stale, "conflicts": conflicts }),
        touched,
    ))
}

#[allow(clippy::too_many_arguments)]
fn record_conflict(
    conn: &Connection,
    app: &str,
    collection: &str,
    id: &str,
    body: Option<&str>,
    deleted: bool,
    stamp: &str,
    winner: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO conflicts (app, collection, doc_id, body, deleted, hlc, winner_hlc, at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            app,
            collection,
            id,
            body,
            i64::from(deleted),
            stamp,
            winner,
            now()
        ],
    )?;
    Ok(())
}

/// The versions that lost a merge, for one application (or all, for the owner).
pub fn conflicts(conn: &Connection, app: Option<&str>, limit: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT id, app, collection, doc_id, body, deleted, hlc, winner_hlc, at FROM conflicts
         WHERE ?1 IS NULL OR app = ?1 ORDER BY id LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![app, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, i64>(5)? != 0,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    let mut items = Vec::new();
    for row in rows {
        let (cid, app, collection, doc_id, body, deleted, stamp, winner, at) = row?;
        let doc = match body {
            Some(text) => serde_json::from_str::<Value>(&text)?,
            None => Value::Null,
        };
        items.push(json!({
            "conflict": cid, "app": app, "collection": collection, "id": doc_id,
            "doc": doc, "deleted": deleted, "hlc": stamp, "winner_hlc": winner, "at": at,
        }));
    }
    Ok(json!({ "items": items }))
}

/// Dismiss a recorded conflict, once the application has merged it by hand.
pub fn resolve(conn: &Connection, app: Option<&str>, conflict: i64) -> Result<()> {
    let removed = conn.execute(
        "DELETE FROM conflicts WHERE id = ?1 AND (?2 IS NULL OR app = ?2)",
        params![conflict, app],
    )?;
    if removed == 0 {
        return Err(Error::not_found("no such conflict"));
    }
    Ok(())
}

/// Remove everything an application stored. Used on uninstall.
pub fn forget_app(conn: &Connection, app: &str) -> Result<()> {
    conn.execute("DELETE FROM docs WHERE app = ?1", params![app])?;
    conn.execute("DELETE FROM conflicts WHERE app = ?1", params![app])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open_in_memory, Schema};

    const APP: &str = "org.example.notes";

    #[test]
    fn a_stale_write_is_refused_with_the_current_revision() -> Result<()> {
        let conn = open_in_memory(Schema::Data)?;
        let mut clock = Clock::new("01".into());
        put(
            &conn,
            &mut clock,
            APP,
            "notes",
            "a",
            &json!({"t": 1}),
            Some(0),
        )?;
        put(
            &conn,
            &mut clock,
            APP,
            "notes",
            "a",
            &json!({"t": 2}),
            Some(1),
        )?;
        let error = put(
            &conn,
            &mut clock,
            APP,
            "notes",
            "a",
            &json!({"t": 3}),
            Some(1),
        )
        .err();
        let error = error.map(|e| (e.kind, e.detail.unwrap_or_default()["rev"].clone()));
        assert_eq!(error, Some((Kind::Conflict, json!(2))));
        // "Must not exist yet" fails on an existing document.
        assert!(put(&conn, &mut clock, APP, "notes", "a", &json!({}), Some(0)).is_err());
        Ok(())
    }

    #[test]
    fn deleted_documents_vanish_from_reads_but_keep_counting() -> Result<()> {
        let conn = open_in_memory(Schema::Data)?;
        let mut clock = Clock::new("01".into());
        put(&conn, &mut clock, APP, "notes", "a", &json!({}), None)?;
        delete(&conn, &mut clock, APP, "notes", "a", None)?;
        assert_eq!(get(&conn, APP, "notes", "a")?, Value::Null);
        assert_eq!(list(&conn, APP, "notes", None, 10)?["items"], json!([]));
        let again = put(&conn, &mut clock, APP, "notes", "a", &json!({}), Some(0))?;
        assert_eq!(again["rev"], 3);
        Ok(())
    }

    #[test]
    fn two_devices_converge_and_the_loser_is_kept() -> Result<()> {
        let (laptop, phone) = (open_in_memory(Schema::Data)?, open_in_memory(Schema::Data)?);
        let (mut lc, mut pc) = (Clock::new("0a".into()), Clock::new("0b".into()));

        // Both start from the same note.
        put(
            &laptop,
            &mut lc,
            APP,
            "notes",
            "n",
            &json!({"v": "base"}),
            None,
        )?;
        let first = export(&laptop, "0a", None, 100)?;
        import(
            &phone,
            &mut pc,
            first["changes"].as_array().map_or(&[][..], Vec::as_slice),
        )?;

        // Offline, both edit it; the phone edits later.
        put(
            &laptop,
            &mut lc,
            APP,
            "notes",
            "n",
            &json!({"v": "laptop"}),
            None,
        )?;
        std::thread::sleep(std::time::Duration::from_millis(3));
        put(
            &phone,
            &mut pc,
            APP,
            "notes",
            "n",
            &json!({"v": "phone"}),
            None,
        )?;

        // Exchange in both directions.
        let from_laptop = export(&laptop, "0a", None, 100)?;
        let from_phone = export(&phone, "0b", None, 100)?;
        let (on_phone, _) = import(
            &phone,
            &mut pc,
            from_laptop["changes"]
                .as_array()
                .map_or(&[][..], Vec::as_slice),
        )?;
        let (on_laptop, _) = import(
            &laptop,
            &mut lc,
            from_phone["changes"]
                .as_array()
                .map_or(&[][..], Vec::as_slice),
        )?;

        // Same answer on both: the later edit.
        assert_eq!(get(&laptop, APP, "notes", "n")?["doc"]["v"], "phone");
        assert_eq!(get(&phone, APP, "notes", "n")?["doc"]["v"], "phone");
        // And the laptop's edit is not gone: each side recorded it.
        assert_eq!(on_phone["conflicts"], 1);
        assert_eq!(on_laptop["conflicts"], 1);
        let kept = conflicts(&laptop, Some(APP), 10)?;
        assert_eq!(kept["items"][0]["doc"]["v"], "laptop");
        Ok(())
    }

    #[test]
    fn a_deletion_syncs_and_importing_twice_changes_nothing() -> Result<()> {
        let (a, b) = (open_in_memory(Schema::Data)?, open_in_memory(Schema::Data)?);
        let (mut ac, mut bc) = (Clock::new("0a".into()), Clock::new("0b".into()));
        put(&a, &mut ac, APP, "notes", "n", &json!({}), None)?;
        let changes = export(&a, "0a", None, 100)?;
        import(
            &b,
            &mut bc,
            changes["changes"].as_array().map_or(&[][..], Vec::as_slice),
        )?;
        delete(&a, &mut ac, APP, "notes", "n", None)?;
        let changes = export(&a, "0a", None, 100)?;
        let list = changes["changes"].as_array().map_or(&[][..], Vec::as_slice);
        import(&b, &mut bc, list)?;
        assert_eq!(get(&b, APP, "notes", "n")?, Value::Null);
        let (again, _) = import(&b, &mut bc, list)?;
        assert_eq!(
            (again["applied"].clone(), again["unchanged"].clone()),
            (json!(0), json!(1))
        );
        Ok(())
    }

    #[test]
    fn export_pages_by_clock_stamp() -> Result<()> {
        let conn = open_in_memory(Schema::Data)?;
        let mut clock = Clock::new("01".into());
        for n in 0..5 {
            put(
                &conn,
                &mut clock,
                APP,
                "c",
                &format!("d{n}"),
                &json!(n),
                None,
            )?;
        }
        let page = export(&conn, "01", None, 3)?;
        assert_eq!(
            (
                page["changes"].as_array().map(Vec::len),
                page["complete"].clone()
            ),
            (Some(3), json!(false))
        );
        let rest = export(&conn, "01", page["until"].as_str(), 3)?;
        assert_eq!(
            (
                rest["changes"].as_array().map(Vec::len),
                rest["complete"].clone()
            ),
            (Some(2), json!(true))
        );
        Ok(())
    }
}

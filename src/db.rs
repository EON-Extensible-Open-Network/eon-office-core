// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The database: opening it, migrating it, backing it up, restoring it.
//!
//! Migrations are ordered SQL scripts, applied in one transaction each. The
//! schema version lives in SQLite's `user_version`. A database written by a
//! *newer* core is refused instead of opened: an older binary that does not
//! know a table would silently drop meaning from it.

use crate::error::{Error, Kind, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

/// Each entry upgrades the schema by one version. Never edit a released entry;
/// append a new one.
const MIGRATIONS: &[&str] = &[
    // 1 -- the first schema.
    r"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

    CREATE TABLE apps (
        id TEXT PRIMARY KEY,
        manifest TEXT NOT NULL,
        path TEXT,
        token_hash TEXT NOT NULL,
        granted TEXT NOT NULL DEFAULT '[]',
        installed_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE users (
        id TEXT PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        display_name TEXT NOT NULL,
        locale TEXT,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE orgs (id TEXT PRIMARY KEY, name TEXT NOT NULL, created_at INTEGER NOT NULL);
    CREATE TABLE workspaces (
        id TEXT PRIMARY KEY,
        org_id TEXT REFERENCES orgs(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE teams (
        id TEXT PRIMARY KEY,
        workspace_id TEXT NOT NULL REFERENCES workspaces(id) ON DELETE CASCADE,
        name TEXT NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE memberships (
        user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        scope_kind TEXT NOT NULL,
        scope_id TEXT NOT NULL,
        role TEXT NOT NULL,
        PRIMARY KEY (user_id, scope_kind, scope_id)
    );

    CREATE TABLE docs (
        app TEXT NOT NULL,
        collection TEXT NOT NULL,
        id TEXT NOT NULL,
        body TEXT,
        rev INTEGER NOT NULL,
        hlc TEXT NOT NULL,
        deleted INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (app, collection, id)
    );
    CREATE INDEX docs_hlc ON docs(hlc);
    CREATE TABLE conflicts (
        id INTEGER PRIMARY KEY,
        app TEXT NOT NULL,
        collection TEXT NOT NULL,
        doc_id TEXT NOT NULL,
        body TEXT,
        deleted INTEGER NOT NULL,
        hlc TEXT NOT NULL,
        winner_hlc TEXT NOT NULL,
        at INTEGER NOT NULL
    );

    CREATE TABLE settings (
        scope TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (scope, key)
    );
    CREATE TABLE themes (id TEXT PRIMARY KEY, body TEXT NOT NULL);

    CREATE TABLE notifications (
        id INTEGER PRIMARY KEY,
        app TEXT NOT NULL,
        title TEXT NOT NULL,
        body TEXT NOT NULL,
        urgency TEXT NOT NULL,
        created_at INTEGER NOT NULL,
        read INTEGER NOT NULL DEFAULT 0
    );

    CREATE TABLE search_docs (
        app TEXT NOT NULL,
        id TEXT NOT NULL,
        title TEXT NOT NULL,
        snippet TEXT NOT NULL,
        visibility TEXT NOT NULL,
        workspace TEXT,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (app, id)
    );
    CREATE TABLE search_terms (
        term TEXT NOT NULL,
        app TEXT NOT NULL,
        id TEXT NOT NULL,
        weight INTEGER NOT NULL,
        PRIMARY KEY (term, app, id)
    ) WITHOUT ROWID;
    CREATE INDEX search_terms_doc ON search_terms(app, id);

    CREATE TABLE jobs (
        app TEXT NOT NULL,
        name TEXT NOT NULL,
        payload TEXT NOT NULL,
        next_at INTEGER NOT NULL,
        every_ms INTEGER,
        created_at INTEGER NOT NULL,
        PRIMARY KEY (app, name)
    );

    CREATE TABLE audit (
        seq INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        actor TEXT NOT NULL,
        action TEXT NOT NULL,
        target TEXT NOT NULL,
        detail TEXT NOT NULL,
        prev TEXT NOT NULL,
        hash TEXT NOT NULL
    );
    ",
];

/// The schema version this binary writes.
#[must_use]
pub fn schema_version() -> i64 {
    MIGRATIONS.len() as i64
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Open (creating if needed) and migrate.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// A private in-memory database, migrated. For tests.
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn)?;
    Ok(conn)
}

fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    let known = schema_version();
    if current > known {
        return Err(Error::new(
            Kind::Incompatible,
            format!(
                "this database has schema {current}, written by a newer core; this core knows {known}"
            ),
        ));
    }
    for (index, script) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as i64 + 1;
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(script)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

/// Copy the live database to `target` with SQLite's online backup, so the
/// core does not have to stop. The copy is checked before it is reported.
pub fn backup(conn: &Connection, target: &Path) -> Result<()> {
    if target.exists() {
        return Err(Error::params(
            "the backup target already exists; refusing to overwrite it",
        ));
    }
    let mut dest = Connection::open(target)?;
    {
        let backup = rusqlite::backup::Backup::new(conn, &mut dest)?;
        backup.run_to_completion(256, Duration::from_millis(5), None)?;
    }
    drop(dest);
    check(target)
}

/// Is `path` a sound database this core can open?
pub fn check(path: &Path) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::new(
            Kind::Incompatible,
            "the backup failed its integrity check",
        ));
    }
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > schema_version() || version == 0 {
        return Err(Error::new(
            Kind::Incompatible,
            format!(
                "the backup has schema {version}; this core knows 1..={}",
                schema_version()
            ),
        ));
    }
    Ok(())
}

/// Replace the database at `live` with `from`. The core must not be running.
/// The previous file is kept beside it, renamed, never deleted.
pub fn restore(live: &Path, from: &Path, stamp: u64) -> Result<Option<std::path::PathBuf>> {
    check(from)?;
    let mut kept = None;
    if live.exists() {
        // Fold the write-ahead log into the main file first, so the kept copy
        // is complete on its own.
        {
            let conn = Connection::open(live)?;
            conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        }
        let aside = live.with_extension(format!("before-restore-{stamp}.sqlite"));
        std::fs::rename(live, &aside)?;
        kept = Some(aside);
    }
    for suffix in ["-wal", "-shm"] {
        let mut side = live.as_os_str().to_owned();
        side.push(suffix);
        let _ = std::fs::remove_file(std::path::PathBuf::from(side));
    }
    std::fs::copy(from, live)?;
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_apply_once_and_record_the_version() -> Result<()> {
        let conn = open_in_memory()?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        assert_eq!(version, schema_version());
        migrate(&conn)?; // a second run is a no-op
        Ok(())
    }

    #[test]
    fn a_database_from_a_newer_core_is_refused() -> Result<()> {
        let conn = open_in_memory()?;
        conn.pragma_update(None, "user_version", schema_version() + 1)?;
        let error = migrate(&conn).err().map(|e| e.kind);
        assert_eq!(error, Some(Kind::Incompatible));
        Ok(())
    }
}

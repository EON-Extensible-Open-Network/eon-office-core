// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The two database files: opening them, migrating them, backing them up,
//! restoring them.
//!
//! `shared.sqlite` is what every EON Office application on the machine meets
//! in; `apps/<id>/data.sqlite` is one application's documents
//! (`eon-office-spec/docs/shared-store.md`). Each has its own schema history:
//! ordered SQL scripts, applied in one transaction each, the version kept in
//! SQLite's `user_version`. A file written by a *newer* EON Office is refused
//! instead of opened: an older program that does not know a table would
//! silently drop meaning from it.

use crate::error::{Error, Kind, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::Path;
use std::time::Duration;

/// Which of the two files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Schema {
    Shared,
    Data,
}

impl Schema {
    fn migrations(self) -> &'static [&'static str] {
        match self {
            Schema::Shared => SHARED,
            Schema::Data => DATA,
        }
    }

    /// The schema version this library writes.
    #[must_use]
    pub fn version(self) -> i64 {
        self.migrations().len() as i64
    }
}

/// `shared.sqlite`. Each entry upgrades the schema by one version. Never edit
/// a released entry; append a new one.
const SHARED: &[&str] = &[
    // 1 -- the first schema.
    r"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

    CREATE TABLE registry (
        id TEXT PRIMARY KEY,
        manifest TEXT NOT NULL,
        path TEXT,
        revoked TEXT NOT NULL DEFAULT '[]',
        registered_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );

    CREATE TABLE settings (
        scope TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        PRIMARY KEY (scope, key)
    );
    CREATE TABLE themes (id TEXT PRIMARY KEY, body TEXT NOT NULL);

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

    CREATE TABLE activity (
        seq INTEGER PRIMARY KEY,
        at INTEGER NOT NULL,
        app TEXT NOT NULL,
        item TEXT NOT NULL,
        title TEXT NOT NULL,
        kind TEXT NOT NULL
    );

    CREATE TABLE public (
        app TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        updated_at INTEGER NOT NULL,
        PRIMARY KEY (app, key)
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
    ",
];

/// `apps/<id>/data.sqlite`. Only that application opens it.
const DATA: &[&str] = &[
    // 1 -- the first schema.
    r"
    CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

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
    ",
];

/// A transaction that takes the write lock at once. Every read-then-write in
/// `shared.sqlite` uses one: several processes write it, and a deferred
/// transaction that has read cannot wait for the write lock -- it fails.
pub fn immediate(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    )?)
}

fn configure(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    Ok(())
}

/// Open (creating if needed) and migrate.
pub fn open(path: &Path, schema: Schema) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn, schema)?;
    Ok(conn)
}

/// A private in-memory database, migrated. For tests.
pub fn open_in_memory(schema: Schema) -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&conn, schema)?;
    Ok(conn)
}

fn user_version(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn migrate(conn: &Connection, schema: Schema) -> Result<()> {
    let known = schema.version();
    let refuse = |current: i64| {
        Error::new(
            Kind::Incompatible,
            format!(
                "this database has schema {current}, written by a newer EON Office; this one knows {known}"
            ),
        )
    };
    let current = user_version(conn)?;
    if current > known {
        return Err(refuse(current));
    }
    if current == known {
        return Ok(());
    }
    // Two applications starting at once may both try; the lock decides, and
    // the second finds the work done.
    let tx = immediate(conn)?;
    let current = user_version(&tx)?;
    if current > known {
        return Err(refuse(current));
    }
    for (index, script) in schema
        .migrations()
        .iter()
        .enumerate()
        .skip(current as usize)
    {
        tx.execute_batch(script)?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}

/// Copy the live database to `target` with SQLite's online backup, so the
/// applications do not have to stop. The copy is checked before it is
/// reported.
pub fn backup(conn: &Connection, target: &Path, schema: Schema) -> Result<()> {
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
    check(target, schema)
}

/// Is `path` a sound database of this schema that this library can open?
pub fn check(path: &Path, schema: Schema) -> Result<()> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let integrity: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::new(
            Kind::Incompatible,
            "the backup failed its integrity check",
        ));
    }
    let version = user_version(&conn)?;
    if version > schema.version() || version == 0 {
        return Err(Error::new(
            Kind::Incompatible,
            format!(
                "the backup has schema {version}; this library knows 1..={}",
                schema.version()
            ),
        ));
    }
    Ok(())
}

/// Replace the database at `live` with `from`. Nothing may have it open.
/// The previous file is kept beside it, renamed, never deleted.
pub fn restore(
    live: &Path,
    from: &Path,
    schema: Schema,
    stamp: u64,
) -> Result<Option<std::path::PathBuf>> {
    check(from, schema)?;
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
        for schema in [Schema::Shared, Schema::Data] {
            let conn = open_in_memory(schema)?;
            assert_eq!(user_version(&conn)?, schema.version());
            migrate(&conn, schema)?; // a second run is a no-op
        }
        Ok(())
    }

    #[test]
    fn a_database_from_a_newer_version_is_refused() -> Result<()> {
        let conn = open_in_memory(Schema::Data)?;
        conn.pragma_update(None, "user_version", Schema::Data.version() + 1)?;
        let error = migrate(&conn, Schema::Data).err().map(|e| e.kind);
        assert_eq!(error, Some(Kind::Incompatible));
        Ok(())
    }

    #[test]
    fn a_backup_restores_and_keeps_the_previous_file() -> Result<()> {
        let dir = std::env::temp_dir().join(format!(
            "eon-office-db-{}-{}",
            std::process::id(),
            crate::util::now_ms()
        ));
        std::fs::create_dir_all(&dir)?;
        let live = dir.join("data.sqlite");
        let conn = open(&live, Schema::Data)?;
        conn.execute("INSERT INTO meta (key, value) VALUES ('k', 'before')", [])?;
        backup(&conn, &dir.join("copy.sqlite"), Schema::Data)?;
        assert!(backup(&conn, &dir.join("copy.sqlite"), Schema::Data).is_err());
        conn.execute("UPDATE meta SET value = 'after' WHERE key = 'k'", [])?;
        drop(conn);
        assert!(check(&dir.join("copy.sqlite"), Schema::Data).is_ok());
        let kept = restore(&live, &dir.join("copy.sqlite"), Schema::Data, 7)?;
        assert!(kept.is_some_and(|k| k.is_file()));
        let back = open(&live, Schema::Data)?;
        let value: String =
            back.query_row("SELECT value FROM meta WHERE key = 'k'", [], |r| r.get(0))?;
        assert_eq!(value, "before");
        drop(back);
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The audit log: an append-only hash chain.
//!
//! What goes in: changes of authority and of shape -- an application
//! installed, granted, revoked, launched, crashed; the directory changed; a
//! preference set; a sync imported; a backup taken; a request refused for a
//! missing capability. What never goes in: what anyone *read* or *wrote*
//! (madde 31, madde 36). The log says an application was allowed to keep
//! documents, not what the documents said.
//!
//! Each entry's hash covers the previous entry's hash, so editing or deleting
//! a row in the middle breaks every hash after it, and `verify` says where.
//! It is tamper-*evident*, not tamper-proof: someone who can rewrite the file
//! can rewrite the whole chain. Shipping the head hash elsewhere (a server, a
//! printout) is what turns this into proof, and is left to the deployment.

use crate::error::Result;
use crate::util::{now_ms, sha256_hex};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

fn entry_hash(
    prev: &str,
    seq: i64,
    at: i64,
    actor: &str,
    action: &str,
    target: &str,
    detail: &str,
) -> String {
    let line = format!("{prev}\n{seq}\n{at}\n{actor}\n{action}\n{target}\n{detail}");
    sha256_hex(line.as_bytes())
}

/// Append one entry. `actor` is `owner`, `app:<id>` or `core`.
pub fn record(
    conn: &Connection,
    actor: &str,
    action: &str,
    target: &str,
    detail: &Value,
) -> Result<()> {
    let tx = crate::db::immediate(conn)?;
    let last: Option<(i64, String)> = tx
        .query_row(
            "SELECT seq, hash FROM audit ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (seq, prev) = match last {
        Some((seq, hash)) => (seq + 1, hash),
        None => (1, GENESIS.to_owned()),
    };
    let at = i64::try_from(now_ms()).unwrap_or(i64::MAX);
    let detail = detail.to_string();
    let hash = entry_hash(&prev, seq, at, actor, action, target, &detail);
    tx.execute(
        "INSERT INTO audit (seq, at, actor, action, target, detail, prev, hash)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![seq, at, actor, action, target, detail, prev, hash],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn list(conn: &Connection, after: i64, limit: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT seq, at, actor, action, target, detail, hash FROM audit
         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after, limit], |row| {
        let detail: String = row.get(5)?;
        Ok(json!({
            "seq": row.get::<_, i64>(0)?,
            "at": row.get::<_, i64>(1)?,
            "actor": row.get::<_, String>(2)?,
            "action": row.get::<_, String>(3)?,
            "target": row.get::<_, String>(4)?,
            "detail": serde_json::from_str::<Value>(&detail).unwrap_or(Value::Null),
            "hash": row.get::<_, String>(6)?,
        }))
    })?;
    let entries: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
    Ok(json!({ "entries": entries }))
}

/// Walk the whole chain. `broken_at` is the first entry whose hash, or whose
/// link to the previous entry, does not hold.
pub fn verify(conn: &Connection) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT seq, at, actor, action, target, detail, prev, hash FROM audit ORDER BY seq",
    )?;
    let mut rows = stmt.query([])?;
    let mut expected_prev = GENESIS.to_owned();
    let mut expected_seq = 1i64;
    let mut count = 0i64;
    while let Some(row) = rows.next()? {
        let seq: i64 = row.get(0)?;
        let prev: String = row.get(6)?;
        let hash: String = row.get(7)?;
        let recomputed = entry_hash(
            &prev,
            seq,
            row.get(1)?,
            &row.get::<_, String>(2)?,
            &row.get::<_, String>(3)?,
            &row.get::<_, String>(4)?,
            &row.get::<_, String>(5)?,
        );
        if seq != expected_seq || prev != expected_prev || recomputed != hash {
            return Ok(json!({ "ok": false, "entries": count, "broken_at": expected_seq }));
        }
        expected_prev = hash;
        expected_seq += 1;
        count += 1;
    }
    Ok(json!({ "ok": true, "entries": count, "head": expected_prev }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open_in_memory, Schema};

    #[test]
    fn an_edited_entry_breaks_the_chain_where_it_was_edited() -> Result<()> {
        let conn = open_in_memory(Schema::Shared)?;
        for n in 0..5 {
            record(&conn, "owner", "test", &format!("t{n}"), &json!({}))?;
        }
        assert_eq!(verify(&conn)?["ok"], true);
        conn.execute("UPDATE audit SET target = 'forged' WHERE seq = 3", [])?;
        let report = verify(&conn)?;
        assert_eq!(report["ok"], false);
        assert_eq!(report["broken_at"], 3);
        Ok(())
    }

    #[test]
    fn a_deleted_entry_breaks_the_chain_too() -> Result<()> {
        let conn = open_in_memory(Schema::Shared)?;
        for n in 0..4 {
            record(&conn, "core", "test", &format!("t{n}"), &json!({ "n": n }))?;
        }
        conn.execute("DELETE FROM audit WHERE seq = 2", [])?;
        assert_eq!(verify(&conn)?["broken_at"], 2);
        Ok(())
    }
}

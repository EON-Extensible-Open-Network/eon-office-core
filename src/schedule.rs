// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Background jobs.
//!
//! An application asks to be woken -- once at a time, or every so often --
//! and the core sends it `job.due` when the time comes. The core runs no
//! application code: a job is a wake-up call carrying the payload the
//! application left, and the work happens in the application's own process.
//!
//! If the application is not running when a job falls due, the job waits
//! and is delivered when it next connects. A repeating job that missed
//! several runs is delivered **once**, not once per missed run: a laptop
//! that slept for a week should not wake to a hundred backups.

use crate::error::{Error, Kind, Result};
use crate::util::check_name;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

pub const MAX_JOBS_PER_APP: i64 = 100;
const MAX_PAYLOAD_BYTES: usize = 16 * 1024;

pub fn add(
    conn: &Connection,
    app: &str,
    name: &str,
    first_at: u64,
    every_ms: Option<u64>,
    payload: &Value,
    now: u64,
) -> Result<Value> {
    check_name("name", name)?;
    if let Some(every) = every_ms {
        if every < 1_000 {
            return Err(Error::params("a repeating job runs at most once a second"));
        }
    }
    let text = payload.to_string();
    if text.len() > MAX_PAYLOAD_BYTES {
        return Err(Error::new(Kind::Limit, "a job payload is at most 16 KiB"));
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM jobs WHERE app = ?1 AND name != ?2",
        params![app, name],
        |r| r.get(0),
    )?;
    if count >= MAX_JOBS_PER_APP {
        return Err(Error::new(Kind::Limit, "at most 100 jobs per application"));
    }
    let as_i64 = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO jobs (app, name, payload, next_at, every_ms, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (app, name) DO UPDATE SET payload = excluded.payload,
           next_at = excluded.next_at, every_ms = excluded.every_ms",
        params![app, name, text, as_i64(first_at), every_ms.map(as_i64), as_i64(now)],
    )?;
    Ok(json!({ "name": name, "next_at": first_at, "every_ms": every_ms }))
}

pub fn list(conn: &Connection, app: &str) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT name, payload, next_at, every_ms FROM jobs WHERE app = ?1 ORDER BY next_at",
    )?;
    let rows = stmt.query_map(params![app], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, Option<i64>>(3)?,
        ))
    })?;
    let mut jobs = Vec::new();
    for row in rows {
        let (name, payload, next_at, every_ms) = row?;
        jobs.push(json!({
            "name": name, "payload": serde_json::from_str::<Value>(&payload)?,
            "next_at": next_at, "every_ms": every_ms,
        }));
    }
    Ok(json!({ "jobs": jobs }))
}

pub fn cancel(conn: &Connection, app: &str, name: &str) -> Result<()> {
    if conn.execute(
        "DELETE FROM jobs WHERE app = ?1 AND name = ?2",
        params![app, name],
    )? == 0
    {
        return Err(Error::not_found("no such job"));
    }
    Ok(())
}

pub fn forget_app(conn: &Connection, app: &str) -> Result<()> {
    conn.execute("DELETE FROM jobs WHERE app = ?1", params![app])?;
    Ok(())
}

pub struct Due {
    pub app: String,
    pub name: String,
    pub payload: Value,
    pub scheduled_at: i64,
}

/// Take the jobs due by `now` for applications in `running`, advancing
/// repeating jobs past `now` and removing one-shot jobs.
pub fn take_due(conn: &Connection, now: u64, running: &dyn Fn(&str) -> bool) -> Result<Vec<Due>> {
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT app, name, payload, next_at, every_ms FROM jobs WHERE next_at <= ?1 ORDER BY next_at",
    )?;
    let rows = stmt.query_map(params![now], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, Option<i64>>(4)?,
        ))
    })?;
    let mut due = Vec::new();
    let mut updates = Vec::new();
    for row in rows {
        let (app, name, payload, next_at, every) = row?;
        if !running(&app) {
            continue;
        }
        match every {
            Some(every) if every > 0 => {
                let skipped = (now - next_at) / every + 1;
                updates.push((app.clone(), name.clone(), Some(next_at + skipped * every)));
            }
            _ => updates.push((app.clone(), name.clone(), None)),
        }
        due.push(Due {
            app,
            name,
            payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            scheduled_at: next_at,
        });
    }
    drop(stmt);
    for (app, name, next) in updates {
        match next {
            Some(next) => conn.execute(
                "UPDATE jobs SET next_at = ?3 WHERE app = ?1 AND name = ?2",
                params![app, name, next],
            )?,
            None => conn.execute(
                "DELETE FROM jobs WHERE app = ?1 AND name = ?2",
                params![app, name],
            )?,
        };
    }
    Ok(due)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[test]
    fn missed_runs_coalesce_into_one() -> Result<()> {
        let conn = open_in_memory()?;
        add(
            &conn,
            "a.b",
            "backup",
            1_000,
            Some(1_000),
            &json!({"k": 1}),
            0,
        )?;
        // A week of missed seconds later: one delivery, and the next run is in the future.
        let due = take_due(&conn, 1_000_000, &|_| true)?;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].payload["k"], 1);
        let next = list(&conn, "a.b")?["jobs"][0]["next_at"]
            .as_i64()
            .unwrap_or_default();
        assert!(next > 1_000_000 && next <= 1_001_000, "{next}");
        Ok(())
    }

    #[test]
    fn a_job_waits_for_its_application_and_one_shots_go_away() -> Result<()> {
        let conn = open_in_memory()?;
        add(&conn, "a.b", "remind", 10, None, &Value::Null, 0)?;
        assert!(
            take_due(&conn, 100, &|_| false)?.is_empty(),
            "not running: waits"
        );
        assert_eq!(take_due(&conn, 200, &|_| true)?.len(), 1);
        assert_eq!(list(&conn, "a.b")?["jobs"], json!([]));
        Ok(())
    }

    #[test]
    fn too_frequent_jobs_are_refused() -> Result<()> {
        let conn = open_in_memory()?;
        assert!(add(&conn, "a.b", "spin", 0, Some(10), &Value::Null, 0).is_err());
        Ok(())
    }
}

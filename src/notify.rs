// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! One notification list for the whole suite.
//!
//! Applications post; the person reads them in one place, whichever
//! application is open. An application sees and dismisses only its own; the
//! owner's shell sees all of them. Posting is rate-limited per application,
//! because an application that floods the list makes every other
//! application's notifications invisible.

use crate::error::{Error, Kind, Result};
use crate::util::now_ms;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};

pub const PER_MINUTE: usize = 30;

#[derive(Default)]
pub struct RateLimit {
    recent: HashMap<String, VecDeque<u64>>,
}

impl RateLimit {
    /// Record a post by `app` at `now`; refuse past `PER_MINUTE` in any 60 s.
    pub fn allow(&mut self, app: &str, now: u64) -> Result<()> {
        let window = self.recent.entry(app.to_owned()).or_default();
        while window
            .front()
            .is_some_and(|t| now.saturating_sub(*t) >= 60_000)
        {
            window.pop_front();
        }
        if window.len() >= PER_MINUTE {
            return Err(Error::new(
                Kind::RateLimited,
                "at most 30 notifications a minute",
            ));
        }
        window.push_back(now);
        Ok(())
    }
}

pub fn post(conn: &Connection, app: &str, title: &str, body: &str, urgency: &str) -> Result<Value> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Error::params("`title` must be 1-200 characters"));
    }
    if body.chars().count() > 2000 {
        return Err(Error::params("`body` is at most 2000 characters"));
    }
    if !matches!(urgency, "low" | "normal" | "high") {
        return Err(Error::params("`urgency` is low, normal or high"));
    }
    let at = i64::try_from(now_ms()).unwrap_or(0);
    conn.execute(
        "INSERT INTO notifications (app, title, body, urgency, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![app, title, body, urgency, at],
    )?;
    let id = conn.last_insert_rowid();
    Ok(json!({ "id": id, "app": app, "title": title, "body": body, "urgency": urgency, "at": at }))
}

/// `app`: `Some` lists that application's own; `None` (the owner) lists all.
pub fn list(conn: &Connection, app: Option<&str>, unread_only: bool, limit: i64) -> Result<Value> {
    let mut stmt = conn.prepare(
        "SELECT id, app, title, body, urgency, created_at, read FROM notifications
         WHERE (?1 IS NULL OR app = ?1) AND (?2 = 0 OR read = 0)
         ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![app, i64::from(unread_only), limit], |r| {
        Ok(json!({
            "id": r.get::<_, i64>(0)?,
            "app": r.get::<_, String>(1)?,
            "title": r.get::<_, String>(2)?,
            "body": r.get::<_, String>(3)?,
            "urgency": r.get::<_, String>(4)?,
            "at": r.get::<_, i64>(5)?,
            "read": r.get::<_, i64>(6)? != 0,
        }))
    })?;
    let items: Vec<Value> = rows.collect::<std::result::Result<_, _>>()?;
    Ok(json!({ "items": items }))
}

pub fn mark_read(conn: &Connection, app: Option<&str>, id: i64) -> Result<()> {
    let changed = conn.execute(
        "UPDATE notifications SET read = 1 WHERE id = ?1 AND (?2 IS NULL OR app = ?2)",
        params![id, app],
    )?;
    if changed == 0 {
        return Err(Error::not_found("no such notification"));
    }
    Ok(())
}

pub fn forget_app(conn: &Connection, app: &str) -> Result<()> {
    conn.execute("DELETE FROM notifications WHERE app = ?1", params![app])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[test]
    fn the_thirty_first_post_in_a_minute_is_refused() {
        let mut limit = RateLimit::default();
        for n in 0..PER_MINUTE as u64 {
            assert!(limit.allow("a.b", 1_000 + n).is_ok());
        }
        assert!(limit.allow("a.b", 2_000).is_err());
        assert!(limit.allow("c.d", 2_000).is_ok(), "others are not affected");
        assert!(limit.allow("a.b", 61_001).is_ok(), "the window moves");
    }

    #[test]
    fn an_application_sees_only_its_own() -> Result<()> {
        let conn = open_in_memory()?;
        let mine = post(&conn, "a.b", "hello", "", "normal")?;
        post(&conn, "c.d", "other", "", "high")?;
        assert_eq!(
            list(&conn, Some("a.b"), false, 10)?["items"]
                .as_array()
                .map(Vec::len),
            Some(1)
        );
        assert_eq!(
            list(&conn, None, false, 10)?["items"]
                .as_array()
                .map(Vec::len),
            Some(2)
        );
        let id = mine["id"].as_i64().unwrap_or_default();
        assert!(
            mark_read(&conn, Some("c.d"), id).is_err(),
            "not theirs to dismiss"
        );
        mark_read(&conn, Some("a.b"), id)?;
        assert_eq!(list(&conn, Some("a.b"), true, 10)?["items"], json!([]));
        Ok(())
    }
}

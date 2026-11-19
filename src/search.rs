// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The shared search index.
//!
//! Applications put *entries* into it -- an id, a title, some text -- and
//! say whether each entry is `private` (only that application finds it) or
//! `shared` (any application allowed to search finds it). A search across
//! the suite only ever sees what its owners chose to share, plus the
//! searcher's own entries.
//!
//! The index keeps folded terms (see `text`) and a short snippet, not the
//! full text: it exists to *find* things, and the application that owns an
//! entry is asked for the rest. Terms are matched as prefixes, so typing
//! "rapo" finds "rapor"; every term in the query must match.

use crate::error::{Error, Kind, Result};
use crate::text::terms;
use crate::util::{check_name, now_ms};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::collections::HashMap;

const MAX_TEXT_BYTES: usize = 256 * 1024;
const SNIPPET_CHARS: usize = 160;
const MAX_QUERY_TERMS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Private,
    Shared,
}

impl Visibility {
    pub fn parse(text: Option<&str>) -> Result<Self> {
        match text {
            None | Some("private") => Ok(Visibility::Private),
            Some("shared") => Ok(Visibility::Shared),
            Some(_) => Err(Error::params("`visibility` is private or shared")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Visibility::Private => "private",
            Visibility::Shared => "shared",
        }
    }
}

pub fn index(
    conn: &Connection,
    app: &str,
    id: &str,
    title: &str,
    text: &str,
    visibility: Visibility,
    workspace: Option<&str>,
) -> Result<()> {
    check_name("id", id)?;
    if title.chars().count() > 300 {
        return Err(Error::params("`title` is at most 300 characters"));
    }
    if text.len() > MAX_TEXT_BYTES {
        return Err(Error::new(
            Kind::Limit,
            "indexed text is at most 256 KiB per entry",
        ));
    }
    let mut weights: HashMap<String, i64> = HashMap::new();
    for term in terms(title) {
        *weights.entry(term).or_default() += 3;
    }
    for term in terms(text) {
        *weights.entry(term).or_default() += 1;
    }
    let snippet: String = text.chars().take(SNIPPET_CHARS).collect();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM search_terms WHERE app = ?1 AND id = ?2",
        params![app, id],
    )?;
    tx.execute(
        "INSERT INTO search_docs (app, id, title, snippet, visibility, workspace, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (app, id) DO UPDATE SET title = excluded.title, snippet = excluded.snippet,
           visibility = excluded.visibility, workspace = excluded.workspace, updated_at = excluded.updated_at",
        params![app, id, title, snippet, visibility.name(), workspace, i64::try_from(now_ms()).unwrap_or(0)],
    )?;
    {
        let mut insert =
            tx.prepare("INSERT INTO search_terms (term, app, id, weight) VALUES (?1, ?2, ?3, ?4)")?;
        for (term, weight) in &weights {
            insert.execute(params![term, app, id, weight])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn remove(conn: &Connection, app: &str, id: &str) -> Result<bool> {
    conn.execute(
        "DELETE FROM search_terms WHERE app = ?1 AND id = ?2",
        params![app, id],
    )?;
    Ok(conn.execute(
        "DELETE FROM search_docs WHERE app = ?1 AND id = ?2",
        params![app, id],
    )? > 0)
}

pub fn forget_app(conn: &Connection, app: &str) -> Result<()> {
    conn.execute("DELETE FROM search_terms WHERE app = ?1", params![app])?;
    conn.execute("DELETE FROM search_docs WHERE app = ?1", params![app])?;
    Ok(())
}

/// Search as `viewer`: `Some(app)` sees its own entries and everyone's shared
/// ones; `None` is the owner and sees everything.
pub fn query(conn: &Connection, viewer: Option<&str>, query: &str, limit: i64) -> Result<Value> {
    let mut wanted = terms(query);
    wanted.dedup();
    if wanted.is_empty() {
        return Ok(json!({ "hits": [] }));
    }
    wanted.truncate(MAX_QUERY_TERMS);

    // For each query term: the entries with a term starting with it, and the
    // best weight among those terms.
    let mut scores: Option<HashMap<(String, String), i64>> = None;
    let mut stmt = conn.prepare(
        "SELECT app, id, MAX(weight) FROM search_terms
         WHERE term >= ?1 AND term < ?2 GROUP BY app, id",
    )?;
    for term in &wanted {
        let upper = format!("{term}\u{10ffff}");
        let rows = stmt.query_map(params![term, upper], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                r.get::<_, i64>(2)?,
            ))
        })?;
        let mut found = HashMap::new();
        for row in rows {
            let (key, weight) = row?;
            found.insert(key, weight);
        }
        scores = Some(match scores {
            None => found,
            Some(previous) => previous
                .into_iter()
                .filter_map(|(key, score)| found.get(&key).map(|w| (key, score + w)))
                .collect(),
        });
    }

    let mut ranked: Vec<((String, String), i64)> = scores.unwrap_or_default().into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    let mut hits = Vec::new();
    let mut doc = conn.prepare(
        "SELECT title, snippet, visibility, workspace FROM search_docs WHERE app = ?1 AND id = ?2",
    )?;
    for ((app, id), score) in ranked {
        if hits.len() as i64 >= limit {
            break;
        }
        let row = doc.query_row(params![app, id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        });
        let Ok((title, snippet, visibility, workspace)) = row else {
            continue;
        };
        let visible = match viewer {
            None => true,
            Some(viewer) => viewer == app || visibility == "shared",
        };
        if visible {
            hits.push(json!({
                "app": app, "id": id, "title": title, "snippet": snippet,
                "workspace": workspace, "score": score,
            }));
        }
    }
    Ok(json!({ "hits": hits }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[test]
    fn turkish_text_is_found_however_it_is_typed() -> Result<()> {
        let conn = open_in_memory()?;
        index(
            &conn,
            "org.a.notes",
            "n1",
            "İstanbul toplantısı",
            "Işık ve ses düzeni",
            Visibility::Private,
            None,
        )?;
        for q in ["istanbul", "İSTANBUL", "ISTANBUL", "isik", "IŞIK", "toplan"] {
            let hits = query(&conn, Some("org.a.notes"), q, 10)?;
            assert_eq!(hits["hits"].as_array().map(Vec::len), Some(1), "query {q}");
        }
        Ok(())
    }

    #[test]
    fn private_entries_stay_with_their_application() -> Result<()> {
        let conn = open_in_memory()?;
        index(
            &conn,
            "org.a.notes",
            "n1",
            "budget draft",
            "",
            Visibility::Private,
            None,
        )?;
        index(
            &conn,
            "org.a.sheets",
            "s1",
            "budget 2027",
            "",
            Visibility::Shared,
            None,
        )?;
        let from_notes = query(&conn, Some("org.a.notes"), "budget", 10)?;
        assert_eq!(from_notes["hits"].as_array().map(Vec::len), Some(2));
        let from_mail = query(&conn, Some("org.a.mail"), "budget", 10)?;
        assert_eq!(from_mail["hits"].as_array().map(Vec::len), Some(1));
        assert_eq!(from_mail["hits"][0]["app"], "org.a.sheets");
        let owner = query(&conn, None, "budget", 10)?;
        assert_eq!(owner["hits"].as_array().map(Vec::len), Some(2));
        Ok(())
    }

    #[test]
    fn every_term_must_match_and_titles_rank_first() -> Result<()> {
        let conn = open_in_memory()?;
        index(
            &conn,
            "org.a.notes",
            "body",
            "misc",
            "quarterly report",
            Visibility::Shared,
            None,
        )?;
        index(
            &conn,
            "org.a.notes",
            "title",
            "quarterly report",
            "",
            Visibility::Shared,
            None,
        )?;
        index(
            &conn,
            "org.a.notes",
            "half",
            "quarterly",
            "",
            Visibility::Shared,
            None,
        )?;
        let hits = query(&conn, None, "quarterly report", 10)?;
        let ids: Vec<&str> = hits["hits"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|h| h["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["title", "body"]);
        Ok(())
    }

    #[test]
    fn reindexing_replaces_and_removing_removes() -> Result<()> {
        let conn = open_in_memory()?;
        index(
            &conn,
            "org.a.notes",
            "n",
            "apples",
            "",
            Visibility::Private,
            None,
        )?;
        index(
            &conn,
            "org.a.notes",
            "n",
            "pears",
            "",
            Visibility::Private,
            None,
        )?;
        assert_eq!(query(&conn, None, "apples", 10)?["hits"], json!([]));
        assert!(remove(&conn, "org.a.notes", "n")?);
        assert_eq!(query(&conn, None, "pears", 10)?["hits"], json!([]));
        Ok(())
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The directory: people, organisations, workspaces, teams, and who holds
//! which role where -- and the one function that answers "may this person
//! do this here?".
//!
//! Roles are *scoped*, the lesson of madde 17: a role is held in a scope, and
//! the same person can be an admin of one workspace and a guest in another.
//! A role in an organisation reaches its workspaces, and a role in a
//! workspace reaches its teams; the strongest role on the path wins.
//!
//! The permission sets are defined in exactly one place (`grants`) and tested
//! there, so no application has its own idea of what "admin" means.

use crate::error::{Error, Result};
use crate::util::{now_ms, random_hex};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Role {
    Guest,
    Member,
    Admin,
    Owner,
}

impl Role {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "guest" => Ok(Role::Guest),
            "member" => Ok(Role::Member),
            "admin" => Ok(Role::Admin),
            "owner" => Ok(Role::Owner),
            _ => Err(Error::params("a role is guest, member, admin or owner")),
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Role::Guest => "guest",
            Role::Member => "member",
            Role::Admin => "admin",
            Role::Owner => "owner",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
    Share,
    Delete,
    ManageMembers,
    ManageSettings,
    /// Delete the scope itself, or make someone an owner.
    Administer,
}

impl Permission {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "read" => Permission::Read,
            "write" => Permission::Write,
            "share" => Permission::Share,
            "delete" => Permission::Delete,
            "manage_members" => Permission::ManageMembers,
            "manage_settings" => Permission::ManageSettings,
            "administer" => Permission::Administer,
            _ => {
                return Err(Error::params(
                    "a permission is read, write, share, delete, manage_members, manage_settings or administer",
                ))
            }
        })
    }
}

/// The single definition of what each role may do.
#[must_use]
pub fn grants(role: Role, permission: Permission) -> bool {
    use Permission as P;
    match role {
        Role::Owner => true,
        Role::Admin => !matches!(permission, P::Administer),
        Role::Member => matches!(permission, P::Read | P::Write | P::Share),
        Role::Guest => matches!(permission, P::Read),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeKind {
    Org,
    Workspace,
    Team,
}

impl ScopeKind {
    pub fn parse(text: &str) -> Result<Self> {
        match text {
            "org" => Ok(ScopeKind::Org),
            "workspace" => Ok(ScopeKind::Workspace),
            "team" => Ok(ScopeKind::Team),
            _ => Err(Error::params("a scope kind is org, workspace or team")),
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ScopeKind::Org => "org",
            ScopeKind::Workspace => "workspace",
            ScopeKind::Team => "team",
        }
    }
}

fn new_id(prefix: &str) -> Result<String> {
    Ok(format!("{prefix}_{}", random_hex(8)?))
}

fn now() -> i64 {
    i64::try_from(now_ms()).unwrap_or(i64::MAX)
}

fn check_label(what: &str, text: &str, max: usize) -> Result<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > max || trimmed.chars().any(char::is_control)
    {
        return Err(Error::params(format!(
            "`{what}` must be 1-{max} printable characters"
        )));
    }
    Ok(trimmed.to_owned())
}

fn check_username(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::params(
            "a user name is 1-64 characters of a-z 0-9 . _ -",
        ))
    }
}

/// The person who owns this installation, created on first start.
pub fn ensure_owner(conn: &Connection) -> Result<String> {
    let existing: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = 'owner_user'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = new_id("u")?;
    conn.execute(
        "INSERT INTO users (id, name, display_name, created_at) VALUES (?1, 'owner', 'Owner', ?2)",
        params![id, now()],
    )?;
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('owner_user', ?1)",
        params![id],
    )?;
    Ok(id)
}

pub fn user(conn: &Connection, id: &str) -> Result<Value> {
    conn.query_row(
        "SELECT id, name, display_name, locale FROM users WHERE id = ?1",
        params![id],
        |r| {
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "display_name": r.get::<_, String>(2)?,
                "locale": r.get::<_, Option<String>>(3)?,
            }))
        },
    )
    .optional()?
    .ok_or_else(|| Error::not_found("no such user"))
}

pub fn create_user(conn: &Connection, name: &str, display_name: Option<&str>) -> Result<Value> {
    check_username(name)?;
    let display = check_label("display_name", display_name.unwrap_or(name), 100)?;
    let taken: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM users WHERE name = ?1)",
        params![name],
        |r| r.get(0),
    )?;
    if taken {
        return Err(Error::new(
            crate::error::Kind::Conflict,
            "that user name is taken",
        ));
    }
    let id = new_id("u")?;
    conn.execute(
        "INSERT INTO users (id, name, display_name, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, display, now()],
    )?;
    user(conn, &id)
}

pub fn update_profile(
    conn: &Connection,
    id: &str,
    display_name: Option<&str>,
    locale: Option<&str>,
) -> Result<Value> {
    user(conn, id)?;
    if let Some(display) = display_name {
        let display = check_label("display_name", display, 100)?;
        conn.execute(
            "UPDATE users SET display_name = ?1 WHERE id = ?2",
            params![display, id],
        )?;
    }
    if let Some(locale) = locale {
        crate::settings::check_locale(locale)?;
        conn.execute(
            "UPDATE users SET locale = ?1 WHERE id = ?2",
            params![locale, id],
        )?;
    }
    user(conn, id)
}

fn exists(conn: &Connection, kind: ScopeKind, id: &str) -> Result<bool> {
    let sql = match kind {
        ScopeKind::Org => "SELECT EXISTS (SELECT 1 FROM orgs WHERE id = ?1)",
        ScopeKind::Workspace => "SELECT EXISTS (SELECT 1 FROM workspaces WHERE id = ?1)",
        ScopeKind::Team => "SELECT EXISTS (SELECT 1 FROM teams WHERE id = ?1)",
    };
    Ok(conn.query_row(sql, params![id], |r| r.get(0))?)
}

/// Create an organisation; its creator becomes its owner.
pub fn create_org(conn: &Connection, name: &str, creator: &str) -> Result<Value> {
    let name = check_label("name", name, 100)?;
    let id = new_id("o")?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO orgs (id, name, created_at) VALUES (?1, ?2, ?3)",
        params![id, name, now()],
    )?;
    tx.execute(
        "INSERT INTO memberships (user_id, scope_kind, scope_id, role) VALUES (?1, 'org', ?2, 'owner')",
        params![creator, id],
    )?;
    tx.commit()?;
    Ok(json!({ "id": id, "name": name }))
}

/// Create a workspace, inside an organisation or standing alone (a person's
/// own). A standalone workspace's creator becomes its owner.
pub fn create_workspace(
    conn: &Connection,
    name: &str,
    org: Option<&str>,
    creator: &str,
) -> Result<Value> {
    let name = check_label("name", name, 100)?;
    if let Some(org) = org {
        if !exists(conn, ScopeKind::Org, org)? {
            return Err(Error::not_found("no such organisation"));
        }
    }
    let id = new_id("w")?;
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO workspaces (id, org_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, org, name, now()],
    )?;
    if org.is_none() {
        tx.execute(
            "INSERT INTO memberships (user_id, scope_kind, scope_id, role) VALUES (?1, 'workspace', ?2, 'owner')",
            params![creator, id],
        )?;
    }
    tx.commit()?;
    Ok(json!({ "id": id, "name": name, "org": org }))
}

pub fn create_team(conn: &Connection, workspace: &str, name: &str) -> Result<Value> {
    let name = check_label("name", name, 100)?;
    if !exists(conn, ScopeKind::Workspace, workspace)? {
        return Err(Error::not_found("no such workspace"));
    }
    let id = new_id("t")?;
    conn.execute(
        "INSERT INTO teams (id, workspace_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, workspace, name, now()],
    )?;
    Ok(json!({ "id": id, "name": name, "workspace": workspace }))
}

pub fn add_member(
    conn: &Connection,
    user_id: &str,
    kind: ScopeKind,
    scope: &str,
    role: Role,
) -> Result<()> {
    user(conn, user_id)?;
    if !exists(conn, kind, scope)? {
        return Err(Error::not_found(format!("no such {}", kind.name())));
    }
    conn.execute(
        "INSERT INTO memberships (user_id, scope_kind, scope_id, role) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, scope_kind, scope_id) DO UPDATE SET role = excluded.role",
        params![user_id, kind.name(), scope, role.name()],
    )?;
    Ok(())
}

pub fn remove_member(
    conn: &Connection,
    user_id: &str,
    kind: ScopeKind,
    scope: &str,
) -> Result<bool> {
    let removed = conn.execute(
        "DELETE FROM memberships WHERE user_id = ?1 AND scope_kind = ?2 AND scope_id = ?3",
        params![user_id, kind.name(), scope],
    )?;
    Ok(removed > 0)
}

/// The scope and every scope above it, nearest first.
fn path(conn: &Connection, kind: ScopeKind, id: &str) -> Result<Vec<(ScopeKind, String)>> {
    let mut out = vec![(kind, id.to_owned())];
    let mut workspace = None;
    match kind {
        ScopeKind::Team => {
            workspace = conn
                .query_row(
                    "SELECT workspace_id FROM teams WHERE id = ?1",
                    params![id],
                    |r| r.get::<_, String>(0),
                )
                .optional()?;
        }
        ScopeKind::Workspace => workspace = Some(id.to_owned()),
        ScopeKind::Org => {}
    }
    if let Some(ws) = workspace {
        if kind == ScopeKind::Team {
            out.push((ScopeKind::Workspace, ws.clone()));
        }
        let org: Option<Option<String>> = conn
            .query_row(
                "SELECT org_id FROM workspaces WHERE id = ?1",
                params![ws],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(Some(org)) = org {
            out.push((ScopeKind::Org, org));
        }
    }
    Ok(out)
}

/// May `user` do `permission` in the scope? Answers with the role that
/// decided it and where that role is held.
pub fn check(
    conn: &Connection,
    user_id: &str,
    permission: Permission,
    kind: ScopeKind,
    scope: &str,
) -> Result<Value> {
    if !exists(conn, kind, scope)? {
        return Err(Error::not_found(format!("no such {}", kind.name())));
    }
    let mut best: Option<(Role, ScopeKind, String)> = None;
    for (k, id) in path(conn, kind, scope)? {
        let role: Option<String> = conn
            .query_row(
                "SELECT role FROM memberships WHERE user_id = ?1 AND scope_kind = ?2 AND scope_id = ?3",
                params![user_id, k.name(), id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(role) = role {
            let role = Role::parse(&role)?;
            if best.as_ref().is_none_or(|(b, _, _)| role > *b) {
                best = Some((role, k, id));
            }
        }
    }
    Ok(match best {
        Some((role, k, id)) => json!({
            "allowed": grants(role, permission),
            "role": role.name(),
            "via": { "kind": k.name(), "id": id },
        }),
        None => json!({ "allowed": false, "role": null, "via": null }),
    })
}

/// Everything, for the owner's administration view and `directory.read`.
pub fn snapshot(conn: &Connection) -> Result<Value> {
    fn rows(conn: &Connection, sql: &str, columns: &[&str]) -> Result<Vec<Value>> {
        let mut stmt = conn.prepare(sql)?;
        let mapped = stmt.query_map([], |row| {
            let mut object = serde_json::Map::new();
            for (i, column) in columns.iter().enumerate() {
                let value: Option<String> = row.get(i)?;
                object.insert(
                    (*column).to_owned(),
                    value.map_or(Value::Null, Value::String),
                );
            }
            Ok(Value::Object(object))
        })?;
        Ok(mapped.collect::<std::result::Result<_, _>>()?)
    }
    Ok(json!({
        "users": rows(conn, "SELECT id, name, display_name, locale FROM users ORDER BY name",
            &["id", "name", "display_name", "locale"])?,
        "orgs": rows(conn, "SELECT id, name FROM orgs ORDER BY name", &["id", "name"])?,
        "workspaces": rows(conn, "SELECT id, org_id, name FROM workspaces ORDER BY name",
            &["id", "org", "name"])?,
        "teams": rows(conn, "SELECT id, workspace_id, name FROM teams ORDER BY name",
            &["id", "workspace", "name"])?,
        "memberships": rows(conn,
            "SELECT user_id, scope_kind, scope_id, role FROM memberships ORDER BY scope_kind, scope_id",
            &["user", "kind", "scope", "role"])?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    #[test]
    fn the_permission_table() {
        use Permission as P;
        let all = [
            P::Read,
            P::Write,
            P::Share,
            P::Delete,
            P::ManageMembers,
            P::ManageSettings,
            P::Administer,
        ];
        assert!(all.iter().all(|p| grants(Role::Owner, *p)));
        assert!(!grants(Role::Admin, P::Administer));
        assert!(grants(Role::Admin, P::ManageMembers));
        assert!(grants(Role::Member, P::Write) && !grants(Role::Member, P::Delete));
        assert!(grants(Role::Guest, P::Read) && !grants(Role::Guest, P::Write));
    }

    #[test]
    fn roles_reach_down_and_the_strongest_wins() -> Result<()> {
        let conn = open_in_memory()?;
        let owner = ensure_owner(&conn)?;
        let ada = create_user(&conn, "ada", Some("Ada"))?["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let org = create_org(&conn, "Acme", &owner)?["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let ws = create_workspace(&conn, "Finance", Some(&org), &owner)?["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let team = create_team(&conn, &ws, "Payroll")?["id"]
            .as_str()
            .unwrap_or_default()
            .to_owned();

        // Nothing held: refused.
        let r = check(&conn, &ada, Permission::Read, ScopeKind::Team, &team)?;
        assert_eq!(r["allowed"], false);

        // A guest in the organisation can read the team, through the org.
        add_member(&conn, &ada, ScopeKind::Org, &org, Role::Guest)?;
        let r = check(&conn, &ada, Permission::Read, ScopeKind::Team, &team)?;
        assert_eq!(
            (r["allowed"].clone(), r["via"]["kind"].clone()),
            (json!(true), json!("org"))
        );
        let r = check(&conn, &ada, Permission::Write, ScopeKind::Team, &team)?;
        assert_eq!(r["allowed"], false);

        // An admin of the workspace outranks the org guest role below it.
        add_member(&conn, &ada, ScopeKind::Workspace, &ws, Role::Admin)?;
        let r = check(
            &conn,
            &ada,
            Permission::ManageMembers,
            ScopeKind::Team,
            &team,
        )?;
        assert_eq!(
            (r["allowed"].clone(), r["role"].clone()),
            (json!(true), json!("admin"))
        );
        // ...but only below it: in the org she is still a guest.
        let r = check(&conn, &ada, Permission::Write, ScopeKind::Org, &org)?;
        assert_eq!(r["allowed"], false);

        // The org's owner administers everything inside it.
        let r = check(
            &conn,
            &owner,
            Permission::Administer,
            ScopeKind::Team,
            &team,
        )?;
        assert_eq!(r["allowed"], true);
        Ok(())
    }

    #[test]
    fn user_names_are_unique_and_narrow() -> Result<()> {
        let conn = open_in_memory()?;
        create_user(&conn, "ada", None)?;
        assert!(create_user(&conn, "ada", None).is_err());
        assert!(create_user(&conn, "Ada Lovelace", None).is_err());
        Ok(())
    }
}

// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The running core: connections, sessions, the method table, the event bus,
//! calls between applications, the process supervisor and the job ticker.
//!
//! **Transport.** TCP on the loopback interface, one JSON-RPC 2.0 object per
//! line (`eon-office-spec/docs/protocol.md`). Every connection has a reader
//! thread and a writer thread; the writer drains a bounded queue. A slow or
//! stuck application fills *its own* queue and loses *its own* events -- it
//! cannot block the core or another application.
//!
//! **Isolation.** Each application is its own process. A crash closes its
//! connection; the core fails that application's pending calls with
//! `app_unavailable`, tells subscribers, and carries on. Nothing an
//! application sends can take another application's session with it.
//!
//! **Authority** is checked on every call against the registry as it is
//! *now*, not as it was at connect time, so a revoked capability stops
//! working on the next request.

use crate::apps::{self, Installed};
use crate::directory::{self, Permission, Role, ScopeKind};
use crate::error::{Error, Kind, Result};
use crate::hlc::Clock;
use crate::manifest::{Capability, Manifest, Restart, API_VERSION};
use crate::notify::{self, RateLimit};
use crate::util::{
    check_app_id, check_name, limit, now_ms, opt_bool, opt_str, opt_u64, random_hex, same_secret,
    str_list, str_param,
};
use crate::{audit, db, schedule, search, settings, storage};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

/// The longest line the core reads. Documents are capped at 512 KiB, so a
/// request carrying one fits with room to spare.
pub const MAX_LINE: usize = 1024 * 1024;
const QUEUE: usize = 1024;
const MAX_SESSIONS: usize = 256;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 10_000;
const MAX_CALL_TIMEOUT_MS: u64 = 60_000;
const MAX_IMPORT: usize = 10_000;

/// Lock a mutex, surviving a panic in another thread that held it: the data
/// behind these locks stays consistent at every await-free step.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Principal {
    Owner,
    App(String),
}

impl Principal {
    fn actor(&self) -> String {
        match self {
            Principal::Owner => "owner".into(),
            Principal::App(id) => format!("app:{id}"),
        }
    }

    fn caller(&self) -> String {
        match self {
            Principal::Owner => "owner".into(),
            Principal::App(id) => id.clone(),
        }
    }
}

struct Session {
    principal: Principal,
    out: mpsc::SyncSender<String>,
    subs: Vec<String>,
    dropped: u64,
}

struct Pending {
    target: u64,
    reply: mpsc::Sender<std::result::Result<Value, Value>>,
}

#[derive(Default)]
struct ChildEntry {
    handle: Option<Arc<Mutex<std::process::Child>>>,
    state: &'static str,
    stop_requested: bool,
    restarts: VecDeque<u64>,
    last_code: Option<i32>,
}

enum Audience<'a> {
    /// Sessions subscribed to a matching pattern.
    Subscribers,
    /// Owner sessions subscribed to a matching pattern.
    OwnerSubscribers,
    /// Every session of one application, subscribed or not.
    App(&'a str),
    /// Every session.
    Everyone,
}

pub struct Core {
    home: PathBuf,
    db: Mutex<Connection>,
    clock: Mutex<Clock>,
    sessions: Mutex<HashMap<u64, Session>>,
    pending: Mutex<HashMap<String, Pending>>,
    children: Mutex<HashMap<String, ChildEntry>>,
    rate: Mutex<RateLimit>,
    counter: AtomicU64,
    owner_token: String,
    owner_user: String,
    node: String,
    address: Mutex<Option<SocketAddr>>,
    stopping: AtomicBool,
}

// ---- files under the home directory --------------------------------------

#[must_use]
pub fn db_path(home: &Path) -> PathBuf {
    home.join("core.sqlite")
}

#[must_use]
pub fn endpoint_path(home: &Path) -> PathBuf {
    home.join("runtime").join("endpoint.json")
}

#[must_use]
pub fn owner_token_path(home: &Path) -> PathBuf {
    home.join("runtime").join("owner.token")
}

#[must_use]
pub fn app_token_path(home: &Path, app: &str) -> PathBuf {
    home.join("apps").join(app).join("token")
}

/// Write a secret readable only by this user where the platform allows it.
fn write_secret(path: &Path, secret: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    // On Windows the home directory sits in the user's profile, whose ACL
    // already excludes other users; there is no portable std call for more.
    Ok(())
}

/// Is a core already answering at the address in `endpoint.json`?
#[must_use]
pub fn running_address(home: &Path) -> Option<SocketAddr> {
    let text = std::fs::read_to_string(endpoint_path(home)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let address: SocketAddr = value.get("address")?.as_str()?.parse().ok()?;
    TcpStream::connect_timeout(&address, Duration::from_millis(500))
        .ok()
        .map(|_| address)
}

impl Core {
    /// Open the home directory: database, node identity, owner, owner token.
    pub fn open(home: &Path) -> Result<Arc<Core>> {
        std::fs::create_dir_all(home.join("runtime"))?;
        std::fs::create_dir_all(home.join("apps"))?;
        let conn = db::open(&db_path(home))?;
        let node: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'node'", [], |r| {
                r.get(0)
            })
            .optional()?;
        let node = if let Some(node) = node {
            node
        } else {
            let node = random_hex(8)?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('node', ?1)",
                rusqlite::params![node],
            )?;
            node
        };
        let owner_user = directory::ensure_owner(&conn)?;
        let token_path = owner_token_path(home);
        let owner_token = match std::fs::read_to_string(&token_path) {
            Ok(token) if token.trim().len() == 64 => token.trim().to_owned(),
            _ => {
                let token = random_hex(32)?;
                write_secret(&token_path, &token)?;
                token
            }
        };
        let mut clock = Clock::new(node.clone());
        // A restart must never stamp behind what is already stored.
        let latest: Option<String> =
            conn.query_row("SELECT MAX(hlc) FROM docs", [], |r| r.get(0))?;
        if let Some(latest) = latest {
            clock.observe(&latest, now_ms())?;
        }
        Ok(Arc::new(Core {
            home: home.to_owned(),
            db: Mutex::new(conn),
            clock: Mutex::new(clock),
            sessions: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            children: Mutex::new(HashMap::new()),
            rate: Mutex::new(RateLimit::default()),
            counter: AtomicU64::new(1),
            owner_token,
            owner_user,
            node,
            address: Mutex::new(None),
            stopping: AtomicBool::new(false),
        }))
    }

    fn next(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::Relaxed)
    }

    fn audit(&self, actor: &str, action: &str, target: &str, detail: &Value) {
        let conn = lock(&self.db);
        if let Err(error) = audit::record(&conn, actor, action, target, detail) {
            eprintln!("eon-office-core: could not write the audit log: {error}");
        }
    }

    fn info(&self) -> Value {
        json!({
            "core_version": env!("CARGO_PKG_VERSION"),
            "api": API_VERSION,
            "node": self.node,
            "schema": db::schema_version(),
        })
    }

    // ---- delivery ---------------------------------------------------------

    fn send(&self, sid: u64, value: &Value) -> bool {
        let line = value.to_string();
        let mut sessions = lock(&self.sessions);
        let Some(session) = sessions.get_mut(&sid) else {
            return false;
        };
        if session.out.try_send(line).is_ok() {
            true
        } else {
            session.dropped += 1;
            false
        }
    }

    fn deliver(
        &self,
        topic: &str,
        from: &str,
        payload: &Value,
        audience: &Audience,
        except: Option<u64>,
    ) {
        let line = json!({
            "jsonrpc": "2.0",
            "method": "event",
            "params": { "topic": topic, "from": from, "payload": payload, "at": now_ms() },
        })
        .to_string();
        let mut sessions = lock(&self.sessions);
        for (sid, session) in sessions.iter_mut() {
            if Some(*sid) == except {
                continue;
            }
            let subscribed = || session.subs.iter().any(|p| matches_topic(p, topic));
            let wanted = match audience {
                Audience::Subscribers => subscribed(),
                Audience::OwnerSubscribers => session.principal == Principal::Owner && subscribed(),
                Audience::App(id) => session.principal == Principal::App((*id).to_owned()),
                Audience::Everyone => true,
            };
            if wanted && session.out.try_send(line.clone()).is_err() {
                session.dropped += 1;
            }
        }
    }

    fn sessions_of(&self, app: &str) -> Vec<u64> {
        let target = Principal::App(app.to_owned());
        let mut sids: Vec<u64> = lock(&self.sessions)
            .iter()
            .filter(|(_, s)| s.principal == target)
            .map(|(sid, _)| *sid)
            .collect();
        sids.sort_unstable();
        sids
    }

    fn is_connected(&self, app: &str) -> bool {
        !self.sessions_of(app).is_empty()
    }

    // ---- the supervisor ---------------------------------------------------

    fn launch(self: &Arc<Self>, id: &str) -> Result<Value> {
        let app = {
            let conn = lock(&self.db);
            apps::get(&conn, id)?.ok_or_else(|| Error::not_found("no such application"))?
        };
        let entry = app.manifest.entry.clone().ok_or_else(|| {
            Error::params("this application has no `entry`; it starts on its own and connects")
        })?;
        {
            let children = lock(&self.children);
            if children.get(id).is_some_and(|c| c.state == "running") {
                return Err(Error::new(Kind::Conflict, "already running"));
            }
        }
        let token = std::fs::read_to_string(app_token_path(&self.home, id)).map_err(|_| {
            Error::internal("the application's token file is missing; reinstall it")
        })?;
        let address = lock(&self.address)
            .map(|a| a.to_string())
            .unwrap_or_default();
        let workdir = app
            .path
            .as_ref()
            .map_or_else(|| self.home.join("apps").join(id), PathBuf::from);
        let program = {
            let candidate = workdir.join(&entry.command);
            if Path::new(&entry.command).is_relative() && candidate.is_file() {
                candidate
            } else {
                PathBuf::from(&entry.command)
            }
        };
        let child = std::process::Command::new(&program)
            .args(&entry.args)
            .current_dir(&workdir)
            .env("EON_OFFICE_ADDRESS", &address)
            .env("EON_OFFICE_APP", id)
            .env("EON_OFFICE_TOKEN", token.trim())
            .env("EON_OFFICE_HOME", &self.home)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .spawn()
            .map_err(|e| Error::new(Kind::AppUnavailable, format!("could not start it: {e}")))?;
        let pid = child.id();
        let handle = Arc::new(Mutex::new(child));
        {
            let mut children = lock(&self.children);
            let slot = children.entry(id.to_owned()).or_default();
            slot.handle = Some(Arc::clone(&handle));
            slot.state = "running";
            slot.stop_requested = false;
        }
        self.deliver(
            "core/app.started",
            "core",
            &json!({ "app": id, "pid": pid }),
            &Audience::Subscribers,
            None,
        );
        let core = Arc::clone(self);
        let id = id.to_owned();
        let restart = app.manifest.restart;
        thread::spawn(move || core.watch(&id, &handle, restart));
        Ok(json!({ "pid": pid }))
    }

    fn watch(
        self: Arc<Self>,
        id: &str,
        handle: &Arc<Mutex<std::process::Child>>,
        restart: Restart,
    ) {
        let status = loop {
            if let Ok(Some(status)) = lock(handle).try_wait() {
                break status;
            }
            thread::sleep(Duration::from_millis(100));
        };
        let code = status.code();
        let (stopped_on_purpose, attempt) = {
            let mut children = lock(&self.children);
            let slot = children.entry(id.to_owned()).or_default();
            slot.handle = None;
            slot.last_code = code;
            let now = now_ms();
            while slot
                .restarts
                .front()
                .is_some_and(|t| now.saturating_sub(*t) > 60_000)
            {
                slot.restarts.pop_front();
            }
            if slot.stop_requested || status.success() {
                slot.state = "stopped";
                (true, None)
            } else if restart == Restart::OnFailure
                && slot.restarts.len() < 3
                && !self.stopping.load(Ordering::Relaxed)
            {
                slot.state = "restarting";
                slot.restarts.push_back(now);
                (false, Some(slot.restarts.len()))
            } else {
                slot.state = if restart == Restart::OnFailure {
                    "failed"
                } else {
                    "crashed"
                };
                (false, None)
            }
        };
        if stopped_on_purpose {
            self.deliver(
                "core/app.stopped",
                "core",
                &json!({ "app": id, "code": code }),
                &Audience::Subscribers,
                None,
            );
            return;
        }
        self.audit("core", "app.crashed", id, &json!({ "code": code }));
        self.deliver(
            "core/app.crashed",
            "core",
            &json!({ "app": id, "code": code }),
            &Audience::Subscribers,
            None,
        );
        if let Some(attempt) = attempt {
            // 1 s, 2 s, 4 s.
            thread::sleep(Duration::from_millis(1000 << (attempt - 1)));
            let still_wanted = !self.stopping.load(Ordering::Relaxed)
                && lock(&self.children)
                    .get(id)
                    .is_some_and(|c| c.state == "restarting");
            if still_wanted {
                if let Err(error) = self.launch(id) {
                    eprintln!("eon-office-core: restarting {id} failed: {error}");
                    if let Some(slot) = lock(&self.children).get_mut(id) {
                        slot.state = "failed";
                    }
                }
            }
        }
    }

    fn stop(&self, id: &str) -> Result<Value> {
        let handle = {
            let mut children = lock(&self.children);
            let slot = children.get_mut(id).filter(|c| c.handle.is_some());
            let Some(slot) = slot else {
                return Err(Error::new(Kind::Conflict, "not running under the core"));
            };
            slot.stop_requested = true;
            slot.handle.clone()
        };
        // Ask first; an application gets three seconds to save its work.
        let ask = json!({ "jsonrpc": "2.0", "method": "shutdown", "params": {} });
        for sid in self.sessions_of(id) {
            self.send(sid, &ask);
        }
        if let Some(handle) = handle {
            for _ in 0..30 {
                if matches!(lock(&handle).try_wait(), Ok(Some(_))) {
                    return Ok(json!({ "stopped": true, "forced": false }));
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = lock(&handle).kill();
            return Ok(json!({ "stopped": true, "forced": true }));
        }
        Ok(json!({ "stopped": true, "forced": false }))
    }

    fn process_state(&self, id: &str) -> Value {
        let children = lock(&self.children);
        match children.get(id) {
            Some(c) => json!({
                "state": c.state, "restarts": c.restarts.len(), "last_code": c.last_code,
            }),
            None => Value::Null,
        }
    }

    // ---- the job ticker ---------------------------------------------------

    fn tick_jobs(&self) {
        let connected: Vec<String> = lock(&self.sessions)
            .values()
            .filter_map(|s| match &s.principal {
                Principal::App(id) => Some(id.clone()),
                Principal::Owner => None,
            })
            .collect();
        let due = {
            let conn = lock(&self.db);
            schedule::take_due(&conn, now_ms(), &|app| connected.iter().any(|c| c == app))
        };
        match due {
            Ok(due) => {
                for job in due {
                    let note = json!({
                        "jsonrpc": "2.0", "method": "job.due",
                        "params": { "name": job.name, "payload": job.payload, "scheduled_at": job.scheduled_at },
                    });
                    if let Some(sid) = self.sessions_of(&job.app).first() {
                        self.send(*sid, &note);
                    }
                }
            }
            Err(error) => eprintln!("eon-office-core: job ticker: {error}"),
        }
    }
}

fn matches_topic(pattern: &str, topic: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => prefix.ends_with('/') && topic.starts_with(prefix),
        None => pattern == topic,
    }
}

fn check_pattern(pattern: &str) -> Result<()> {
    if pattern == "*" {
        return Ok(());
    }
    let (namespace, rest) = pattern.split_once('/').ok_or_else(|| {
        Error::params("a pattern is `*`, `<namespace>/*` or `<namespace>/<event>`")
    })?;
    if namespace != "core" {
        check_app_id(namespace)?;
    }
    if rest != "*" {
        check_name("event", rest)?;
    }
    Ok(())
}

// ---- requests ---------------------------------------------------------------

struct Ctx<'a> {
    core: &'a Arc<Core>,
    sid: u64,
    principal: &'a Principal,
}

impl Ctx<'_> {
    fn owner(&self) -> Result<()> {
        if *self.principal == Principal::Owner {
            Ok(())
        } else {
            Err(Error::owner_only())
        }
    }

    /// The calling application, as the registry has it *now*.
    fn app(&self) -> Result<(String, Installed)> {
        match self.principal {
            Principal::App(id) => {
                let conn = lock(&self.core.db);
                let app = apps::get(&conn, id)?.ok_or_else(|| {
                    Error::new(Kind::Unauthenticated, "this application was uninstalled")
                })?;
                Ok((id.clone(), app))
            }
            Principal::Owner => Err(Error::params("this method is called by an application")),
        }
    }

    fn require(&self, capability: &Capability) -> Result<(String, Installed)> {
        let (id, app) = self.app()?;
        if app.has(capability) {
            Ok((id, app))
        } else {
            let name = capability.as_string();
            self.core
                .audit(&self.principal.actor(), "denied", &name, &json!({}));
            Err(Error::missing(&name))
        }
    }

    /// `None` for the owner; the application's id if it holds `capability`.
    fn owner_or(&self, capability: &Capability) -> Result<Option<String>> {
        match self.principal {
            Principal::Owner => Ok(None),
            Principal::App(_) => self.require(capability).map(|(id, _)| Some(id)),
        }
    }

    fn settings_scope(&self) -> String {
        match self.principal {
            Principal::Owner => "owner".into(),
            Principal::App(id) => format!("app:{id}"),
        }
    }
}

fn dispatch(ctx: &Ctx<'_>, method: &str, params: &Value) -> Result<Value> {
    let core = ctx.core;
    match method {
        "session.ping" => Ok(json!({ "pong": now_ms() })),
        "core.info" => Ok(core.info()),
        "core.shutdown" => {
            ctx.owner()?;
            core.stopping.store(true, Ordering::SeqCst);
            core.audit("owner", "core.shutdown", "core", &json!({}));
            if let Some(address) = *lock(&core.address) {
                // Wake the accept loop so it notices.
                let _ = TcpStream::connect_timeout(&address, Duration::from_millis(200));
            }
            Ok(json!({ "stopping": true }))
        }

        // ---- applications ----
        "apps.install" => {
            ctx.owner()?;
            let manifest =
                Manifest::from_json(params.get("manifest").cloned().unwrap_or(Value::Null))?;
            let path = opt_str(params, "path")?;
            let grant = str_list(params, "grant")?;
            let outcome = {
                let conn = lock(&core.db);
                apps::install(&conn, manifest, path, &grant)?
            };
            let id = outcome.installed.manifest.id.clone();
            if let Some(token) = &outcome.token {
                write_secret(&app_token_path(&core.home, &id), token)?;
            }
            core.audit(
                "owner",
                "app.installed",
                &id,
                &json!({
                    "version": outcome.installed.manifest.version,
                    "upgraded_from": outcome.upgraded_from,
                    "granted": outcome.installed.granted_strings(),
                }),
            );
            core.deliver(
                "core/app.installed",
                "core",
                &json!({ "app": id, "version": outcome.installed.manifest.version }),
                &Audience::Subscribers,
                None,
            );
            let mut out = apps::describe(&outcome.installed);
            out["token_file"] = json!(app_token_path(&core.home, &id));
            out["upgraded_from"] = json!(outcome.upgraded_from);
            Ok(out)
        }
        "apps.uninstall" => {
            ctx.owner()?;
            let id = str_param(params, "app")?;
            if lock(&core.children)
                .get(id)
                .is_some_and(|c| c.handle.is_some())
            {
                core.stop(id)?;
            }
            {
                let conn = lock(&core.db);
                apps::uninstall(&conn, id)?;
            }
            let _ = std::fs::remove_dir_all(core.home.join("apps").join(id));
            let leaving = core.sessions_of(id);
            {
                let mut sessions = lock(&core.sessions);
                for sid in &leaving {
                    sessions.remove(sid); // dropping the sender closes the connection's writer
                }
            }
            core.audit("owner", "app.uninstalled", id, &json!({}));
            core.deliver(
                "core/app.uninstalled",
                "core",
                &json!({ "app": id }),
                &Audience::Subscribers,
                None,
            );
            Ok(json!({ "uninstalled": id }))
        }
        "apps.grant" | "apps.revoke" => {
            ctx.owner()?;
            let id = str_param(params, "app")?;
            let list = str_list(params, "capabilities")?;
            let app = {
                let conn = lock(&core.db);
                if method == "apps.grant" {
                    apps::grant(&conn, id, &list)?
                } else {
                    apps::revoke(&conn, id, &list)?
                }
            };
            if method == "apps.revoke" && !app.has(&Capability::EventsSubscribe) {
                for sid in core.sessions_of(id) {
                    if let Some(session) = lock(&core.sessions).get_mut(&sid) {
                        session.subs.clear();
                    }
                }
            }
            let action = if method == "apps.grant" {
                "app.granted"
            } else {
                "app.revoked"
            };
            core.audit("owner", action, id, &json!({ "capabilities": list }));
            Ok(apps::describe(&app))
        }
        "apps.list" => {
            ctx.owner()?;
            let conn = lock(&core.db);
            let mut out = Vec::new();
            for id in apps::ids(&conn)? {
                if let Some(app) = apps::get(&conn, &id)? {
                    let mut entry = apps::describe(&app);
                    entry["connected"] = json!(core.is_connected(&id));
                    entry["process"] = core.process_state(&id);
                    out.push(entry);
                }
            }
            Ok(json!({ "apps": out }))
        }
        "apps.available" => {
            // What an application may integrate with: installed, what it
            // offers, whether it is up. No grants, no paths.
            let conn = lock(&core.db);
            let mut out = Vec::new();
            for id in apps::ids(&conn)? {
                if let Some(app) = apps::get(&conn, &id)? {
                    out.push(json!({
                        "id": id, "name": app.manifest.name, "version": app.manifest.version,
                        "provides": app.manifest.provides, "publishes": app.manifest.publishes,
                        "connected": core.is_connected(&id),
                    }));
                }
            }
            Ok(json!({ "apps": out }))
        }
        "apps.launch" => {
            ctx.owner()?;
            let id = str_param(params, "app")?;
            let out = core.launch(id)?;
            core.audit("owner", "app.launched", id, &json!({}));
            Ok(out)
        }
        "apps.stop" => {
            ctx.owner()?;
            let id = str_param(params, "app")?;
            let out = core.stop(id)?;
            core.audit("owner", "app.stopped", id, &out);
            Ok(out)
        }
        "apps.status" => {
            ctx.owner()?;
            let id = str_param(params, "app")?;
            Ok(
                json!({ "app": id, "connected": core.is_connected(id), "process": core.process_state(id) }),
            )
        }

        // ---- identity and the directory ----
        "identity.me" => {
            let conn = lock(&core.db);
            let user = directory::user(&conn, &core.owner_user)?;
            Ok(match ctx.principal {
                Principal::Owner => json!({ "principal": "owner", "user": user }),
                Principal::App(id) => json!({ "principal": "app", "app": id, "user": user }),
            })
        }
        "profile.update" => {
            ctx.owner()?;
            let conn = lock(&core.db);
            directory::update_profile(
                &conn,
                &core.owner_user,
                opt_str(params, "display_name")?,
                opt_str(params, "locale")?,
            )
        }
        "directory.get" => {
            ctx.owner_or(&Capability::DirectoryRead)?;
            let conn = lock(&core.db);
            directory::snapshot(&conn)
        }
        "access.check" => {
            ctx.owner_or(&Capability::DirectoryRead)?;
            let conn = lock(&core.db);
            directory::check(
                &conn,
                str_param(params, "user")?,
                Permission::parse(str_param(params, "permission")?)?,
                ScopeKind::parse(str_param(params, "kind")?)?,
                str_param(params, "scope")?,
            )
        }
        "directory.create_user"
        | "directory.create_org"
        | "directory.create_workspace"
        | "directory.create_team"
        | "directory.add_member"
        | "directory.remove_member" => {
            ctx.owner()?;
            let out = {
                let conn = lock(&core.db);
                match method {
                    "directory.create_user" => directory::create_user(
                        &conn,
                        str_param(params, "name")?,
                        opt_str(params, "display_name")?,
                    )?,
                    "directory.create_org" => {
                        directory::create_org(&conn, str_param(params, "name")?, &core.owner_user)?
                    }
                    "directory.create_workspace" => directory::create_workspace(
                        &conn,
                        str_param(params, "name")?,
                        opt_str(params, "org")?,
                        &core.owner_user,
                    )?,
                    "directory.create_team" => directory::create_team(
                        &conn,
                        str_param(params, "workspace")?,
                        str_param(params, "name")?,
                    )?,
                    "directory.add_member" => {
                        directory::add_member(
                            &conn,
                            str_param(params, "user")?,
                            ScopeKind::parse(str_param(params, "kind")?)?,
                            str_param(params, "scope")?,
                            Role::parse(str_param(params, "role")?)?,
                        )?;
                        json!({ "added": true })
                    }
                    _ => {
                        let removed = directory::remove_member(
                            &conn,
                            str_param(params, "user")?,
                            ScopeKind::parse(str_param(params, "kind")?)?,
                            str_param(params, "scope")?,
                        )?;
                        json!({ "removed": removed })
                    }
                }
            };
            core.audit(
                "owner",
                method,
                out.get("id").and_then(Value::as_str).unwrap_or("-"),
                params,
            );
            Ok(out)
        }

        // ---- storage ----
        "storage.put" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let conn = lock(&core.db);
            let mut clock = lock(&core.clock);
            storage::put(
                &conn,
                &mut clock,
                &app,
                str_param(params, "collection")?,
                str_param(params, "id")?,
                params.get("doc").unwrap_or(&Value::Null),
                opt_u64(params, "rev")?,
            )
        }
        "storage.get" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let conn = lock(&core.db);
            storage::get(
                &conn,
                &app,
                str_param(params, "collection")?,
                str_param(params, "id")?,
            )
        }
        "storage.delete" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let conn = lock(&core.db);
            let mut clock = lock(&core.clock);
            storage::delete(
                &conn,
                &mut clock,
                &app,
                str_param(params, "collection")?,
                str_param(params, "id")?,
                opt_u64(params, "rev")?,
            )
        }
        "storage.list" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let conn = lock(&core.db);
            storage::list(
                &conn,
                &app,
                str_param(params, "collection")?,
                opt_str(params, "after")?,
                limit(params, 100, 1000)?,
            )
        }
        "storage.conflicts" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let conn = lock(&core.db);
            storage::conflicts(&conn, Some(&app), limit(params, 100, 1000)?)
        }
        "storage.resolve" => {
            let (app, _) = ctx.require(&Capability::Storage)?;
            let id = params
                .get("conflict")
                .and_then(Value::as_i64)
                .ok_or_else(|| Error::params("`conflict` is required"))?;
            let conn = lock(&core.db);
            storage::resolve(&conn, Some(&app), id)?;
            Ok(json!({ "resolved": id }))
        }

        // ---- sync (between devices; the owner's) ----
        "sync.export" => {
            ctx.owner()?;
            let conn = lock(&core.db);
            storage::export(
                &conn,
                &core.node,
                opt_str(params, "since")?,
                limit(params, 1000, 10_000)?,
            )
        }
        "sync.import" => {
            ctx.owner()?;
            let changes = params
                .get("changes")
                .and_then(Value::as_array)
                .ok_or_else(|| Error::params("`changes` is required"))?;
            if changes.len() > MAX_IMPORT {
                return Err(Error::new(Kind::Limit, "at most 10000 changes per import"));
            }
            let (summary, touched) = {
                let conn = lock(&core.db);
                let mut clock = lock(&core.clock);
                storage::import(&conn, &mut clock, changes)?
            };
            core.audit(
                "owner",
                "sync.import",
                params.get("node").and_then(Value::as_str).unwrap_or("-"),
                &summary,
            );
            for (app, collection, id) in touched {
                core.deliver(
                    "core/storage.changed",
                    "core",
                    &json!({ "collection": collection, "id": id }),
                    &Audience::App(&app),
                    None,
                );
            }
            Ok(summary)
        }
        "sync.conflicts" => {
            ctx.owner()?;
            let conn = lock(&core.db);
            storage::conflicts(&conn, opt_str(params, "app")?, limit(params, 100, 1000)?)
        }

        // ---- events ----
        "events.publish" => {
            let (app, installed) = ctx.require(&Capability::EventsPublish)?;
            let event = str_param(params, "event")?;
            if !installed.manifest.publishes.iter().any(|e| e == event) {
                return Err(Error::params(format!(
                    "`{event}` is not in the manifest's `publishes`; every event an application emits is declared"
                )));
            }
            let payload = params.get("payload").cloned().unwrap_or(Value::Null);
            if payload.to_string().len() > 64 * 1024 {
                return Err(Error::new(
                    Kind::Limit,
                    "an event payload is at most 64 KiB",
                ));
            }
            core.deliver(
                &format!("{app}/{event}"),
                &app,
                &payload,
                &Audience::Subscribers,
                Some(ctx.sid),
            );
            Ok(json!({ "published": format!("{app}/{event}") }))
        }
        "events.subscribe" | "events.unsubscribe" => {
            if let Principal::App(_) = ctx.principal {
                ctx.require(&Capability::EventsSubscribe)?;
            }
            let pattern = str_param(params, "pattern")?;
            check_pattern(pattern)?;
            let mut sessions = lock(&core.sessions);
            let session = sessions
                .get_mut(&ctx.sid)
                .ok_or_else(|| Error::internal("no session"))?;
            if method == "events.subscribe" {
                if session.subs.len() >= 64 {
                    return Err(Error::new(
                        Kind::Limit,
                        "at most 64 subscriptions per connection",
                    ));
                }
                if !session.subs.iter().any(|p| p == pattern) {
                    session.subs.push(pattern.to_owned());
                }
            } else {
                session.subs.retain(|p| p != pattern);
            }
            Ok(json!({ "subscriptions": session.subs }))
        }

        // ---- notifications ----
        "notifications.post" => {
            let (app, _) = ctx.require(&Capability::Notifications)?;
            lock(&core.rate).allow(&app, now_ms())?;
            let posted = {
                let conn = lock(&core.db);
                notify::post(
                    &conn,
                    &app,
                    str_param(params, "title")?,
                    opt_str(params, "body")?.unwrap_or(""),
                    opt_str(params, "urgency")?.unwrap_or("normal"),
                )?
            };
            core.deliver(
                "core/notification.posted",
                &app,
                &posted,
                &Audience::OwnerSubscribers,
                None,
            );
            Ok(posted)
        }
        "notifications.list" => {
            let viewer = ctx.owner_or(&Capability::Notifications)?;
            let conn = lock(&core.db);
            notify::list(
                &conn,
                viewer.as_deref(),
                opt_bool(params, "unread_only")?.unwrap_or(false),
                limit(params, 50, 500)?,
            )
        }
        "notifications.mark_read" => {
            let viewer = ctx.owner_or(&Capability::Notifications)?;
            let id = params
                .get("id")
                .and_then(Value::as_i64)
                .ok_or_else(|| Error::params("`id` is required"))?;
            let conn = lock(&core.db);
            notify::mark_read(&conn, viewer.as_deref(), id)?;
            Ok(json!({ "read": id }))
        }

        // ---- search ----
        "search.index" => {
            let (app, _) = ctx.require(&Capability::SearchIndex)?;
            let conn = lock(&core.db);
            search::index(
                &conn,
                &app,
                str_param(params, "id")?,
                opt_str(params, "title")?.unwrap_or(""),
                opt_str(params, "text")?.unwrap_or(""),
                search::Visibility::parse(opt_str(params, "visibility")?)?,
                opt_str(params, "workspace")?,
            )?;
            Ok(json!({ "indexed": true }))
        }
        "search.remove" => {
            let (app, _) = ctx.require(&Capability::SearchIndex)?;
            let conn = lock(&core.db);
            Ok(json!({ "removed": search::remove(&conn, &app, str_param(params, "id")?)? }))
        }
        "search.query" => {
            let viewer = ctx.owner_or(&Capability::SearchQuery)?;
            let conn = lock(&core.db);
            search::query(
                &conn,
                viewer.as_deref(),
                str_param(params, "query")?,
                limit(params, 20, 200)?,
            )
        }

        // ---- background jobs ----
        "schedule.add" => {
            let (app, _) = ctx.require(&Capability::Schedule)?;
            let now = now_ms();
            let first_at = match (opt_u64(params, "at")?, opt_u64(params, "delay_ms")?) {
                (Some(at), None) => at,
                (None, Some(delay)) => now.saturating_add(delay),
                (None, None) => now,
                (Some(_), Some(_)) => {
                    return Err(Error::params("give `at` or `delay_ms`, not both"))
                }
            };
            let conn = lock(&core.db);
            schedule::add(
                &conn,
                &app,
                str_param(params, "name")?,
                first_at,
                opt_u64(params, "every_ms")?,
                params.get("payload").unwrap_or(&Value::Null),
                now,
            )
        }
        "schedule.list" => {
            let (app, _) = ctx.require(&Capability::Schedule)?;
            let conn = lock(&core.db);
            schedule::list(&conn, &app)
        }
        "schedule.cancel" => {
            let (app, _) = ctx.require(&Capability::Schedule)?;
            let conn = lock(&core.db);
            schedule::cancel(&conn, &app, str_param(params, "name")?)?;
            Ok(json!({ "cancelled": true }))
        }

        // ---- settings, preferences, themes ----
        "settings.get" => {
            let conn = lock(&core.db);
            Ok(
                json!({ "value": settings::get(&conn, &ctx.settings_scope(), str_param(params, "key")?)? }),
            )
        }
        "settings.set" => {
            let conn = lock(&core.db);
            settings::set(
                &conn,
                &ctx.settings_scope(),
                str_param(params, "key")?,
                params.get("value").unwrap_or(&Value::Null),
            )?;
            Ok(json!({ "saved": true }))
        }
        "settings.list" => {
            let conn = lock(&core.db);
            settings::list(&conn, &ctx.settings_scope())
        }
        "prefs.get" => {
            let conn = lock(&core.db);
            settings::prefs(&conn)
        }
        "prefs.set" => {
            ctx.owner()?;
            let key = str_param(params, "key")?;
            let value = params.get("value").cloned().unwrap_or(Value::Null);
            {
                let conn = lock(&core.db);
                settings::set_pref(&conn, key, &value)?;
            }
            core.audit("owner", "prefs.set", key, &value);
            core.deliver(
                "core/prefs.changed",
                "core",
                &json!({ "key": key, "value": value }),
                &Audience::Everyone,
                None,
            );
            Ok(json!({ "saved": true }))
        }
        "themes.list" => {
            let conn = lock(&core.db);
            settings::themes(&conn)
        }
        "themes.put" => {
            ctx.owner()?;
            let theme = params.get("theme").cloned().unwrap_or(Value::Null);
            let conn = lock(&core.db);
            settings::put_theme(&conn, &theme)
        }

        // ---- audit and backup ----
        "audit.list" => {
            ctx.owner()?;
            let after =
                opt_u64(params, "after")?.map_or(0, |a| i64::try_from(a).unwrap_or(i64::MAX));
            let conn = lock(&core.db);
            audit::list(&conn, after, limit(params, 100, 1000)?)
        }
        "audit.verify" => {
            ctx.owner()?;
            let conn = lock(&core.db);
            audit::verify(&conn)
        }
        "backup.create" => {
            ctx.owner()?;
            let target = PathBuf::from(str_param(params, "path")?);
            if !target.is_absolute() {
                return Err(Error::params("`path` must be absolute"));
            }
            {
                let conn = lock(&core.db);
                db::backup(&conn, &target)?;
            }
            core.audit(
                "owner",
                "backup.created",
                &target.display().to_string(),
                &json!({}),
            );
            Ok(json!({ "path": target, "schema": db::schema_version() }))
        }

        _ => Err(Error::new(
            Kind::MethodNotFound,
            format!("no method `{method}`"),
        )),
    }
}

/// `apps.call`: forwarded to the target's connection as `invoke`, answered on
/// a thread of its own so the caller's connection keeps reading (and can
/// itself be invoked meanwhile -- two applications calling each other do not
/// deadlock).
fn call(core: &Arc<Core>, caller_sid: u64, principal: &Principal, id: Value, params: &Value) {
    let core = Arc::clone(core);
    let principal = principal.clone();
    let params = params.clone();
    thread::spawn(move || {
        let outcome = (|| -> Result<Value> {
            let target = str_param(&params, "app")?.to_owned();
            let method = str_param(&params, "method")?.to_owned();
            let timeout = opt_u64(&params, "timeout_ms")?
                .unwrap_or(DEFAULT_CALL_TIMEOUT_MS)
                .clamp(1, MAX_CALL_TIMEOUT_MS);
            let ctx = Ctx {
                core: &core,
                sid: caller_sid,
                principal: &principal,
            };
            if let Principal::App(_) = principal {
                ctx.require(&Capability::Call(target.clone()))?;
            }
            let installed = {
                let conn = lock(&core.db);
                apps::get(&conn, &target)?
            };
            let installed = installed.ok_or_else(|| {
                Error::new(Kind::AppUnavailable, "that application is not installed")
                    .with(json!({ "app": target }))
            })?;
            if !installed.manifest.provides_method(&method) {
                return Err(Error::new(
                    Kind::MethodNotFound,
                    format!("{target} does not provide `{method}`"),
                ));
            }
            let target_sid = *core.sessions_of(&target).first().ok_or_else(|| {
                Error::new(Kind::AppUnavailable, "that application is not running")
                    .with(json!({ "app": target }))
            })?;
            let invoke_id = format!("i{}", core.next());
            let (tx, rx) = mpsc::channel();
            lock(&core.pending).insert(
                invoke_id.clone(),
                Pending {
                    target: target_sid,
                    reply: tx,
                },
            );
            let request = json!({
                "jsonrpc": "2.0", "id": invoke_id, "method": "invoke",
                "params": { "from": principal.caller(), "method": method, "params": params.get("params").cloned().unwrap_or(Value::Null) },
            });
            if !core.send(target_sid, &request) {
                lock(&core.pending).remove(&invoke_id);
                return Err(
                    Error::new(Kind::AppUnavailable, "that application is not keeping up")
                        .with(json!({ "app": target })),
                );
            }
            let answer = rx.recv_timeout(Duration::from_millis(timeout));
            lock(&core.pending).remove(&invoke_id);
            match answer {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(error))
                    if error.get("data").and_then(|d| d.get("code"))
                        == Some(&json!("app_unavailable")) =>
                {
                    Err(Error::new(
                        Kind::AppUnavailable,
                        "that application went away during the call",
                    )
                    .with(json!({ "app": target })))
                }
                Ok(Err(error)) => Err(Error::new(
                    Kind::Remote,
                    "the application answered with an error",
                )
                .with(json!({ "app": target, "error": error }))),
                Err(_) => Err(
                    Error::new(Kind::Timeout, "the application did not answer in time")
                        .with(json!({ "app": target })),
                ),
            }
        })();
        respond(&core, caller_sid, id, outcome);
    });
}

fn respond(core: &Core, sid: u64, id: Value, outcome: Result<Value>) {
    let message = match outcome {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
    };
    core.send(sid, &message);
}

// ---- connections --------------------------------------------------------------

enum Frame {
    Line(Vec<u8>),
    TooLong,
    Eof,
}

fn read_frame(reader: &mut impl BufRead, max: usize) -> std::io::Result<Frame> {
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(if line.is_empty() {
                Frame::Eof
            } else {
                Frame::Line(line)
            });
        }
        if let Some(pos) = available.iter().position(|b| *b == b'\n') {
            line.extend_from_slice(&available[..pos]);
            reader.consume(pos + 1);
            return Ok(if line.len() > max {
                Frame::TooLong
            } else {
                Frame::Line(line)
            });
        }
        let n = available.len();
        line.extend_from_slice(available);
        reader.consume(n);
        if line.len() > max {
            return Ok(Frame::TooLong);
        }
    }
}

fn hello(core: &Arc<Core>, params: &Value) -> Result<Principal> {
    if let Some(api) = params.get("api") {
        if api.as_u64() != Some(u64::from(API_VERSION)) {
            return Err(Error::new(
                Kind::Incompatible,
                format!("this core speaks api {API_VERSION}"),
            )
            .with(json!({ "api": API_VERSION })));
        }
    }
    let token = str_param(params, "token")?;
    let refused = || {
        // Slow down guessing; the answer is the same whatever was wrong.
        thread::sleep(Duration::from_millis(100));
        Error::new(Kind::Unauthenticated, "unknown application or wrong token")
    };
    match opt_str(params, "app")? {
        Some(app) => {
            let ok = check_app_id(app).is_ok() && {
                let conn = lock(&core.db);
                apps::verify_token(&conn, app, token)?
            };
            if ok {
                Ok(Principal::App(app.to_owned()))
            } else {
                Err(refused())
            }
        }
        None => {
            if same_secret(token, &core.owner_token) {
                Ok(Principal::Owner)
            } else {
                Err(refused())
            }
        }
    }
}

fn hello_result(core: &Core, sid: u64, principal: &Principal) -> Result<Value> {
    let mut out = core.info();
    out["session"] = json!(sid);
    match principal {
        Principal::Owner => out["principal"] = json!("owner"),
        Principal::App(id) => {
            let conn = lock(&core.db);
            let app = apps::get(&conn, id)?.ok_or_else(|| Error::internal("vanished"))?;
            out["principal"] = json!("app");
            out["app"] = json!(id);
            out["granted"] = json!(app.granted_strings());
            out["pending"] = json!(app.pending());
        }
    }
    Ok(out)
}

fn connection(core: Arc<Core>, stream: TcpStream) {
    let sid = core.next();
    let Ok(write_half) = stream.try_clone() else {
        return;
    };
    let (tx, rx) = mpsc::sync_channel::<String>(QUEUE);
    let writer = thread::spawn(move || {
        let mut out = std::io::BufWriter::new(write_half);
        for line in rx {
            if out
                .write_all(line.as_bytes())
                .and_then(|()| out.write_all(b"\n"))
                .and_then(|()| out.flush())
                .is_err()
            {
                break;
            }
        }
        let _ = out.get_ref().shutdown(std::net::Shutdown::Both);
    });
    let mut reader = BufReader::new(stream);
    let mut principal: Option<Principal> = None;
    // Until hello, replies go straight to the channel; there is no session yet.
    let reply = |principal: &Option<Principal>, value: &Value| {
        if principal.is_some() {
            core.send(sid, value);
        } else {
            let _ = tx.try_send(value.to_string());
        }
    };

    while let Ok(frame) = read_frame(&mut reader, MAX_LINE) {
        let line = match frame {
            Frame::Eof => break,
            Frame::TooLong => {
                let error = Error::new(Kind::Limit, "a message is at most 1 MiB; closing");
                reply(
                    &principal,
                    &json!({ "jsonrpc": "2.0", "id": null, "error": error.to_json() }),
                );
                break;
            }
            Frame::Line(line) => line,
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(message) = serde_json::from_slice::<Value>(&line) else {
            let error = Error::new(Kind::Parse, "not JSON");
            reply(
                &principal,
                &json!({ "jsonrpc": "2.0", "id": null, "error": error.to_json() }),
            );
            continue;
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str);

        // A reply to an `invoke` the core forwarded to this application.
        if method.is_none() && (message.get("result").is_some() || message.get("error").is_some()) {
            if let Some(key) = id.as_ref().and_then(Value::as_str) {
                let waiting = lock(&core.pending).remove(key);
                if let Some(waiting) = waiting {
                    if waiting.target == sid {
                        let answer = match message.get("error") {
                            Some(error) => Err(error.clone()),
                            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        let _ = waiting.reply.send(answer);
                    } else {
                        // Not yours to answer; put it back.
                        lock(&core.pending).insert(key.to_owned(), waiting);
                    }
                }
            }
            continue;
        }

        let Some(method) = method else {
            let error = Error::new(
                Kind::InvalidRequest,
                "a request has `method` (batches are not supported)",
            );
            reply(
                &principal,
                &json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
            );
            continue;
        };
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let error = Error::new(Kind::InvalidRequest, "`jsonrpc` must be \"2.0\"");
            reply(
                &principal,
                &json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
            );
            continue;
        }
        let params = message.get("params").cloned().unwrap_or_else(|| json!({}));

        let outcome = match (&principal, method) {
            (None, "session.hello") => {
                let full = lock(&core.sessions).len() >= MAX_SESSIONS;
                if full {
                    Err(Error::new(Kind::Limit, "too many connections"))
                } else {
                    hello(&core, &params).and_then(|who| {
                        lock(&core.sessions).insert(
                            sid,
                            Session {
                                principal: who.clone(),
                                out: tx.clone(),
                                subs: Vec::new(),
                                dropped: 0,
                            },
                        );
                        principal = Some(who.clone());
                        if let Principal::App(app) = &who {
                            core.deliver(
                                "core/app.connected",
                                "core",
                                &json!({ "app": app }),
                                &Audience::Subscribers,
                                Some(sid),
                            );
                        }
                        hello_result(&core, sid, &who)
                    })
                }
            }
            (None, "core.info") => Ok(core.info()),
            (None, _) => Err(Error::new(Kind::Unauthenticated, "say session.hello first")),
            (Some(_), "session.hello") => Err(Error::new(
                Kind::InvalidRequest,
                "this connection already said hello",
            )),
            (Some(who), "apps.call") => {
                if let Some(id) = id {
                    call(&core, sid, who, id, &params);
                }
                continue;
            }
            (Some(who), _) => dispatch(
                &Ctx {
                    core: &core,
                    sid,
                    principal: who,
                },
                method,
                &params,
            ),
        };
        if let Some(id) = id {
            let message = match outcome {
                Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error.to_json() }),
            };
            reply(&principal, &message);
        }
        if core.stopping.load(Ordering::Relaxed) && method == "core.shutdown" {
            break;
        }
    }

    // Gone: forget the session, fail calls waiting on it, tell subscribers.
    lock(&core.sessions).remove(&sid);
    drop(tx);
    let orphaned: Vec<Pending> = {
        let mut pending = lock(&core.pending);
        let keys: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.target == sid)
            .map(|(k, _)| k.clone())
            .collect();
        keys.iter().filter_map(|k| pending.remove(k)).collect()
    };
    for waiting in orphaned {
        let _ = waiting
            .reply
            .send(Err(Error::new(Kind::AppUnavailable, "gone").to_json()));
    }
    if let Some(Principal::App(app)) = &principal {
        core.deliver(
            "core/app.disconnected",
            "core",
            &json!({ "app": app }),
            &Audience::Subscribers,
            None,
        );
    }
    let _ = writer.join();
}

/// Run the core until `core.shutdown`. `listen` is a loopback address;
/// port 0 lets the operating system choose.
pub fn serve(core: &Arc<Core>, listen: &str) -> Result<SocketAddr> {
    let address: SocketAddr = listen
        .parse()
        .map_err(|_| Error::params("`--listen` is ip:port"))?;
    if !address.ip().is_loopback() {
        return Err(Error::params(
            "the core listens on the loopback interface only; a network-facing service is eon-office-server's job",
        ));
    }
    if let Some(running) = running_address(&core.home) {
        return Err(Error::new(
            Kind::Conflict,
            format!("a core is already running for this home at {running}"),
        ));
    }
    let listener = TcpListener::bind(address)?;
    let bound = listener.local_addr()?;
    *lock(&core.address) = Some(bound);
    let endpoint = json!({
        "api": API_VERSION, "address": bound.to_string(), "pid": std::process::id(),
        "core_version": env!("CARGO_PKG_VERSION"), "node": core.node,
    });
    std::fs::write(
        endpoint_path(&core.home),
        serde_json::to_string_pretty(&endpoint)?,
    )?;
    core.audit(
        "core",
        "core.started",
        &bound.to_string(),
        &json!({ "version": env!("CARGO_PKG_VERSION") }),
    );

    let ticker = Arc::clone(core);
    thread::spawn(move || {
        while !ticker.stopping.load(Ordering::Relaxed) {
            ticker.tick_jobs();
            thread::sleep(Duration::from_millis(200));
        }
    });

    for stream in listener.incoming() {
        if core.stopping.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        let _ = stream.set_nodelay(true);
        let core = Arc::clone(core);
        thread::spawn(move || connection(core, stream));
    }

    // Leaving: stop what the core started, tell everyone, take down the sign.
    let launched: Vec<String> = lock(&core.children)
        .iter()
        .filter(|(_, c)| c.handle.is_some())
        .map(|(id, _)| id.clone())
        .collect();
    for id in launched {
        let _ = core.stop(&id);
    }
    let bye = json!({ "jsonrpc": "2.0", "method": "shutdown", "params": {} });
    let sids: Vec<u64> = lock(&core.sessions).keys().copied().collect();
    for sid in sids {
        core.send(sid, &bye);
    }
    let _ = std::fs::remove_file(endpoint_path(&core.home));
    Ok(bound)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn topic_patterns() {
        assert!(matches_topic("*", "org.a.notes/x"));
        assert!(matches_topic("org.a.notes/*", "org.a.notes/note.created"));
        assert!(!matches_topic("org.a.notes/*", "org.a.notesx/note.created"));
        assert!(matches_topic("core/app.crashed", "core/app.crashed"));
        assert!(!matches_topic("core/app", "core/app.crashed"));
        assert!(check_pattern("core/*").is_ok());
        assert!(check_pattern("org.a.notes/note.created").is_ok());
        assert!(check_pattern("notes").is_err());
        assert!(check_pattern("Bad/*").is_err());
    }

    #[test]
    fn frames_are_capped() -> std::io::Result<()> {
        let mut input = std::io::Cursor::new(b"short\nxxxxxxxxxxxx\n".to_vec());
        assert!(matches!(read_frame(&mut input, 8)?, Frame::Line(l) if l == b"short"));
        assert!(matches!(read_frame(&mut input, 8)?, Frame::TooLong));
        Ok(())
    }
}

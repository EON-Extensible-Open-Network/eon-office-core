// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The protocol, end to end, against the real binary.
//!
//! Every test starts its own core in its own home directory and talks to it
//! over a socket. Nothing here links the core's internals: if these pass,
//! an application written in any language that can open a TCP connection
//! and write JSON can do the same.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod support;

use serde_json::{json, Value};
use std::time::Duration;
use support::{code, manifest, CoreProcess};

const SECOND: Duration = Duration::from_secs(1);

#[test]
fn nothing_works_before_hello_and_bad_tokens_are_refused() {
    let core = CoreProcess::start();
    let mut client = core.connect();

    // Discovery is allowed; anything else is not.
    let info = client.call("core.info", json!({})).unwrap();
    assert_eq!(info["api"], 0);
    let refused = client.call("storage.get", json!({})).unwrap_err();
    assert_eq!(code(&refused), "unauthenticated");

    let wrong = client
        .call("session.hello", json!({ "token": "0".repeat(64) }))
        .unwrap_err();
    assert_eq!(code(&wrong), "unauthenticated");
    let unknown_app = client
        .call(
            "session.hello",
            json!({ "app": "org.nobody.here", "token": "x" }),
        )
        .unwrap_err();
    assert_eq!(code(&unknown_app), "unauthenticated");
    let future = client
        .call(
            "session.hello",
            json!({ "token": core.owner_token, "api": 7 }),
        )
        .unwrap_err();
    assert_eq!(code(&future), "incompatible");

    let hello = client
        .call("session.hello", json!({ "token": core.owner_token }))
        .unwrap();
    assert_eq!(hello["principal"], "owner");
    let again = client
        .call("session.hello", json!({ "token": core.owner_token }))
        .unwrap_err();
    assert_eq!(code(&again), "invalid_request");

    // Garbage does not end the connection.
    client.send_raw("{ this is not json");
    let parse = client.next_message(5 * SECOND).unwrap();
    assert_eq!(parse["error"]["data"]["code"], "parse_error");
    assert!(client.call("session.ping", json!({})).is_ok());
    let missing = client.call("no.such.method", json!({})).unwrap_err();
    assert_eq!(code(&missing), "method_not_found");
}

#[test]
fn capabilities_are_granted_by_the_owner_and_checked_on_every_call() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let token = core.install(&mut owner, manifest("org.test.notes", &["storage"]), &[]);

    let mut app = core.app("org.test.notes", &token);
    let put = json!({ "collection": "notes", "id": "a", "doc": { "t": "hello" } });
    let denied = app.call("storage.put", put.clone()).unwrap_err();
    assert_eq!(code(&denied), "forbidden");
    assert_eq!(denied["data"]["capability"], "storage");

    // Granted while connected: works on the next call, no reconnect.
    owner
        .call(
            "apps.grant",
            json!({ "app": "org.test.notes", "capabilities": ["storage"] }),
        )
        .unwrap();
    assert_eq!(app.call("storage.put", put.clone()).unwrap()["rev"], 1);
    assert_eq!(
        app.call("storage.get", json!({ "collection": "notes", "id": "a" }))
            .unwrap()["doc"]["t"],
        "hello"
    );

    // Revoked while connected: refused on the next call.
    owner
        .call(
            "apps.revoke",
            json!({ "app": "org.test.notes", "capabilities": ["storage"] }),
        )
        .unwrap();
    assert_eq!(
        code(&app.call("storage.put", put).unwrap_err()),
        "forbidden"
    );

    // Only requested capabilities can be granted; apps cannot grant themselves.
    let unrequested = owner
        .call(
            "apps.grant",
            json!({ "app": "org.test.notes", "capabilities": ["schedule"] }),
        )
        .unwrap_err();
    assert_eq!(code(&unrequested), "invalid_params");
    let not_owner = app
        .call(
            "apps.grant",
            json!({ "app": "org.test.notes", "capabilities": ["storage"] }),
        )
        .unwrap_err();
    assert_eq!(code(&not_owner), "forbidden");

    // Every change of authority, and each refusal, is in the audit chain.
    let log = owner.call("audit.list", json!({ "limit": 100 })).unwrap();
    let actions: Vec<&str> = log["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect();
    for expected in ["app.installed", "app.granted", "app.revoked", "denied"] {
        assert!(
            actions.contains(&expected),
            "{expected} missing from {actions:?}"
        );
    }
    assert_eq!(owner.call("audit.verify", json!({})).unwrap()["ok"], true);
}

#[test]
fn applications_cannot_see_each_others_storage() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let a = core.install(
        &mut owner,
        manifest("org.test.a", &["storage"]),
        &["storage"],
    );
    let b = core.install(
        &mut owner,
        manifest("org.test.b", &["storage"]),
        &["storage"],
    );
    let mut a = core.app("org.test.a", &a);
    let mut b = core.app("org.test.b", &b);

    a.call(
        "storage.put",
        json!({ "collection": "c", "id": "same", "doc": "from a" }),
    )
    .unwrap();
    b.call(
        "storage.put",
        json!({ "collection": "c", "id": "same", "doc": "from b" }),
    )
    .unwrap();
    assert_eq!(
        a.call("storage.get", json!({ "collection": "c", "id": "same" }))
            .unwrap()["doc"],
        "from a"
    );
    assert_eq!(
        b.call("storage.get", json!({ "collection": "c", "id": "same" }))
            .unwrap()["doc"],
        "from b"
    );

    // A stale write is refused with the current revision.
    let stale = a
        .call(
            "storage.put",
            json!({ "collection": "c", "id": "same", "doc": "x", "rev": 0 }),
        )
        .unwrap_err();
    assert_eq!(
        (code(&stale), stale["data"]["rev"].clone()),
        ("conflict", json!(1))
    );
}

#[test]
fn events_flow_only_where_declared_and_subscribed() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let mut notes_manifest = manifest("org.test.notes", &["events.publish"]);
    notes_manifest["publishes"] = json!(["note.created"]);
    let notes = core.install(&mut owner, notes_manifest, &["events.publish"]);
    let tasks = core.install(
        &mut owner,
        manifest("org.test.tasks", &["events.subscribe"]),
        &["events.subscribe"],
    );
    let mail = core.install(&mut owner, manifest("org.test.mail", &["storage"]), &[]);

    let mut notes = core.app("org.test.notes", &notes);
    let mut tasks = core.app("org.test.tasks", &tasks);
    let mut mail = core.app("org.test.mail", &mail);

    tasks
        .call("events.subscribe", json!({ "pattern": "org.test.notes/*" }))
        .unwrap();
    assert_eq!(
        code(
            &mail
                .call("events.subscribe", json!({ "pattern": "*" }))
                .unwrap_err()
        ),
        "forbidden"
    );

    notes
        .call(
            "events.publish",
            json!({ "event": "note.created", "payload": { "id": "n1" } }),
        )
        .unwrap();
    let event = tasks
        .wait_for(5 * SECOND, |m| m["method"] == "event")
        .unwrap();
    assert_eq!(event["params"]["topic"], "org.test.notes/note.created");
    assert_eq!(event["params"]["from"], "org.test.notes");
    assert_eq!(event["params"]["payload"]["id"], "n1");

    // An event not declared in the manifest is not an event the app may emit.
    let undeclared = notes
        .call("events.publish", json!({ "event": "secret.leak" }))
        .unwrap_err();
    assert_eq!(code(&undeclared), "invalid_params");
}

#[test]
fn calls_reach_the_provider_and_a_crashing_provider_takes_nobody_with_it() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let mut tasks_manifest = manifest("org.test.tasks", &[]);
    tasks_manifest["provides"] = json!([{ "method": "create", "description": "make a task" }]);
    let tasks = core.install(&mut owner, tasks_manifest, &[]);
    let notes = core.install(
        &mut owner,
        manifest("org.test.notes", &["call:org.test.tasks"]),
        &["call:org.test.tasks"],
    );
    let other = core.install(
        &mut owner,
        manifest("org.test.other", &["storage"]),
        &["storage"],
    );

    let mut notes = core.app("org.test.notes", &notes);
    let mut other = core.app("org.test.other", &other);

    // Not running: a clear answer, not a hang.
    let down = notes
        .call(
            "apps.call",
            json!({ "app": "org.test.tasks", "method": "create" }),
        )
        .unwrap_err();
    assert_eq!(code(&down), "app_unavailable");

    // Running: the call is forwarded as `invoke`, and the answer comes back.
    let tasks_token = tasks.clone();
    let address = core.address.clone();
    let provider = std::thread::spawn(move || {
        let mut tasks = provider_session(&address, &tasks_token);
        let invoke = tasks
            .wait_for(10 * SECOND, |m| m["method"] == "invoke")
            .unwrap();
        assert_eq!(invoke["params"]["from"], "org.test.notes");
        assert_eq!(invoke["params"]["method"], "create");
        let title = invoke["params"]["params"]["title"].clone();
        tasks.answer(&invoke["id"], json!({ "task": "t1", "title": title }));
        // Second invoke: die without answering.
        let _second = tasks
            .wait_for(10 * SECOND, |m| m["method"] == "invoke")
            .unwrap();
        tasks.close();
    });
    wait_until_connected(&mut owner, "org.test.tasks");

    let created = notes
        .call("apps.call", json!({ "app": "org.test.tasks", "method": "create", "params": { "title": "Buy milk" } }))
        .unwrap();
    assert_eq!(created, json!({ "task": "t1", "title": "Buy milk" }));

    let not_provided = notes
        .call(
            "apps.call",
            json!({ "app": "org.test.tasks", "method": "drop_all" }),
        )
        .unwrap_err();
    assert_eq!(code(&not_provided), "method_not_found");

    let started = std::time::Instant::now();
    let crashed = notes
        .call(
            "apps.call",
            json!({ "app": "org.test.tasks", "method": "create", "timeout_ms": 30000 }),
        )
        .unwrap_err();
    assert_eq!(code(&crashed), "app_unavailable");
    assert!(
        started.elapsed() < 10 * SECOND,
        "a dead provider fails the call promptly"
    );
    provider.join().unwrap();

    // Everyone else is still fine.
    assert!(notes.call("session.ping", json!({})).is_ok());
    assert!(other
        .call(
            "storage.put",
            json!({ "collection": "c", "id": "x", "doc": 1 })
        )
        .is_ok());

    // An app without the capability cannot call at all.
    let refused = other
        .call(
            "apps.call",
            json!({ "app": "org.test.tasks", "method": "create" }),
        )
        .unwrap_err();
    assert_eq!(code(&refused), "forbidden");
}

fn provider_session(address: &str, token: &str) -> support::Client {
    let mut client = support::Client::connect(address);
    client
        .call(
            "session.hello",
            json!({ "app": "org.test.tasks", "token": token }),
        )
        .unwrap();
    client
}

fn wait_until_connected(owner: &mut support::Client, app: &str) {
    for _ in 0..200 {
        let status = owner.call("apps.status", json!({ "app": app })).unwrap();
        if status["connected"] == true {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{app} never connected");
}

#[test]
fn notifications_are_rate_limited_and_reach_the_owner() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    owner
        .call(
            "events.subscribe",
            json!({ "pattern": "core/notification.posted" }),
        )
        .unwrap();
    let token = core.install(
        &mut owner,
        manifest("org.test.mail", &["notifications"]),
        &["notifications"],
    );
    let mut mail = core.app("org.test.mail", &token);

    mail.call(
        "notifications.post",
        json!({ "title": "New mail", "urgency": "high" }),
    )
    .unwrap();
    let event = owner
        .wait_for(5 * SECOND, |m| m["method"] == "event")
        .unwrap();
    assert_eq!(event["params"]["payload"]["title"], "New mail");

    for _ in 1..notify_limit() {
        mail.call("notifications.post", json!({ "title": "more" }))
            .unwrap();
    }
    let flood = mail
        .call("notifications.post", json!({ "title": "one too many" }))
        .unwrap_err();
    assert_eq!(code(&flood), "rate_limited");
    let listed = owner
        .call("notifications.list", json!({ "limit": 500 }))
        .unwrap();
    assert_eq!(listed["items"].as_array().unwrap().len(), notify_limit());
}

fn notify_limit() -> usize {
    30
}

#[test]
fn search_shows_only_what_was_shared() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let notes = core.install(
        &mut owner,
        manifest("org.test.notes", &["search.index", "search.query"]),
        &["search.index", "search.query"],
    );
    let sheets = core.install(
        &mut owner,
        manifest("org.test.sheets", &["search.index", "search.query"]),
        &["search.index", "search.query"],
    );
    let mut notes = core.app("org.test.notes", &notes);
    let mut sheets = core.app("org.test.sheets", &sheets);

    notes
        .call(
            "search.index",
            json!({ "id": "n1", "title": "Bütçe taslağı", "text": "gizli" }),
        )
        .unwrap();
    sheets
        .call(
            "search.index",
            json!({ "id": "s1", "title": "BÜTÇE 2027", "text": "", "visibility": "shared" }),
        )
        .unwrap();

    let from_sheets = sheets
        .call("search.query", json!({ "query": "butce" }))
        .unwrap();
    let hits: Vec<&str> = from_sheets["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["id"].as_str().unwrap())
        .collect();
    assert_eq!(hits, vec!["s1"], "a private note stays private");
    let from_notes = notes
        .call("search.query", json!({ "query": "BÜTÇE" }))
        .unwrap();
    assert_eq!(from_notes["hits"].as_array().unwrap().len(), 2);
}

#[test]
fn a_scheduled_job_wakes_its_application() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let token = core.install(
        &mut owner,
        manifest("org.test.backup", &["schedule"]),
        &["schedule"],
    );
    let mut app = core.app("org.test.backup", &token);
    app.call(
        "schedule.add",
        json!({ "name": "nightly", "delay_ms": 300, "payload": { "kind": "full" } }),
    )
    .unwrap();
    let due = app
        .wait_for(10 * SECOND, |m| m["method"] == "job.due")
        .unwrap();
    assert_eq!(due["params"]["name"], "nightly");
    assert_eq!(due["params"]["payload"]["kind"], "full");
    assert_eq!(
        app.call("schedule.list", json!({})).unwrap()["jobs"],
        json!([])
    );
}

#[test]
fn preferences_reach_every_application_without_asking() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let token = core.install(&mut owner, manifest("org.test.writer", &[]), &[]);
    let mut writer = core.app("org.test.writer", &token);
    owner
        .call(
            "prefs.set",
            json!({ "key": "reduce_motion", "value": true }),
        )
        .unwrap();
    let event = writer
        .wait_for(5 * SECOND, |m| m["method"] == "event")
        .unwrap();
    assert_eq!(event["params"]["topic"], "core/prefs.changed");
    assert_eq!(
        writer.call("prefs.get", json!({})).unwrap()["reduce_motion"],
        true
    );
    let not_owner = writer
        .call("prefs.set", json!({ "key": "locale", "value": "tr" }))
        .unwrap_err();
    assert_eq!(code(&not_owner), "forbidden");
    // Its own settings need no capability and are its own.
    writer
        .call("settings.set", json!({ "key": "font", "value": "serif" }))
        .unwrap();
    assert_eq!(
        writer
            .call("settings.get", json!({ "key": "font" }))
            .unwrap()["value"],
        "serif"
    );
}

fn failing_command() -> Value {
    if cfg!(windows) {
        json!({ "command": "cmd", "args": ["/C", "exit 3"] })
    } else {
        json!({ "command": "sh", "args": ["-c", "exit 3"] })
    }
}

fn idle_command() -> Value {
    if cfg!(windows) {
        json!({ "command": "cmd", "args": ["/C", "ping -n 60 127.0.0.1 >NUL"] })
    } else {
        json!({ "command": "sh", "args": ["-c", "sleep 60"] })
    }
}

#[test]
fn the_supervisor_restarts_a_crashing_app_three_times_then_gives_up() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    owner
        .call("events.subscribe", json!({ "pattern": "core/*" }))
        .unwrap();
    let mut crashy = manifest("org.test.crashy", &[]);
    crashy["entry"] = failing_command();
    crashy["restart"] = json!("on-failure");
    core.install(&mut owner, crashy, &[]);

    owner
        .call("apps.launch", json!({ "app": "org.test.crashy" }))
        .unwrap();
    let mut crashes = 0;
    while crashes < 4 {
        let event = owner
            .wait_for(15 * SECOND, |m| m["params"]["topic"] == "core/app.crashed")
            .unwrap_or_else(|| panic!("only {crashes} crashes seen"));
        assert_eq!(event["params"]["payload"]["code"], 3);
        crashes += 1;
    }
    std::thread::sleep(SECOND);
    let status = owner
        .call("apps.status", json!({ "app": "org.test.crashy" }))
        .unwrap();
    assert_eq!(status["process"]["state"], "failed");
    assert_eq!(status["process"]["restarts"], 3);
    // The core shrugged it off.
    assert!(owner.call("session.ping", json!({})).is_ok());
}

#[test]
fn stopping_an_app_that_ignores_shutdown_forces_it() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let mut idle = manifest("org.test.idle", &[]);
    idle["entry"] = idle_command();
    core.install(&mut owner, idle, &[]);
    owner
        .call("apps.launch", json!({ "app": "org.test.idle" }))
        .unwrap();
    let again = owner
        .call("apps.launch", json!({ "app": "org.test.idle" }))
        .unwrap_err();
    assert_eq!(code(&again), "conflict");
    let stopped = owner
        .call("apps.stop", json!({ "app": "org.test.idle" }))
        .unwrap();
    assert_eq!(stopped["forced"], true);
    std::thread::sleep(Duration::from_millis(500));
    let status = owner
        .call("apps.status", json!({ "app": "org.test.idle" }))
        .unwrap();
    assert_eq!(status["process"]["state"], "stopped");
}

#[test]
fn two_devices_sync_through_export_and_import() {
    let laptop = CoreProcess::start();
    let phone = CoreProcess::start();
    let caps = ["storage"];
    let mut laptop_owner = laptop.owner();
    let mut phone_owner = phone.owner();
    let lt = laptop.install(&mut laptop_owner, manifest("org.test.notes", &caps), &caps);
    let pt = phone.install(&mut phone_owner, manifest("org.test.notes", &caps), &caps);
    let mut on_laptop = laptop.app("org.test.notes", &lt);
    let mut on_phone = phone.app("org.test.notes", &pt);

    on_laptop
        .call(
            "storage.put",
            json!({ "collection": "n", "id": "shopping", "doc": { "v": "base" } }),
        )
        .unwrap();
    let changes = laptop_owner.call("sync.export", json!({})).unwrap();
    phone_owner
        .call(
            "sync.import",
            json!({ "changes": changes["changes"], "node": changes["node"] }),
        )
        .unwrap();
    assert_eq!(
        on_phone
            .call(
                "storage.get",
                json!({ "collection": "n", "id": "shopping" })
            )
            .unwrap()["doc"]["v"],
        "base"
    );
    // The phone's copy of the app was told its data changed.
    let told = on_phone
        .wait_for(5 * SECOND, |m| {
            m["params"]["topic"] == "core/storage.changed"
        })
        .unwrap();
    assert_eq!(told["params"]["payload"]["id"], "shopping");

    // Both edit offline; exchange both ways; both converge and both keep the loser.
    on_laptop
        .call(
            "storage.put",
            json!({ "collection": "n", "id": "shopping", "doc": { "v": "laptop" } }),
        )
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    on_phone
        .call(
            "storage.put",
            json!({ "collection": "n", "id": "shopping", "doc": { "v": "phone" } }),
        )
        .unwrap();
    let from_laptop = laptop_owner.call("sync.export", json!({})).unwrap();
    let from_phone = phone_owner.call("sync.export", json!({})).unwrap();
    phone_owner
        .call("sync.import", json!({ "changes": from_laptop["changes"] }))
        .unwrap();
    laptop_owner
        .call("sync.import", json!({ "changes": from_phone["changes"] }))
        .unwrap();
    for app in [&mut on_laptop, &mut on_phone] {
        assert_eq!(
            app.call(
                "storage.get",
                json!({ "collection": "n", "id": "shopping" })
            )
            .unwrap()["doc"]["v"],
            "phone"
        );
    }
    let kept = on_laptop.call("storage.conflicts", json!({})).unwrap();
    assert_eq!(kept["items"][0]["doc"]["v"], "laptop");
    let conflict = kept["items"][0]["conflict"].clone();
    on_laptop
        .call("storage.resolve", json!({ "conflict": conflict }))
        .unwrap();
    assert_eq!(
        on_laptop.call("storage.conflicts", json!({})).unwrap()["items"],
        json!([])
    );
}

#[test]
fn the_directory_answers_access_questions() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let ada = owner
        .call(
            "directory.create_user",
            json!({ "name": "ada", "display_name": "Ada" }),
        )
        .unwrap();
    let org = owner
        .call("directory.create_org", json!({ "name": "Acme" }))
        .unwrap();
    let ws = owner
        .call(
            "directory.create_workspace",
            json!({ "name": "Finance", "org": org["id"] }),
        )
        .unwrap();
    owner
        .call(
            "directory.add_member",
            json!({ "user": ada["id"], "kind": "workspace", "scope": ws["id"], "role": "member" }),
        )
        .unwrap();

    let token = core.install(
        &mut owner,
        manifest("org.test.sheets", &["directory.read"]),
        &["directory.read"],
    );
    let mut sheets = core.app("org.test.sheets", &token);
    let write = sheets.call("access.check", json!({ "user": ada["id"], "permission": "write", "kind": "workspace", "scope": ws["id"] })).unwrap();
    assert_eq!(write["allowed"], true);
    let delete = sheets.call("access.check", json!({ "user": ada["id"], "permission": "delete", "kind": "workspace", "scope": ws["id"] })).unwrap();
    assert_eq!(delete["allowed"], false);
    let snapshot = sheets.call("directory.get", json!({})).unwrap();
    assert_eq!(
        snapshot["users"].as_array().unwrap().len(),
        2,
        "the owner and ada"
    );
    let not_owner = sheets
        .call("directory.create_user", json!({ "name": "mallory" }))
        .unwrap_err();
    assert_eq!(code(&not_owner), "forbidden");
}

#[test]
fn an_oversized_message_closes_only_that_connection() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let mut big = core.owner();
    big.send_raw(&format!("{{\"x\":\"{}\"}}", "a".repeat(1024 * 1024 + 10)));
    let answer = big.next_message(5 * SECOND).unwrap();
    assert_eq!(answer["error"]["data"]["code"], "limit");
    assert!(big.next_message(5 * SECOND).is_none(), "closed");
    assert!(owner.call("session.ping", json!({})).is_ok());
}

#[test]
fn uninstall_removes_the_app_and_its_data_and_drops_its_connection() {
    let core = CoreProcess::start();
    let mut owner = core.owner();
    let token = core.install(
        &mut owner,
        manifest("org.test.gone", &["storage"]),
        &["storage"],
    );
    let mut app = core.app("org.test.gone", &token);
    app.call(
        "storage.put",
        json!({ "collection": "c", "id": "d", "doc": 1 }),
    )
    .unwrap();
    owner
        .call("apps.uninstall", json!({ "app": "org.test.gone" }))
        .unwrap();
    assert!(
        app.next_message(5 * SECOND).is_none(),
        "its connection was closed"
    );
    assert!(!core.home.join("apps").join("org.test.gone").exists());
    let listed = owner.call("apps.list", json!({})).unwrap();
    assert_eq!(listed["apps"], json!([]));
    // Reinstalling starts from nothing.
    let token = core.install(
        &mut owner,
        manifest("org.test.gone", &["storage"]),
        &["storage"],
    );
    let mut app = core.app("org.test.gone", &token);
    assert_eq!(
        app.call("storage.get", json!({ "collection": "c", "id": "d" }))
            .unwrap(),
        Value::Null
    );
}

#[test]
fn backup_restore_and_a_second_core_refused() {
    let home = support::fresh_home();
    let backup = home.join("backup.sqlite");
    {
        let mut core = CoreProcess::start_in(home.clone(), true);
        let mut owner = core.owner();
        let token = core.install(
            &mut owner,
            manifest("org.test.keep", &["storage"]),
            &["storage"],
        );
        let mut app = core.app("org.test.keep", &token);
        app.call(
            "storage.put",
            json!({ "collection": "c", "id": "d", "doc": "before" }),
        )
        .unwrap();
        owner
            .call("backup.create", json!({ "path": backup }))
            .unwrap();
        app.call(
            "storage.put",
            json!({ "collection": "c", "id": "d", "doc": "after" }),
        )
        .unwrap();

        // One core per home.
        let second = std::process::Command::new(support::binary())
            .args(["serve", "--home"])
            .arg(&home)
            .output()
            .unwrap();
        assert!(!second.status.success());
        assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));
        // Restoring under a running core is refused.
        let refused = std::process::Command::new(support::binary())
            .args(["restore", "--home"])
            .arg(&home)
            .arg("--from")
            .arg(&backup)
            .output()
            .unwrap();
        assert!(!refused.status.success());

        owner.call("core.shutdown", json!({})).unwrap();
        core.wait();
    }
    let restored = std::process::Command::new(support::binary())
        .args(["restore", "--home"])
        .arg(&home)
        .arg("--from")
        .arg(&backup)
        .output()
        .unwrap();
    assert!(
        restored.status.success(),
        "{}",
        String::from_utf8_lossy(&restored.stderr)
    );

    let core = CoreProcess::start_in(home.clone(), false);
    let token =
        std::fs::read_to_string(home.join("apps").join("org.test.keep").join("token")).unwrap();
    let mut app = core.app("org.test.keep", token.trim());
    assert_eq!(
        app.call("storage.get", json!({ "collection": "c", "id": "d" }))
            .unwrap()["doc"],
        "before"
    );
    let verified = std::process::Command::new(support::binary())
        .args(["verify-audit", "--home"])
        .arg(&home)
        .output()
        .unwrap();
    assert!(verified.status.success());
}

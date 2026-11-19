// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The library as applications use it: a home folder, two applications in it,
//! and the shared folder they meet in. No service runs anywhere.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use eon_office_core::manifest::Manifest;
use eon_office_core::{links, Home, Kind};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

struct TempHome(PathBuf);

impl TempHome {
    fn new(name: &str) -> TempHome {
        let dir = std::env::temp_dir().join(format!(
            "eon-office-core-{name}-{}-{}",
            std::process::id(),
            eon_office_core::util::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempHome(dir)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn manifest(id: &str, version: &str, caps: &[&str]) -> Manifest {
    Manifest::from_json(json!({
        "manifest": 0, "id": id, "name": id, "version": version,
        "api": { "min": 0, "max": 0 },
        "entry": { "command": "app" },
        "capabilities": caps,
    }))
    .unwrap()
}

const ALL: &[&str] = &["storage", "search.index", "search.query", "events.publish"];

#[test]
fn an_application_registers_and_reopens_its_store() {
    let dir = TempHome::new("reopen");
    let home = Home::open(&dir.0).unwrap();
    {
        let mut notes = home
            .app(&manifest("org.test.notes", "0.2.0", ALL), None)
            .unwrap();
        let written = notes.put("n", "a", &json!({ "v": 1 }), Some(0)).unwrap();
        assert_eq!(written["rev"], 1);
    }
    let notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    assert_eq!(notes.get("n", "a").unwrap()["doc"]["v"], 1);
    assert!(dir.0.join("apps/org.test.notes/data.sqlite").is_file());
    let apps = home.shared().unwrap().apps().unwrap();
    assert_eq!(apps[0]["id"], "org.test.notes");
    assert_eq!(apps[0]["version"], "0.2.0");
}

#[test]
fn an_older_copy_does_not_downgrade_the_registry() {
    let dir = TempHome::new("downgrade");
    let home = Home::open(&dir.0).unwrap();
    home.app(
        &manifest("org.test.notes", "0.3.0", ALL),
        Some(Path::new("/new")),
    )
    .unwrap();
    home.app(
        &manifest("org.test.notes", "0.2.0", ALL),
        Some(Path::new("/old")),
    )
    .unwrap();
    let apps = home.shared().unwrap().apps().unwrap();
    assert_eq!(
        (apps[0]["version"].clone(), apps[0]["path"].clone()),
        (json!("0.3.0"), json!("/new"))
    );
}

#[test]
fn two_applications_cannot_read_each_others_documents() {
    let dir = TempHome::new("isolation");
    let home = Home::open(&dir.0).unwrap();
    let mut notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    let tasks = home
        .app(&manifest("org.test.tasks", "0.2.0", ALL), None)
        .unwrap();
    notes
        .put("n", "secret", &json!({ "v": "mine" }), None)
        .unwrap();
    assert_eq!(tasks.get("n", "secret").unwrap(), Value::Null);
    assert_eq!(tasks.list("n", None, 100).unwrap()["items"], json!([]));
}

#[test]
fn search_shows_only_what_was_shared() {
    let dir = TempHome::new("search");
    let home = Home::open(&dir.0).unwrap();
    let notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    let tasks = home
        .app(&manifest("org.test.tasks", "0.2.0", ALL), None)
        .unwrap();
    notes.index("p", "Gizli günlük", "özel", false).unwrap();
    notes
        .index("s", "Şirket bütçesi", "paylaşılan", true)
        .unwrap();
    let seen = tasks.search("butce", 10).unwrap();
    assert_eq!(seen["hits"].as_array().unwrap().len(), 1);
    assert_eq!(seen["hits"][0]["id"], "s");
    assert_eq!(tasks.search("gunluk", 10).unwrap()["hits"], json!([]));
    assert_eq!(notes.search("gunluk", 10).unwrap()["hits"][0]["id"], "p");
    // The person, through the hub, sees everything.
    assert_eq!(
        home.shared().unwrap().query(None, "gunluk", 10).unwrap()["hits"][0]["id"],
        "p"
    );
    notes.unindex("s").unwrap();
    assert_eq!(tasks.search("butce", 10).unwrap()["hits"], json!([]));
}

#[test]
fn a_capability_not_declared_or_revoked_is_refused() {
    let dir = TempHome::new("caps");
    let home = Home::open(&dir.0).unwrap();
    let plain = home
        .app(&manifest("org.test.plain", "0.2.0", &["storage"]), None)
        .unwrap();
    let refused = plain.index("x", "t", "b", true).unwrap_err();
    assert_eq!(refused.kind, Kind::Forbidden);
    assert_eq!(refused.to_json()["data"]["capability"], "search.index");

    let notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    notes.index("x", "t", "b", true).unwrap();
    home.shared()
        .unwrap()
        .revoke("org.test.notes", "search.index")
        .unwrap();
    assert_eq!(
        notes.index("x", "t", "b", true).unwrap_err().kind,
        Kind::Forbidden
    );
    home.shared()
        .unwrap()
        .grant("org.test.notes", "search.index")
        .unwrap();
    notes.index("x", "t", "b", true).unwrap();
    // Revoking and granting are on the record.
    let audit = home.shared().unwrap().audit_list(0, 100).unwrap();
    let actions: Vec<&str> = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["action"].as_str())
        .collect();
    assert!(
        actions.contains(&"capability.revoked") && actions.contains(&"capability.granted"),
        "{actions:?}"
    );
    assert_eq!(home.shared().unwrap().audit_verify().unwrap()["ok"], true);
}

#[test]
fn two_threads_with_their_own_connections_write_shared_at_once() {
    let dir = TempHome::new("concurrent");
    let home = Home::open(&dir.0).unwrap();
    let handles: Vec<_> = ["org.test.notes", "org.test.tasks"]
        .into_iter()
        .map(|id| {
            let root = dir.0.clone();
            std::thread::spawn(move || {
                let home = Home::open(&root).unwrap();
                let app = home.app(&manifest(id, "0.2.0", ALL), None).unwrap();
                for i in 0..200 {
                    app.index(&format!("i{i}"), "başlık", "metin", true)
                        .unwrap();
                    app.activity(&format!("i{i}"), "başlık", "edited").unwrap();
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let shared = home.shared().unwrap();
    assert_eq!(
        shared.query(None, "baslik", 1000).unwrap()["hits"]
            .as_array()
            .unwrap()
            .len(),
        400
    );
    assert_eq!(shared.audit_verify().unwrap()["ok"], true);
}

#[test]
fn activity_keeps_titles_and_the_last_500() {
    let dir = TempHome::new("activity");
    let home = Home::open(&dir.0).unwrap();
    let notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    for i in 0..520 {
        notes
            .activity(&format!("n{i}"), &format!("Not {i}"), "edited")
            .unwrap();
    }
    let shared = home.shared().unwrap();
    let recent = shared.recent(1000).unwrap();
    let items = recent["items"].as_array().unwrap();
    assert_eq!(items.len(), 500);
    assert_eq!(
        (
            items[0]["item"].clone(),
            items[0]["title"].clone(),
            items[0]["app"].clone()
        ),
        (json!("n519"), json!("Not 519"), json!("org.test.notes"))
    );
    let plain = home
        .app(&manifest("org.test.plain", "0.2.0", &["storage"]), None)
        .unwrap();
    assert_eq!(
        plain.activity("x", "t", "edited").unwrap_err().kind,
        Kind::Forbidden
    );
}

#[test]
fn published_summaries_are_read_by_another_app() {
    let dir = TempHome::new("public");
    let home = Home::open(&dir.0).unwrap();
    let contacts = home
        .app(&manifest("org.test.contacts", "0.2.0", ALL), None)
        .unwrap();
    let calendar = home
        .app(
            &manifest(
                "org.test.calendar",
                "0.2.0",
                &["storage", "call:org.test.contacts"],
            ),
            None,
        )
        .unwrap();
    let stranger = home
        .app(&manifest("org.test.other", "0.2.0", &["storage"]), None)
        .unwrap();
    contacts
        .publish(
            "contact:1",
            &json!({ "name": "Ayşe", "emails": ["a@example.org"] }),
        )
        .unwrap();
    contacts
        .publish("contact:2", &json!({ "name": "Can" }))
        .unwrap();
    contacts.unpublish("contact:2").unwrap();
    let seen = calendar.public_of("org.test.contacts").unwrap();
    assert_eq!(
        seen,
        vec![(
            "contact:1".to_owned(),
            json!({ "name": "Ayşe", "emails": ["a@example.org"] })
        )]
    );
    assert_eq!(
        stranger.public_of("org.test.contacts").unwrap_err().kind,
        Kind::Forbidden
    );
}

#[test]
fn a_newer_shared_schema_is_refused_but_the_store_opens() {
    let dir = TempHome::new("newer");
    Home::open(&dir.0).unwrap();
    {
        let conn = rusqlite::Connection::open(dir.0.join("shared.sqlite")).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }
    assert_eq!(
        Home::open(&dir.0).unwrap().shared().unwrap_err().kind,
        Kind::Incompatible
    );
    // The application still opens and keeps working with its own documents.
    let home = Home::open(&dir.0).unwrap();
    let mut notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    notes.put("n", "a", &json!({}), None).unwrap();
    assert!(!notes.shared_available());
    assert_eq!(notes.search("x", 5).unwrap_err().kind, Kind::Incompatible);
}

#[test]
fn links_open_reports_missing_files() {
    let dir = TempHome::new("links");
    let home = Home::open(&dir.0).unwrap();
    let link = links::parse("eon-office://org.test.notes/n42?line=3").unwrap();
    assert_eq!(
        (link.app.as_str(), link.item.as_str(), link.query.as_str()),
        ("org.test.notes", "n42", "line=3")
    );
    assert!(links::parse("https://example.org/x").is_none());
    assert!(links::parse("eon-office://not an id/x").is_none());

    assert_eq!(
        links::open(&dir.0, "eon-office://org.test.notes/n42").unwrap()["reason"],
        "not_installed"
    );
    let gone = dir.0.join("gone");
    home.app(&manifest("org.test.notes", "0.2.0", ALL), Some(&gone))
        .unwrap();
    let answer = links::open(&dir.0, "eon-office://org.test.notes/n42").unwrap();
    assert_eq!(
        (answer["opened"].clone(), answer["reason"].clone()),
        (json!(false), json!("missing_file"))
    );
    assert_eq!(
        links::open(&dir.0, "nonsense").unwrap_err().kind,
        Kind::InvalidParams
    );
}

#[test]
fn prefs_version_moves_on_every_change() {
    let dir = TempHome::new("prefs");
    let home = Home::open(&dir.0).unwrap();
    let shared = home.shared().unwrap();
    let before = shared.prefs_version().unwrap();
    shared.set_pref("font_scale", &json!(1.25)).unwrap();
    let after = shared.prefs_version().unwrap();
    assert!(after > before);
    assert_eq!(shared.prefs().unwrap()["font_scale"], 1.25);
    assert!(shared.set_pref("font_scale", &json!(9)).is_err());
    assert_eq!(shared.prefs_version().unwrap(), after);
}

#[test]
fn two_devices_sync_through_export_and_import() {
    let (a, b) = (TempHome::new("laptop"), TempHome::new("phone"));
    let (laptop, phone) = (Home::open(&a.0).unwrap(), Home::open(&b.0).unwrap());
    let m = manifest("org.test.notes", "0.2.0", ALL);
    let (mut on_laptop, mut on_phone) =
        (laptop.app(&m, None).unwrap(), phone.app(&m, None).unwrap());

    on_laptop
        .put("n", "shopping", &json!({ "v": "base" }), None)
        .unwrap();
    let changes = on_laptop.export(None, 1000).unwrap();
    let merged = on_phone
        .import(changes["changes"].as_array().unwrap())
        .unwrap();
    assert_eq!(merged["touched"][0]["id"], "shopping");
    assert_eq!(on_phone.get("n", "shopping").unwrap()["doc"]["v"], "base");

    on_laptop
        .put("n", "shopping", &json!({ "v": "laptop" }), None)
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    on_phone
        .put("n", "shopping", &json!({ "v": "phone" }), None)
        .unwrap();
    let from_laptop = on_laptop.export(None, 1000).unwrap();
    let from_phone = on_phone.export(None, 1000).unwrap();
    on_phone
        .import(from_laptop["changes"].as_array().unwrap())
        .unwrap();
    on_laptop
        .import(from_phone["changes"].as_array().unwrap())
        .unwrap();
    for app in [&on_laptop, &on_phone] {
        assert_eq!(app.get("n", "shopping").unwrap()["doc"]["v"], "phone");
    }
    let kept = on_laptop.conflicts(10).unwrap();
    assert_eq!(kept["items"][0]["doc"]["v"], "laptop");
    on_laptop
        .resolve(kept["items"][0]["conflict"].as_i64().unwrap())
        .unwrap();
    assert_eq!(on_laptop.conflicts(10).unwrap()["items"], json!([]));
}

#[test]
fn another_applications_changes_are_refused_on_import() {
    let dir = TempHome::new("foreign");
    let home = Home::open(&dir.0).unwrap();
    let mut notes = home
        .app(&manifest("org.test.notes", "0.2.0", ALL), None)
        .unwrap();
    let foreign = json!([{ "app": "org.test.tasks", "collection": "t", "id": "x", "doc": {}, "deleted": false, "hlc": "0000000000001-0000-ff" }]);
    assert_eq!(
        notes.import(foreign.as_array().unwrap()).unwrap_err().kind,
        Kind::InvalidParams
    );
}

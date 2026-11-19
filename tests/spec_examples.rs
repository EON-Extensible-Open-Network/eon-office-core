// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! The spec's examples must mean the same thing to the core as to the schema.
//!
//! `eon-office-spec` validates `examples/valid` and `examples/invalid` against
//! its JSON Schemas. This test reads the same files through the core's own
//! readers. A schema and an implementation that disagree produce a manifest
//! that validates and is then refused -- the failure EON Stream met once with
//! a pattern Python accepted and JavaScript did not.
//!
//! Needs an `eon-office-spec` checkout: `EON_OFFICE_SPEC_DIR=../eon-office-spec
//! cargo test --test spec_examples -- --ignored`. CI checks it out and runs it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use eon_office_core::manifest::Manifest;
use eon_office_core::settings::check_theme;
use std::path::PathBuf;

fn examples(kind: &str, valid: bool) -> Vec<(String, serde_json::Value)> {
    let dir = PathBuf::from(
        std::env::var_os("EON_OFFICE_SPEC_DIR")
            .expect("set EON_OFFICE_SPEC_DIR to an eon-office-spec checkout"),
    )
    .join("examples")
    .join(if valid { "valid" } else { "invalid" });
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let is_json = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("json"));
        if name.starts_with(&format!("{kind}-")) && is_json {
            let value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            out.push((name, value));
        }
    }
    assert!(!out.is_empty(), "no {kind} examples in {}", dir.display());
    out
}

#[test]
#[ignore = "needs EON_OFFICE_SPEC_DIR pointing at an eon-office-spec checkout; CI sets it"]
fn manifests_agree_with_the_schema() {
    for (name, value) in examples("manifest", true) {
        assert!(
            Manifest::from_json(value).is_ok(),
            "{name}: the schema accepts it, the core refuses it"
        );
    }
    for (name, value) in examples("manifest", false) {
        assert!(
            Manifest::from_json(value).is_err(),
            "{name}: the schema refuses it, the core accepts it"
        );
    }
}

#[test]
#[ignore = "needs EON_OFFICE_SPEC_DIR pointing at an eon-office-spec checkout; CI sets it"]
fn themes_agree_with_the_schema() {
    for (name, value) in examples("theme", true) {
        assert!(
            check_theme(&value).is_ok(),
            "{name}: the schema accepts it, the core refuses it"
        );
    }
    for (name, value) in examples("theme", false) {
        assert!(
            check_theme(&value).is_err(),
            "{name}: the schema refuses it, the core accepts it"
        );
    }
}

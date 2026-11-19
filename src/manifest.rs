// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Application manifests and capabilities.
//!
//! The manifest is how an application tells the core -- and the person
//! installing it -- what it is, what it needs and what it offers. It is the
//! Rust reading of `eon-office-spec/schemas/app-manifest.v0.schema.json`;
//! the two are kept in step by the spec's examples, which this crate's tests
//! also read.
//!
//! Nothing requested is granted by installing. Capabilities are granted by
//! the owner, one by one, and can be revoked at any time.

use crate::error::{Error, Kind, Result};
use crate::util::{check_app_id, check_name};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// The protocol version this core speaks. `0` is unstable, as every EON
/// contract starts (plan, "Kapsam notu").
pub const API_VERSION: u32 = 0;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Capability {
    /// The application's own document store.
    Storage,
    /// Post to the person's notification list.
    Notifications,
    /// Put the application's own items into the shared search index.
    SearchIndex,
    /// Search the shared index (other applications' *shared* items, and its own).
    SearchQuery,
    /// Publish events under the application's own namespace.
    EventsPublish,
    /// Subscribe to other applications' and the core's events.
    EventsSubscribe,
    /// Background jobs: be woken at a time, or every so often.
    Schedule,
    /// Read the directory: users, organisations, workspaces, teams, roles.
    DirectoryRead,
    /// Call the methods another application provides.
    Call(String),
}

impl Capability {
    pub fn parse(text: &str) -> Result<Self> {
        Ok(match text {
            "storage" => Capability::Storage,
            "notifications" => Capability::Notifications,
            "search.index" => Capability::SearchIndex,
            "search.query" => Capability::SearchQuery,
            "events.publish" => Capability::EventsPublish,
            "events.subscribe" => Capability::EventsSubscribe,
            "schedule" => Capability::Schedule,
            "directory.read" => Capability::DirectoryRead,
            other => match other.strip_prefix("call:") {
                Some(target) => {
                    check_app_id(target)?;
                    Capability::Call(target.to_owned())
                }
                None => {
                    return Err(Error::params(format!("unknown capability `{other}`")));
                }
            },
        })
    }

    #[must_use]
    pub fn as_string(&self) -> String {
        match self {
            Capability::Storage => "storage".into(),
            Capability::Notifications => "notifications".into(),
            Capability::SearchIndex => "search.index".into(),
            Capability::SearchQuery => "search.query".into(),
            Capability::EventsPublish => "events.publish".into(),
            Capability::EventsSubscribe => "events.subscribe".into(),
            Capability::Schedule => "schedule".into(),
            Capability::DirectoryRead => "directory.read".into(),
            Capability::Call(target) => format!("call:{target}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ApiRange {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Provided {
    pub method: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    #[default]
    Never,
    OnFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The manifest format version. `0` today.
    pub manifest: u32,
    pub id: String,
    pub name: String,
    pub version: String,
    pub api: ApiRange,
    #[serde(default)]
    pub description: String,
    /// How the core starts the application. Absent: the application starts on
    /// its own and connects, and the core cannot launch it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<Entry>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Methods other applications may call, with `call:<this id>`.
    #[serde(default)]
    pub provides: Vec<Provided>,
    /// Event names published under `<id>/`. Publishing anything else is refused,
    /// so every event an application emits is documented in its manifest.
    #[serde(default)]
    pub publishes: Vec<String>,
    #[serde(default)]
    pub restart: Restart,
}

impl Manifest {
    pub fn from_json(value: serde_json::Value) -> Result<Self> {
        let manifest: Manifest = serde_json::from_value(value)
            .map_err(|e| Error::params(format!("not a valid manifest: {e}")))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<()> {
        if self.manifest != 0 {
            return Err(Error::new(
                Kind::Incompatible,
                format!(
                    "manifest format {} is not known; this core reads 0",
                    self.manifest
                ),
            ));
        }
        check_app_id(&self.id)?;
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 80 {
            return Err(Error::params("`name` must be 1-80 characters"));
        }
        if self.description.chars().count() > 500 {
            return Err(Error::params("`description` is at most 500 characters"));
        }
        Version::parse(&self.version)?;
        if self.api.min > self.api.max {
            return Err(Error::params("`api.min` is greater than `api.max`"));
        }
        if !(self.api.min..=self.api.max).contains(&API_VERSION) {
            return Err(Error::new(
                Kind::Incompatible,
                format!(
                    "the application targets api {}..={}; this core speaks {API_VERSION}",
                    self.api.min, self.api.max
                ),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for text in &self.capabilities {
            let capability = Capability::parse(text)?;
            if capability == Capability::Call(self.id.clone()) {
                return Err(Error::params("an application does not need to call itself"));
            }
            if !seen.insert(capability) {
                return Err(Error::params(format!(
                    "capability `{text}` is listed twice"
                )));
            }
        }
        unique_names("provides", self.provides.iter().map(|p| p.method.as_str()))?;
        unique_names("publishes", self.publishes.iter().map(String::as_str))?;
        if let Some(entry) = &self.entry {
            if entry.command.trim().is_empty() || entry.args.len() > 64 {
                return Err(Error::params(
                    "`entry.command` is required; at most 64 args",
                ));
            }
        }
        Ok(())
    }

    /// The parsed capability list. Valid by construction after `validate`.
    #[must_use]
    pub fn requested(&self) -> Vec<Capability> {
        self.capabilities
            .iter()
            .filter_map(|c| Capability::parse(c).ok())
            .collect()
    }

    #[must_use]
    pub fn provides_method(&self, method: &str) -> bool {
        self.provides.iter().any(|p| p.method == method)
    }
}

fn unique_names<'a>(what: &str, names: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = std::collections::BTreeSet::new();
    for (count, name) in names.enumerate() {
        if count >= 64 {
            return Err(Error::params(format!("`{what}` has more than 64 entries")));
        }
        check_name(what, name)?;
        if !seen.insert(name) {
            return Err(Error::params(format!("`{what}` lists `{name}` twice")));
        }
    }
    Ok(())
}

/// Semantic versions (semver.org 2.0.0), compared by precedence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: [u64; 3],
    pre: Vec<String>,
}

impl Version {
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || Error::params(format!("`{text}` is not a semantic version (1.2.3)"));
        let without_build = text.split('+').next().unwrap_or(text);
        let (core, pre) = match without_build.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (without_build, None),
        };
        let mut numbers = [0u64; 3];
        let parts: Vec<&str> = core.split('.').collect();
        if parts.len() != 3 {
            return Err(bad());
        }
        for (slot, part) in numbers.iter_mut().zip(&parts) {
            if part.is_empty() || (part.len() > 1 && part.starts_with('0')) {
                return Err(bad());
            }
            *slot = part.parse().map_err(|_| bad())?;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
                let valid = ids.iter().all(|id| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if !valid {
                    return Err(bad());
                }
                ids
            }
        };
        Ok(Version { core: numbers, pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let order = match (a.parse::<u64>(), b.parse::<u64>()) {
                            (Ok(x), Ok(y)) => x.cmp(&y),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => a.cmp(b),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> serde_json::Value {
        json!({
            "manifest": 0,
            "id": "org.example.notes",
            "name": "Notes",
            "version": "0.1.0",
            "api": { "min": 0, "max": 0 },
            "capabilities": ["storage", "call:org.example.tasks"],
            "provides": [{ "method": "open" }],
            "publishes": ["note.created"]
        })
    }

    #[test]
    fn a_good_manifest_reads() {
        let manifest = Manifest::from_json(base());
        assert!(manifest.is_ok(), "{manifest:?}");
    }

    #[test]
    fn unknown_fields_and_capabilities_are_refused() {
        let mut m = base();
        m["colour"] = json!("blue");
        assert!(Manifest::from_json(m).is_err());
        let mut m = base();
        m["capabilities"] = json!(["filesystem"]);
        assert!(Manifest::from_json(m).is_err());
        let mut m = base();
        m["capabilities"] = json!(["storage", "storage"]);
        assert!(Manifest::from_json(m).is_err());
    }

    #[test]
    fn an_api_this_core_does_not_speak_is_incompatible() {
        let mut m = base();
        m["api"] = json!({ "min": 1, "max": 2 });
        let kind = Manifest::from_json(m).err().map(|e| e.kind);
        assert_eq!(kind, Some(Kind::Incompatible));
    }

    #[test]
    fn versions_order_by_semver_precedence() -> Result<()> {
        let order = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.10.0",
        ];
        for pair in order.windows(2) {
            assert!(
                Version::parse(pair[0])? < Version::parse(pair[1])?,
                "{pair:?}"
            );
        }
        assert!(Version::parse("01.0.0").is_err());
        assert!(Version::parse("1.0").is_err());
        assert_eq!(Version::parse("1.0.0+build.7")?, Version::parse("1.0.0")?);
        Ok(())
    }
}

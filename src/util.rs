// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Small things several modules need: time, randomness, hashing, and the
//! shape every name in the protocol must have.

use crate::error::{Error, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. A clock before 1970 reads as 0 rather
/// than failing: the hybrid logical clock (`hlc`) keeps order regardless.
#[must_use]
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// `bytes` random bytes from the operating system, as lowercase hex.
pub fn random_hex(bytes: usize) -> Result<String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::getrandom(&mut buffer)
        .map_err(|e| Error::internal(format!("no randomness from the OS: {e}")))?;
    Ok(hex(&buffer))
}

#[must_use]
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[must_use]
pub fn sha256_hex(input: &[u8]) -> String {
    hex(&Sha256::digest(input))
}

/// Constant-time comparison for secrets of equal, public length.
#[must_use]
pub fn same_secret(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Collection names, document ids, setting keys, job names, method names:
/// 1-128 characters of `A-Z a-z 0-9 . _ : -`. Narrow on purpose -- these end
/// up in logs, audit entries and file names, and none of them is prose.
pub fn check_name(what: &str, value: &str) -> Result<()> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(Error::params(format!(
            "`{what}` must be 1-128 characters of A-Z a-z 0-9 . _ : -"
        )))
    }
}

/// Application ids: reverse-DNS, lowercase, at least two labels
/// (`org.example.notes`).
pub fn check_app_id(value: &str) -> Result<()> {
    let labels: Vec<&str> = value.split('.').collect();
    let ok = value.len() <= 100
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        });
    if ok {
        Ok(())
    } else {
        Err(Error::params(
            "an application id is reverse-DNS: lowercase labels of a-z 0-9 -, at least two",
        ))
    }
}

// ---- reading parameters -------------------------------------------------

pub fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::params(format!("`{key}` is required and must be a string")))
}

pub fn opt_str<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(Error::params(format!("`{key}` must be a string"))),
    }
}

pub fn opt_u64(params: &Value, key: &str) -> Result<Option<u64>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .map(Some)
            .ok_or_else(|| Error::params(format!("`{key}` must be a non-negative integer"))),
    }
}

pub fn opt_bool(params: &Value, key: &str) -> Result<Option<bool>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(Error::params(format!("`{key}` must be true or false"))),
    }
}

pub fn str_list(params: &Value, key: &str) -> Result<Vec<String>> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::params(format!("`{key}` must be a list of strings")))
            })
            .collect(),
        Some(_) => Err(Error::params(format!("`{key}` must be a list of strings"))),
    }
}

/// A `limit` parameter, defaulted and capped.
pub fn limit(params: &Value, default: u64, max: u64) -> Result<i64> {
    let value = opt_u64(params, "limit")?.unwrap_or(default).clamp(1, max);
    Ok(i64::try_from(value).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_narrow() {
        assert!(check_name("id", "note:2026-11-19_a.b").is_ok());
        assert!(check_name("id", "").is_err());
        assert!(check_name("id", "has space").is_err());
        assert!(check_name("id", "../escape").is_err());
        assert!(check_name("id", &"x".repeat(129)).is_err());
    }

    #[test]
    fn app_ids_are_reverse_dns() {
        assert!(check_app_id("org.eon.office.notes").is_ok());
        assert!(check_app_id("dev.example.my-app2").is_ok());
        assert!(check_app_id("notes").is_err());
        assert!(check_app_id("Org.Eon.Notes").is_err());
        assert!(check_app_id("org..notes").is_err());
        assert!(check_app_id("org.-notes").is_err());
    }

    #[test]
    fn secrets_compare_by_value() {
        assert!(same_secret("abc", "abc"));
        assert!(!same_secret("abc", "abd"));
        assert!(!same_secret("abc", "abcd"));
    }
}

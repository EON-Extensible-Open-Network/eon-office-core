// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Errors, as the protocol reports them.
//!
//! Every error carries a JSON-RPC numeric code and a stable string code in
//! `data.code`. The string is the contract; the English `message` is for a
//! developer reading a log. An application shows its own words, in its own
//! language, chosen by the string code (madde 40) -- the core never ships
//! user-facing prose.

use serde_json::{json, Value};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Parse,
    InvalidRequest,
    MethodNotFound,
    InvalidParams,
    Internal,
    Unauthenticated,
    Forbidden,
    NotFound,
    Conflict,
    AppUnavailable,
    Timeout,
    RateLimited,
    Incompatible,
    Limit,
    /// The called application answered with an error of its own.
    Remote,
}

impl Kind {
    #[must_use]
    pub fn number(self) -> i64 {
        match self {
            Kind::Parse => -32700,
            Kind::InvalidRequest => -32600,
            Kind::MethodNotFound => -32601,
            Kind::InvalidParams => -32602,
            Kind::Internal => -32603,
            Kind::Unauthenticated => -32001,
            Kind::Forbidden => -32002,
            Kind::NotFound => -32003,
            Kind::Conflict => -32004,
            Kind::AppUnavailable => -32005,
            Kind::Timeout => -32006,
            Kind::RateLimited => -32007,
            Kind::Incompatible => -32008,
            Kind::Limit => -32009,
            Kind::Remote => -32010,
        }
    }

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Kind::Parse => "parse_error",
            Kind::InvalidRequest => "invalid_request",
            Kind::MethodNotFound => "method_not_found",
            Kind::InvalidParams => "invalid_params",
            Kind::Internal => "internal",
            Kind::Unauthenticated => "unauthenticated",
            Kind::Forbidden => "forbidden",
            Kind::NotFound => "not_found",
            Kind::Conflict => "conflict",
            Kind::AppUnavailable => "app_unavailable",
            Kind::Timeout => "timeout",
            Kind::RateLimited => "rate_limited",
            Kind::Incompatible => "incompatible",
            Kind::Limit => "limit",
            Kind::Remote => "remote_error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Error {
    pub kind: Kind,
    pub message: String,
    /// Extra machine-readable detail, merged into `data` beside `code`.
    pub detail: Option<Value>,
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
            detail: None,
        }
    }

    #[must_use]
    pub fn with(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn params(message: impl Into<String>) -> Self {
        Error::new(Kind::InvalidParams, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(Kind::NotFound, message)
    }

    /// A capability the caller was not granted. The capability is named, so an
    /// application can tell the person exactly what to allow.
    #[must_use]
    pub fn missing(capability: &str) -> Self {
        Error::new(
            Kind::Forbidden,
            format!("this needs the `{capability}` capability, which is not granted"),
        )
        .with(json!({ "capability": capability }))
    }

    #[must_use]
    pub fn owner_only() -> Self {
        Error::new(Kind::Forbidden, "only the owner may do this")
            .with(json!({ "capability": "owner" }))
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(Kind::Internal, message)
    }

    /// The `error` member of a JSON-RPC response.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut data = json!({ "code": self.kind.code() });
        if let (Some(Value::Object(extra)), Some(object)) = (&self.detail, data.as_object_mut()) {
            for (key, value) in extra {
                object.insert(key.clone(), value.clone());
            }
        }
        json!({ "code": self.kind.number(), "message": self.message, "data": data })
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(error: rusqlite::Error) -> Self {
        // The database's own text can name tables and values; it goes to the
        // core's log, not to the application that happened to trigger it.
        eprintln!("eon-office-core: database error: {error}");
        Error::internal("storage error")
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Error::params(format!("malformed value: {error}"))
    }
}

impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        eprintln!("eon-office-core: i/o error: {error}");
        Error::internal("i/o error")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_is_merged_beside_the_stable_code() {
        let error = Error::missing("storage").to_json();
        assert_eq!(error["code"], -32002);
        assert_eq!(error["data"]["code"], "forbidden");
        assert_eq!(error["data"]["capability"], "storage");
    }
}

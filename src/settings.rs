// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Settings, preferences and themes.
//!
//! - **Settings** belong to one application: a small key-value store it needs
//!   no capability for, because nobody else can read it.
//! - **Preferences** belong to the person and every application reads them:
//!   language, theme, reduced motion, text size, high contrast. Only the owner
//!   changes them, and a change reaches every connected application at once,
//!   so turning on reduced motion does not depend on which app you were in.
//! - **Themes** are declarative documents -- colours, no code (the rule of
//!   madde 4 carried over). A theme whose text does not reach WCAG AA contrast
//!   against its background is refused with the measured ratios, the same
//!   check EON Stream runs on its own themes (madde 35).

use crate::error::{Error, Result};
use crate::util::check_name;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

const MAX_VALUE_BYTES: usize = 64 * 1024;
const MAX_KEYS_PER_APP: i64 = 1000;

pub fn get(conn: &Connection, scope: &str, key: &str) -> Result<Value> {
    check_name("key", key)?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE scope = ?1 AND key = ?2",
            params![scope, key],
            |r| r.get(0),
        )
        .optional()?;
    Ok(match value {
        Some(text) => serde_json::from_str(&text)?,
        None => Value::Null,
    })
}

/// Set a setting; `null` removes it.
pub fn set(conn: &Connection, scope: &str, key: &str, value: &Value) -> Result<()> {
    check_name("key", key)?;
    if value.is_null() {
        conn.execute(
            "DELETE FROM settings WHERE scope = ?1 AND key = ?2",
            params![scope, key],
        )?;
        return Ok(());
    }
    let text = value.to_string();
    if text.len() > MAX_VALUE_BYTES {
        return Err(Error::new(
            crate::error::Kind::Limit,
            "a setting is at most 64 KiB",
        ));
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM settings WHERE scope = ?1 AND key != ?2",
        params![scope, key],
        |r| r.get(0),
    )?;
    if count >= MAX_KEYS_PER_APP {
        return Err(Error::new(
            crate::error::Kind::Limit,
            "at most 1000 settings per application",
        ));
    }
    conn.execute(
        "INSERT INTO settings (scope, key, value) VALUES (?1, ?2, ?3)
         ON CONFLICT (scope, key) DO UPDATE SET value = excluded.value",
        params![scope, key, text],
    )?;
    Ok(())
}

pub fn list(conn: &Connection, scope: &str) -> Result<Value> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings WHERE scope = ?1 ORDER BY key")?;
    let mut out = serde_json::Map::new();
    let rows = stmt.query_map(params![scope], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (key, value) = row?;
        out.insert(key, serde_json::from_str(&value)?);
    }
    Ok(Value::Object(out))
}

// ---- preferences ---------------------------------------------------------

const PREFS_SCOPE: &str = "prefs";

fn defaults() -> Value {
    json!({
        "locale": "system",
        "theme": "system",
        "reduce_motion": false,
        "high_contrast": false,
        "font_scale": 1.0,
    })
}

pub fn check_locale(locale: &str) -> Result<()> {
    // A BCP 47 language tag, kept to the shape anything here needs:
    // "tr", "en-GB", "pt-BR", "zh-Hant". Or "system": follow the OS.
    let mut parts = locale.split('-');
    let language = parts.next().unwrap_or_default();
    let ok = locale == "system"
        || ((2..=3).contains(&language.len())
            && language.bytes().all(|b| b.is_ascii_lowercase())
            && parts.all(|p| {
                (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric())
            }));
    if ok {
        Ok(())
    } else {
        Err(Error::params(
            "`locale` is a language tag such as tr, en-GB, or `system`",
        ))
    }
}

pub fn prefs(conn: &Connection) -> Result<Value> {
    let mut out = defaults();
    if let (Value::Object(stored), Some(target)) = (list(conn, PREFS_SCOPE)?, out.as_object_mut()) {
        for (key, value) in stored {
            target.insert(key, value);
        }
    }
    Ok(out)
}

pub fn set_pref(conn: &Connection, key: &str, value: &Value) -> Result<()> {
    match key {
        "locale" => check_locale(value.as_str().unwrap_or("-"))?,
        "theme" => {
            let theme = value.as_str().unwrap_or_default();
            let builtin = matches!(theme, "system" | "light" | "dark");
            if !builtin && theme_body(conn, theme)?.is_none() {
                return Err(Error::params(
                    "`theme` is system, light, dark or a registered theme id",
                ));
            }
        }
        "reduce_motion" | "high_contrast" => {
            if !value.is_boolean() {
                return Err(Error::params(format!("`{key}` is true or false")));
            }
        }
        "font_scale" => {
            let scale = value.as_f64().unwrap_or(0.0);
            if !(0.75..=2.0).contains(&scale) {
                return Err(Error::params("`font_scale` is between 0.75 and 2.0"));
            }
        }
        _ => {
            return Err(Error::params(
                "a preference is locale, theme, reduce_motion, high_contrast or font_scale",
            ))
        }
    }
    set(conn, PREFS_SCOPE, key, value)
}

// ---- themes --------------------------------------------------------------

const THEME_COLOURS: [&str; 6] = [
    "background",
    "surface",
    "text",
    "muted",
    "accent",
    "accent_text",
];

/// Text pairs that must reach WCAG 2.1 AA for normal text (4.5:1).
const CONTRAST_PAIRS: [(&str, &str); 4] = [
    ("text", "background"),
    ("text", "surface"),
    ("muted", "background"),
    ("accent_text", "accent"),
];

fn parse_hex(colour: &str) -> Option<[f64; 3]> {
    let hex = colour.strip_prefix('#')?;
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let channel = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(f64::from);
    Some([channel(0)?, channel(2)?, channel(4)?])
}

/// WCAG 2.1 relative luminance.
fn luminance(rgb: [f64; 3]) -> f64 {
    let lin = |c: f64| {
        let c = c / 255.0;
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

#[must_use]
pub fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

fn theme_body(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT body FROM themes WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?)
}

/// Check a theme document; on success, the measured ratios.
pub fn check_theme(theme: &Value) -> Result<Value> {
    let object = theme
        .as_object()
        .ok_or_else(|| Error::params("a theme is an object"))?;
    for key in object.keys() {
        if !matches!(key.as_str(), "id" | "name" | "colors") {
            return Err(Error::params(format!(
                "a theme has no field `{key}`; themes carry no code"
            )));
        }
    }
    let id = theme.get("id").and_then(Value::as_str).unwrap_or_default();
    check_name("id", id)?;
    if matches!(id, "system" | "light" | "dark") {
        return Err(Error::params("system, light and dark are built in"));
    }
    let colours = theme
        .get("colors")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::params("`colors` is required"))?;
    let mut rgb = std::collections::BTreeMap::new();
    for name in THEME_COLOURS {
        let value = colours
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or_default();
        let parsed = parse_hex(value)
            .ok_or_else(|| Error::params(format!("`colors.{name}` must be #rrggbb")))?;
        rgb.insert(name, parsed);
    }
    if colours.len() != THEME_COLOURS.len() {
        return Err(Error::params(format!(
            "`colors` has exactly: {}",
            THEME_COLOURS.join(", ")
        )));
    }
    let mut ratios = serde_json::Map::new();
    let mut failing = Vec::new();
    for (fg, bg) in CONTRAST_PAIRS {
        let ratio = contrast(rgb[fg], rgb[bg]);
        let rounded = (ratio * 100.0).round() / 100.0;
        ratios.insert(format!("{fg}/{bg}"), json!(rounded));
        if ratio < 4.5 {
            failing.push(format!("{fg} on {bg} is {rounded}:1"));
        }
    }
    if !failing.is_empty() {
        return Err(
            Error::params(format!("below WCAG AA (4.5:1): {}", failing.join("; ")))
                .with(json!({ "ratios": ratios })),
        );
    }
    Ok(Value::Object(ratios))
}

pub fn put_theme(conn: &Connection, theme: &Value) -> Result<Value> {
    let ratios = check_theme(theme)?;
    let id = theme.get("id").and_then(Value::as_str).unwrap_or_default();
    conn.execute(
        "INSERT INTO themes (id, body) VALUES (?1, ?2) ON CONFLICT (id) DO UPDATE SET body = excluded.body",
        params![id, theme.to_string()],
    )?;
    Ok(json!({ "id": id, "ratios": ratios }))
}

pub fn themes(conn: &Connection) -> Result<Value> {
    let mut stmt = conn.prepare("SELECT body FROM themes ORDER BY id")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(serde_json::from_str::<Value>(&row?)?);
    }
    Ok(json!({ "builtin": ["system", "light", "dark"], "themes": out }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_in_memory;

    fn theme(text: &str) -> Value {
        json!({ "id": "paper", "name": "Paper", "colors": {
            "background": "#ffffff", "surface": "#f4f4f4", "text": text,
            "muted": "#595959", "accent": "#0b57d0", "accent_text": "#ffffff" } })
    }

    #[test]
    fn contrast_matches_the_wcag_reference_values() {
        let black = [0.0, 0.0, 0.0];
        let white = [255.0, 255.0, 255.0];
        assert!((contrast(black, white) - 21.0).abs() < 0.01);
        assert!((contrast(white, white) - 1.0).abs() < 0.01);
    }

    #[test]
    fn a_theme_below_aa_is_refused_with_its_ratios() -> Result<()> {
        let conn = open_in_memory()?;
        assert!(put_theme(&conn, &theme("#111111")).is_ok());
        let error = put_theme(&conn, &theme("#bbbbbb")).err();
        let detail = error.and_then(|e| e.detail).unwrap_or_default();
        assert!(detail["ratios"]["text/background"].as_f64().unwrap_or(99.0) < 4.5);
        Ok(())
    }

    #[test]
    fn a_theme_cannot_smuggle_anything_but_colours() {
        let mut t = theme("#111111");
        t["script"] = json!("alert(1)");
        assert!(check_theme(&t).is_err());
    }

    #[test]
    fn preferences_are_validated_and_fall_back_to_defaults() -> Result<()> {
        let conn = open_in_memory()?;
        assert_eq!(prefs(&conn)?["font_scale"], 1.0);
        set_pref(&conn, "locale", &json!("tr"))?;
        set_pref(&conn, "reduce_motion", &json!(true))?;
        assert!(set_pref(&conn, "font_scale", &json!(9)).is_err());
        assert!(set_pref(&conn, "locale", &json!("Turkish")).is_err());
        assert!(set_pref(&conn, "theme", &json!("nonexistent")).is_err());
        assert!(set_pref(&conn, "wallpaper", &json!("x")).is_err());
        let p = prefs(&conn)?;
        assert_eq!(
            (p["locale"].clone(), p["reduce_motion"].clone()),
            (json!("tr"), json!(true))
        );
        Ok(())
    }
}

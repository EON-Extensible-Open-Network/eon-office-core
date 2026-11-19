// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Folding text for search.
//!
//! Madde 40 names the mistake this exists to avoid: Turkish has four i's
//! (`I ı İ i`), and a search that lower-cases with the root locale turns
//! "İstanbul" into "i̇stanbul" -- with a combining dot -- and never finds it
//! again. Folding here is deliberately *lossy*: every i collapses to `i` and
//! common diacritics are dropped, so a person typing "isik" on a keyboard
//! without Turkish letters still finds "ışık". Search wants recall; the
//! original text is stored untouched beside the folded terms.

/// Fold one character to its search key. Returns `None` for characters that
/// vanish (combining marks).
#[must_use]
pub fn fold_char(c: char) -> Option<char> {
    Some(match c {
        'I' | 'İ' | 'ı' | 'i' | 'Î' | 'î' | 'Ì' | 'ì' | 'Í' | 'í' | 'Ï' | 'ï' => 'i',
        'Ş' | 'ş' => 's',
        'Ğ' | 'ğ' => 'g',
        'Ç' | 'ç' => 'c',
        'Ö' | 'ö' | 'Ò' | 'ò' | 'Ó' | 'ó' | 'Ô' | 'ô' | 'Õ' | 'õ' | 'Ø' | 'ø' => 'o',
        'Ü' | 'ü' | 'Ù' | 'ù' | 'Ú' | 'ú' | 'Û' | 'û' => 'u',
        'À' | 'à' | 'Á' | 'á' | 'Â' | 'â' | 'Ã' | 'ã' | 'Ä' | 'ä' | 'Å' | 'å' => 'a',
        'È' | 'è' | 'É' | 'é' | 'Ê' | 'ê' | 'Ë' | 'ë' => 'e',
        'Ñ' | 'ñ' => 'n',
        '\u{0300}'..='\u{036f}' => return None,
        other => other.to_lowercase().next().unwrap_or(other),
    })
}

/// Fold a whole string.
#[must_use]
pub fn fold(text: &str) -> String {
    text.chars().filter_map(fold_char).collect()
}

/// The longest term kept. Longer runs are almost always data (hashes, base64)
/// that no person types into a search box.
pub const MAX_TERM: usize = 48;

/// Split into folded terms. Anything that is not a letter or digit separates.
#[must_use]
pub fn terms(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || ('\u{0300}'..='\u{036f}').contains(&c) {
            if let Some(f) = fold_char(c) {
                current.push(f);
            }
        } else if !current.is_empty() {
            push_term(&mut out, std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        push_term(&mut out, current);
    }
    out
}

fn push_term(out: &mut Vec<String>, term: String) {
    if term.chars().count() <= MAX_TERM {
        out.push(term);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_turkish_is_meet() {
        assert_eq!(fold("İSTANBUL"), "istanbul");
        assert_eq!(fold("istanbul"), "istanbul");
        assert_eq!(fold("IŞIK"), fold("ışık"));
        assert_eq!(fold("ışık"), "isik");
    }

    #[test]
    fn a_combining_dot_does_not_survive() {
        // What a root-locale lowercase of "İ" produces.
        assert_eq!(fold("i\u{0307}stanbul"), "istanbul");
    }

    #[test]
    fn terms_split_on_anything_but_letters_and_digits() {
        assert_eq!(
            terms("Çağrı: 2026 bütçe-raporu, ÖZET!"),
            vec!["cagri", "2026", "butce", "raporu", "ozet"]
        );
        assert!(terms(&"x".repeat(MAX_TERM + 1)).is_empty());
    }
}

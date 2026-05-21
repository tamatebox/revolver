//! String normalization for fuzzy search (#6).
//!
//! Applied once at scan / upsert time into shadow `*_norm` columns, and once
//! at query time against the user's search input. Both sides going through
//! the same function guarantees a deterministic match regardless of accent
//! marks, halfwidth / fullwidth differences, or hiragana / katakana drift.
//!
//! Pipeline (in order):
//!
//! 1. **NFKD** — decomposes accent characters into base + combining mark,
//!    and folds fullwidth Latin / halfwidth katakana to canonical forms.
//!    e.g. `café` → `cafe` + ` ́`; `Ｂｅａｔｌｅｓ` → `Beatles`;
//!    `ｶﾝﾄﾞｰ` → `カンドー`.
//! 2. **Strip combining marks** — drops the diacritics left over from
//!    step 1 (`é` → `e`, `ö` → `o`).
//! 3. **Lowercase** — single-pass ASCII + Unicode lowercase so the column
//!    compares case-insensitively without depending on `COLLATE NOCASE`.
//! 4. **Katakana → hiragana** — maps `カ` (U+30AB)..`ヶ` (U+30F6) down by
//!    `0x60`. After NFKD the halfwidth katakana family has already been
//!    promoted to fullwidth katakana, so this single shift covers all
//!    three scripts (hiragana / fullwidth katakana / halfwidth katakana).
//!
//! Out of scope (separate follow-ups): romaji conversion, edit distance,
//! FTS5 trigram. See issue #6.

use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// Normalize `s` for fuzzy comparison. The returned string is used both as
/// the shadow column value and as the search-input transform.
pub fn for_search(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.nfkd() {
        if is_combining_mark(c) {
            continue;
        }
        // Manually fold katakana → hiragana (U+30A1..U+30F6 → U+3041..U+3096).
        // NFKD already folded halfwidth katakana into this fullwidth range,
        // so one shift catches all three forms.
        let folded = if ('\u{30A1}'..='\u{30F6}').contains(&c) {
            char::from_u32(c as u32 - 0x60).unwrap_or(c)
        } else {
            c
        };
        // `to_lowercase` returns an iterator because some characters lower
        // to multiple scalars (e.g. German `ß` → `ss`); push each.
        for lc in folded.to_lowercase() {
            out.push(lc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── NFKD + combining marks ──────────────────────────────────────────

    #[test]
    fn n1_accents_are_stripped() {
        assert_eq!(for_search("café"), "cafe");
        assert_eq!(for_search("Björk"), "bjork");
        assert_eq!(for_search("Sigur Rós"), "sigur ros");
    }

    #[test]
    fn n2_halfwidth_and_fullwidth_latin_fold_to_ascii() {
        // Fullwidth letters fold to half-width via NFKD compatibility.
        assert_eq!(for_search("Ｂｅａｔｌｅｓ"), "beatles");
        assert_eq!(for_search("Ｂｊｏｒｋ"), "bjork");
    }

    #[test]
    fn n3_case_is_folded() {
        assert_eq!(for_search("ABBEY ROAD"), "abbey road");
        assert_eq!(for_search("Abbey Road"), "abbey road");
        assert_eq!(for_search("abbey road"), "abbey road");
    }

    // ── Katakana / hiragana / halfwidth katakana ────────────────────────

    #[test]
    fn n4_katakana_normalizes_to_hiragana() {
        // Full-width katakana → hiragana directly.
        assert_eq!(for_search("ミユキ"), "みゆき");
    }

    #[test]
    fn n5_halfwidth_katakana_normalizes_to_hiragana() {
        // NFKD promotes ﾐﾕｷ to ミユキ first, then the katakana→hiragana shift applies.
        assert_eq!(for_search("ﾐﾕｷ"), "みゆき");
    }

    #[test]
    fn n6_hiragana_unchanged() {
        // Already lowercase + no marks; round-trips through the pipeline.
        assert_eq!(for_search("みゆき"), "みゆき");
    }

    // ── Idempotency: normalizing twice is the same as normalizing once
    // (required so a backfilled column never drifts from a re-upserted value).
    #[test]
    fn n7_idempotent_on_mixed_input() {
        let once = for_search("Café Ｂeatles ﾐﾕｷ");
        let twice = for_search(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn n8_empty_string_round_trip() {
        assert_eq!(for_search(""), "");
    }
}

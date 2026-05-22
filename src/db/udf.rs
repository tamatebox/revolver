//! Connection-scoped SQL user-defined functions (#28).
//!
//! Registered on every pooled connection via the `r2d2` init hook in
//! [`crate::db::pool`]. UDFs added here are visible to every query in
//! the process without per-call setup.
//!
//! Currently registers:
//! - `jaccard_trigram(a, b)`: trigram-set Jaccard similarity ∈ [0, 1]
//!   used by fuzzy Search to threshold-cut FTS5-trigram-OR candidates
//!   and to rank typo hits inside bucket 4. NULL on either side returns 0.0.

use std::collections::HashSet;

use rusqlite::functions::FunctionFlags;
use rusqlite::Connection;

/// Register all per-connection UDFs. Idempotent — re-registering replaces
/// the previous binding, which is what we want when a pooled connection
/// is re-acquired.
pub fn register(conn: &Connection) -> rusqlite::Result<()> {
    conn.create_scalar_function(
        "jaccard_trigram",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let a: Option<String> = ctx.get(0)?;
            let b: Option<String> = ctx.get(1)?;
            let (a, b) = match (a, b) {
                (Some(a), Some(b)) => (a, b),
                _ => return Ok(0.0_f64),
            };
            Ok(jaccard_trigram(&a, &b))
        },
    )?;
    Ok(())
}

/// Trigram-set Jaccard similarity: `|A ∩ B| / |A ∪ B|`. PostgreSQL `pg_trgm`
/// uses the same formulation. Returns 0.0 when either side is shorter than
/// 3 characters (no trigrams to compare). Inputs are expected to already
/// be normalized via [`crate::normalize::for_search`] so case / accent /
/// kana variants converge before the trigram split.
fn jaccard_trigram(a: &str, b: &str) -> f64 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let intersection = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// Distinct trigrams of `s`. Duplicates collapse so a repeated 3-gram
/// doesn't inflate the union (e.g. "aaaa" gives `{"aaa"}`, not `{"aaa", "aaa"}`).
fn trigrams(s: &str) -> HashSet<String> {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 3 {
        return HashSet::new();
    }
    chars.windows(3).map(|w| w.iter().collect()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn j1_identical_strings_score_one() {
        assert_eq!(jaccard_trigram("beatles", "beatles"), 1.0);
    }

    #[test]
    fn j2_disjoint_strings_score_zero() {
        // "abc" and "xyz" share no trigram.
        assert_eq!(jaccard_trigram("abc", "xyz"), 0.0);
    }

    #[test]
    fn j3_typo_swap_scores_high_enough_for_default_threshold() {
        // "beatles" vs "beatlse" (adjacent-letter swap) — common pg_trgm
        // default threshold is 0.3. Should clear.
        let s = jaccard_trigram("beatles", "beatlse");
        assert!(s >= 0.3, "expected ≥ 0.3, got {s}");
    }

    #[test]
    fn j4_one_trigram_overlap_scores_below_threshold() {
        // The bucket-4 noise case that motivated the threshold: only `atl`
        // is shared between these two strings. Must score well under 0.3.
        let s = jaccard_trigram("beatles", "atlas");
        assert!(s < 0.3, "expected < 0.3, got {s}");
    }

    #[test]
    fn j5_substring_relationship_scores_well() {
        // The trade-off the Levenshtein approach would have broken:
        // "beatles" as substring of "the beatles" must still pass.
        let s = jaccard_trigram("beatles", "the beatles");
        assert!(s >= 0.3, "expected ≥ 0.3, got {s}");
    }

    #[test]
    fn j6_short_input_returns_zero() {
        // Fewer than 3 chars has no trigram; we return 0 instead of NaN.
        assert_eq!(jaccard_trigram("ab", "abc"), 0.0);
        assert_eq!(jaccard_trigram("", ""), 0.0);
    }

    #[test]
    fn j7_repeated_trigrams_dedupe_in_union() {
        // "aaaa" has trigrams {"aaa"} (after dedup) → Jaccard with itself = 1.0,
        // and with "aaab" = 1/2 (shared "aaa", "aab" unique).
        assert_eq!(jaccard_trigram("aaaa", "aaaa"), 1.0);
        let s = jaccard_trigram("aaaa", "aaab");
        assert!((s - 0.5).abs() < f64::EPSILON, "got {s}");
    }
}

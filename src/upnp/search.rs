//! Minimal parser for UPnP ContentDirectory:1 SearchCriteria (SPEC §5.4).
//!
//! Does not implement the full UPnP spec — only extracts **one** of
//! `dc:title contains "X"` / `upnp:artist contains "X"` / `upnp:album contains "X"`.
//! Auxiliary conditions like `derivedfrom` are ignored, and unsupported criteria
//! return `Unsupported` (empty result), realizing the "no-op over misbehavior"
//! policy from SPEC §5.4.

/// Parse result. One of the 3 supported properties, or unsupported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchExpr {
    Title(String),
    Artist(String),
    Album(String),
    Unsupported,
}

/// Extract a `SearchExpr` from a SearchCriteria string. Priority: title > artist > album.
/// Extraction uses substring matching anywhere in the input (ignores auxiliary
/// conditions such as `derivedfrom "..."`).
pub fn parse_criteria(input: &str) -> SearchExpr {
    if let Some(value) = find_contains(input, "dc:title") {
        return SearchExpr::Title(value);
    }
    if let Some(value) = find_contains(input, "upnp:artist") {
        return SearchExpr::Artist(value);
    }
    if let Some(value) = find_contains(input, "upnp:album") {
        return SearchExpr::Album(value);
    }
    SearchExpr::Unsupported
}

/// Extract value from the `{prop} contains "{value}"` pattern. Any amount of whitespace
/// is allowed. Unescapes one level of `\"` in the string (minimal UPnP-spec handling).
fn find_contains(input: &str, prop: &str) -> Option<String> {
    let start = input.find(prop)?;
    let rest = &input[start + prop.len()..];
    let after_contains = skip_token(rest, "contains")?;
    let quoted = after_contains.trim_start();
    let body = quoted.strip_prefix('"')?;
    // Find the closing `"`. `\"` is not a terminator, so skip it.
    let mut out = String::new();
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if matches!(chars.peek(), Some('"')) => {
                chars.next();
                out.push('"');
            }
            '"' => return Some(out),
            other => out.push(other),
        }
    }
    None
}

/// Skip leading whitespace, then consume `token`. Returns None if it does not match.
fn skip_token<'a>(s: &'a str, token: &str) -> Option<&'a str> {
    let trimmed = s.trim_start();
    let rest = trimmed.strip_prefix(token)?;
    // Ensure a word boundary follows `token` (prevents false matches like `containss`).
    match rest.chars().next() {
        None => Some(rest),
        Some(c) if c.is_whitespace() || c == '"' => Some(rest),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sp1_parse_title_contains() {
        assert_eq!(
            parse_criteria(r#"dc:title contains "abc""#),
            SearchExpr::Title("abc".to_string())
        );
    }

    #[test]
    fn sp2_parse_artist_contains() {
        assert_eq!(
            parse_criteria(r#"upnp:artist contains "Eloy""#),
            SearchExpr::Artist("Eloy".to_string())
        );
    }

    #[test]
    fn sp3_parse_album_contains() {
        assert_eq!(
            parse_criteria(r#"upnp:album contains "Live""#),
            SearchExpr::Album("Live".to_string())
        );
    }

    #[test]
    fn sp4_parse_ignores_derivedfrom_when_title_present() {
        // The upnp:class auxiliary condition is ignored; title is picked as the main condition.
        let raw = r#"upnp:class derivedfrom "object.item.audioItem" and dc:title contains "x""#;
        assert_eq!(parse_criteria(raw), SearchExpr::Title("x".to_string()));
    }

    #[test]
    fn sp5_parse_unsupported_for_wildcard_or_empty_or_unknown() {
        assert_eq!(parse_criteria("*"), SearchExpr::Unsupported);
        assert_eq!(parse_criteria(""), SearchExpr::Unsupported);
        assert_eq!(
            parse_criteria(r#"upnp:genre contains "Rock""#),
            SearchExpr::Unsupported
        );
    }

    #[test]
    fn sp_handles_escaped_quote_in_value() {
        // `\"` inside the value is captured as `"`.
        assert_eq!(
            parse_criteria(r#"dc:title contains "She said \"hi\"""#),
            SearchExpr::Title("She said \"hi\"".to_string())
        );
    }

    #[test]
    fn sp_handles_extra_whitespace() {
        assert_eq!(
            parse_criteria(r#"dc:title   contains   "abc""#),
            SearchExpr::Title("abc".to_string())
        );
    }

    #[test]
    fn sp_title_takes_priority_over_artist_when_both_present() {
        // Rare, but if both are present prefer title (order of checks in parse_criteria).
        let raw = r#"dc:title contains "T" and upnp:artist contains "A""#;
        assert_eq!(parse_criteria(raw), SearchExpr::Title("T".to_string()));
    }

    #[test]
    fn sp5b_parse_derivedfrom_only_is_unsupported() {
        // When Linn issues a class-filter-only query (e.g., "all under audioItem"),
        // treat it as Unsupported (SPEC §5.4 policy: no-op as a safety net over partial support).
        assert_eq!(
            parse_criteria(r#"upnp:class derivedfrom "object.item.audioItem""#),
            SearchExpr::Unsupported
        );
        // Trailing semicolons / extra parens do not change the outcome.
        assert_eq!(
            parse_criteria(r#"(upnp:class derivedfrom "object.item.audioItem.musicTrack")"#),
            SearchExpr::Unsupported
        );
    }

    #[test]
    fn sp_rejects_unterminated_string() {
        assert_eq!(
            parse_criteria(r#"dc:title contains "abc"#),
            SearchExpr::Unsupported
        );
    }
}

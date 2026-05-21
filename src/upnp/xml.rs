//! Shared XML escape helpers for UPnP output (DIDL-Lite, device description,
//! SOAP envelopes). Single-pass `with_capacity` + push to avoid the
//! `String::replace`-chain alloc storm on hot Browse pages.
//!
//! Control characters disallowed by XML 1.0 (`\x00..=\x08`, `\x0B`, `\x0C`,
//! `\x0E..=\x1F`) are **dropped** so a stray byte in a tag value cannot make
//! Linn (or any strict parser) reject the whole document. `\t \n \r` are kept.

/// Escape `s` for use as XML text content. Escapes `& < >`; drops disallowed
/// control characters. `"` and `'` are intentionally not escaped — neither is
/// required in text nodes per XML 1.0.
pub fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            other => out.push(other),
        }
    }
    out
}

/// Escape `s` for use as an XML attribute value delimited by `"..."`. Adds `"`
/// to the [`escape_text`] set. `'` stays unescaped because our templates never
/// use `'`-delimited attributes.
pub fn escape_attr(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {}
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_escapes_amp_lt_gt() {
        assert_eq!(
            escape_text("Tom & Jerry <Album>"),
            "Tom &amp; Jerry &lt;Album&gt;"
        );
    }

    #[test]
    fn text_preserves_apostrophe_and_quote() {
        assert_eq!(escape_text(r#"Jerry's "Album""#), r#"Jerry's "Album""#);
    }

    #[test]
    fn text_drops_disallowed_control_chars_keeps_tab_newline_cr() {
        let raw = "a\x01b\x07c\x0Bd\x1Fe\tf\ng\rh";
        assert_eq!(escape_text(raw), "abcde\tf\ng\rh");
    }

    #[test]
    fn attr_escapes_quote_too() {
        assert_eq!(escape_attr(r#"a"b<c"#), "a&quot;b&lt;c");
    }

    #[test]
    fn attr_preserves_apostrophe() {
        assert_eq!(escape_attr("Jerry's"), "Jerry's");
    }
}

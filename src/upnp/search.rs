//! Parser for UPnP ContentDirectory:1 `SearchCriteria` (SPEC §5.4).
//!
//! Supports the subset Linn / Kazoo actually send (observed via #4):
//!
//! - `upnp:class derivedfrom "X"` — class filter (Album / Artist / Track).
//! - `dc:title contains "X"`, `upnp:album contains "X"`, `upnp:artist contains "X"`,
//!   `upnp:genre contains "X"` — substring predicates.
//! - `upnp:artist[@role="Composer"] contains "X"` — role-attribute filter.
//!   The role string is captured but the SQL layer currently ignores it
//!   (will start mattering once #9 lands the COMPOSER tag).
//! - `and` / `or` composition with parentheses.
//!
//! Anything outside this subset (unknown property, malformed quoting,
//! unrecognized `derivedfrom`) collapses to `Predicate::True` or
//! `ClassFilter::Any`, preferring a no-op over a 500.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassFilter {
    Album,  // object.container.album...
    Artist, // object.container.person.musicArtist...
    Track,  // object.item.audioItem...
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Property {
    Title,
    Album,
    Artist,
    Genre,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Predicate {
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Contains {
        prop: Property,
        role: Option<String>,
        value: String,
    },
    /// Sentinel: pulled out into [`ClassFilter`] by [`split_class_filter`].
    DerivedFrom(String),
    /// Identity (vacuously true), produced when a branch yields nothing.
    True,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchExpr {
    pub class: ClassFilter,
    pub predicate: Predicate,
}

impl SearchExpr {
    pub fn is_no_op(&self) -> bool {
        self.class == ClassFilter::Any && self.predicate == Predicate::True
    }
}

pub fn parse_criteria(input: &str) -> SearchExpr {
    let trimmed = input.trim();
    if trimmed.is_empty() || trimmed == "*" {
        return SearchExpr {
            class: ClassFilter::Any,
            predicate: Predicate::True,
        };
    }
    let mut parser = Parser::new(trimmed);
    let raw = parser.parse_expression().unwrap_or(Predicate::True);
    let (class, residual) = split_class_filter(raw);
    SearchExpr {
        class,
        predicate: residual,
    }
}

/// Walk the tree pulling every `DerivedFrom(...)` leaf out into a
/// `ClassFilter`. Multiple `derivedfrom`s collapse to the **last one seen**
/// (Linn never sends more than one in practice).
fn split_class_filter(p: Predicate) -> (ClassFilter, Predicate) {
    let mut class = ClassFilter::Any;
    let residual = rewrite(p, &mut class);
    (class, residual)
}

fn rewrite(p: Predicate, class: &mut ClassFilter) -> Predicate {
    match p {
        Predicate::DerivedFrom(s) => {
            *class = classify(&s);
            Predicate::True
        }
        Predicate::And(children) => {
            let kept: Vec<_> = children
                .into_iter()
                .map(|c| rewrite(c, class))
                .filter(|c| !matches!(c, Predicate::True))
                .collect();
            match kept.len() {
                0 => Predicate::True,
                1 => kept.into_iter().next().unwrap(),
                _ => Predicate::And(kept),
            }
        }
        Predicate::Or(children) => {
            let kept: Vec<_> = children
                .into_iter()
                .map(|c| rewrite(c, class))
                .filter(|c| !matches!(c, Predicate::True))
                .collect();
            match kept.len() {
                0 => Predicate::True,
                1 => kept.into_iter().next().unwrap(),
                _ => Predicate::Or(kept),
            }
        }
        Predicate::Contains { .. } | Predicate::True => p,
    }
}

fn classify(s: &str) -> ClassFilter {
    let s_lower = s.to_ascii_lowercase();
    if s_lower.starts_with("object.container.person.musicartist") {
        ClassFilter::Artist
    } else if s_lower.starts_with("object.container.album") {
        ClassFilter::Album
    } else if s_lower.starts_with("object.item.audioitem") {
        ClassFilter::Track
    } else {
        ClassFilter::Any
    }
}

// ── Tokenizer + recursive-descent parser ──────────────────────────────────

#[derive(Debug)]
enum Token {
    Word(String),
    String(String),
    OpenParen,
    CloseParen,
    Bracketed(String),
}

struct Parser<'a> {
    src: &'a str,
    pos: usize,
    peeked: Option<Token>,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src,
            pos: 0,
            peeked: None,
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        if let Some(t) = self.peeked.take() {
            return Some(t);
        }
        self.skip_whitespace();
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() {
            return None;
        }
        match bytes[self.pos] {
            b'(' => {
                self.pos += 1;
                Some(Token::OpenParen)
            }
            b')' => {
                self.pos += 1;
                Some(Token::CloseParen)
            }
            b'"' => self.read_string().map(Token::String),
            b'[' => self.read_bracketed().map(Token::Bracketed),
            _ => self.read_word().map(Token::Word),
        }
    }

    fn peek_token(&mut self) -> Option<&Token> {
        if self.peeked.is_none() {
            self.peeked = self.next_token();
        }
        self.peeked.as_ref()
    }

    fn skip_whitespace(&mut self) {
        let bytes = self.src.as_bytes();
        while self.pos < bytes.len() && bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn read_string(&mut self) -> Option<String> {
        // Walks the source byte-by-byte to locate the closing `"` and `\"`
        // escape boundaries, then copies the in-between content via slices of
        // `self.src` (a `&str`) so multibyte UTF-8 sequences inside the string
        // literal are preserved verbatim. The pre-#6 impl pushed individual
        // bytes as `char`, which corrupted any non-ASCII search value
        // ("Björk" → "BjÃ¶rk"). Delimiters (`"`, `\`) are all single-byte
        // ASCII so byte-level seeking remains correct.
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() || bytes[self.pos] != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        let mut segment_start = self.pos;
        while self.pos < bytes.len() {
            let c = bytes[self.pos];
            if c == b'\\' && self.pos + 1 < bytes.len() && bytes[self.pos + 1] == b'"' {
                out.push_str(&self.src[segment_start..self.pos]);
                out.push('"');
                self.pos += 2;
                segment_start = self.pos;
                continue;
            }
            if c == b'"' {
                out.push_str(&self.src[segment_start..self.pos]);
                self.pos += 1;
                return Some(out);
            }
            self.pos += 1;
        }
        None
    }

    fn read_bracketed(&mut self) -> Option<String> {
        let bytes = self.src.as_bytes();
        if self.pos >= bytes.len() || bytes[self.pos] != b'[' {
            return None;
        }
        let start = self.pos + 1;
        let mut p = start;
        while p < bytes.len() && bytes[p] != b']' {
            p += 1;
        }
        if p >= bytes.len() {
            return None;
        }
        let content = self.src[start..p].to_string();
        self.pos = p + 1;
        Some(content)
    }

    fn read_word(&mut self) -> Option<String> {
        let bytes = self.src.as_bytes();
        let start = self.pos;
        while self.pos < bytes.len() {
            let c = bytes[self.pos];
            if c.is_ascii_whitespace() || c == b'(' || c == b')' || c == b'"' || c == b'[' {
                break;
            }
            self.pos += 1;
        }
        if self.pos == start {
            return None;
        }
        Some(self.src[start..self.pos].to_string())
    }

    fn parse_expression(&mut self) -> Option<Predicate> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Option<Predicate> {
        let first = self.parse_and()?;
        let mut alts = vec![first];
        while let Some(t) = self.peek_token() {
            if let Token::Word(w) = t {
                if w.eq_ignore_ascii_case("or") {
                    let _ = self.next_token();
                    if let Some(rhs) = self.parse_and() {
                        alts.push(rhs);
                        continue;
                    }
                }
            }
            break;
        }
        Some(if alts.len() == 1 {
            alts.into_iter().next().unwrap()
        } else {
            Predicate::Or(alts)
        })
    }

    fn parse_and(&mut self) -> Option<Predicate> {
        let first = self.parse_primary()?;
        let mut alts = vec![first];
        while let Some(t) = self.peek_token() {
            if let Token::Word(w) = t {
                if w.eq_ignore_ascii_case("and") {
                    let _ = self.next_token();
                    if let Some(rhs) = self.parse_primary() {
                        alts.push(rhs);
                        continue;
                    }
                }
            }
            break;
        }
        Some(if alts.len() == 1 {
            alts.into_iter().next().unwrap()
        } else {
            Predicate::And(alts)
        })
    }

    fn parse_primary(&mut self) -> Option<Predicate> {
        match self.next_token()? {
            Token::OpenParen => {
                let inner = self.parse_or().unwrap_or(Predicate::True);
                if let Some(Token::CloseParen) = self.peek_token() {
                    let _ = self.next_token();
                }
                Some(inner)
            }
            Token::Word(prop_word) => {
                let role = match self.peek_token() {
                    Some(Token::Bracketed(_)) => {
                        if let Some(Token::Bracketed(content)) = self.next_token() {
                            extract_role(&content)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                let op = match self.next_token()? {
                    Token::Word(w) => w,
                    _ => return None,
                };
                let value = match self.next_token()? {
                    Token::String(s) => s,
                    _ => return None,
                };
                Some(build_predicate(&prop_word, role, &op, value))
            }
            _ => None,
        }
    }
}

fn extract_role(bracket: &str) -> Option<String> {
    let trimmed = bracket.trim_start_matches('@').trim();
    let (lhs, rhs) = trimmed.split_once('=')?;
    if !lhs.trim().eq_ignore_ascii_case("role") {
        return None;
    }
    let v = rhs.trim().trim_matches('"');
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn build_predicate(prop: &str, role: Option<String>, op: &str, value: String) -> Predicate {
    if prop.eq_ignore_ascii_case("upnp:class") && op.eq_ignore_ascii_case("derivedfrom") {
        return Predicate::DerivedFrom(value);
    }
    if !op.eq_ignore_ascii_case("contains") {
        return Predicate::True;
    }
    let p = match prop.to_ascii_lowercase().as_str() {
        "dc:title" => Property::Title,
        "upnp:album" => Property::Album,
        "upnp:artist" => Property::Artist,
        "upnp:genre" => Property::Genre,
        _ => return Predicate::True, // drop unknown properties as no-op
    };
    Predicate::Contains {
        prop: p,
        role,
        value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> SearchExpr {
        parse_criteria(s)
    }

    #[test]
    fn sp1_dc_title_contains_only() {
        let e = parse(r#"dc:title contains "abc""#);
        assert_eq!(e.class, ClassFilter::Any);
        assert_eq!(
            e.predicate,
            Predicate::Contains {
                prop: Property::Title,
                role: None,
                value: "abc".to_string()
            }
        );
    }

    #[test]
    fn sp2_class_album_with_title() {
        let e =
            parse(r#"upnp:class derivedfrom "object.container.album" and dc:title contains "X""#);
        assert_eq!(e.class, ClassFilter::Album);
        match &e.predicate {
            Predicate::Contains {
                prop, value, role, ..
            } => {
                assert_eq!(*prop, Property::Title);
                assert_eq!(value, "X");
                assert!(role.is_none());
            }
            other => panic!("expected single Contains, got {:?}", other),
        }
    }

    #[test]
    fn sp3_class_artist_with_title() {
        let e = parse(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "X""#,
        );
        assert_eq!(e.class, ClassFilter::Artist);
    }

    #[test]
    fn sp4_track_with_or_composition() {
        let e = parse(
            r#"upnp:class derivedfrom "object.item.audioItem" and ( dc:title contains "X" or upnp:album contains "X" or upnp:artist contains "X" or upnp:genre contains "X" )"#,
        );
        assert_eq!(e.class, ClassFilter::Track);
        match &e.predicate {
            Predicate::Or(children) => {
                assert_eq!(children.len(), 4);
                let props: Vec<_> = children
                    .iter()
                    .filter_map(|c| match c {
                        Predicate::Contains { prop, .. } => Some(prop.clone()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    props,
                    vec![
                        Property::Title,
                        Property::Album,
                        Property::Artist,
                        Property::Genre
                    ]
                );
            }
            other => panic!("expected Or(4), got {:?}", other),
        }
    }

    #[test]
    fn sp5_composer_role_attribute() {
        let e = parse(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and upnp:artist[@role="Composer"] contains "X""#,
        );
        assert_eq!(e.class, ClassFilter::Artist);
        match &e.predicate {
            Predicate::Contains { prop, role, value } => {
                assert_eq!(*prop, Property::Artist);
                assert_eq!(role.as_deref(), Some("Composer"));
                assert_eq!(value, "X");
            }
            other => panic!("expected role-tagged Contains, got {:?}", other),
        }
    }

    #[test]
    fn sp6_wildcard_and_empty_are_noop() {
        assert!(parse("*").is_no_op());
        assert!(parse("").is_no_op());
    }

    #[test]
    fn sp7_unsupported_property_dropped_to_true() {
        let e = parse(r#"upnp:rating contains "5""#);
        assert_eq!(e.predicate, Predicate::True);
    }

    #[test]
    fn sp8_unterminated_string_returns_true_noop() {
        let e = parse(r#"dc:title contains "abc"#);
        assert_eq!(e.predicate, Predicate::True);
    }

    #[test]
    fn sp9_escaped_quote_in_value() {
        let e = parse(r#"dc:title contains "She said \"hi\"""#);
        match &e.predicate {
            Predicate::Contains { value, .. } => assert_eq!(value, "She said \"hi\""),
            other => panic!("expected Contains, got {:?}", other),
        }
    }

    #[test]
    fn sp10_derivedfrom_only_yields_class_without_predicate() {
        let e = parse(r#"upnp:class derivedfrom "object.item.audioItem""#);
        assert_eq!(e.class, ClassFilter::Track);
        assert_eq!(e.predicate, Predicate::True);
    }

    #[test]
    fn sp11_unknown_class_derivedfrom_becomes_any() {
        let e = parse(r#"upnp:class derivedfrom "object.container.unknown""#);
        assert_eq!(e.class, ClassFilter::Any);
    }
}

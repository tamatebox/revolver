//! UPnP ObjectID encode / decode (SPEC §6.1).
//!
//! - `0`           — root
//! - `cat:aa/ar/al/gn/recent/played/random/hires/lossy/mixed` — category (fixed)
//! - `aa:<b64>` `ar:<b64>` `gn:<b64>` — name-based (URL-safe base64, no padding)
//! - `alb:<id>` `trk:<id>` — albums.id / tracks.id

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectId {
    Root,
    CatAa,
    CatAr,
    CatAl,
    CatGn,
    CatRecent,
    CatPlayed,
    CatRandom,
    CatHires,
    CatLossy,
    CatMixed,
    AlbumArtist(String),
    Artist(String),
    Genre(String),
    Album(i64),
    Track(i64),
}

pub fn parse(s: &str) -> Option<ObjectId> {
    match s {
        "0" => Some(ObjectId::Root),
        "cat:aa" => Some(ObjectId::CatAa),
        "cat:ar" => Some(ObjectId::CatAr),
        "cat:al" => Some(ObjectId::CatAl),
        "cat:gn" => Some(ObjectId::CatGn),
        "cat:recent" => Some(ObjectId::CatRecent),
        "cat:played" => Some(ObjectId::CatPlayed),
        "cat:random" => Some(ObjectId::CatRandom),
        "cat:hires" => Some(ObjectId::CatHires),
        "cat:lossy" => Some(ObjectId::CatLossy),
        "cat:mixed" => Some(ObjectId::CatMixed),
        _ => {
            if let Some(rest) = s.strip_prefix("aa:") {
                decode_name(rest).map(ObjectId::AlbumArtist)
            } else if let Some(rest) = s.strip_prefix("ar:") {
                decode_name(rest).map(ObjectId::Artist)
            } else if let Some(rest) = s.strip_prefix("gn:") {
                decode_name(rest).map(ObjectId::Genre)
            } else if let Some(rest) = s.strip_prefix("alb:") {
                rest.parse().ok().map(ObjectId::Album)
            } else if let Some(rest) = s.strip_prefix("trk:") {
                rest.parse().ok().map(ObjectId::Track)
            } else {
                None
            }
        }
    }
}

pub fn encode(id: &ObjectId) -> String {
    match id {
        ObjectId::Root => "0".to_string(),
        ObjectId::CatAa => "cat:aa".to_string(),
        ObjectId::CatAr => "cat:ar".to_string(),
        ObjectId::CatAl => "cat:al".to_string(),
        ObjectId::CatGn => "cat:gn".to_string(),
        ObjectId::CatRecent => "cat:recent".to_string(),
        ObjectId::CatPlayed => "cat:played".to_string(),
        ObjectId::CatRandom => "cat:random".to_string(),
        ObjectId::CatHires => "cat:hires".to_string(),
        ObjectId::CatLossy => "cat:lossy".to_string(),
        ObjectId::CatMixed => "cat:mixed".to_string(),
        ObjectId::AlbumArtist(name) => format!("aa:{}", encode_name(name)),
        ObjectId::Artist(name) => format!("ar:{}", encode_name(name)),
        ObjectId::Genre(name) => format!("gn:{}", encode_name(name)),
        ObjectId::Album(id) => format!("alb:{}", id),
        ObjectId::Track(id) => format!("trk:{}", id),
    }
}

fn encode_name(name: &str) -> String {
    URL_SAFE_NO_PAD.encode(name.as_bytes())
}

fn decode_name(encoded: &str) -> Option<String> {
    URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn o1_parse_root() {
        assert_eq!(parse("0"), Some(ObjectId::Root));
    }

    #[test]
    fn o2_parse_categories() {
        assert_eq!(parse("cat:aa"), Some(ObjectId::CatAa));
        assert_eq!(parse("cat:ar"), Some(ObjectId::CatAr));
        assert_eq!(parse("cat:al"), Some(ObjectId::CatAl));
        assert_eq!(parse("cat:gn"), Some(ObjectId::CatGn));
        assert_eq!(parse("cat:recent"), Some(ObjectId::CatRecent));
        assert_eq!(parse("cat:random"), Some(ObjectId::CatRandom));
        assert_eq!(parse("cat:hires"), Some(ObjectId::CatHires));
        assert_eq!(parse("cat:lossy"), Some(ObjectId::CatLossy));
        assert_eq!(parse("cat:mixed"), Some(ObjectId::CatMixed));
        assert_eq!(parse("cat:played"), Some(ObjectId::CatPlayed));
    }

    #[test]
    fn o3_parse_album_artist_via_roundtrip() {
        let encoded = encode(&ObjectId::AlbumArtist("Beatles".to_string()));
        assert!(encoded.starts_with("aa:"));
        assert_eq!(
            parse(&encoded),
            Some(ObjectId::AlbumArtist("Beatles".to_string()))
        );
    }

    #[test]
    fn o4_parse_alb_and_trk_id() {
        assert_eq!(parse("alb:123"), Some(ObjectId::Album(123)));
        assert_eq!(parse("trk:456"), Some(ObjectId::Track(456)));
    }

    #[test]
    fn o5_invalid_returns_none() {
        assert_eq!(parse("bogus"), None);
        assert_eq!(parse("alb:notnum"), None);
        assert_eq!(parse("aa:not!valid!base64"), None);
        assert_eq!(parse(""), None);
    }

    #[test]
    fn o5b_legacy_recent_range_ids_return_none() {
        // Prior versions exposed cat:recent:day / cat:recent:year:YYYY. The
        // hierarchy was dropped (#16) so these now parse as None (Linn will
        // surface "no such object" if a control point cached an old ObjectID).
        assert_eq!(parse("cat:recent:day"), None);
        assert_eq!(parse("cat:recent:week"), None);
        assert_eq!(parse("cat:recent:all"), None);
        assert_eq!(parse("cat:recent:year:2024"), None);
    }

    // ── proptest: encode → parse round-trip for arbitrary strings ─────────────────
    proptest::proptest! {
        /// AlbumArtist / Artist / Genre encoded with an arbitrary name must round-trip
        /// back to the original (guarantees URL-safe base64 handles any byte sequence
        /// including unicode).
        #[test]
        fn op1_name_roundtrip_any_string(name in ".*") {
            for build in &[
                ObjectId::AlbumArtist as fn(String) -> ObjectId,
                ObjectId::Artist,
                ObjectId::Genre,
            ] {
                let id = build(name.clone());
                let encoded = encode(&id);
                let parsed = parse(&encoded).expect("encode/parse roundtrip");
                proptest::prop_assert_eq!(parsed, id);
            }
        }

        /// Album / Track round-trip through encode → parse for any i64.
        #[test]
        fn op2_id_roundtrip_any_i64(id in any::<i64>()) {
            for v in [ObjectId::Album(id), ObjectId::Track(id)] {
                let encoded = encode(&v);
                let parsed = parse(&encoded).expect("encode/parse roundtrip");
                proptest::prop_assert_eq!(parsed, v);
            }
        }

        /// `parse` must never panic on arbitrary input (just returns `None` or `Some`).
        #[test]
        fn op3_parse_never_panics(s in ".*") {
            let _ = parse(&s);
        }
    }

    use proptest::prelude::any;

    #[test]
    fn o6_roundtrip_all_variants() {
        let cases = vec![
            ObjectId::Root,
            ObjectId::CatAa,
            ObjectId::CatAr,
            ObjectId::CatAl,
            ObjectId::CatGn,
            ObjectId::CatRecent,
            ObjectId::CatPlayed,
            ObjectId::CatRandom,
            ObjectId::CatHires,
            ObjectId::CatLossy,
            ObjectId::CatMixed,
            ObjectId::AlbumArtist("Various Artists".to_string()),
            ObjectId::AlbumArtist("Björk Guðmundsdóttir".to_string()), // non-ASCII
            ObjectId::Artist("Miles Davis".to_string()),
            ObjectId::Genre("Jazz / Fusion".to_string()), // slash & space
            ObjectId::Album(42),
            ObjectId::Track(99),
        ];
        for case in cases {
            let encoded = encode(&case);
            let parsed = parse(&encoded).expect("roundtrip");
            assert_eq!(parsed, case, "failed for {:?} (encoded: {})", case, encoded);
        }
    }
}

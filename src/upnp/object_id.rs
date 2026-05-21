//! UPnP ObjectID encode / decode (SPEC §6.1).
//!
//! - `0`           — root
//! - `cat:aa/ar/al/gn/recent/played/random/hires/lossy/mixed/cm/cn/pf/yr/dec` — category (fixed)
//! - `aa:<b64>` `ar:<b64>` `gn:<b64>` `cm:<b64>` `cn:<b64>` `pf:<b64>` —
//!   name-based (URL-safe base64, no padding)
//! - `yr:<YYYY>` `dec:<YYYY>` — year / decade buckets (#2). Plain integer,
//!   no base64 (digits are URL-safe). `dec:<YYYY>` is the first year of
//!   the decade (e.g. `dec:1980` covers 1980-1989).
//! - `alb:<id>` `trk:<id>` — albums.id / tracks.id
//! - `disc:<album_id>:<disc>` — multi-disc divider container (#17)

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
    /// #9: classical facet — composers.
    CatCm,
    /// #9: classical facet — conductors.
    CatCn,
    /// #9: classical facet — performers (orchestra / ensemble).
    CatPf,
    /// #2: per-release-year facet.
    CatYr,
    /// #2: per-decade facet (buckets of 10 calendar years).
    CatDec,
    AlbumArtist(String),
    Artist(String),
    Genre(String),
    Composer(String),
    Conductor(String),
    Performer(String),
    /// #2: a single release year. Used as parent for albums released in that year.
    Year(i32),
    /// #2: a 10-year bucket starting at this year (e.g. `Decade(1980)`
    /// covers 1980-1989).
    Decade(i32),
    Album(i64),
    Track(i64),
    /// Disc-divider container injected into a multi-disc album's child list.
    /// Encoded as `disc:{album_id}:{disc}`. Browsing into it returns just that
    /// disc's tracks (a redundant subset of the album's flat view).
    Disc {
        album_id: i64,
        disc: i64,
    },
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
        "cat:cm" => Some(ObjectId::CatCm),
        "cat:cn" => Some(ObjectId::CatCn),
        "cat:pf" => Some(ObjectId::CatPf),
        "cat:yr" => Some(ObjectId::CatYr),
        "cat:dec" => Some(ObjectId::CatDec),
        _ => {
            if let Some(rest) = s.strip_prefix("aa:") {
                decode_name(rest).map(ObjectId::AlbumArtist)
            } else if let Some(rest) = s.strip_prefix("ar:") {
                decode_name(rest).map(ObjectId::Artist)
            } else if let Some(rest) = s.strip_prefix("gn:") {
                decode_name(rest).map(ObjectId::Genre)
            } else if let Some(rest) = s.strip_prefix("cm:") {
                decode_name(rest).map(ObjectId::Composer)
            } else if let Some(rest) = s.strip_prefix("cn:") {
                decode_name(rest).map(ObjectId::Conductor)
            } else if let Some(rest) = s.strip_prefix("pf:") {
                decode_name(rest).map(ObjectId::Performer)
            } else if let Some(rest) = s.strip_prefix("yr:") {
                rest.parse().ok().map(ObjectId::Year)
            } else if let Some(rest) = s.strip_prefix("dec:") {
                // Reject non-decade-aligned values to keep IDs canonical
                // (`dec:1985` must round-trip through the encoder).
                let y: i32 = rest.parse().ok()?;
                if y % 10 == 0 {
                    Some(ObjectId::Decade(y))
                } else {
                    None
                }
            } else if let Some(rest) = s.strip_prefix("alb:") {
                rest.parse().ok().map(ObjectId::Album)
            } else if let Some(rest) = s.strip_prefix("trk:") {
                rest.parse().ok().map(ObjectId::Track)
            } else if let Some(rest) = s.strip_prefix("disc:") {
                let (a, d) = rest.split_once(':')?;
                Some(ObjectId::Disc {
                    album_id: a.parse().ok()?,
                    disc: d.parse().ok()?,
                })
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
        ObjectId::CatCm => "cat:cm".to_string(),
        ObjectId::CatCn => "cat:cn".to_string(),
        ObjectId::CatPf => "cat:pf".to_string(),
        ObjectId::CatYr => "cat:yr".to_string(),
        ObjectId::CatDec => "cat:dec".to_string(),
        ObjectId::AlbumArtist(name) => format!("aa:{}", encode_name(name)),
        ObjectId::Artist(name) => format!("ar:{}", encode_name(name)),
        ObjectId::Genre(name) => format!("gn:{}", encode_name(name)),
        ObjectId::Composer(name) => format!("cm:{}", encode_name(name)),
        ObjectId::Conductor(name) => format!("cn:{}", encode_name(name)),
        ObjectId::Performer(name) => format!("pf:{}", encode_name(name)),
        ObjectId::Year(y) => format!("yr:{}", y),
        ObjectId::Decade(y) => format!("dec:{}", y),
        ObjectId::Album(id) => format!("alb:{}", id),
        ObjectId::Track(id) => format!("trk:{}", id),
        ObjectId::Disc { album_id, disc } => format!("disc:{}:{}", album_id, disc),
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
    fn o7_year_and_decade_parse_and_round_trip() {
        // #2: yr:YYYY parses any positive integer; dec:YYYY accepts only
        // decade-aligned values so encoded ↔ parsed is canonical.
        assert_eq!(parse("cat:yr"), Some(ObjectId::CatYr));
        assert_eq!(parse("cat:dec"), Some(ObjectId::CatDec));
        assert_eq!(parse("yr:1969"), Some(ObjectId::Year(1969)));
        assert_eq!(parse("dec:1980"), Some(ObjectId::Decade(1980)));
        // Non-decade-aligned input is rejected (would not survive round-trip).
        assert_eq!(parse("dec:1985"), None);
        assert_eq!(parse("yr:notnum"), None);
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
            ObjectId::Disc {
                album_id: 42,
                disc: 2,
            },
            ObjectId::CatCm,
            ObjectId::CatCn,
            ObjectId::CatPf,
            ObjectId::Composer("J.S. Bach".to_string()),
            ObjectId::Conductor("Karajan".to_string()),
            ObjectId::Performer("Berlin Philharmonic".to_string()),
            ObjectId::CatYr,
            ObjectId::CatDec,
            ObjectId::Year(1969),
            ObjectId::Decade(1980),
        ];
        for case in cases {
            let encoded = encode(&case);
            let parsed = parse(&encoded).expect("roundtrip");
            assert_eq!(parsed, case, "failed for {:?} (encoded: {})", case, encoded);
        }
    }
}

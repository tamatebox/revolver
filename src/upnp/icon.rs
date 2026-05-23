//! UPnP device icons (SPEC §5.2).
//!
//! Two PNGs embedded at build time and served from `/icon/{size}.png`. The
//! Device Description's `<iconList>` advertises them so control points can
//! show a thumbnail next to the server name.

pub const ICON_48_PNG: &[u8] = include_bytes!("../../assets/icon-48.png");
pub const ICON_120_PNG: &[u8] = include_bytes!("../../assets/icon-120.png");
pub const ICON_512_PNG: &[u8] = include_bytes!("../../assets/icon-512.png");
pub const ICON_1024_PNG: &[u8] = include_bytes!("../../assets/icon-1024.png");

/// Source SVG used as the admin UI favicon (`/icon.svg`). Not referenced by
/// `<iconList>` — UPnP control points stick to the PNG sizes.
pub const ICON_SVG: &[u8] = include_bytes!("../../assets/icon.svg");
pub const SVG_MIME: &str = "image/svg+xml";

/// Placeholder served by `/art/{id}` when an album row exists but no embedded
/// picture or folder image could be extracted. Single eighth note on the
/// shared cream/border palette — intentionally distinct from `cat-al`
/// (which uses the sleeve+record metaphor) so a Linn grid distinguishes
/// "this is the Albums facet" from "this album has no art".
///
/// Served with a short `Cache-Control` so adding art later (re-scan,
/// folder-image drop) refreshes within minutes, not the 24h applied to real
/// art bytes.
pub const ALBUM_FALLBACK_PNG: &[u8] = include_bytes!("../../assets/album-fallback.png");

/// Per-category icons (#24). Each entry is the `{slug}` half of the
/// `/icon/cat-{slug}.png` URL paired with the embedded PNG bytes. Source SVGs
/// live next to the PNGs (`assets/category-icons/`) and are rasterized at
/// commit time via
///   `rsvg-convert -w 1024 -h 1024 cat-{slug}.svg -o cat-{slug}.png`
///   `oxipng -o 4 --strip all cat-*.png`
/// 1024 px matches DLNA JPEG_MED headroom and stays sharp at retina list
/// sizes; `oxipng` shaves ~20% off the lossless PNG output.
#[rustfmt::skip]
pub const CATEGORY_ICONS: &[(&str, &[u8])] = &[
    ("aa",     include_bytes!("../../assets/category-icons/cat-aa.png")),
    ("al",     include_bytes!("../../assets/category-icons/cat-al.png")),
    ("ar",     include_bytes!("../../assets/category-icons/cat-ar.png")),
    ("at",     include_bytes!("../../assets/category-icons/cat-at.png")),
    ("cm",     include_bytes!("../../assets/category-icons/cat-cm.png")),
    ("cn",     include_bytes!("../../assets/category-icons/cat-cn.png")),
    ("dec",    include_bytes!("../../assets/category-icons/cat-dec.png")),
    ("gn",     include_bytes!("../../assets/category-icons/cat-gn.png")),
    ("hires",  include_bytes!("../../assets/category-icons/cat-hires.png")),
    ("lossy",  include_bytes!("../../assets/category-icons/cat-lossy.png")),
    ("mixed",  include_bytes!("../../assets/category-icons/cat-mixed.png")),
    ("pf",     include_bytes!("../../assets/category-icons/cat-pf.png")),
    ("played", include_bytes!("../../assets/category-icons/cat-played.png")),
    ("random", include_bytes!("../../assets/category-icons/cat-random.png")),
    ("recent", include_bytes!("../../assets/category-icons/cat-recent.png")),
    ("yr",     include_bytes!("../../assets/category-icons/cat-yr.png")),
];

/// Resolve a `cat-{slug}.png` lookup to the embedded PNG bytes. Linear scan
/// over a 15-entry slice is faster than any map at this size.
pub fn category_icon(slug: &str) -> Option<&'static [u8]> {
    CATEGORY_ICONS
        .iter()
        .find(|(s, _)| *s == slug)
        .map(|(_, bytes)| *bytes)
}

pub const MIME: &str = "image/png";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ic1_category_icons_slugs_are_sorted_for_easy_diffing() {
        let slugs: Vec<&str> = CATEGORY_ICONS.iter().map(|(s, _)| *s).collect();
        let mut sorted = slugs.clone();
        sorted.sort_unstable();
        assert_eq!(slugs, sorted, "keep CATEGORY_ICONS entries sorted by slug");
    }

    #[test]
    fn ic2_category_icon_lookup_round_trips_and_misses_return_none() {
        for (slug, _) in CATEGORY_ICONS {
            let bytes = category_icon(slug).expect("registered slug must resolve");
            assert!(!bytes.is_empty());
            // PNG signature: 8 bytes starting with 0x89 P N G.
            assert_eq!(&bytes[..4], b"\x89PNG", "{slug} payload is not a PNG");
        }
        assert!(category_icon("nope").is_none());
        assert!(category_icon("").is_none());
    }
}

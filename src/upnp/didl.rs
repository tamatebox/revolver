//! DIDL-Lite XML generator (SPEC §7). Builds the XML string embedded in the
//! `<Result>` of Browse responses. Output is format!-based with hand-written
//! escape helpers.

use std::fmt::Write;

const ENVELOPE_OPEN: &str = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">"#;
const ENVELOPE_CLOSE: &str = "</DIDL-Lite>";

pub struct Container {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub upnp_class: &'static str,
    pub child_count: Option<i64>,
    pub artist: Option<String>,
    pub album_art_uri: Option<String>,
}

pub struct Item {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub upnp_class: &'static str,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub original_track_number: Option<u32>,
    /// Emitted as `<upnp:originalDiscNumber>` when set. `None` (or 0 at the
    /// call site) is omitted — single-disc albums shouldn't broadcast "Disc 1".
    pub original_disc_number: Option<u32>,
    pub album_art_uri: Option<String>,
    pub res: Resource,
}

/// Order-preserving DIDL child. Used when the response needs to interleave
/// `<container>` and `<item>` elements (e.g. multi-disc album with disc-divider
/// containers between track items). Container/Item-only responses can use the
/// `build_didl(&[Container], &[Item])` form instead.
pub enum DidlNode {
    Container(Container),
    Item(Item),
}

pub struct Resource {
    pub url: String,
    pub protocol_info: String,
    pub size: u64,
    pub duration_ms: Option<u64>,
    pub bitrate: Option<u32>,
    pub sample_frequency: Option<u32>,
    /// SPEC §7.3: emitted only for lossless (FLAC / ALAC / PCM); `None` (omitted) for lossy.
    pub bits_per_sample: Option<u8>,
    pub nr_audio_channels: Option<u8>,
}

pub fn build_didl(containers: &[Container], items: &[Item]) -> String {
    let mut s = String::new();
    s.push_str(ENVELOPE_OPEN);
    for c in containers {
        push_container(&mut s, c);
    }
    for i in items {
        push_item(&mut s, i);
    }
    s.push_str(ENVELOPE_CLOSE);
    s
}

/// Order-preserving variant: emits each node in the order given. Use when
/// containers and items need to interleave (multi-disc dividers).
pub fn build_didl_nodes(nodes: &[DidlNode]) -> String {
    let mut s = String::new();
    s.push_str(ENVELOPE_OPEN);
    for n in nodes {
        match n {
            DidlNode::Container(c) => push_container(&mut s, c),
            DidlNode::Item(i) => push_item(&mut s, i),
        }
    }
    s.push_str(ENVELOPE_CLOSE);
    s
}

fn push_container(s: &mut String, c: &Container) {
    write!(
        s,
        r#"<container id="{}" parentID="{}" restricted="1""#,
        xml_attr(&c.id),
        xml_attr(&c.parent_id)
    )
    .unwrap();
    if let Some(n) = c.child_count {
        write!(s, r#" childCount="{}""#, n).unwrap();
    }
    s.push('>');
    write!(s, "<dc:title>{}</dc:title>", xml_escape(&c.title)).unwrap();
    write!(s, "<upnp:class>{}</upnp:class>", c.upnp_class).unwrap();
    if let Some(a) = &c.artist {
        write!(s, "<upnp:artist>{}</upnp:artist>", xml_escape(a)).unwrap();
    }
    if let Some(uri) = &c.album_art_uri {
        write!(
            s,
            "<upnp:albumArtURI>{}</upnp:albumArtURI>",
            xml_escape(uri)
        )
        .unwrap();
    }
    s.push_str("</container>");
}

fn push_item(s: &mut String, item: &Item) {
    write!(
        s,
        r#"<item id="{}" parentID="{}" restricted="1">"#,
        xml_attr(&item.id),
        xml_attr(&item.parent_id)
    )
    .unwrap();
    write!(s, "<dc:title>{}</dc:title>", xml_escape(&item.title)).unwrap();
    write!(s, "<upnp:class>{}</upnp:class>", item.upnp_class).unwrap();
    if let Some(a) = &item.artist {
        write!(s, "<upnp:artist>{}</upnp:artist>", xml_escape(a)).unwrap();
    }
    if let Some(a) = &item.album {
        write!(s, "<upnp:album>{}</upnp:album>", xml_escape(a)).unwrap();
    }
    if let Some(g) = &item.genre {
        write!(s, "<upnp:genre>{}</upnp:genre>", xml_escape(g)).unwrap();
    }
    if let Some(n) = item.original_track_number {
        write!(
            s,
            "<upnp:originalTrackNumber>{}</upnp:originalTrackNumber>",
            n
        )
        .unwrap();
    }
    if let Some(n) = item.original_disc_number {
        write!(
            s,
            "<upnp:originalDiscNumber>{}</upnp:originalDiscNumber>",
            n
        )
        .unwrap();
    }
    if let Some(uri) = &item.album_art_uri {
        write!(
            s,
            "<upnp:albumArtURI>{}</upnp:albumArtURI>",
            xml_escape(uri)
        )
        .unwrap();
    }
    push_res(s, &item.res);
    s.push_str("</item>");
}

fn push_res(s: &mut String, r: &Resource) {
    write!(s, r#"<res protocolInfo="{}""#, xml_attr(&r.protocol_info)).unwrap();
    write!(s, r#" size="{}""#, r.size).unwrap();
    if let Some(ms) = r.duration_ms {
        write!(s, r#" duration="{}""#, format_duration(ms)).unwrap();
    }
    if let Some(b) = r.bitrate {
        write!(s, r#" bitrate="{}""#, b).unwrap();
    }
    if let Some(sf) = r.sample_frequency {
        write!(s, r#" sampleFrequency="{}""#, sf).unwrap();
    }
    if let Some(bd) = r.bits_per_sample {
        write!(s, r#" bitsPerSample="{}""#, bd).unwrap();
    }
    if let Some(ch) = r.nr_audio_channels {
        write!(s, r#" nrAudioChannels="{}""#, ch).unwrap();
    }
    write!(s, ">{}</res>", xml_escape(&r.url)).unwrap();
}

/// Build a `H:MM:SS.fff` duration string from milliseconds (UPnP `<res duration>` format).
fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1000;
    let frac = ms % 1000;
    let hours = total_secs / 3600;
    let mins = (total_secs % 3600) / 60;
    let secs = total_secs % 60;
    format!("{}:{:02}:{:02}.{:03}", hours, mins, secs, frac)
}

/// Single-pass char-by-char escape (perf §P1).
///
/// The previous implementation chained `String::replace` 3 times → 4 full allocs & copies
/// (each replace returns a new String). On the hot path one page of 100 items × 5-7
/// fields meant 500-700 allocs. `with_capacity(s.len())` + push compresses this to 1 alloc.
///
/// Control characters disallowed by XML 1.0 (`\x00..=\x08`, `\x0B`, `\x0C`, `\x0E..=\x1F`)
/// are **dropped** (the DIDL XML in SPEC §7 must be valid; `\t \n \r` are kept).
/// Prevents Linn from silently discarding entire tracks on XML parse failure.
fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // Strip control characters disallowed by the XML 1.0 spec.
            '\t' | '\n' | '\r' => out.push(c),
            c if (c as u32) < 0x20 => {
                // Silently drop (prioritize Linn not failing XML parse).
            }
            other => out.push(other),
        }
    }
    out
}

/// For attribute values. Adds `"` to the escape set. `'` is unnecessary because we
/// always use `"..."` as attribute delimiters internally (SPEC scope never uses
/// `'`-delimited attributes). Control characters are dropped, same as `xml_escape`.
fn xml_attr(s: &str) -> String {
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
    fn dl1_empty_didl_has_envelope() {
        let xml = build_didl(&[], &[]);
        assert!(
            xml.starts_with(r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/""#)
        );
        assert!(xml.ends_with("</DIDL-Lite>"));
    }

    #[test]
    fn dl2_container_has_required_fields() {
        let c = Container {
            id: "cat:aa".to_string(),
            parent_id: "0".to_string(),
            title: "Album Artist".to_string(),
            upnp_class: "object.container",
            child_count: Some(5),
            artist: None,
            album_art_uri: None,
        };
        let xml = build_didl(&[c], &[]);
        assert!(
            xml.contains(r#"<container id="cat:aa" parentID="0" restricted="1" childCount="5">"#)
        );
        assert!(xml.contains("<dc:title>Album Artist</dc:title>"));
        assert!(xml.contains("<upnp:class>object.container</upnp:class>"));
    }

    #[test]
    fn dl3_item_has_res_attributes() {
        let item = Item {
            id: "trk:42".to_string(),
            parent_id: "alb:1".to_string(),
            title: "Song".to_string(),
            upnp_class: "object.item.audioItem.musicTrack",
            artist: Some("Artist".to_string()),
            album: Some("Album".to_string()),
            genre: None,
            original_track_number: Some(3),
            original_disc_number: Some(2),
            album_art_uri: Some("http://x/art/1".to_string()),
            res: Resource {
                url: "http://x/stream/42".to_string(),
                protocol_info: "http-get:*:audio/flac:*".to_string(),
                size: 12345678,
                duration_ms: Some(245_000), // 4:05.000
                bitrate: Some(950_000),
                sample_frequency: Some(44100),
                bits_per_sample: Some(16),
                nr_audio_channels: Some(2),
            },
        };
        let xml = build_didl(&[], &[item]);
        assert!(xml.contains(r#"<item id="trk:42" parentID="alb:1" restricted="1">"#));
        assert!(xml.contains("<upnp:originalTrackNumber>3</upnp:originalTrackNumber>"));
        assert!(xml.contains("<upnp:originalDiscNumber>2</upnp:originalDiscNumber>"));
        assert!(xml.contains(r#"protocolInfo="http-get:*:audio/flac:*""#));
        assert!(xml.contains(r#"size="12345678""#));
        assert!(xml.contains(r#"duration="0:04:05.000""#));
        assert!(xml.contains(r#"sampleFrequency="44100""#));
        assert!(xml.contains(r#"bitsPerSample="16""#));
        assert!(xml.contains(r#"nrAudioChannels="2""#));
        assert!(xml.contains("http://x/stream/42"));
    }

    #[test]
    fn dl3b_item_omits_original_disc_number_when_none() {
        let item = Item {
            id: "trk:1".to_string(),
            parent_id: "alb:1".to_string(),
            title: "Song".to_string(),
            upnp_class: "object.item.audioItem.musicTrack",
            artist: None,
            album: None,
            genre: None,
            original_track_number: Some(1),
            original_disc_number: None,
            album_art_uri: None,
            res: Resource {
                url: "http://x/stream/1".to_string(),
                protocol_info: "http-get:*:audio/flac:*".to_string(),
                size: 1,
                duration_ms: None,
                bitrate: None,
                sample_frequency: None,
                bits_per_sample: None,
                nr_audio_channels: None,
            },
        };
        let xml = build_didl(&[], &[item]);
        assert!(!xml.contains("originalDiscNumber"));
    }

    #[test]
    fn dl5_xml_escape_preserves_apostrophe_in_title() {
        // SPEC does not require escaping `'`, and escaping it accidentally would make
        // Linn display `&apos;` literally — so keep it as-is.
        let c = Container {
            id: "alb:1".to_string(),
            parent_id: "0".to_string(),
            title: "Don't Stop".to_string(),
            upnp_class: "object.container",
            child_count: None,
            artist: None,
            album_art_uri: None,
        };
        let xml = build_didl(&[c], &[]);
        assert!(xml.contains("<dc:title>Don't Stop</dc:title>"));
        assert!(!xml.contains("&apos;"));
        assert!(!xml.contains("&#39;"));
    }

    #[test]
    fn dl6_xml_escape_drops_disallowed_control_chars() {
        // Control characters not valid in XML 1.0 (\x00..=\x08, \x0B, \x0C, \x0E..=\x1F)
        // are dropped. Tab / LF / CR are kept (they are valid).
        let c = Container {
            id: "alb:1".to_string(),
            parent_id: "0".to_string(),
            title: "A\x00B\x07C\x1FD\tE\nF".to_string(),
            upnp_class: "object.container",
            child_count: None,
            artist: None,
            album_art_uri: None,
        };
        let xml = build_didl(&[c], &[]);
        // Control chars are dropped, yielding "ABCD\tE\nF".
        assert!(
            xml.contains("<dc:title>ABCD\tE\nF</dc:title>"),
            "got: {}",
            xml
        );
        assert!(!xml.contains('\x00'));
        assert!(!xml.contains('\x07'));
        assert!(!xml.contains('\x1F'));
    }

    #[test]
    fn dl4_xml_escape_in_title_and_attribute() {
        let c = Container {
            id: r#"alb:1"funky""#.to_string(),
            parent_id: "0".to_string(),
            title: "Tom & Jerry's <Album>".to_string(),
            upnp_class: "object.container",
            child_count: None,
            artist: None,
            album_art_uri: None,
        };
        let xml = build_didl(&[c], &[]);
        // Special characters inside the title.
        assert!(xml.contains("Tom &amp; Jerry's &lt;Album&gt;"));
        // Special characters inside the attribute.
        assert!(xml.contains(r#"id="alb:1&quot;funky&quot;""#));
    }
}

//! Shared logic for building a DIDL Item from a track row.
//! Used by both Browse (under album) and Search.

use rusqlite::params;

use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::{Item, Resource};

/// DB values for a single track row, as fetched by the `load_*` helpers.
pub(crate) struct TrackRow {
    pub album_id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub genre: Option<String>,
    pub track_num: Option<i64>,
    pub disc_num: Option<i64>,
    /// `true` when the parent album has tracks across more than one disc.
    /// Drives the `"N. title"` title prefix in `build_track_item` (Linn fallback,
    /// since Linn ignores `<upnp:originalDiscNumber>` in UI rendering).
    pub multi_disc: bool,
    pub duration_ms: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub bitrate: Option<i64>,
    pub mime_type: String,
    pub file_size: i64,
    pub album: String,
}

/// BrowseMetadata (`trk:{id}`). Returns a single Item.
pub fn track_metadata(ctx: &BrowseContext, track_id: i64) -> Result<DidlOutput> {
    let item = load_track_item(ctx, track_id)?;
    Ok(DidlOutput {
        containers: vec![],
        items: vec![item],
    })
}

/// BrowseDirectChildren (`alb:{id}`). Returns the track list.
pub fn album_tracks_children(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?1",
        params![album_id],
        |r| r.get(0),
    )?;
    let items = load_album_tracks(ctx, album_id, start, count)?;
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers: vec![],
            items,
        },
        total_matches: total as usize,
    })
}

fn load_track_item(ctx: &BrowseContext, track_id: i64) -> Result<Item> {
    let row: TrackRow = ctx.conn.query_row(
        "SELECT t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                (SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = t.album_id) > 1
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.id = ?1",
        params![track_id],
        |r| {
            Ok(TrackRow {
                album_id: r.get(0)?,
                title: r.get(1)?,
                artist: r.get(2)?,
                genre: r.get(3)?,
                track_num: r.get(4)?,
                disc_num: r.get(5)?,
                duration_ms: r.get(6)?,
                sample_rate: r.get(7)?,
                bit_depth: r.get(8)?,
                channels: r.get(9)?,
                bitrate: r.get(10)?,
                mime_type: r.get(11)?,
                file_size: r.get(12)?,
                album: r.get(13)?,
                multi_disc: r.get::<_, i64>(14)? != 0,
            })
        },
    )?;
    Ok(build_track_item(ctx, track_id, &row))
}

fn load_album_tracks(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<Vec<Item>> {
    // One probe to learn if this album spans multiple discs; reused for every row.
    let multi_disc: bool = ctx
        .conn
        .query_row(
            "SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = ?1",
            params![album_id],
            |r| r.get::<_, i64>(0).map(|n| n > 1),
        )
        .unwrap_or(false);
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.album_id = ?1
         ORDER BY t.disc_num, t.track_num
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(params![album_id, count as i64, start as i64], |r| {
            Ok((
                r.get(0)?,
                TrackRow {
                    album_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    genre: r.get(4)?,
                    track_num: r.get(5)?,
                    disc_num: r.get(6)?,
                    multi_disc,
                    duration_ms: r.get(7)?,
                    sample_rate: r.get(8)?,
                    bit_depth: r.get(9)?,
                    channels: r.get(10)?,
                    bitrate: r.get(11)?,
                    mime_type: r.get(12)?,
                    file_size: r.get(13)?,
                    album: r.get(14)?,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows
        .into_iter()
        .map(|(id, row)| build_track_item(ctx, id, &row))
        .collect())
}

/// Track row → DIDL Item. Used by both Browse and Search.
///
/// Multi-disc albums get a `"N. "` prefix on `dc:title` because Linn ignores
/// `<upnp:originalDiscNumber>` in UI rendering — without the prefix, disc 2's
/// "Track 1" sits next to disc 1's "Track 1" and looks like a duplicate.
/// Single-disc albums skip the prefix to avoid polluting every title.
pub(crate) fn build_track_item(ctx: &BrowseContext, track_id: i64, row: &TrackRow) -> Item {
    let protocol_info = format!("http-get:*:{}:*", row.mime_type);
    let base_title = row.title.clone().unwrap_or_else(|| "Unknown".to_string());
    let title = match (row.multi_disc, row.disc_num) {
        (true, Some(n)) if n > 0 => format!("{n}. {base_title}"),
        _ => base_title,
    };
    Item {
        id: format!("trk:{track_id}"),
        parent_id: format!("alb:{}", row.album_id),
        title,
        upnp_class: "object.item.audioItem.musicTrack",
        artist: row.artist.clone(),
        album: Some(row.album.clone()),
        genre: row.genre.clone(),
        original_track_number: row.track_num.map(|n| n as u32),
        original_disc_number: if row.multi_disc {
            row.disc_num.filter(|&n| n > 0).map(|n| n as u32)
        } else {
            None
        },
        album_art_uri: Some(format!("{}/{}", ctx.art_base_url, row.album_id)),
        res: Resource {
            url: format!("{}/{}", ctx.stream_base_url, track_id),
            protocol_info,
            size: row.file_size as u64,
            duration_ms: row.duration_ms.map(|n| n as u64),
            bitrate: row.bitrate.map(|n| n as u32),
            sample_frequency: row.sample_rate.map(|n| n as u32),
            bits_per_sample: row.bit_depth.map(|n| n as u8),
            nr_audio_channels: row.channels.map(|n| n as u8),
        },
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db::{albums, schema, tracks as db_tracks};

    fn seed(conn: &Connection, album_artist: &str, album: &str, discs: &[(u32, u32, &str)]) -> i64 {
        let album_id = albums::upsert(
            conn,
            &albums::AlbumKey {
                effective_album_artist: album_artist,
                album,
                compilation: false,
            },
            Some(album_artist),
            100,
        )
        .unwrap();
        for (disc, track, title) in discs {
            db_tracks::upsert(
                conn,
                &db_tracks::TrackRow {
                    album_id,
                    path: &format!("/m/{title}.flac"),
                    title: Some(title),
                    artist: Some(album_artist),
                    genre: Some("Rock"),
                    track_num: Some(*track),
                    disc_num: Some(*disc),
                    duration_ms: Some(200_000),
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    bitrate: Some(1000),
                    codec: "flac",
                    mime_type: "audio/flac",
                    file_size: 1234,
                    added_at: 100,
                    mtime: 200,
                },
            )
            .unwrap();
        }
        albums::recalc_counts(conn).unwrap();
        album_id
    }

    fn ctx(conn: &Connection) -> BrowseContext<'_> {
        static RS: std::sync::OnceLock<crate::random::RandomState> = std::sync::OnceLock::new();
        static BS: std::sync::OnceLock<crate::state::BrowseSettings> = std::sync::OnceLock::new();
        BrowseContext {
            conn,
            art_base_url: "http://x/art",
            stream_base_url: "http://x/stream",
            random_state: RS.get_or_init(crate::random::RandomState::new),
            now_secs: 0,
            settings: BS.get_or_init(crate::state::BrowseSettings::default),
        }
    }

    #[test]
    fn bt1_single_disc_album_omits_prefix_and_disc_number() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Single",
            &[(1, 1, "First"), (1, 2, "Second"), (1, 3, "Third")],
        );
        let result = album_tracks_children(&ctx(&conn), album_id, 0, 100).unwrap();
        assert_eq!(result.total_matches, 3);
        for item in &result.didl.items {
            assert!(
                !item.title.starts_with("1. "),
                "single-disc title got prefixed: {}",
                item.title
            );
            assert!(
                item.original_disc_number.is_none(),
                "single-disc emitted disc number"
            );
        }
    }

    #[test]
    fn bt2_multi_disc_album_prefixes_title_and_emits_disc_number() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Multi",
            &[(1, 1, "D1T1"), (1, 2, "D1T2"), (2, 1, "D2T1")],
        );
        let result = album_tracks_children(&ctx(&conn), album_id, 0, 100).unwrap();
        assert_eq!(result.total_matches, 3);
        let titles: Vec<&str> = result.didl.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["1. D1T1", "1. D1T2", "2. D2T1"]);
        let discs: Vec<Option<u32>> = result
            .didl
            .items
            .iter()
            .map(|i| i.original_disc_number)
            .collect();
        assert_eq!(discs, vec![Some(1), Some(1), Some(2)]);
    }

    #[test]
    fn bt3_track_metadata_reflects_multi_disc_flag() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(&conn, "A", "Multi", &[(1, 1, "Alpha"), (2, 1, "Beta")]);
        let track_id: i64 = conn
            .query_row(
                "SELECT id FROM tracks WHERE album_id = ?1 AND disc_num = 2",
                rusqlite::params![album_id],
                |r| r.get(0),
            )
            .unwrap();
        let didl = track_metadata(&ctx(&conn), track_id).unwrap();
        assert_eq!(didl.items.len(), 1);
        assert_eq!(didl.items[0].title, "2. Beta");
        assert_eq!(didl.items[0].original_disc_number, Some(2));
    }
}

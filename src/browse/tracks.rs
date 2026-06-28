//! Shared logic for building a DIDL Item from a track row.
//! Used by both Browse (under album) and Search.

use rusqlite::params;

use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::{Error, Result};
use crate::upnp::didl::{Author, Container, Item, Resource};

/// DB values for a single track row, as fetched by the `load_*` helpers.
pub(crate) struct TrackRow {
    pub album_id: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub genre: Option<String>,
    pub track_num: Option<i64>,
    pub disc_num: Option<i64>,
    /// `true` when the parent album has tracks across more than one disc.
    /// Gates `<upnp:originalDiscNumber>` emission on the item (single-disc
    /// albums omit it to avoid broadcasting a meaningless "Disc 1").
    pub multi_disc: bool,
    pub duration_ms: Option<i64>,
    pub sample_rate: Option<i64>,
    pub bit_depth: Option<i64>,
    pub channels: Option<i64>,
    pub bitrate: Option<i64>,
    pub mime_type: String,
    pub file_size: i64,
    pub album: String,
    /// #9: classical metadata for the `<upnp:author role="...">` DIDL fields.
    pub composer: Option<String>,
    pub conductor: Option<String>,
    pub performer: Option<String>,
}

/// BrowseMetadata (`trk:{id}`). Returns a single Item.
pub fn track_metadata(ctx: &BrowseContext, track_id: i64) -> Result<DidlOutput> {
    let item = load_track_item(ctx, track_id)?;
    Ok(DidlOutput {
        containers: vec![],
        items: vec![item],
    })
}

/// Classifies an album's disc layout: returns `(folder_layout, distinct_discs)`
/// where `folder_layout` is true iff the album is *cleanly* multi-disc (≥ 2
/// distinct disc numbers AND every track carries a positive disc tag). Shared
/// by [`album_tracks_children`] (what to list) and [`album_child_count`] (how
/// many `alb:{id}` direct children to advertise) so the two never disagree.
fn disc_layout(ctx: &BrowseContext, album_id: i64) -> Result<(bool, i64)> {
    let (distinct_discs, missing): (i64, i64) = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT disc_num),
                IFNULL(SUM(CASE WHEN disc_num IS NULL OR disc_num <= 0 THEN 1 ELSE 0 END), 0)
         FROM tracks WHERE album_id = ?1",
        params![album_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((distinct_discs >= 2 && missing == 0, distinct_discs))
}

/// Number of direct children `alb:{id}` exposes via BrowseDirectChildren: the
/// per-disc folder count for a cleanly multi-disc album, otherwise the track
/// count. Keeps `alb:{id}` BrowseMetadata `childCount` consistent with the
/// [`album_tracks_children`] listing. `track_count` is the album's stored
/// count, passed in to avoid a second query when the caller already has it.
pub(crate) fn album_child_count(ctx: &BrowseContext, album_id: i64, track_count: i64) -> i64 {
    match disc_layout(ctx, album_id) {
        Ok((true, distinct_discs)) => distinct_discs,
        _ => track_count,
    }
}

/// BrowseDirectChildren (`alb:{id}`). On a cleanly multi-disc album the
/// children are per-disc **containers** (`Disc 1` / `Disc 2` …); the tracks
/// themselves live one level down under [`disc_tracks_children`]. Single-disc
/// albums (and albums with inconsistent disc tags) return a flat track-item
/// list instead.
///
/// The folder layout exists to fix a play-queue duplication bug: a track
/// `<item>` and a disc `<container>` must never share one album listing.
/// Control points (verified on Linn) build a play queue by enqueuing the
/// listing's items *and* recursing into its child containers — when both sat
/// in the album's direct children, every disc was enqueued twice (once as
/// flat items, once via the divider container). Keeping tracks strictly under
/// disc containers means "play album" recurses each disc exactly once.
///
/// "Cleanly multi-disc" = at least two distinct disc numbers AND every track
/// carries a positive disc number. If any track lacks a disc tag we fall back
/// to the flat list so no track is orphaned under a disc folder that never
/// gets enumerated.
pub fn album_tracks_children(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let (folder_layout, distinct_discs) = disc_layout(ctx, album_id)?;

    if folder_layout {
        let mut stmt = ctx.conn.prepare_cached(
            "SELECT disc_num, COUNT(*) FROM tracks
             WHERE album_id = ?1 AND disc_num IS NOT NULL AND disc_num > 0
             GROUP BY disc_num ORDER BY disc_num
             LIMIT ?2 OFFSET ?3",
        )?;
        let containers: Vec<Container> = stmt
            .query_map(params![album_id, count as i64, start as i64], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .filter_map(|r| r.ok())
            .map(|(disc, cnt)| build_disc_container(ctx, album_id, disc, Some(cnt)))
            .collect();
        Ok(ChildrenResult {
            didl: DidlOutput {
                containers,
                items: vec![],
            },
            total_matches: distinct_discs as usize,
        })
    } else {
        let total: i64 = ctx.conn.query_row(
            "SELECT COUNT(*) FROM tracks WHERE album_id = ?1",
            params![album_id],
            |r| r.get(0),
        )?;
        let items = load_disc_tracks_paged(ctx, album_id, None, start, count)?;
        Ok(ChildrenResult {
            didl: DidlOutput {
                containers: vec![],
                items,
            },
            total_matches: total as usize,
        })
    }
}

/// BrowseMetadata (`disc:{album_id}:{disc}`). Returns the disc container
/// metadata. Used when a control point asks "what is this object?" for a
/// disc folder id.
pub fn disc_container(ctx: &BrowseContext, album_id: i64, disc: i64) -> Result<Container> {
    let child_count: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?1 AND disc_num = ?2",
        params![album_id, disc],
        |r| r.get(0),
    )?;
    Ok(build_disc_container(ctx, album_id, disc, Some(child_count)))
}

/// BrowseDirectChildren (`disc:{album_id}:{disc}`). Returns that disc's
/// tracks — the leaf level of the multi-disc album hierarchy.
pub fn disc_tracks_children(
    ctx: &BrowseContext,
    album_id: i64,
    disc: i64,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?1 AND disc_num = ?2",
        params![album_id, disc],
        |r| r.get(0),
    )?;
    let items = load_disc_tracks_paged(ctx, album_id, Some(disc), start, count)?;
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers: vec![],
            items,
        },
        total_matches: total as usize,
    })
}

fn load_track_item(ctx: &BrowseContext, track_id: i64) -> Result<Item> {
    let row: TrackRow = ctx
        .conn
        .query_row(
            "SELECT t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                (SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = t.album_id) > 1,
                t.composer, t.conductor, t.performer
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
                    composer: r.get(15)?,
                    conductor: r.get(16)?,
                    performer: r.get(17)?,
                })
            },
        )
        .map_err(|e| Error::sqlite_or_not_found(e, "track", track_id))?;
    Ok(build_track_item(ctx, track_id, &row))
}

/// Loads tracks for an album with an optional disc filter. `disc = None`
/// returns every track (single-disc album view); `Some(n)` returns only that
/// disc (disc-divider drill-down). `multi_disc` is forwarded into the
/// returned `TrackRow`s so `build_track_item` can gate
/// `<upnp:originalDiscNumber>` correctly.
fn load_disc_tracks_paged(
    ctx: &BrowseContext,
    album_id: i64,
    disc: Option<i64>,
    start: usize,
    count: usize,
) -> Result<Vec<Item>> {
    // When disc filter is given, we're inside a multi-disc album by construction.
    let multi_disc = disc.is_some();
    let sql = if disc.is_some() {
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                t.composer, t.conductor, t.performer
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.album_id = ?1 AND t.disc_num = ?2
         ORDER BY t.track_num
         LIMIT ?3 OFFSET ?4"
    } else {
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                t.composer, t.conductor, t.performer
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.album_id = ?1
         ORDER BY t.disc_num, t.track_num
         LIMIT ?2 OFFSET ?3"
    };
    let mut stmt = ctx.conn.prepare_cached(sql)?;
    let mapper = |r: &rusqlite::Row| -> rusqlite::Result<(i64, TrackRow)> {
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
                composer: r.get(15)?,
                conductor: r.get(16)?,
                performer: r.get(17)?,
            },
        ))
    };
    let rows: Vec<(i64, TrackRow)> = if let Some(d) = disc {
        stmt.query_map(params![album_id, d, count as i64, start as i64], mapper)?
            .filter_map(|r| r.ok())
            .collect()
    } else {
        stmt.query_map(params![album_id, count as i64, start as i64], mapper)?
            .filter_map(|r| r.ok())
            .collect()
    };
    Ok(rows
        .into_iter()
        .map(|(id, row)| build_track_item(ctx, id, &row))
        .collect())
}

/// Track row → DIDL Item. Used by both Browse and Search.
///
/// `<upnp:originalDiscNumber>` is emitted only for multi-disc albums.
/// `dc:title` is **not** modified for disc info — the disc boundary on
/// multi-disc albums is presented as a `<container>` divider injected into
/// the album's child list (see [`build_disc_divider`]), so titles stay clean.
pub(crate) fn build_track_item(ctx: &BrowseContext, track_id: i64, row: &TrackRow) -> Item {
    let protocol_info = format!("http-get:*:{}:*", row.mime_type);
    // #9: build `<upnp:author>` entries from the classical tag columns.
    let mut authors = Vec::new();
    if let Some(c) = row.composer.as_deref().filter(|s| !s.is_empty()) {
        authors.push(Author {
            role: "Composer",
            name: c.to_string(),
        });
    }
    if let Some(c) = row.conductor.as_deref().filter(|s| !s.is_empty()) {
        authors.push(Author {
            role: "Conductor",
            name: c.to_string(),
        });
    }
    if let Some(c) = row.performer.as_deref().filter(|s| !s.is_empty()) {
        authors.push(Author {
            role: "Performer",
            name: c.to_string(),
        });
    }
    Item {
        id: format!("trk:{track_id}"),
        parent_id: format!("alb:{}", row.album_id),
        title: row.title.clone().unwrap_or_else(|| "Unknown".to_string()),
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
        authors,
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

/// Per-disc sub-container under a multi-disc album. The album's direct
/// children are these `Disc N` folders (never the tracks directly — see
/// [`album_tracks_children`] for why mixing item + container in one album
/// listing double-queues on play). Tapping a disc folder drills into
/// [`disc_tracks_children`] for that disc's tracks. Carries the album cover as
/// `<upnp:albumArtURI>` so the disc grid shows the sleeve rather than a blank
/// folder icon.
fn build_disc_container(
    ctx: &BrowseContext,
    album_id: i64,
    disc: i64,
    child_count: Option<i64>,
) -> Container {
    Container {
        id: format!("disc:{album_id}:{disc}"),
        parent_id: format!("alb:{album_id}"),
        title: format!("Disc {disc}"),
        upnp_class: "object.container",
        child_count,
        artist: None,
        album_art_uri: Some(format!("{}/{}", ctx.art_base_url, album_id)),
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
                    composer: None,
                    conductor: None,
                    performer: None,
                    year: None,
                    rg_track_gain: None,
                    rg_track_peak: None,
                    rg_album_gain: None,
                    rg_album_peak: None,
                    artist_sort: None,
                    album_artist_sort: None,
                    album_sort: None,
                    title_sort: None,
                    composer_sort: None,
                    original_year: None,
                    mb_recording_id: None,
                    mb_release_id: None,
                    mb_release_group_id: None,
                    mb_artist_id: None,
                    mb_release_artist_id: None,
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
    fn bt1_single_disc_album_no_dividers_no_disc_number() {
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
        // Single-disc albums return track items directly (no disc folders).
        assert!(
            result.didl.containers.is_empty(),
            "single-disc emitted disc folders"
        );
        let titles: Vec<&str> = result.didl.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["First", "Second", "Third"]);
        for item in &result.didl.items {
            assert!(
                item.original_disc_number.is_none(),
                "single-disc emitted disc number"
            );
        }
    }

    #[test]
    fn bt2_multi_disc_album_children_are_disc_folders_not_tracks() {
        // The core fix: a cleanly multi-disc album lists per-disc *containers*
        // only. No track items sit at the album level, so a control point that
        // enqueues items + recurses containers can't double-queue a disc.
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
        // 2 discs → 2 folders, and crucially zero items at the album level.
        assert_eq!(result.total_matches, 2);
        assert!(
            result.didl.items.is_empty(),
            "tracks must not sit beside disc folders"
        );
        let titles: Vec<&str> = result
            .didl
            .containers
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Disc 1", "Disc 2"]);
        // Folder ids drill into the disc leaf; childCount reflects per-disc tracks.
        let ids: Vec<&str> = result
            .didl
            .containers
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec![
                &format!("disc:{album_id}:1")[..],
                &format!("disc:{album_id}:2")[..]
            ]
        );
        let child_counts: Vec<Option<i64>> = result
            .didl
            .containers
            .iter()
            .map(|c| c.child_count)
            .collect();
        assert_eq!(child_counts, vec![Some(2), Some(1)]);
    }

    #[test]
    fn bt2b_album_with_missing_disc_tags_falls_back_to_flat_list() {
        // Inconsistent disc tags (one track has no disc) → flat track list, not
        // disc folders, so the untagged track is never orphaned.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Patchy",
            &[(1, 1, "Tagged1"), (2, 1, "Tagged2"), (0, 2, "Untagged")],
        );
        let result = album_tracks_children(&ctx(&conn), album_id, 0, 100).unwrap();
        assert!(
            result.didl.containers.is_empty(),
            "patchy disc tags must not produce folders"
        );
        assert_eq!(result.total_matches, 3);
        assert_eq!(result.didl.items.len(), 3);
    }

    #[test]
    fn bt2c_album_child_count_matches_direct_children() {
        // BrowseMetadata childCount must equal BrowseDirectChildren totalMatches
        // for the same album object: disc-folder count when multi-disc, track
        // count when single-disc.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();

        let multi = seed(
            &conn,
            "A",
            "Multi",
            &[(1, 1, "D1T1"), (1, 2, "D1T2"), (2, 1, "D2T1")],
        );
        // 3 tracks across 2 discs → childCount is 2 (folders), not 3 (tracks).
        assert_eq!(album_child_count(&ctx(&conn), multi, 3), 2);
        assert_eq!(
            album_tracks_children(&ctx(&conn), multi, 0, 100)
                .unwrap()
                .total_matches,
            2
        );

        let single = seed(&conn, "B", "Single", &[(1, 1, "S1"), (1, 2, "S2")]);
        assert_eq!(album_child_count(&ctx(&conn), single, 2), 2);
        assert_eq!(
            album_tracks_children(&ctx(&conn), single, 0, 100)
                .unwrap()
                .total_matches,
            2
        );
    }

    #[test]
    fn bt3_track_metadata_keeps_title_clean_on_multi_disc() {
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
        assert_eq!(didl.items[0].title, "Beta");
        assert_eq!(didl.items[0].original_disc_number, Some(2));
    }

    #[test]
    fn bt4_disc_folder_pagination() {
        // Disc folders paginate like any other container list: total_matches is
        // the disc count, and start/count slice the folder list.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Multi",
            &[(1, 1, "A1"), (2, 1, "B1"), (3, 1, "C1")],
        );
        let result = album_tracks_children(&ctx(&conn), album_id, 1, 1).unwrap();
        assert_eq!(result.total_matches, 3);
        let titles: Vec<&str> = result
            .didl
            .containers
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert_eq!(titles, vec!["Disc 2"]);
    }

    #[test]
    fn bt5_disc_tracks_children_returns_only_that_disc() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Multi",
            &[(1, 1, "A1"), (1, 2, "A2"), (2, 1, "B1"), (2, 2, "B2")],
        );
        let result = disc_tracks_children(&ctx(&conn), album_id, 2, 0, 100).unwrap();
        assert_eq!(result.total_matches, 2);
        let titles: Vec<&str> = result.didl.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["B1", "B2"]);
        // Leaf tracks of a multi-disc album carry originalDiscNumber.
        for item in &result.didl.items {
            assert_eq!(item.original_disc_number, Some(2));
        }
    }
}

//! Shared logic for building a DIDL Item from a track row.
//! Used by both Browse (under album) and Search.

use rusqlite::params;

use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::{Author, Container, DidlNode, Item, Resource};

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
        nodes: vec![],
    })
}

/// BrowseDirectChildren (`alb:{id}`). Returns the album's children. On
/// multi-disc albums (`MAX(disc_num) > 1`), disc-divider containers are
/// interleaved at disc boundaries via `DidlOutput.nodes`. Single-disc
/// albums take the simpler items-only path (no nodes), preserving the
/// pre-multi-disc behavior.
pub fn album_tracks_children(
    ctx: &BrowseContext,
    album_id: i64,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let multi_disc: bool = ctx
        .conn
        .query_row(
            "SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = ?1",
            params![album_id],
            |r| r.get::<_, i64>(0).map(|n| n > 1),
        )
        .unwrap_or(false);
    if multi_disc {
        let all_nodes = load_multi_disc_album_nodes(ctx, album_id)?;
        let total = all_nodes.len();
        let nodes: Vec<DidlNode> = all_nodes.into_iter().skip(start).take(count).collect();
        Ok(ChildrenResult {
            didl: DidlOutput {
                containers: vec![],
                items: vec![],
                nodes,
            },
            total_matches: total,
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
                nodes: vec![],
            },
            total_matches: total as usize,
        })
    }
}

/// BrowseMetadata (`disc:{album_id}:{disc}`). Returns the disc-divider
/// container metadata. Used when a control point asks "what is this object?"
/// for a previously-cached divider id.
pub fn disc_container(ctx: &BrowseContext, album_id: i64, disc: i64) -> Result<Container> {
    let child_count: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE album_id = ?1 AND disc_num = ?2",
        params![album_id, disc],
        |r| r.get(0),
    )?;
    Ok(build_disc_divider(album_id, disc, Some(child_count)))
}

/// BrowseDirectChildren (`disc:{album_id}:{disc}`). Returns only that disc's
/// tracks — a redundant subset of the parent album's flat view, served so
/// that tapping the divider container leads somewhere coherent rather than
/// erroring out.
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
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

fn load_track_item(ctx: &BrowseContext, track_id: i64) -> Result<Item> {
    let row: TrackRow = ctx.conn.query_row(
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
    )?;
    Ok(build_track_item(ctx, track_id, &row))
}

/// Loads every track row for the album and interleaves disc dividers
/// (as `<container>` entries) at the disc boundaries. Caller has already
/// determined `multi_disc = true`.
fn load_multi_disc_album_nodes(ctx: &BrowseContext, album_id: i64) -> Result<Vec<DidlNode>> {
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                t.composer, t.conductor, t.performer
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.album_id = ?1
         ORDER BY t.disc_num, t.track_num",
    )?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(params![album_id], |r| {
            Ok((
                r.get(0)?,
                TrackRow {
                    album_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    genre: r.get(4)?,
                    track_num: r.get(5)?,
                    disc_num: r.get(6)?,
                    multi_disc: true,
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
        })?
        .filter_map(|r| r.ok())
        .collect();

    // Pre-count per-disc children for the divider's `childCount` attribute.
    let mut per_disc_count: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (_, row) in &rows {
        if let Some(d) = row.disc_num.filter(|&n| n > 0) {
            *per_disc_count.entry(d).or_default() += 1;
        }
    }

    let mut nodes = Vec::with_capacity(rows.len() + 4);
    let mut current_disc: Option<i64> = None;
    for (id, row) in rows {
        if let Some(d) = row.disc_num.filter(|&n| n > 0) {
            if Some(d) != current_disc {
                let child_count = per_disc_count.get(&d).copied();
                nodes.push(DidlNode::Container(build_disc_divider(
                    album_id,
                    d,
                    child_count,
                )));
                current_disc = Some(d);
            }
        }
        nodes.push(DidlNode::Item(build_track_item(ctx, id, &row)));
    }
    Ok(nodes)
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

/// Disc-divider container, injected into a multi-disc album's child list
/// right before the first track of each disc. Renders as a tappable folder
/// row whose `dc:title` is `">> Disc N"`. MinimServer ships the same pattern
/// (a `<container>` interleaved with track `<item>`s) and Linn renders it
/// inline because Linn preserves the server's response order.
///
/// Tapping the divider drills into [`disc_tracks_children`], which returns
/// just that disc's tracks — a redundant subset of the parent flat view, but
/// it keeps the divider from being a dead-end navigation target.
fn build_disc_divider(album_id: i64, disc: i64, child_count: Option<i64>) -> Container {
    Container {
        id: format!("disc:{album_id}:{disc}"),
        parent_id: format!("alb:{album_id}"),
        title: format!(">> Disc {disc}"),
        upnp_class: "object.container",
        child_count,
        artist: None,
        album_art_uri: None,
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

    fn node_title(n: &DidlNode) -> &str {
        match n {
            DidlNode::Container(c) => &c.title,
            DidlNode::Item(i) => &i.title,
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
        // Single-disc albums skip the nodes path and return items directly.
        assert!(result.didl.nodes.is_empty(), "single-disc emitted nodes");
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
    fn bt2_multi_disc_album_injects_dividers_with_clean_titles() {
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
        // 3 tracks + 2 dividers (one before each disc)
        assert_eq!(result.total_matches, 5);
        let titles: Vec<&str> = result.didl.nodes.iter().map(node_title).collect();
        assert_eq!(
            titles,
            vec![">> Disc 1", "D1T1", "D1T2", ">> Disc 2", "D2T1"]
        );
        let kinds: Vec<&'static str> = result
            .didl
            .nodes
            .iter()
            .map(|n| match n {
                DidlNode::Container(_) => "container",
                DidlNode::Item(_) => "item",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["container", "item", "item", "container", "item"]
        );
        // Disc-1 divider points at 2 children, disc-2 divider at 1.
        let child_counts: Vec<Option<i64>> = result
            .didl
            .nodes
            .iter()
            .filter_map(|n| match n {
                DidlNode::Container(c) => Some(c.child_count),
                _ => None,
            })
            .collect();
        assert_eq!(child_counts, vec![Some(2), Some(1)]);
        // originalDiscNumber emitted on tracks (multi-disc).
        let discs: Vec<Option<u32>> = result
            .didl
            .nodes
            .iter()
            .filter_map(|n| match n {
                DidlNode::Item(i) => Some(i.original_disc_number),
                _ => None,
            })
            .collect();
        assert_eq!(discs, vec![Some(1), Some(1), Some(2)]);
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
    fn bt4_multi_disc_pagination_spans_divider() {
        // Page boundary lands on a divider — make sure pagination doesn't drop
        // or duplicate elements.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let album_id = seed(
            &conn,
            "A",
            "Multi",
            &[(1, 1, "A1"), (1, 2, "A2"), (2, 1, "B1"), (2, 2, "B2")],
        );
        // Full list: ">> Disc 1", "A1", "A2", ">> Disc 2", "B1", "B2"  (6 nodes)
        let result = album_tracks_children(&ctx(&conn), album_id, 2, 2).unwrap();
        assert_eq!(result.total_matches, 6);
        let titles: Vec<&str> = result.didl.nodes.iter().map(node_title).collect();
        assert_eq!(titles, vec!["A2", ">> Disc 2"]);
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
    }
}

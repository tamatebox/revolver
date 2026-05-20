//! DB query + DIDL assembly for ContentDirectory Search (SPEC §5.4).
//!
//! Only case-insensitive `contains` over 3 properties (`dc:title` / `upnp:artist`
//! / `upnp:album`). Sort is fixed to title order (SortCriteria ignored).

use rusqlite::params;

use super::tracks::{build_track_item, TrackRow};
use super::{BrowseContext, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::Item;
use crate::upnp::search::SearchExpr;

pub struct SearchResult {
    pub didl: DidlOutput,
    pub total_matches: usize,
}

/// Issues the appropriate `LIKE` query for `expr` and converts hit tracks to DIDL Items.
/// `Unsupported` is short-circuited to an empty result by the caller, so it
/// should not reach the SQL path.
pub fn search_tracks(
    ctx: &BrowseContext,
    expr: &SearchExpr,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    let pattern = like_pattern(match expr {
        SearchExpr::Title(s) | SearchExpr::Artist(s) | SearchExpr::Album(s) => s,
        SearchExpr::Unsupported => {
            return Ok(SearchResult {
                didl: DidlOutput {
                    containers: vec![],
                    items: vec![],
                },
                total_matches: 0,
            });
        }
    });

    let (count_sql, list_sql) = sql_for(expr);

    let total: i64 = ctx
        .conn
        .query_row(count_sql, params![pattern], |r| r.get(0))?;

    let mut stmt = ctx.conn.prepare_cached(list_sql)?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(params![pattern, count as i64, start as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                TrackRow {
                    album_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    genre: r.get(4)?,
                    track_num: r.get(5)?,
                    duration_ms: r.get(6)?,
                    sample_rate: r.get(7)?,
                    bit_depth: r.get(8)?,
                    channels: r.get(9)?,
                    bitrate: r.get(10)?,
                    mime_type: r.get(11)?,
                    file_size: r.get(12)?,
                    album: r.get(13)?,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let items: Vec<Item> = rows
        .into_iter()
        .map(|(id, row)| build_track_item(ctx, id, &row))
        .collect();

    Ok(SearchResult {
        didl: DidlOutput {
            containers: vec![],
            items,
        },
        total_matches: total as usize,
    })
}

fn like_pattern(needle: &str) -> String {
    format!("%{}%", needle)
}

fn sql_for(expr: &SearchExpr) -> (&'static str, &'static str) {
    match expr {
        SearchExpr::Title(_) => (
            "SELECT COUNT(*) FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE t.title LIKE ?1 COLLATE NOCASE",
            "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num,
                    t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                    t.bitrate, t.mime_type, t.file_size, a.album
             FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE t.title LIKE ?1 COLLATE NOCASE
             ORDER BY t.title COLLATE NOCASE
             LIMIT ?2 OFFSET ?3",
        ),
        SearchExpr::Artist(_) => (
            "SELECT COUNT(*) FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE t.artist LIKE ?1 COLLATE NOCASE",
            "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num,
                    t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                    t.bitrate, t.mime_type, t.file_size, a.album
             FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE t.artist LIKE ?1 COLLATE NOCASE
             ORDER BY t.title COLLATE NOCASE
             LIMIT ?2 OFFSET ?3",
        ),
        SearchExpr::Album(_) => (
            "SELECT COUNT(*) FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE a.album LIKE ?1 COLLATE NOCASE",
            "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num,
                    t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                    t.bitrate, t.mime_type, t.file_size, a.album
             FROM tracks t JOIN albums a ON t.album_id = a.id
             WHERE a.album LIKE ?1 COLLATE NOCASE
             ORDER BY t.title COLLATE NOCASE
             LIMIT ?2 OFFSET ?3",
        ),
        SearchExpr::Unsupported => unreachable!("Unsupported handled earlier"),
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db::{albums, schema, tracks};

    /// Seed of 3 tracks / 2 albums (Abbey Road x2 + Various Artists' Hits x1).
    /// Intentionally identical to browse/mod.rs's seed_db (test independence preferred).
    fn seed_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        let beatles_id = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "The Beatles",
                album: "Abbey Road",
                compilation: false,
            },
            Some("The Beatles"),
            100,
        )
        .unwrap();
        let va_id = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "Various Artists",
                album: "Hits",
                compilation: true,
            },
            None,
            100,
        )
        .unwrap();
        for (album_id, path, title, artist) in [
            (
                beatles_id,
                "/m/come_together.flac",
                "Come Together",
                "The Beatles",
            ),
            (beatles_id, "/m/something.flac", "Something", "The Beatles"),
            (va_id, "/m/va_track.mp3", "VA Track", "Some Singer"),
        ] {
            tracks::upsert(
                &conn,
                &tracks::TrackRow {
                    album_id,
                    path,
                    title: Some(title),
                    artist: Some(artist),
                    genre: Some("Rock"),
                    track_num: Some(1),
                    disc_num: Some(1),
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
        albums::recalc_counts(&conn).unwrap();
        conn
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
    fn st1_title_contains_hits_one_track() {
        let conn = seed_db();
        let r = search_tracks(&ctx(&conn), &SearchExpr::Title("Come".to_string()), 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.items.len(), 1);
        assert_eq!(r.didl.items[0].title, "Come Together");
    }

    #[test]
    fn st2_album_contains_case_insensitive() {
        let conn = seed_db();
        // "ABBEY" matches seed's "Abbey Road" (NOCASE).
        let r =
            search_tracks(&ctx(&conn), &SearchExpr::Album("ABBEY".to_string()), 0, 100).unwrap();
        assert!(
            r.total_matches >= 2,
            "expected >=2 matches under Abbey Road, got {}",
            r.total_matches
        );
        assert!(r.didl.items.iter().any(|i| i.title == "Come Together"));
    }

    #[test]
    fn st3_artist_contains_hits_va_track() {
        let conn = seed_db();
        let r = search_tracks(
            &ctx(&conn),
            &SearchExpr::Artist("Some Singer".to_string()),
            0,
            100,
        )
        .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.items[0].title, "VA Track");
    }

    #[test]
    fn st4_no_match_returns_empty() {
        let conn = seed_db();
        let r = search_tracks(
            &ctx(&conn),
            &SearchExpr::Title("xyz_no_match_xyz".to_string()),
            0,
            100,
        )
        .unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.items.is_empty());
    }

    #[test]
    fn st5_unsupported_returns_empty_without_query() {
        let conn = seed_db();
        let r = search_tracks(&ctx(&conn), &SearchExpr::Unsupported, 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.items.is_empty());
    }

    #[test]
    fn st6_pagination_offset_limit() {
        let conn = seed_db();
        // Album "Abbey Road" has 2 tracks: Come Together / Something.
        let r = search_tracks(
            &ctx(&conn),
            &SearchExpr::Album("Abbey Road".to_string()),
            1,
            1,
        )
        .unwrap();
        assert_eq!(r.total_matches, 2);
        assert_eq!(r.didl.items.len(), 1);
        // ORDER BY t.title COLLATE NOCASE → "Come Together" (C) < "Something" (S).
        // offset=1, limit=1 returns the 2nd row = "Something".
        assert_eq!(r.didl.items[0].title, "Something");
    }
}

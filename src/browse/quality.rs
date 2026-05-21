//! Browse under quality categories (`cat:hires` / `cat:lossy` / `cat:mixed`)
//! (SPEC §4.6 / §6.2 / §6.4).
//!
//! The `albums.quality` column is auto-computed from tracks' codec / sample_rate /
//! bit_depth by [`crate::db::albums::recalc_quality`] in scan's post-tx.

use rusqlite::params;

use super::albums::album_container;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;

/// Under `cat:hires` / `cat:lossy` / `cat:mixed`: returns the album list filtered
/// by `albums.quality = ?`. Sorted by `effective_album_artist`, `album`.
pub fn quality_albums_children(
    ctx: &BrowseContext,
    quality_label: &str,
    parent_id: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums WHERE quality = ?1",
        params![quality_label],
        |r| r.get(0),
    )?;

    let mut stmt = ctx.conn.prepare_cached(
        "SELECT id, album, effective_album_artist, track_count
         FROM albums
         WHERE quality = ?1
         ORDER BY effective_album_artist, album
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![quality_label, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, parent_id))
        .collect();
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    use crate::browse::test_helpers::{default_ctx, open_in_memory_migrated};
    use crate::browse::BrowseContext;
    use crate::db::albums;
    use crate::random::RandomState;

    /// Build a DB containing one each of hires/lossless/lossy/mixed/unknown.
    /// Quality detection needs per-track codec / sample_rate / bit_depth, so
    /// we set rows via raw SQL instead of `default_track_row` (different shape
    /// from other views is acceptable here).
    fn seed_mixed_quality() -> Connection {
        let conn = open_in_memory_migrated();

        let make = |aa: &str, album: &str| -> i64 {
            albums::upsert(
                &conn,
                &albums::AlbumKey {
                    effective_album_artist: aa,
                    album,
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap()
        };
        let insert = |aid: i64, path: &str, codec: &str, sr: u32, bd: u32| {
            conn.execute(
                "INSERT INTO tracks (album_id, path, codec, sample_rate, bit_depth,
                                     duration_ms, added_at, mtime, mime_type, file_size)
                 VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 'x', 0)",
                params![aid, path, codec, sr, bd],
            )
            .unwrap();
        };

        // hires
        let hd = make("AA1", "HD");
        insert(hd, "/m/hd.flac", "flac", 96000, 24);
        // lossless
        let cd = make("AA2", "CD");
        insert(cd, "/m/cd.flac", "flac", 44100, 16);
        // lossy
        let mp3 = make("AA3", "Pop");
        insert(mp3, "/m/p.mp3", "mp3", 44100, 0);
        // mixed (flac + mp3)
        let mix = make("AA4", "Mix");
        insert(mix, "/m/m1.flac", "flac", 44100, 16);
        insert(mix, "/m/m2.mp3", "mp3", 44100, 0);

        albums::recalc_counts(&conn).unwrap();
        albums::recalc_quality(&conn).unwrap();
        conn
    }

    fn ctx_with<'a>(conn: &'a Connection, rs: &'a RandomState) -> BrowseContext<'a> {
        static BS: std::sync::OnceLock<crate::state::BrowseSettings> = std::sync::OnceLock::new();
        default_ctx(
            conn,
            rs,
            BS.get_or_init(crate::state::BrowseSettings::default),
            0,
        )
    }

    #[test]
    fn qb1_hires_returns_only_hires_albums() {
        let conn = seed_mixed_quality();
        let rs = RandomState::new();
        let r =
            quality_albums_children(&ctx_with(&conn, &rs), "hires", "cat:hires", 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "HD");
        assert_eq!(r.didl.containers[0].parent_id, "cat:hires");
    }

    #[test]
    fn qb2_lossy_returns_only_lossy_albums() {
        let conn = seed_mixed_quality();
        let rs = RandomState::new();
        let r =
            quality_albums_children(&ctx_with(&conn, &rs), "lossy", "cat:lossy", 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Pop");
    }

    #[test]
    fn qb3_mixed_returns_only_mixed_albums() {
        let conn = seed_mixed_quality();
        let rs = RandomState::new();
        let r =
            quality_albums_children(&ctx_with(&conn, &rs), "mixed", "cat:mixed", 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Mix");
    }

    #[test]
    fn qb4_pagination_offset_and_count() {
        let conn = seed_mixed_quality();
        // Add one more hires album to reach 2 entries.
        let hd2 = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "AA0",
                album: "HD2",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, codec, sample_rate, bit_depth,
                                 duration_ms, added_at, mtime, mime_type, file_size)
             VALUES (?1, '/m/hd2.flac', 'flac', 192000, 24, 0, 0, 0, 'x', 0)",
            params![hd2],
        )
        .unwrap();
        albums::recalc_quality(&conn).unwrap();

        let rs = RandomState::new();
        let r = quality_albums_children(&ctx_with(&conn, &rs), "hires", "cat:hires", 1, 1).unwrap();
        assert_eq!(r.total_matches, 2);
        assert_eq!(r.didl.containers.len(), 1);
        // AA0 < AA1, so AA0(HD2) comes first; offset=1 returns AA1(HD).
        assert_eq!(r.didl.containers[0].title, "HD");
    }

    #[test]
    fn qb5_empty_quality_returns_zero() {
        let conn = open_in_memory_migrated();
        let rs = RandomState::new();
        let r =
            quality_albums_children(&ctx_with(&conn, &rs), "hires", "cat:hires", 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }
}

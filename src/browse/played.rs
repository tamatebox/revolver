//! Browse under `cat:played` (Recently Played) (SPEC §6.4, §6.8).
//!
//! Sorted by `albums.last_played_at` DESC (denormalized; updated on track playback
//! via `stream::bump_play_stats` → `albums::bump_album_last_played_at`).
//! Albums never played (`last_played_at IS NULL`) are excluded.
//!
//! The old implementation ran `MAX(tracks.last_played_at) GROUP BY album_id` per
//! page; denormalization lets `idx_alb_last_played` handle it in one lookup (perf §P0).
//!
//! play_count / last_played_at updates happen in the [`crate::http::stream`]
//! handler (counted only on requests with no Range or start=0, SPEC §6.8).

use rusqlite::params;

use super::albums::album_container;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;

/// Under `cat:played`: albums with playback history, sorted by last-played time DESC.
pub fn played_albums_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    // total is the count of albums where last_played_at IS NOT NULL (uses idx_alb_last_played).
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums WHERE last_played_at IS NOT NULL",
        [],
        |r| r.get(0),
    )?;

    let mut stmt = ctx.conn.prepare_cached(
        "SELECT id, album, effective_album_artist, track_count
         FROM albums
         WHERE last_played_at IS NOT NULL
         ORDER BY last_played_at DESC, id DESC
         LIMIT ?1 OFFSET ?2",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, "cat:played"))
        .collect();
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
        },
        total_matches: total as usize,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    use crate::browse::test_helpers::{default_ctx, default_track_row, open_in_memory_migrated};
    use crate::browse::BrowseContext;
    use crate::db::{albums, tracks};

    fn seed_with_play_history(history: &[(&str, Option<i64>)]) -> Connection {
        let conn = open_in_memory_migrated();
        for (name, last_played) in history {
            let aid = albums::upsert(
                &conn,
                &albums::AlbumKey {
                    effective_album_artist: "AA",
                    album: name,
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap();
            let path = format!("/m/{}.flac", name);
            tracks::upsert(&conn, &default_track_row(aid, &path, 0)).unwrap();
            if let Some(lp) = last_played {
                conn.execute(
                    "UPDATE tracks SET last_played_at = ?1, play_count = 1
                     WHERE album_id = ?2",
                    params![lp, aid],
                )
                .unwrap();
                // denormalize: in production this happens in the hot path via
                // bump_album_last_played_at. Batch recalc in tests yields the same result.
                albums::bump_album_last_played_at(&conn, aid, *lp).unwrap();
            }
        }
        albums::recalc_counts(&conn).unwrap();
        conn
    }

    fn ctx(conn: &Connection) -> BrowseContext<'_> {
        static RS: std::sync::OnceLock<crate::random::RandomState> = std::sync::OnceLock::new();
        static BS: std::sync::OnceLock<crate::state::BrowseSettings> = std::sync::OnceLock::new();
        default_ctx(
            conn,
            RS.get_or_init(crate::random::RandomState::new),
            BS.get_or_init(crate::state::BrowseSettings::default),
            0,
        )
    }

    #[test]
    fn pb1_orders_by_max_last_played_at_desc() {
        let conn = seed_with_play_history(&[
            ("Old", Some(100)),
            ("Newest", Some(300)),
            ("Middle", Some(200)),
        ]);
        let r = played_albums_children(&ctx(&conn), 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["Newest", "Middle", "Old"]);
    }

    #[test]
    fn pb2_unplayed_albums_are_excluded() {
        let conn = seed_with_play_history(&[("Played", Some(100)), ("NeverPlayed", None)]);
        let r = played_albums_children(&ctx(&conn), 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Played");
    }

    #[test]
    fn pb3_pagination() {
        let conn = seed_with_play_history(&[("A", Some(100)), ("B", Some(200)), ("C", Some(300))]);
        let r = played_albums_children(&ctx(&conn), 1, 1).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 1);
        // DESC [C, B, A] with offset=1 → B
        assert_eq!(r.didl.containers[0].title, "B");
    }

    #[test]
    fn pb4_parent_id_is_cat_played() {
        let conn = seed_with_play_history(&[("X", Some(100))]);
        let r = played_albums_children(&ctx(&conn), 0, 100).unwrap();
        for c in &r.didl.containers {
            assert_eq!(c.parent_id, "cat:played");
        }
    }

    #[test]
    fn pb5_empty_history_returns_zero() {
        let conn = open_in_memory_migrated();
        let r = played_albums_children(&ctx(&conn), 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }
}

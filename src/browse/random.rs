//! Browse under `cat:random` (Random Albums) (SPEC §6.6).
//!
//! Core logic lives in `crate::random::RandomState` (full reshuffle at startup /
//! after scan / on `POST /admin/reshuffle`). This module just borrows that state
//! from `BrowseContext` and, for each paged album_id, fetches the row from
//! `albums` and packs it into a Container.

use rusqlite::{params, OptionalExtension};

use super::albums::album_container;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;

/// Under `cat:random`: returns albums in `random_state.page(start, count)` order.
/// `count` is capped at `config.browse.random_albums_limit`.
pub fn random_albums_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let count = count.min(ctx.settings.random_albums_limit);
    let ids = ctx.random_state.page(start, count);
    let total = ctx.random_state.len();

    let mut stmt = ctx.conn.prepare_cached(
        "SELECT album, effective_album_artist, track_count FROM albums WHERE id = ?1",
    )?;
    let mut containers = Vec::with_capacity(ids.len());
    for id in ids {
        // The id may have been deleted between reshuffle and now (timing skew
        // during scan), so skip QueryReturnedNoRows (the next post-scan reshuffle
        // will restore consistency).
        let row: Option<(String, String, i64)> = stmt
            .query_row(params![id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        if let Some((album, aa, tc)) = row {
            containers.push(album_container(ctx, id, &album, &aa, tc, "cat:random"));
        }
    }

    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
            nodes: vec![],
        },
        total_matches: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    use crate::browse::test_helpers::{default_ctx, default_track_row, open_in_memory_migrated};
    use crate::browse::BrowseContext;
    use crate::db::{albums, tracks};
    use crate::random::RandomState;

    fn seed_three_albums() -> Connection {
        let conn = open_in_memory_migrated();
        for name in ["A", "B", "C"] {
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
        }
        albums::recalc_counts(&conn).unwrap();
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
    fn rb1_random_follows_state_order() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        rs.reshuffle(&conn).unwrap();

        let r = random_albums_children(&ctx_with(&conn, &rs), 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 3);
        // Order must match page(0,100) (reshuffle result and DB-fetched result are consistent).
        let got_ids: Vec<i64> = r
            .didl
            .containers
            .iter()
            .map(|c| c.id.strip_prefix("alb:").unwrap().parse().unwrap())
            .collect();
        let expected = rs.page(0, 100);
        assert_eq!(got_ids, expected);
    }

    #[test]
    fn rb2_pagination_offset_and_count() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        rs.reshuffle(&conn).unwrap();

        let r = random_albums_children(&ctx_with(&conn, &rs), 1, 1).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 1);
        let got_id: i64 = r.didl.containers[0]
            .id
            .strip_prefix("alb:")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(got_id, rs.page(1, 1)[0]);
    }

    #[test]
    fn rb3_empty_state_returns_zero() {
        let conn = seed_three_albums();
        // Un-reshuffled RandomState → empty array.
        let rs = RandomState::new();
        let r = random_albums_children(&ctx_with(&conn, &rs), 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }

    #[test]
    fn rb4_parent_id_is_cat_random() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        rs.reshuffle(&conn).unwrap();
        let r = random_albums_children(&ctx_with(&conn, &rs), 0, 100).unwrap();
        for c in &r.didl.containers {
            assert_eq!(c.parent_id, "cat:random");
        }
    }

    #[test]
    fn rb5_stale_album_id_in_state_is_skipped() {
        // Stale album_ids in state that no longer exist are skipped without panic.
        let conn = seed_three_albums();
        let rs = RandomState::new();
        rs.reshuffle(&conn).unwrap();
        // Delete all tracks then delete_orphans on albums → only state retains stale ids.
        conn.execute("DELETE FROM tracks", []).unwrap();
        albums::delete_orphans(&conn).unwrap();

        let r = random_albums_children(&ctx_with(&conn, &rs), 0, 100).unwrap();
        // total is state-based (3); containers is 0 since rows are missing in DB.
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 0);
    }
}

//! Browse under `cat:recent` (Recently Added) (SPEC §6.4, §6.7).
//!
//! Previously `cat:recent` was a sub-container hierarchy (`day` / `week` /
//! `month` / `3months` / `year:YYYY` / `all`). Issue #16 flattened this:
//! opening `cat:recent` now returns the album list directly, sorted by
//! `albums.last_added_at` DESC, capped by:
//! - `browse.recently_added_limit` (count cap),
//! - `browse.recently_added_max_age_days` (age cap; `None` = no age cap).
//!
//! `albums.last_added_at` is a denormalized column maintained by
//! `recalc_last_added_at` after every scan (so adding a new track to an
//! existing album re-floats the album to the top of the list).

use rusqlite::params;

use super::albums::album_container;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;

const DAY_SECS: i64 = 86400;

/// Children of `cat:recent`: a flat list of albums by recency.
///
/// Both `recently_added_limit` and `recently_added_max_age_days` are applied to
/// `total_matches` and to the row slice — clients (Linn etc.) page strictly to
/// `total_matches`, so capping only the page size would still surface every
/// album in the window across N pages.
pub fn recent_root_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let limit = ctx.settings.recently_added_limit;
    let lower_bound: Option<i64> = ctx
        .settings
        .recently_added_max_age_days
        .map(|days| ctx.now_secs - (days as i64) * DAY_SECS);

    let actual = count_in_window(ctx, lower_bound)? as usize;
    let total = actual.min(limit);
    let remaining = total.saturating_sub(start);
    let effective_count = count.min(remaining);
    let rows = if effective_count == 0 {
        Vec::new()
    } else {
        list_in_window(ctx, lower_bound, start, effective_count)?
    };

    let parent_id = "cat:recent";
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
        total_matches: total,
    })
}

fn count_in_window(ctx: &BrowseContext, lower_bound: Option<i64>) -> Result<i64> {
    let n: i64 = match lower_bound {
        Some(lo) => ctx.conn.query_row(
            "SELECT COUNT(*) FROM albums WHERE last_added_at IS NOT NULL AND last_added_at >= ?1",
            params![lo],
            |r| r.get(0),
        )?,
        None => ctx.conn.query_row(
            "SELECT COUNT(*) FROM albums WHERE last_added_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?,
    };
    Ok(n)
}

fn list_in_window(
    ctx: &BrowseContext,
    lower_bound: Option<i64>,
    start: usize,
    count: usize,
) -> Result<Vec<(i64, String, String, i64)>> {
    let (sql, params): (&str, Vec<i64>) = match lower_bound {
        Some(lo) => (
            "SELECT id, album, effective_album_artist, track_count
             FROM albums
             WHERE last_added_at IS NOT NULL AND last_added_at >= ?1
             ORDER BY last_added_at DESC, id DESC
             LIMIT ?2 OFFSET ?3",
            vec![lo, count as i64, start as i64],
        ),
        None => (
            "SELECT id, album, effective_album_artist, track_count
             FROM albums
             WHERE last_added_at IS NOT NULL
             ORDER BY last_added_at DESC, id DESC
             LIMIT ?1 OFFSET ?2",
            vec![count as i64, start as i64],
        ),
    };

    let mut stmt = ctx.conn.prepare_cached(sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    use crate::browse::test_helpers::{default_ctx, default_track_row, open_in_memory_migrated};
    use crate::browse::BrowseContext;
    use crate::db::{albums, tracks};
    use crate::state::BrowseSettings;

    /// Create albums with arbitrary `added_at` values. Each album has 1 track.
    fn seed_with_added_at(added_ats: &[(&str, i64)]) -> Connection {
        let conn = open_in_memory_migrated();
        for (name, added) in added_ats {
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
            tracks::upsert(&conn, &default_track_row(aid, &path, *added)).unwrap();
        }
        albums::recalc_counts(&conn).unwrap();
        albums::recalc_last_added_at(&conn).unwrap();
        conn
    }

    fn ctx_at(conn: &Connection, now_secs: i64) -> BrowseContext<'_> {
        static RS: std::sync::OnceLock<crate::random::RandomState> = std::sync::OnceLock::new();
        static BS: std::sync::OnceLock<BrowseSettings> = std::sync::OnceLock::new();
        default_ctx(
            conn,
            RS.get_or_init(crate::random::RandomState::new),
            BS.get_or_init(BrowseSettings::default),
            now_secs,
        )
    }

    fn ctx_with_max_age(
        conn: &Connection,
        now_secs: i64,
        max_age_days: u32,
    ) -> (BrowseContext<'_>, BrowseSettings) {
        let settings = BrowseSettings {
            recently_added_max_age_days: Some(max_age_days),
            ..BrowseSettings::default()
        };
        let rs = crate::random::RandomState::new();
        // Box::leak: tests only — the leaked allocations live for the test
        // process lifetime, which is fine for a single assertion.
        let settings_clone = settings.clone();
        let ctx = BrowseContext {
            conn,
            art_base_url: "http://x/art",
            stream_base_url: "http://x/stream",
            random_state: Box::leak(Box::new(rs)),
            now_secs,
            settings: Box::leak(Box::new(settings)),
        };
        (ctx, settings_clone)
    }

    // Use 2024-01-01 00:00:00 UTC as the reference time (fixed for reproducible tests).
    const NOW_2024_01_01: i64 = 1704067200;

    #[test]
    fn rr1_no_age_cap_returns_all_albums_in_recency_order() {
        let conn = seed_with_added_at(&[
            ("Old", NOW_2024_01_01 - 2 * 365 * DAY_SECS),
            ("Recent", NOW_2024_01_01 - DAY_SECS),
        ]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01), 0, 100).unwrap();
        assert_eq!(r.total_matches, 2);
        assert_eq!(r.didl.containers[0].title, "Recent");
        assert_eq!(r.didl.containers[1].title, "Old");
    }

    #[test]
    fn rr2_age_cap_excludes_older_albums() {
        let conn = seed_with_added_at(&[
            ("InsideWindow", NOW_2024_01_01 - 3 * DAY_SECS),
            ("OutsideWindow", NOW_2024_01_01 - 10 * DAY_SECS),
        ]);
        // 5-day cap: only InsideWindow qualifies.
        let (ctx, _keep) = ctx_with_max_age(&conn, NOW_2024_01_01, 5);
        let r = recent_root_children(&ctx, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "InsideWindow");
    }

    #[test]
    fn rr3_parent_id_is_cat_recent() {
        let conn = seed_with_added_at(&[("A", NOW_2024_01_01 - DAY_SECS)]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01), 0, 100).unwrap();
        assert_eq!(r.didl.containers[0].parent_id, "cat:recent");
    }

    #[test]
    fn rr4_pagination_offset_and_count() {
        let conn = seed_with_added_at(&[
            ("A", NOW_2024_01_01 - 3),
            ("B", NOW_2024_01_01 - 2),
            ("C", NOW_2024_01_01 - 1),
        ]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01), 1, 1).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 1);
        // DESC [C, B, A] with offset=1 → B
        assert_eq!(r.didl.containers[0].title, "B");
    }

    #[test]
    fn rr5_empty_library_returns_empty() {
        let conn = open_in_memory_migrated();
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01), 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }

    fn ctx_with_limit(
        conn: &Connection,
        now_secs: i64,
        limit: usize,
    ) -> (BrowseContext<'_>, BrowseSettings) {
        let settings = BrowseSettings {
            recently_added_limit: limit,
            ..BrowseSettings::default()
        };
        let rs = crate::random::RandomState::new();
        let settings_clone = settings.clone();
        let ctx = BrowseContext {
            conn,
            art_base_url: "http://x/art",
            stream_base_url: "http://x/stream",
            random_state: Box::leak(Box::new(rs)),
            now_secs,
            settings: Box::leak(Box::new(settings)),
        };
        (ctx, settings_clone)
    }

    #[test]
    fn rr7_limit_caps_total_matches_and_rows() {
        // 5 albums in window, limit=2 → total_matches clamps to 2 so Linn does
        // not paginate past the cap; subsequent pages return empty.
        let conn = seed_with_added_at(&[
            ("A", NOW_2024_01_01 - 5),
            ("B", NOW_2024_01_01 - 4),
            ("C", NOW_2024_01_01 - 3),
            ("D", NOW_2024_01_01 - 2),
            ("E", NOW_2024_01_01 - 1),
        ]);
        let (ctx, _keep) = ctx_with_limit(&conn, NOW_2024_01_01, 2);
        let page1 = recent_root_children(&ctx, 0, 100).unwrap();
        assert_eq!(page1.total_matches, 2);
        assert_eq!(page1.didl.containers.len(), 2);
        // DESC by recency → E, D
        assert_eq!(page1.didl.containers[0].title, "E");
        assert_eq!(page1.didl.containers[1].title, "D");

        let page2 = recent_root_children(&ctx, 2, 100).unwrap();
        assert_eq!(page2.total_matches, 2);
        assert!(page2.didl.containers.is_empty());
    }

    #[test]
    fn rr6_existing_album_with_new_track_resurfaces() {
        // Adding a new track to an existing album updates MAX(added_at), so the
        // album re-floats to the top of cat:recent (SPEC §6.4 "resurface").
        let conn = seed_with_added_at(&[("Old", NOW_2024_01_01 - 30 * DAY_SECS)]);
        let old_id: i64 = conn
            .query_row("SELECT id FROM albums WHERE album = 'Old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                 duration_ms)
             VALUES (?1, '/m/Old_bonus.flac', ?2, 0, 'flac', 'audio/flac', 0, 0)",
            params![old_id, NOW_2024_01_01 - 3600],
        )
        .unwrap();
        albums::recalc_last_added_at(&conn).unwrap();

        // With a 1-day age cap the album now qualifies because its latest track
        // is 1 hour old (the album itself was originally seeded 30d back).
        let (ctx, _keep) = ctx_with_max_age(&conn, NOW_2024_01_01, 1);
        let r = recent_root_children(&ctx, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Old");
    }
}

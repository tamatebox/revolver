//! Browse under `cat:recent` (Recently Added) (SPEC §6.4, §6.7).
//!
//! Hierarchy:
//! ```text
//! cat:recent
//! ├── cat:recent:day        ← last 24h
//! ├── cat:recent:week       ← last 7d
//! ├── cat:recent:month      ← last 30d
//! ├── cat:recent:3months    ← last 90d
//! ├── cat:recent:year:YYYY  ← per year (dynamically enumerated only for years with data)
//! └── cat:recent:all        ← all
//! ```
//!
//! Dynamic omission rule: a wider range with the same COUNT as the next-shorter
//! range is hidden. e.g. if both `Last day` and `Last week` have 5 items, `Last week`
//! is hidden (don't show empty choices). Time-bounded ranges with COUNT == 0 are
//! also hidden. `cat:recent:all` is always shown.
//!
//! Sorted by `albums.last_added_at` DESC, tie-broken by `albums.id DESC`.
//! `albums.last_added_at` is a denormalized column bulk-recalced after scan
//! (`MAX(tracks.added_at) GROUP BY album_id`); Browse reads it with a single
//! index lookup (perf §P0; the old implementation re-ran the GROUP BY 5 times
//! per root open). The "resurface on new track added" behavior is preserved.

use rusqlite::params;

use super::albums::album_container;
use super::categories::plain_cat;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::object_id::{self, ObjectId, RecentRange};

const DAY: i64 = 86400;
const WEEK: i64 = 7 * DAY;
const MONTH: i64 = 30 * DAY;
const THREE_MONTHS: i64 = 90 * DAY;
/// Max number of per-year sub-containers (SPEC §6.7, latest 10 years).
const MAX_YEAR_ENTRIES: usize = 10;

/// Under `cat:recent`: list of time-range sub-containers (SPEC §6.7).
///
/// The old implementation fired 5 queries per request (4 COUNTs over
/// `MAX(added_at) GROUP BY album_id` + 1 year-extraction). With
/// `albums.last_added_at` denormalized, a single query aggregates all 4 range
/// COUNTs at once via CASE.
pub fn recent_root_children(ctx: &BrowseContext) -> Result<ChildrenResult> {
    let now = ctx.now_secs;

    // Aggregate the lower bounds for all 4 ranges in one query. NULL last_added_at
    // means an orphan album with no tracks (normally removed by delete_orphans,
    // but `IS NOT NULL` guards against mid-scan states).
    let (cnt_day, cnt_week, cnt_month, cnt_3m): (i64, i64, i64, i64) = ctx.conn.query_row(
        "SELECT
           SUM(CASE WHEN last_added_at >= ?1 THEN 1 ELSE 0 END),
           SUM(CASE WHEN last_added_at >= ?2 THEN 1 ELSE 0 END),
           SUM(CASE WHEN last_added_at >= ?3 THEN 1 ELSE 0 END),
           SUM(CASE WHEN last_added_at >= ?4 THEN 1 ELSE 0 END)
         FROM albums
         WHERE last_added_at IS NOT NULL",
        params![now - DAY, now - WEEK, now - MONTH, now - THREE_MONTHS],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
            ))
        },
    )?;

    let bounded = [
        (RecentRange::Day, "Last day", cnt_day),
        (RecentRange::Week, "Last week", cnt_week),
        (RecentRange::Month, "Last month", cnt_month),
        (RecentRange::ThreeMonths, "Last 3 months", cnt_3m),
    ];
    let mut containers = Vec::with_capacity(4 + MAX_YEAR_ENTRIES + 1);
    let mut prev_count: i64 = 0;
    for (range, title, count) in bounded {
        // Hide empty ranges, and hide ranges whose COUNT matches the next-shorter range (redundant).
        if count > prev_count {
            containers.push(plain_cat(
                &object_id::encode(&ObjectId::CatRecentRange(range)),
                "cat:recent",
                title,
            ));
            prev_count = count;
        }
    }

    // Per-year (latest 10). Only years with data.
    let mut year_stmt = ctx.conn.prepare_cached(
        "SELECT CAST(strftime('%Y', last_added_at, 'unixepoch') AS INTEGER) AS y
         FROM albums
         WHERE last_added_at IS NOT NULL
         GROUP BY y
         ORDER BY y DESC
         LIMIT ?1",
    )?;
    let years: Vec<u16> = year_stmt
        .query_map(params![MAX_YEAR_ENTRIES as i64], |r| r.get::<_, i64>(0))?
        .filter_map(|r| r.ok())
        .filter_map(|y| u16::try_from(y).ok())
        .collect();
    for y in years {
        containers.push(plain_cat(
            &object_id::encode(&ObjectId::CatRecentRange(RecentRange::Year(y))),
            "cat:recent",
            &y.to_string(),
        ));
    }

    // All (always shown).
    containers.push(plain_cat(
        &object_id::encode(&ObjectId::CatRecentRange(RecentRange::All)),
        "cat:recent",
        "Show All",
    ));

    let total = containers.len();
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
        },
        total_matches: total,
    })
}

/// Album list under the given range. Sorted by `albums.last_added_at` DESC.
/// `count` is capped at `config.browse.recently_added_limit`.
pub fn recent_range_children(
    ctx: &BrowseContext,
    range: RecentRange,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let count = count.min(ctx.settings.recently_added_limit);
    let (lower, upper) = range_bounds(ctx.now_secs, range);
    let total = count_in_window(ctx, lower, upper)?;

    let (sql, params): (&str, Vec<i64>) = match (lower, upper) {
        (Some(lo), Some(hi)) => (
            "SELECT id, album, effective_album_artist, track_count
             FROM albums
             WHERE last_added_at BETWEEN ?1 AND ?2
             ORDER BY last_added_at DESC, id DESC
             LIMIT ?3 OFFSET ?4",
            vec![lo, hi, count as i64, start as i64],
        ),
        (Some(lo), None) => (
            "SELECT id, album, effective_album_artist, track_count
             FROM albums
             WHERE last_added_at >= ?1
             ORDER BY last_added_at DESC, id DESC
             LIMIT ?2 OFFSET ?3",
            vec![lo, count as i64, start as i64],
        ),
        (None, None) => (
            "SELECT id, album, effective_album_artist, track_count
             FROM albums
             WHERE last_added_at IS NOT NULL
             ORDER BY last_added_at DESC, id DESC
             LIMIT ?1 OFFSET ?2",
            vec![count as i64, start as i64],
        ),
        (None, Some(_)) => unreachable!("range_bounds never yields (None, Some)"),
    };

    let parent_id = object_id::encode(&ObjectId::CatRecentRange(range));
    let mut stmt = ctx.conn.prepare_cached(sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
        .collect();
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
        },
        total_matches: total as usize,
    })
}

/// Lower / upper unix-second bounds for the range. `All` is (None, None);
/// `Day/Week/Month/3Months` have only a lower bound; `Year(YYYY)` has both.
fn range_bounds(now: i64, range: RecentRange) -> (Option<i64>, Option<i64>) {
    match range {
        RecentRange::Day => (Some(now - DAY), None),
        RecentRange::Week => (Some(now - WEEK), None),
        RecentRange::Month => (Some(now - MONTH), None),
        RecentRange::ThreeMonths => (Some(now - THREE_MONTHS), None),
        RecentRange::Year(y) => {
            // unix seconds at year start .. 1 second before next year's start. UTC.
            let start = unix_year_start(y);
            let end = unix_year_start(y + 1).saturating_sub(1);
            (Some(start), Some(end))
        }
        RecentRange::All => (None, None),
    }
}

/// unix seconds for `YYYY-01-01T00:00:00Z`. Instead of letting SQLite run
/// `strftime('%s', 'YYYY-01-01')`, compute via day count in Rust (year is
/// already clamped to [1900, 2100]). Naive leap-year handling included.
fn unix_year_start(year: u16) -> i64 {
    // Cumulative seconds since 1970 (UTC). Could be done in SQLite, but kept in Rust.
    let y = year as i64;
    let mut days: i64 = 0;
    if y >= 1970 {
        for yi in 1970..y {
            days += if is_leap(yi) { 366 } else { 365 };
        }
    } else {
        for yi in y..1970 {
            days -= if is_leap(yi) { 366 } else { 365 };
        }
    }
    days * DAY
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn count_in_window(ctx: &BrowseContext, lower: Option<i64>, upper: Option<i64>) -> Result<i64> {
    let n: i64 = match (lower, upper) {
        (Some(lo), Some(hi)) => ctx.conn.query_row(
            "SELECT COUNT(*) FROM albums WHERE last_added_at BETWEEN ?1 AND ?2",
            params![lo, hi],
            |r| r.get(0),
        )?,
        (Some(lo), None) => ctx.conn.query_row(
            "SELECT COUNT(*) FROM albums WHERE last_added_at >= ?1",
            params![lo],
            |r| r.get(0),
        )?,
        (None, None) => ctx.conn.query_row(
            "SELECT COUNT(*) FROM albums WHERE last_added_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?,
        (None, Some(_)) => unreachable!(),
    };
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    use crate::browse::test_helpers::{default_ctx, default_track_row, open_in_memory_migrated};
    use crate::browse::BrowseContext;
    use crate::db::{albums, tracks};

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
        static BS: std::sync::OnceLock<crate::state::BrowseSettings> = std::sync::OnceLock::new();
        default_ctx(
            conn,
            RS.get_or_init(crate::random::RandomState::new),
            BS.get_or_init(crate::state::BrowseSettings::default),
            now_secs,
        )
    }

    // Use 2024-01-01 00:00:00 UTC as the reference time (fixed for reproducible tests).
    const NOW_2024_01_01: i64 = 1704067200;

    // ── recent_range_children ──────────────────────────────────────────

    #[test]
    fn rr1_range_all_returns_all_albums() {
        let conn = seed_with_added_at(&[
            ("Old", NOW_2024_01_01 - 2 * 365 * DAY),
            ("Recent", NOW_2024_01_01 - DAY),
        ]);
        let r = recent_range_children(&ctx_at(&conn, NOW_2024_01_01), RecentRange::All, 0, 100)
            .unwrap();
        assert_eq!(r.total_matches, 2);
        // Recent (newer added_at) comes first.
        assert_eq!(r.didl.containers[0].title, "Recent");
        assert_eq!(r.didl.containers[1].title, "Old");
    }

    #[test]
    fn rr2_range_day_filters_to_last_24h() {
        let conn = seed_with_added_at(&[
            ("InsideDay", NOW_2024_01_01 - 3600), // 1h ago
            ("Yesterday", NOW_2024_01_01 - 2 * DAY),
        ]);
        let r = recent_range_children(&ctx_at(&conn, NOW_2024_01_01), RecentRange::Day, 0, 100)
            .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "InsideDay");
        assert_eq!(r.didl.containers[0].parent_id, "cat:recent:day");
    }

    #[test]
    fn rr3_range_year_filters_by_year_window() {
        // Add one entry each to 2023, 2024, 2025.
        let conn = seed_with_added_at(&[
            ("Y2023", unix_year_start(2023) + DAY),
            ("Y2024", unix_year_start(2024) + DAY),
            ("Y2025", unix_year_start(2025) + DAY),
        ]);
        let r = recent_range_children(
            &ctx_at(&conn, NOW_2024_01_01),
            RecentRange::Year(2024),
            0,
            100,
        )
        .unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Y2024");
        assert_eq!(r.didl.containers[0].parent_id, "cat:recent:year:2024");
    }

    #[test]
    fn rr4_range_pagination_offset_and_count() {
        let conn = seed_with_added_at(&[
            ("A", NOW_2024_01_01 - 3),
            ("B", NOW_2024_01_01 - 2),
            ("C", NOW_2024_01_01 - 1),
        ]);
        let r =
            recent_range_children(&ctx_at(&conn, NOW_2024_01_01), RecentRange::All, 1, 1).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.containers.len(), 1);
        // DESC [C, B, A] with offset=1 → B
        assert_eq!(r.didl.containers[0].title, "B");
    }

    // ── recent_root_children ───────────────────────────────────────────

    #[test]
    fn rt1_root_returns_relevant_ranges_plus_all() {
        // day=1, week=2 (differs from day), month=2 (same as week → hidden), 3months=2 (same → hidden).
        let conn = seed_with_added_at(&[
            ("InDay", NOW_2024_01_01 - 3600),
            ("InWeek", NOW_2024_01_01 - 3 * DAY),
        ]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01)).unwrap();
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"cat:recent:day"));
        assert!(ids.contains(&"cat:recent:week"));
        // month / 3months share the same COUNT (=2) as week → dynamically omitted.
        assert!(!ids.contains(&"cat:recent:month"));
        assert!(!ids.contains(&"cat:recent:3months"));
        // all is always shown.
        assert!(ids.contains(&"cat:recent:all"));
    }

    #[test]
    fn rt2_root_omits_zero_count_ranges() {
        // All albums are 5 years old. day/week/month/3months all 0 → hidden, only all.
        let conn = seed_with_added_at(&[("Old", NOW_2024_01_01 - 5 * 365 * DAY)]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01)).unwrap();
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(!ids.contains(&"cat:recent:day"));
        assert!(!ids.contains(&"cat:recent:week"));
        // year has data; all is always shown.
        assert!(ids.contains(&"cat:recent:all"));
    }

    #[test]
    fn rt3_root_lists_distinct_years_descending() {
        let conn = seed_with_added_at(&[
            ("Y2023", unix_year_start(2023) + DAY),
            ("Y2024", unix_year_start(2024) + DAY),
            ("Y2025", unix_year_start(2025) + DAY),
        ]);
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01)).unwrap();
        let year_titles: Vec<&str> = r
            .didl
            .containers
            .iter()
            .filter(|c| c.id.starts_with("cat:recent:year:"))
            .map(|c| c.title.as_str())
            .collect();
        // DESC
        assert_eq!(year_titles, vec!["2025", "2024", "2023"]);
    }

    #[test]
    fn rt4_root_empty_library_returns_only_all() {
        let conn = open_in_memory_migrated();
        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01)).unwrap();
        assert_eq!(r.didl.containers.len(), 1);
        assert_eq!(r.didl.containers[0].id, "cat:recent:all");
    }

    #[test]
    fn rt5_root_existing_album_with_new_track_resurfaces() {
        // Adding a new track to an existing album updates MAX(added_at), so it
        // appears under day (SPEC §6.4 "resurface"). After denormalization, this
        // also exercises that recalc_last_added_at is wired up.
        let conn = seed_with_added_at(&[("Old", NOW_2024_01_01 - 30 * DAY)]);
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
        // Mimic the scan recalc (in production this runs in scan/mod.rs's post-tx).
        albums::recalc_last_added_at(&conn).unwrap();

        let r = recent_root_children(&ctx_at(&conn, NOW_2024_01_01)).unwrap();
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"cat:recent:day"));
    }

    #[test]
    fn rb_year_unix_year_start_matches_1970_baseline() {
        assert_eq!(unix_year_start(1970), 0);
        // 2024-01-01 is 1704067200 (UTC).
        assert_eq!(unix_year_start(2024), 1704067200);
        // 1969 is -365 days (not a leap year).
        assert_eq!(unix_year_start(1969), -365 * DAY);
    }
}

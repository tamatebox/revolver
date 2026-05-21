//! Album container Browse (BrowseMetadata / BrowseDirectChildren).
//! Aggregates direct `albums` table reads with album-list queries under
//! Album Artist / Artist / Genre.

use rusqlite::params;

use super::{single, BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::Container;
use crate::upnp::object_id::{self, ObjectId};

/// BrowseMetadata (`alb:{id}`). Returns the single album container.
pub fn album_metadata(ctx: &BrowseContext, album_id: i64) -> Result<DidlOutput> {
    let (album, eff_aa, track_count): (String, String, i64) = ctx.conn.query_row(
        "SELECT album, effective_album_artist, track_count FROM albums WHERE id = ?1",
        params![album_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(single(Container {
        id: format!("alb:{album_id}"),
        parent_id: "cat:al".to_string(),
        title: album,
        upnp_class: "object.container.album.musicAlbum",
        child_count: Some(track_count),
        artist: Some(eff_aa),
        album_art_uri: Some(format!("{}/{}", ctx.art_base_url, album_id)),
    }))
}

/// Under `aa:{name}`: album list filtered by Album Artist.
pub fn albums_by_aa_children(
    ctx: &BrowseContext,
    aa_name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums WHERE effective_album_artist = ?1",
        params![aa_name],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT id, album, track_count FROM albums
         WHERE effective_album_artist = ?1 ORDER BY album LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, String, i64)> = stmt
        .query_map(params![aa_name, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&ObjectId::AlbumArtist(aa_name.to_string()));
    let containers = rows
        .into_iter()
        .map(|(id, album, tc)| album_container(ctx, id, &album, aa_name, tc, &parent_id))
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

/// Under `ar:{name}`: album list filtered by track artist.
///
/// perf §P1: built as a semi-join (`WHERE EXISTS`) instead of `DISTINCT JOIN`.
/// Avoids intermediate row blowup for large artists like Various Artists
/// (`tracks(artist)` index works under EXISTS, no `DISTINCT` sort dedup needed).
pub fn albums_by_artist_children(
    ctx: &BrowseContext,
    artist_name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.artist = ?1)",
        params![artist_name],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.artist = ?1)
         ORDER BY a.album LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![artist_name, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&ObjectId::Artist(artist_name.to_string()));
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

/// Under `gn:{name}`: album list filtered by track genre. perf §P1: same as artist,
/// built as a `WHERE EXISTS` semi-join (avoids row blowup, uses `tracks(genre)` index).
pub fn albums_by_genre_children(
    ctx: &BrowseContext,
    genre_name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.genre = ?1)",
        params![genre_name],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.genre = ?1)
         ORDER BY a.album LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![genre_name, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&ObjectId::Genre(genre_name.to_string()));
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

/// #9: Under `cm:{name}` / `cn:{name}` / `pf:{name}` — album list filtered by
/// the classical facet column. Mirrors `albums_by_artist_children` and uses a
/// `WHERE EXISTS` semi-join for the same perf reasons.
pub fn albums_by_composer_children(
    ctx: &BrowseContext,
    name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_facet_children(ctx, "composer", "cm", name, start, count, |s| {
        ObjectId::Composer(s)
    })
}

pub fn albums_by_conductor_children(
    ctx: &BrowseContext,
    name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_facet_children(ctx, "conductor", "cn", name, start, count, |s| {
        ObjectId::Conductor(s)
    })
}

pub fn albums_by_performer_children(
    ctx: &BrowseContext,
    name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_facet_children(ctx, "performer", "pf", name, start, count, |s| {
        ObjectId::Performer(s)
    })
}

/// #2: Under `yr:{YYYY}` — albums with at least one track in that year.
/// Same `WHERE EXISTS` shape as `albums_by_genre_children`, using
/// `tracks(year)` index.
pub fn albums_by_year_children(
    ctx: &BrowseContext,
    year: i32,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.year = ?1)",
        params![year],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.year = ?1)
         ORDER BY a.album LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![year, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&ObjectId::Year(year));
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

/// Under `gn:` (UnknownGenre) — albums where every track has NULL / empty
/// `genre`. Mirrors `albums_by_genre_children` shape but flips the EXISTS
/// to NOT EXISTS so a single tagged track is enough to exclude the album.
pub fn albums_by_unknown_genre_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_unknown_facet_children(
        ctx,
        "t.genre IS NOT NULL AND t.genre != ''",
        ObjectId::UnknownGenre,
        start,
        count,
    )
}

/// Under `yr:0` (UnknownYear) — albums where no track has a populated `year`.
pub fn albums_by_unknown_year_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_unknown_facet_children(
        ctx,
        "t.year IS NOT NULL",
        ObjectId::UnknownYear,
        start,
        count,
    )
}

/// Under `dec:0` (UnknownDecade) — same source as Unknown Year (decade is
/// derived from `year`). Kept as a distinct parent so the breadcrumb under
/// `cat:dec` stays intuitive.
pub fn albums_by_unknown_decade_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    albums_by_unknown_facet_children(
        ctx,
        "t.year IS NOT NULL",
        ObjectId::UnknownDecade,
        start,
        count,
    )
}

/// Shared body for the three Unknown-bucket album lists.
///
/// `tagged_pred` is the SQL predicate that recognizes a *tagged* track for the
/// facet being filtered (e.g. `"t.genre IS NOT NULL AND t.genre != ''"`).
/// The query selects albums where **no** track matches that predicate,
/// i.e. the album is fully untagged for that facet.
fn albums_by_unknown_facet_children(
    ctx: &BrowseContext,
    tagged_pred: &str,
    parent: ObjectId,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let count_sql = format!(
        "SELECT COUNT(*) FROM albums a
         WHERE NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND {tagged_pred})"
    );
    let total: i64 = ctx.conn.query_row(&count_sql, [], |r| r.get(0))?;
    let list_sql = format!(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE NOT EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND {tagged_pred})
         ORDER BY a.album LIMIT ?1 OFFSET ?2"
    );
    let mut stmt = ctx.conn.prepare_cached(&list_sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&parent);
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

/// #2: Under `dec:{YYYY}` — albums released in the 10-year window
/// `[decade, decade+9]`. The caller is responsible for passing a
/// decade-aligned year (already enforced by `ObjectId::parse`).
pub fn albums_by_decade_children(
    ctx: &BrowseContext,
    decade: i32,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let lo = decade;
    let hi = decade + 9;
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id
                         AND t.year BETWEEN ?1 AND ?2)",
        params![lo, hi],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id
                         AND t.year BETWEEN ?1 AND ?2)
         ORDER BY a.album LIMIT ?3 OFFSET ?4",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![lo, hi, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&ObjectId::Decade(decade));
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

fn albums_by_facet_children(
    ctx: &BrowseContext,
    column: &'static str,
    _prefix: &'static str,
    name: &str,
    start: usize,
    count: usize,
    parent_id_builder: impl Fn(String) -> ObjectId,
) -> Result<ChildrenResult> {
    // `column` is a caller-controlled literal ("composer" / "conductor" / "performer");
    // never user input — same SQL-injection guard as `categories::facet_children`.
    let total: i64 = ctx.conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM albums a
             WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.{col} = ?1)",
            col = column
        ),
        params![name],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(&format!(
        "SELECT a.id, a.album, a.effective_album_artist, a.track_count
         FROM albums a
         WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.{col} = ?1)
         ORDER BY a.album LIMIT ?2 OFFSET ?3",
        col = column
    ))?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![name, count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let parent_id = object_id::encode(&parent_id_builder(name.to_string()));
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, &parent_id))
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

/// Helper that builds a single album container (shared by `cat:al` and each facet).
pub(crate) fn album_container(
    ctx: &BrowseContext,
    album_id: i64,
    album: &str,
    eff_aa: &str,
    track_count: i64,
    parent: &str,
) -> Container {
    Container {
        id: format!("alb:{album_id}"),
        parent_id: parent.to_string(),
        title: album.to_string(),
        upnp_class: "object.container.album.musicAlbum",
        child_count: Some(track_count),
        artist: Some(eff_aa.to_string()),
        album_art_uri: Some(format!("{}/{}", ctx.art_base_url, album_id)),
    }
}

#[cfg(test)]
mod unknown_bucket_tests {
    use super::*;
    use rusqlite::Connection;

    use crate::browse::test_helpers::{default_ctx, default_track_row, open_in_memory_migrated};
    use crate::db::{albums as db_albums, tracks as db_tracks};
    use crate::random::RandomState;
    use crate::state::BrowseSettings;

    /// 3 albums:
    /// - "Mixed"     → 2 tracks, one tagged with genre + year, one untagged
    /// - "AllTagged" → 1 track with genre + year
    /// - "AllUntagged" → 1 track with neither
    ///
    /// "Mixed" must NOT land in the Unknown bucket because at least one track
    /// carries the tag. Only "AllUntagged" should appear there.
    fn seed_three_albums() -> Connection {
        let conn = open_in_memory_migrated();
        #[allow(clippy::type_complexity)]
        let cases: &[(&str, Vec<(&str, Option<&str>, Option<i32>)>)] = &[
            (
                "Mixed",
                vec![
                    ("/m/mix1.flac", Some("Rock"), Some(1985)),
                    ("/m/mix2.flac", None, None),
                ],
            ),
            ("AllTagged", vec![("/m/t.flac", Some("Jazz"), Some(1970))]),
            ("AllUntagged", vec![("/m/u.flac", None, None)]),
        ];
        for (album, tracks) in cases {
            let aid = db_albums::upsert(
                &conn,
                &db_albums::AlbumKey {
                    effective_album_artist: "AA",
                    album,
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap();
            for (path, genre, year) in tracks {
                let mut row = default_track_row(aid, path, 0);
                row.genre = *genre;
                row.year = *year;
                db_tracks::upsert(&conn, &row).unwrap();
            }
        }
        db_albums::recalc_counts(&conn).unwrap();
        conn
    }

    #[test]
    fn ub_alb1_unknown_genre_contains_only_fully_untagged_albums() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let ctx = default_ctx(&conn, &rs, &s, 0);
        let r = albums_by_unknown_genre_children(&ctx, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["AllUntagged"]);
        assert_eq!(r.didl.containers[0].parent_id, "gn:");
    }

    #[test]
    fn ub_alb2_unknown_year_uses_same_rule_against_year_column() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let ctx = default_ctx(&conn, &rs, &s, 0);
        let r = albums_by_unknown_year_children(&ctx, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "AllUntagged");
        assert_eq!(r.didl.containers[0].parent_id, "yr:0");
    }

    #[test]
    fn ub_alb3_unknown_decade_is_separate_parent_with_same_filter() {
        let conn = seed_three_albums();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let ctx = default_ctx(&conn, &rs, &s, 0);
        let r = albums_by_unknown_decade_children(&ctx, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "AllUntagged");
        assert_eq!(r.didl.containers[0].parent_id, "dec:0");
    }
}

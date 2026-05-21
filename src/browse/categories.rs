//! Browse for the 4 top-level facets (`cat:aa` / `cat:ar` / `cat:al` / `cat:gn`)
//! and the Root container. Enumerates DISTINCT values under each facet.

use rusqlite::params;

use super::albums::album_container;
use super::{BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::Container;
use crate::upnp::object_id::{self, ObjectId};

/// Children of Root (`"0"`): selection + order driven by
/// `browse.top_level` (#8, SPEC §6.2).
///
/// Iteration order follows the configured array. For each entry:
/// - Unknown IDs are silently dropped (forward-compat with future facets).
/// - `cat:cm` / `cat:cn` / `cat:pf` are dropped when the library has no
///   rows in the corresponding column (keeps the root clean on
///   non-classical libraries — #9).
/// - Duplicates after the first occurrence are dropped.
pub fn root_children(ctx: &BrowseContext) -> ChildrenResult {
    let mut containers = Vec::with_capacity(ctx.settings.top_level.len());
    let mut seen = std::collections::HashSet::new();
    for id in &ctx.settings.top_level {
        if !seen.insert(id.as_str()) {
            continue;
        }
        if let Some(c) = build_root_facet(ctx, id) {
            containers.push(c);
        }
    }
    let total = containers.len();
    ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
            nodes: vec![],
        },
        total_matches: total,
    }
}

/// Builds one root-level facet container. Returns `None` if the ID is unknown
/// or the facet is currently disabled (a classical / year column with no
/// populated rows).
#[rustfmt::skip]
fn build_root_facet(ctx: &BrowseContext, id: &str) -> Option<Container> {
    // #2: cat:yr / cat:dec self-hide when zero tracks carry a release year,
    // and the classical facets (cm / cn / pf) self-hide on libraries with
    // no rows in the corresponding column (#9).
    match id {
        "cat:aa"     => Some(cat_with_icon(ctx, "cat:aa",     "0", "Album Artist",    "aa")),
        "cat:ar"     => Some(cat_with_icon(ctx, "cat:ar",     "0", "Artist",          "ar")),
        "cat:al"     => Some(cat_with_icon(ctx, "cat:al",     "0", "Album",           "al")),
        "cat:gn"     => Some(cat_with_icon(ctx, "cat:gn",     "0", "Genre",           "gn")),
        "cat:recent" => Some(cat_with_icon(ctx, "cat:recent", "0", "Recently Added",  "recent")),
        "cat:played" => Some(cat_with_icon(ctx, "cat:played", "0", "Recently Played", "played")),
        "cat:random" => Some(cat_with_icon(ctx, "cat:random", "0", "Random Albums",   "random")),
        "cat:hires"  => Some(cat_with_icon(ctx, "cat:hires",  "0", "Hi-Res Albums",   "hires")),
        "cat:lossy"  => Some(cat_with_icon(ctx, "cat:lossy",  "0", "Lossy Albums",    "lossy")),
        "cat:mixed"  => Some(cat_with_icon(ctx, "cat:mixed",  "0", "Mixed Quality",   "mixed")),
        "cat:cm"  if facet_has_any(ctx, "composer" ).unwrap_or(false)
            => Some(cat_with_icon(ctx, "cat:cm",  "0", "Composer",  "cm")),
        "cat:cn"  if facet_has_any(ctx, "conductor").unwrap_or(false)
            => Some(cat_with_icon(ctx, "cat:cn",  "0", "Conductor", "cn")),
        "cat:pf"  if facet_has_any(ctx, "performer").unwrap_or(false)
            => Some(cat_with_icon(ctx, "cat:pf",  "0", "Performer", "pf")),
        "cat:yr"  if facet_has_any(ctx, "year"     ).unwrap_or(false)
            => Some(cat_with_icon(ctx, "cat:yr",  "0", "Year",      "yr")),
        "cat:dec" if facet_has_any(ctx, "year"     ).unwrap_or(false)
            => Some(cat_with_icon(ctx, "cat:dec", "0", "Decade",    "dec")),
        _ => None,
    }
}

/// Under `cat:aa`: DISTINCT effective_album_artist.
pub fn album_artists_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT effective_album_artist) FROM albums",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT DISTINCT effective_album_artist FROM albums
         ORDER BY effective_album_artist LIMIT ?1 OFFSET ?2",
    )?;
    let names: Vec<String> = stmt
        .query_map(params![count as i64, start as i64], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let containers = names
        .into_iter()
        .map(|name| person_container(&ObjectId::AlbumArtist(name.clone()), "cat:aa", &name))
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

/// Under `cat:ar`: DISTINCT track artist.
pub fn artists_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT artist) FROM tracks WHERE artist IS NOT NULL AND artist != ''",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT DISTINCT artist FROM tracks
         WHERE artist IS NOT NULL AND artist != ''
         ORDER BY artist LIMIT ?1 OFFSET ?2",
    )?;
    let names: Vec<String> = stmt
        .query_map(params![count as i64, start as i64], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let containers = names
        .into_iter()
        .map(|name| person_container(&ObjectId::Artist(name.clone()), "cat:ar", &name))
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

/// Under `cat:al`: full album list.
pub fn albums_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let total: i64 = ctx
        .conn
        .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT id, album, effective_album_artist, track_count
         FROM albums ORDER BY album LIMIT ?1 OFFSET ?2",
    )?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(params![count as i64, start as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let containers = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, "cat:al"))
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

/// #9: Under `cat:cm` / `cat:cn` / `cat:pf` — DISTINCT composer / conductor /
/// performer. Mirrors `artists_children`.
pub fn composers_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    facet_children(ctx, "composer", "cat:cm", start, count, |name| {
        person_container(&ObjectId::Composer(name.clone()), "cat:cm", &name)
    })
}

pub fn conductors_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    facet_children(ctx, "conductor", "cat:cn", start, count, |name| {
        person_container(&ObjectId::Conductor(name.clone()), "cat:cn", &name)
    })
}

pub fn performers_children(
    ctx: &BrowseContext,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    facet_children(ctx, "performer", "cat:pf", start, count, |name| {
        person_container(&ObjectId::Performer(name.clone()), "cat:pf", &name)
    })
}

/// Generic "DISTINCT $column FROM tracks WHERE $column IS NOT NULL" enumerator
/// for the #9 classical facets. `column` must be a literal identifier (caller
/// passes a hard-coded string; never user input — guards against SQL injection).
fn facet_children(
    ctx: &BrowseContext,
    column: &'static str,
    _parent_id: &'static str,
    start: usize,
    count: usize,
    make: impl Fn(String) -> Container,
) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        &format!(
            "SELECT COUNT(DISTINCT {col}) FROM tracks
             WHERE {col} IS NOT NULL AND {col} != ''",
            col = column
        ),
        [],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(&format!(
        "SELECT DISTINCT {col} FROM tracks
         WHERE {col} IS NOT NULL AND {col} != ''
         ORDER BY {col} COLLATE NOCASE LIMIT ?1 OFFSET ?2",
        col = column
    ))?;
    let names: Vec<String> = stmt
        .query_map(params![count as i64, start as i64], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let containers: Vec<Container> = names.into_iter().map(make).collect();
    Ok(ChildrenResult {
        didl: DidlOutput {
            containers,
            items: vec![],
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

/// Returns `true` if at least one row has `tracks.{column}` populated.
/// Used by `root_children` to hide empty classical facets from non-classical
/// libraries. Same SQL-injection guard as `facet_children`: `column` is a
/// caller-controlled literal.
fn facet_has_any(ctx: &BrowseContext, column: &'static str) -> Result<bool> {
    let any: i64 = ctx.conn.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM tracks
             WHERE {col} IS NOT NULL AND {col} != '')",
            col = column
        ),
        [],
        |r| r.get(0),
    )?;
    Ok(any != 0)
}

/// #2: Under `cat:yr` — DISTINCT release year as `Year` containers, newest first.
///
/// Appends an `UnknownYear` bucket at the end when at least one album has
/// **no track with a populated year**. The bucket sits at virtual index
/// `sorted_total` so pagination math has to know it.
pub fn years_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let sorted_total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT year) FROM tracks WHERE year IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let unknown_exists = exists_album_with_all_tracks_missing(ctx, "year")?;
    let total = (sorted_total + i64::from(unknown_exists)) as usize;

    let real_count = take_count(start, count, sorted_total as usize);
    let mut years: Vec<i32> = Vec::new();
    if real_count > 0 {
        let mut stmt = ctx.conn.prepare_cached(
            "SELECT DISTINCT year FROM tracks
             WHERE year IS NOT NULL
             ORDER BY year DESC LIMIT ?1 OFFSET ?2",
        )?;
        years = stmt
            .query_map(params![real_count as i64, start as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
    }
    let mut containers: Vec<Container> = years
        .into_iter()
        .map(|y| year_container(&ObjectId::Year(y), "cat:yr", &y.to_string()))
        .collect();
    if unknown_exists
        && containers.len() < count
        && start + containers.len() == sorted_total as usize
    {
        containers.push(year_container(
            &ObjectId::UnknownYear,
            "cat:yr",
            "Unknown Year",
        ));
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

/// #2: Under `cat:dec` — DISTINCT 10-year buckets, newest first. Buckets are
/// computed as `(year / 10) * 10` so 1985 → 1980. Negative years cannot occur
/// (the tag parser rejects them at scan time). Unknown bucket gating mirrors
/// `cat:yr` (same source column).
pub fn decades_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let sorted_total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT (year/10)*10) FROM tracks WHERE year IS NOT NULL",
        [],
        |r| r.get(0),
    )?;
    let unknown_exists = exists_album_with_all_tracks_missing(ctx, "year")?;
    let total = (sorted_total + i64::from(unknown_exists)) as usize;

    let real_count = take_count(start, count, sorted_total as usize);
    let mut decades: Vec<i32> = Vec::new();
    if real_count > 0 {
        let mut stmt = ctx.conn.prepare_cached(
            "SELECT DISTINCT (year/10)*10 AS d FROM tracks
             WHERE year IS NOT NULL
             ORDER BY d DESC LIMIT ?1 OFFSET ?2",
        )?;
        decades = stmt
            .query_map(params![real_count as i64, start as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
    }
    let mut containers: Vec<Container> = decades
        .into_iter()
        .map(|d| year_container(&ObjectId::Decade(d), "cat:dec", &format!("{d}s")))
        .collect();
    if unknown_exists
        && containers.len() < count
        && start + containers.len() == sorted_total as usize
    {
        containers.push(year_container(
            &ObjectId::UnknownDecade,
            "cat:dec",
            "Unknown Decade",
        ));
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

/// Under `cat:gn`: DISTINCT track genre, with an `UnknownGenre` bucket
/// appended at the tail when at least one album has no genre on any track.
pub fn genres_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let sorted_total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT genre) FROM tracks WHERE genre IS NOT NULL AND genre != ''",
        [],
        |r| r.get(0),
    )?;
    let unknown_exists = exists_album_with_all_tracks_missing(ctx, "genre")?;
    let total = (sorted_total + i64::from(unknown_exists)) as usize;

    let real_count = take_count(start, count, sorted_total as usize);
    let mut names: Vec<String> = Vec::new();
    if real_count > 0 {
        let mut stmt = ctx.conn.prepare_cached(
            "SELECT DISTINCT genre FROM tracks
             WHERE genre IS NOT NULL AND genre != ''
             ORDER BY genre LIMIT ?1 OFFSET ?2",
        )?;
        names = stmt
            .query_map(params![real_count as i64, start as i64], |r| r.get(0))?
            .filter_map(|r| r.ok())
            .collect();
    }
    let mut containers: Vec<Container> = names
        .into_iter()
        .map(|name| genre_container(&ObjectId::Genre(name.clone()), "cat:gn", &name))
        .collect();
    if unknown_exists
        && containers.len() < count
        && start + containers.len() == sorted_total as usize
    {
        containers.push(genre_container(
            &ObjectId::UnknownGenre,
            "cat:gn",
            "Unknown Genre",
        ));
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

/// True when at least one album has zero tracks with `tracks.{column}` populated
/// (all tracks for the album have NULL / empty in that column). Drives the
/// "Unknown" bucket gate for cat:gn / cat:yr / cat:dec.
///
/// `column` is a caller-controlled literal (`"genre"` / `"year"`); the
/// dynamic SQL is safe (no user input).
fn exists_album_with_all_tracks_missing(ctx: &BrowseContext, column: &'static str) -> Result<bool> {
    // Year is INTEGER (no empty-string sentinel); genre / others are TEXT and
    // also need the != '' guard so an explicitly-empty tag counts as missing.
    let empty_pred = if column == "year" {
        "t.year IS NOT NULL".to_string()
    } else {
        format!("t.{column} IS NOT NULL AND t.{column} != ''")
    };
    let sql = format!(
        "SELECT EXISTS (
           SELECT 1 FROM albums a
           WHERE NOT EXISTS (
             SELECT 1 FROM tracks t
             WHERE t.album_id = a.id AND {empty_pred}
           )
         )"
    );
    let any: i64 = ctx.conn.query_row(&sql, [], |r| r.get(0))?;
    Ok(any != 0)
}

/// How many rows of the sorted (real) enumeration to fetch given a Browse
/// page `[start, start+count)` and `sorted_total` real rows. Returns 0 when
/// the page is entirely past the real-row range (the Unknown bucket may
/// still be appended by the caller).
fn take_count(start: usize, count: usize, sorted_total: usize) -> usize {
    if start >= sorted_total {
        0
    } else {
        count.min(sorted_total - start)
    }
}

// ── Container builder helpers ────────────────────────────────────────────

pub(crate) fn root_container(ctx: &BrowseContext) -> Container {
    // childCount is informational; recompute from the same top_level pipeline
    // so it stays consistent with what a follow-up DirectChildren returns.
    let count = root_children(ctx).total_matches as i64;
    Container {
        id: "0".to_string(),
        parent_id: "-1".to_string(),
        title: "Music Library".to_string(),
        upnp_class: "object.container",
        child_count: Some(count),
        artist: None,
        album_art_uri: None,
    }
}

pub(crate) fn plain_cat(id: &str, parent: &str, title: &str) -> Container {
    Container {
        id: id.to_string(),
        parent_id: parent.to_string(),
        title: title.to_string(),
        upnp_class: "object.container",
        child_count: None,
        artist: None,
        album_art_uri: None,
    }
}

/// Root facet container with a bespoke icon (#24). `icon_slug` selects the
/// `/icon/cat-{slug}.png` served by [`crate::http::upnp`]; the URL is
/// reconstructed by trimming the trailing `/art` segment off `art_base_url`
/// to reach the host base.
fn cat_with_icon(
    ctx: &BrowseContext,
    id: &str,
    parent: &str,
    title: &str,
    icon_slug: &str,
) -> Container {
    let mut c = plain_cat(id, parent, title);
    let host_base = ctx.art_base_url.trim_end_matches("/art");
    c.album_art_uri = Some(format!("{host_base}/icon/cat/{icon_slug}"));
    c
}

pub(crate) fn person_container(id: &ObjectId, parent: &str, name: &str) -> Container {
    Container {
        id: object_id::encode(id),
        parent_id: parent.to_string(),
        title: name.to_string(),
        upnp_class: "object.container.person.musicArtist",
        child_count: None,
        artist: None,
        album_art_uri: None,
    }
}

pub(crate) fn genre_container(id: &ObjectId, parent: &str, name: &str) -> Container {
    Container {
        id: object_id::encode(id),
        parent_id: parent.to_string(),
        title: name.to_string(),
        upnp_class: "object.container.genre.musicGenre",
        child_count: None,
        artist: None,
        album_art_uri: None,
    }
}

/// #2: container for a year (`yr:YYYY`) or decade (`dec:YYYY`). Plain
/// `object.container` (no canonical UPnP class for year buckets).
pub(crate) fn year_container(id: &ObjectId, parent: &str, label: &str) -> Container {
    Container {
        id: object_id::encode(id),
        parent_id: parent.to_string(),
        title: label.to_string(),
        upnp_class: "object.container",
        child_count: None,
        artist: None,
        album_art_uri: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::random::RandomState;
    use crate::state::BrowseSettings;
    use rusqlite::Connection;

    fn ctx_with<'a>(
        conn: &'a Connection,
        rs: &'a RandomState,
        settings: &'a BrowseSettings,
    ) -> BrowseContext<'a> {
        BrowseContext {
            conn,
            art_base_url: "http://x/art",
            stream_base_url: "http://x/stream",
            random_state: rs,
            now_secs: 0,
            settings,
        }
    }

    #[test]
    fn cr1_root_children_with_defaults_returns_10() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = root_children(&ctx_with(&conn, &rs, &s));
        assert_eq!(r.total_matches, 10);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"cat:hires"));
        assert!(ids.contains(&"cat:lossy"));
        assert!(ids.contains(&"cat:mixed"));
    }

    #[test]
    fn cr2_root_facets_carry_per_facet_icon_album_art_uri() {
        // #24: every surfaced cat:* root facet advertises an
        // `/icon/cat-{slug}.png` URL whose slug matches a registered entry in
        // `crate::upnp::icon::CATEGORY_ICONS`. URLs are reconstructed against
        // the same host as `art_base_url` (the `/art` segment is stripped).
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = root_children(&ctx_with(&conn, &rs, &s));
        for c in &r.didl.containers {
            let uri = c
                .album_art_uri
                .as_deref()
                .unwrap_or_else(|| panic!("{} must carry an albumArtURI", c.id));
            let slug = uri
                .strip_prefix("http://x/icon/cat/")
                .unwrap_or_else(|| panic!("{} URL has unexpected shape: {uri}", c.id));
            assert!(
                crate::upnp::icon::category_icon(slug).is_some(),
                "{} references slug {slug} but it is not in CATEGORY_ICONS",
                c.id
            );
        }
        // Spot-check a couple of slugs we know must be present under defaults.
        let gn = r.didl.containers.iter().find(|c| c.id == "cat:gn").unwrap();
        assert_eq!(gn.album_art_uri.as_deref(), Some("http://x/icon/cat/gn"));
        let recent = r
            .didl
            .containers
            .iter()
            .find(|c| c.id == "cat:recent")
            .unwrap();
        assert_eq!(
            recent.album_art_uri.as_deref(),
            Some("http://x/icon/cat/recent")
        );
    }

    #[test]
    fn cr3_top_level_order_drives_root() {
        // Issue #8: array order is the order surfaced by Browse.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            Some(50),
            None,
            Some(100),
            vec![
                "cat:recent".into(),
                "cat:al".into(),
                "cat:aa".into(),
                "cat:played".into(),
            ],
        );
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat:recent", "cat:al", "cat:aa", "cat:played"]);
        assert_eq!(r.total_matches, 4);
    }

    #[test]
    fn cr4_top_level_drops_unknown_and_duplicates() {
        // Unknown IDs ("cat:nope") and duplicates after the first occurrence
        // are silently dropped per issue #8 spec.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            Some(50),
            None,
            Some(100),
            vec![
                "cat:aa".into(),
                "cat:nope".into(),
                "cat:aa".into(), // duplicate
                "cat:played".into(),
            ],
        );
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat:aa", "cat:played"]);
    }

    #[test]
    fn cr5_quality_categories_hidden_via_top_level() {
        // Hi-Res / Lossy / Mixed are surfaced solely by `top_level`. Dropping
        // them from the array hides them at the root.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            Some(50),
            None,
            Some(100),
            vec!["cat:aa".into(), "cat:al".into()],
        );
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat:aa", "cat:al"]);
    }

    #[test]
    fn cr6_classical_facets_self_hide_when_empty() {
        // On a library with no composer/conductor/performer rows,
        // cat:cm / cat:cn / cat:pf are dropped even when listed.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            Some(50),
            None,
            Some(100),
            vec![
                "cat:aa".into(),
                "cat:cm".into(),
                "cat:cn".into(),
                "cat:pf".into(),
            ],
        );
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["cat:aa"]);
    }

    #[test]
    fn cr7_empty_top_level_returns_no_children() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(Some(50), None, Some(100), vec![]);
        let r = root_children(&ctx_with(&conn, &rs, &s));
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }

    // ── #2: Year / Decade facets ────────────────────────────────────────

    fn seed_year_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let aid = crate::db::albums::upsert(
            &conn,
            &crate::db::albums::AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        // 1969 + 1985 + 1987 → years [1987, 1985, 1969]; decades [1980, 1960].
        for (path, year) in [
            ("/m/a.flac", 1969),
            ("/m/b.flac", 1985),
            ("/m/c.flac", 1987),
        ] {
            let mut row = crate::browse::test_helpers::default_track_row(aid, path, 0);
            row.year = Some(year);
            crate::db::tracks::upsert(&conn, &row).unwrap();
        }
        crate::db::albums::recalc_counts(&conn).unwrap();
        conn
    }

    #[test]
    fn yr1_years_children_returns_distinct_years_desc() {
        let conn = seed_year_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = years_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["1987", "1985", "1969"]);
        // IDs round-trip through ObjectId.
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["yr:1987", "yr:1985", "yr:1969"]);
    }

    #[test]
    fn yr2_decades_children_returns_distinct_decades_desc() {
        let conn = seed_year_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = decades_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        assert_eq!(r.total_matches, 2);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["1980s", "1960s"]);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, vec!["dec:1980", "dec:1960"]);
    }

    #[test]
    fn yr3_root_self_hides_year_and_decade_when_empty() {
        // No tracks → cat:yr / cat:dec must be silently dropped from root.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(
            !ids.contains(&"cat:yr"),
            "empty year column must hide cat:yr"
        );
        assert!(
            !ids.contains(&"cat:dec"),
            "empty year column must hide cat:dec"
        );
    }

    #[test]
    fn yr4_root_surfaces_year_and_decade_when_any_track_has_year() {
        let conn = seed_year_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = root_children(&ctx_with(&conn, &rs, &s));
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"cat:yr"));
        assert!(ids.contains(&"cat:dec"));
    }

    // ── Unknown bucket tail (cat:gn / cat:yr / cat:dec) ──────────────────

    /// Two albums:
    /// - Album "Tagged" → 1 track with genre="Rock", year=1985
    /// - Album "Untagged" → 1 track with NULL/empty genre + NULL year
    ///
    /// Genre enumeration has 1 real ("Rock") + Unknown Genre at tail;
    /// year enumeration has 1 real (1985) + Unknown Year at tail.
    fn seed_mixed_tagged_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        for (album, path, genre, year) in [
            ("Tagged", "/m/t.flac", Some("Rock"), Some(1985)),
            ("Untagged", "/m/u.flac", None, None),
        ] {
            let aid = crate::db::albums::upsert(
                &conn,
                &crate::db::albums::AlbumKey {
                    effective_album_artist: "AA",
                    album,
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap();
            let mut row = crate::browse::test_helpers::default_track_row(aid, path, 0);
            row.genre = genre;
            row.year = year;
            crate::db::tracks::upsert(&conn, &row).unwrap();
        }
        crate::db::albums::recalc_counts(&conn).unwrap();
        conn
    }

    #[test]
    fn ub1_genre_enumeration_appends_unknown_when_any_album_untagged() {
        let conn = seed_mixed_tagged_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = genres_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        // 1 real ("Rock") + Unknown.
        assert_eq!(r.total_matches, 2);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["Rock", "Unknown Genre"]);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        // Unknown encodes as `gn:` (empty suffix), distinguishing it from any base64'd name.
        assert_eq!(ids[1], "gn:");
    }

    #[test]
    fn ub2_year_enumeration_appends_unknown_when_any_album_yearless() {
        let conn = seed_mixed_tagged_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = years_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        assert_eq!(r.total_matches, 2);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["1985", "Unknown Year"]);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[1], "yr:0");
    }

    #[test]
    fn ub3_decade_enumeration_appends_unknown_when_any_album_yearless() {
        let conn = seed_mixed_tagged_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = decades_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        assert_eq!(r.total_matches, 2);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["1980s", "Unknown Decade"]);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[1], "dec:0");
    }

    #[test]
    fn ub4_no_unknown_bucket_when_all_albums_tagged() {
        // seed_year_db: every track has both year and (the default) NULL genre.
        // No album is fully missing year → Unknown Year must not appear.
        let conn = seed_year_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let r = years_children(&ctx_with(&conn, &rs, &s), 0, 100).unwrap();
        assert_eq!(r.total_matches, 3); // 1969, 1985, 1987 — no Unknown.
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(!ids.contains(&"yr:0"));
    }

    #[test]
    fn ub5_unknown_bucket_lands_on_correct_page() {
        // Page size 1 over (1 real + 1 unknown): page 0 returns real, page 1 returns Unknown.
        let conn = seed_mixed_tagged_db();
        let rs = RandomState::new();
        let s = BrowseSettings::default();
        let p0 = genres_children(&ctx_with(&conn, &rs, &s), 0, 1).unwrap();
        assert_eq!(p0.total_matches, 2);
        assert_eq!(p0.didl.containers.len(), 1);
        assert_eq!(p0.didl.containers[0].title, "Rock");

        let p1 = genres_children(&ctx_with(&conn, &rs, &s), 1, 1).unwrap();
        assert_eq!(p1.total_matches, 2);
        assert_eq!(p1.didl.containers.len(), 1);
        assert_eq!(p1.didl.containers[0].title, "Unknown Genre");

        // start past the cap returns empty rows but total stays 2.
        let p2 = genres_children(&ctx_with(&conn, &rs, &s), 2, 1).unwrap();
        assert_eq!(p2.total_matches, 2);
        assert!(p2.didl.containers.is_empty());
    }
}

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
/// - `cat:hires` / `cat:lossy` / `cat:mixed` are dropped when
///   `browse.quality_categories = false`.
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
/// or the facet is currently disabled (`quality_categories=false`, or a
/// classical column with no populated rows).
fn build_root_facet(ctx: &BrowseContext, id: &str) -> Option<Container> {
    match id {
        "cat:aa" => Some(plain_cat("cat:aa", "0", "Album Artist")),
        "cat:ar" => Some(plain_cat("cat:ar", "0", "Artist")),
        "cat:al" => Some(plain_cat("cat:al", "0", "Album")),
        "cat:gn" => Some(plain_cat("cat:gn", "0", "Genre")),
        "cat:recent" => Some(plain_cat("cat:recent", "0", "Recently Added")),
        "cat:played" => Some(plain_cat("cat:played", "0", "Recently Played")),
        "cat:random" => Some(plain_cat("cat:random", "0", "Random Albums")),
        "cat:hires" if ctx.settings.quality_categories => {
            Some(plain_cat("cat:hires", "0", "Hi-Res Albums"))
        }
        "cat:lossy" if ctx.settings.quality_categories => {
            Some(plain_cat("cat:lossy", "0", "Lossy Albums"))
        }
        "cat:mixed" if ctx.settings.quality_categories => {
            Some(plain_cat("cat:mixed", "0", "Mixed Quality"))
        }
        "cat:cm" if facet_has_any(ctx, "composer").unwrap_or(false) => {
            Some(plain_cat("cat:cm", "0", "Composer"))
        }
        "cat:cn" if facet_has_any(ctx, "conductor").unwrap_or(false) => {
            Some(plain_cat("cat:cn", "0", "Conductor"))
        }
        "cat:pf" if facet_has_any(ctx, "performer").unwrap_or(false) => {
            Some(plain_cat("cat:pf", "0", "Performer"))
        }
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

/// Under `cat:gn`: DISTINCT track genre.
pub fn genres_children(ctx: &BrowseContext, start: usize, count: usize) -> Result<ChildrenResult> {
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(DISTINCT genre) FROM tracks WHERE genre IS NOT NULL AND genre != ''",
        [],
        |r| r.get(0),
    )?;
    let mut stmt = ctx.conn.prepare_cached(
        "SELECT DISTINCT genre FROM tracks
         WHERE genre IS NOT NULL AND genre != ''
         ORDER BY genre LIMIT ?1 OFFSET ?2",
    )?;
    let names: Vec<String> = stmt
        .query_map(params![count as i64, start as i64], |r| r.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    let containers = names
        .into_iter()
        .map(|name| genre_container(&ObjectId::Genre(name.clone()), "cat:gn", &name))
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
    fn cr1_root_children_with_quality_default_returns_10() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::default(); // quality_categories = true
        let r = root_children(&ctx_with(&conn, &rs, &s));
        assert_eq!(r.total_matches, 10);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.contains(&"cat:hires"));
        assert!(ids.contains(&"cat:lossy"));
        assert!(ids.contains(&"cat:mixed"));
    }

    #[test]
    fn cr2_root_children_with_quality_off_omits_quality_categories() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            50,
            None,
            100,
            false, // quality_categories = false
            crate::config::default_top_level(),
        );
        let r = root_children(&ctx_with(&conn, &rs, &s));
        assert_eq!(r.total_matches, 7);
        let ids: Vec<&str> = r.didl.containers.iter().map(|c| c.id.as_str()).collect();
        assert!(!ids.contains(&"cat:hires"));
        assert!(!ids.contains(&"cat:lossy"));
        assert!(!ids.contains(&"cat:mixed"));
        // The remaining 7 are required.
        for expected in [
            "cat:aa",
            "cat:ar",
            "cat:al",
            "cat:gn",
            "cat:recent",
            "cat:played",
            "cat:random",
        ] {
            assert!(ids.contains(&expected), "missing {}", expected);
        }
    }

    #[test]
    fn cr3_top_level_order_drives_root() {
        // Issue #8: array order is the order surfaced by Browse.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            50,
            None,
            100,
            true,
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
            50,
            None,
            100,
            true,
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
    fn cr5_quality_categories_false_overrides_top_level() {
        // When quality_categories = false, hi-res / lossy / mixed must not
        // appear even if explicitly listed in top_level.
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let rs = RandomState::new();
        let s = BrowseSettings::from_parts(
            50,
            None,
            100,
            false, // quality_categories = false
            vec![
                "cat:aa".into(),
                "cat:hires".into(),
                "cat:lossy".into(),
                "cat:mixed".into(),
                "cat:al".into(),
            ],
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
            50,
            None,
            100,
            true,
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
        let s = BrowseSettings::from_parts(50, None, 100, true, vec![]);
        let r = root_children(&ctx_with(&conn, &rs, &s));
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
    }
}

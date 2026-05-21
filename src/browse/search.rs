//! Search action implementation (SPEC §5.4).
//!
//! Parses the `SearchCriteria` upstream in [`crate::upnp::search`], then
//! routes the work here based on the extracted `ClassFilter`:
//!
//! - `ClassFilter::Album`  → search `albums.album` (+ optional artist/genre
//!   OR branches from a Track-style criteria when it ever lands here),
//!   return album containers (`alb:{id}`).
//! - `ClassFilter::Artist` → search `albums.effective_album_artist` (distinct),
//!   return `aa:{base64}` containers.
//! - `ClassFilter::Track` / `Any` → search `tracks` with any combination of
//!   title / album / artist / genre predicates, return track items.
//!
//! All comparisons are `LIKE '%X%' COLLATE NOCASE`. Linn's role attribute
//! (`upnp:artist[@role="Composer"]`) is parsed but ignored at the SQL layer
//! until #9 lands a `composer` column.

use rusqlite::types::Value as SqlValue;

use super::albums::album_container;
use super::tracks::{build_track_item, TrackRow};
use super::{BrowseContext, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::{Container, Item};
use crate::upnp::object_id::{self, ObjectId};
use crate::upnp::search::{ClassFilter, Predicate, Property, SearchExpr};

pub struct SearchResult {
    pub didl: DidlOutput,
    pub total_matches: usize,
}

pub fn search_tracks(
    ctx: &BrowseContext,
    expr: &SearchExpr,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    if expr.is_no_op() {
        return Ok(empty());
    }
    match expr.class {
        ClassFilter::Album => search_albums(ctx, &expr.predicate, start, count),
        ClassFilter::Artist => search_artists(ctx, &expr.predicate, start, count),
        ClassFilter::Track | ClassFilter::Any => {
            search_track_items(ctx, &expr.predicate, start, count)
        }
    }
}

fn empty() -> SearchResult {
    SearchResult {
        didl: DidlOutput {
            containers: vec![],
            items: vec![],
        },
        total_matches: 0,
    }
}

// ── Album search ──────────────────────────────────────────────────────────

fn search_albums(
    ctx: &BrowseContext,
    predicate: &Predicate,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    // For Album-class searches Linn always sends `dc:title contains "X"`,
    // but we also accept upnp:album / upnp:artist / upnp:genre as forgiving
    // fallbacks — `dc:title` against an album container means the album name.
    let where_clause = predicate_to_sql_albums(predicate);
    if where_clause.is_empty() {
        return Ok(empty());
    }

    let total: i64 = {
        let sql = format!("SELECT COUNT(*) FROM albums WHERE {}", where_clause.sql);
        ctx.conn.query_row(
            &sql,
            rusqlite::params_from_iter(&where_clause.params),
            |r| r.get(0),
        )?
    };

    let mut list_params = where_clause.params.clone();
    list_params.push(SqlValue::from(count as i64));
    list_params.push(SqlValue::from(start as i64));
    let sql = format!(
        "SELECT id, album, effective_album_artist, track_count
         FROM albums
         WHERE {}
         ORDER BY album COLLATE NOCASE
         LIMIT ?{lim} OFFSET ?{off}",
        where_clause.sql,
        lim = where_clause.params.len() + 1,
        off = where_clause.params.len() + 2,
    );
    let mut stmt = ctx.conn.prepare_cached(&sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(rusqlite::params_from_iter(&list_params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers: Vec<Container> = rows
        .into_iter()
        .map(|(id, album, aa, tc)| album_container(ctx, id, &album, &aa, tc, "cat:al"))
        .collect();
    Ok(SearchResult {
        didl: DidlOutput {
            containers,
            items: vec![],
        },
        total_matches: total as usize,
    })
}

fn predicate_to_sql_albums(p: &Predicate) -> WhereClause {
    // Map title→album, album→album, artist→effective_album_artist. Genre is
    // a track-level attribute on revolver's schema and doesn't appear on
    // albums; if it shows up here we drop that branch.
    walk(p, &|prop| match prop {
        Property::Title | Property::Album => Some("album"),
        Property::Artist => Some("effective_album_artist"),
        Property::Genre => None,
    })
}

// ── Artist search ─────────────────────────────────────────────────────────

fn search_artists(
    ctx: &BrowseContext,
    predicate: &Predicate,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    // For Artist-class searches Linn sends `dc:title contains "X"` — meaning
    // the artist name. Map title→effective_album_artist. We also accept
    // upnp:artist for the same reason.
    let where_clause = walk(predicate, &|prop| match prop {
        Property::Title | Property::Artist => Some("effective_album_artist"),
        _ => None,
    });
    if where_clause.is_empty() {
        return Ok(empty());
    }

    let total: i64 = {
        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT DISTINCT effective_album_artist FROM albums WHERE {})",
            where_clause.sql
        );
        ctx.conn.query_row(
            &sql,
            rusqlite::params_from_iter(&where_clause.params),
            |r| r.get(0),
        )?
    };

    let mut list_params = where_clause.params.clone();
    list_params.push(SqlValue::from(count as i64));
    list_params.push(SqlValue::from(start as i64));
    let sql = format!(
        "SELECT DISTINCT effective_album_artist
         FROM albums
         WHERE {}
         ORDER BY effective_album_artist COLLATE NOCASE
         LIMIT ?{lim} OFFSET ?{off}",
        where_clause.sql,
        lim = where_clause.params.len() + 1,
        off = where_clause.params.len() + 2,
    );
    let mut stmt = ctx.conn.prepare_cached(&sql)?;
    let names: Vec<String> = stmt
        .query_map(rusqlite::params_from_iter(&list_params), |r| {
            r.get::<_, String>(0)
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers: Vec<Container> = names
        .into_iter()
        .map(|name| {
            let id = ObjectId::AlbumArtist(name.clone());
            Container {
                id: object_id::encode(&id),
                parent_id: "cat:aa".to_string(),
                title: name,
                upnp_class: "object.container.person.musicArtist",
                child_count: None,
                artist: None,
                album_art_uri: None,
            }
        })
        .collect();
    Ok(SearchResult {
        didl: DidlOutput {
            containers,
            items: vec![],
        },
        total_matches: total as usize,
    })
}

// ── Track search ──────────────────────────────────────────────────────────

fn search_track_items(
    ctx: &BrowseContext,
    predicate: &Predicate,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    let where_clause = walk(predicate, &|prop| match prop {
        Property::Title => Some("t.title"),
        Property::Album => Some("a.album"),
        Property::Artist => Some("t.artist"),
        Property::Genre => Some("t.genre"),
    });
    if where_clause.is_empty() {
        return Ok(empty());
    }

    let total: i64 = {
        let sql = format!(
            "SELECT COUNT(*) FROM tracks t JOIN albums a ON t.album_id = a.id WHERE {}",
            where_clause.sql
        );
        ctx.conn.query_row(
            &sql,
            rusqlite::params_from_iter(&where_clause.params),
            |r| r.get(0),
        )?
    };

    let mut list_params = where_clause.params.clone();
    list_params.push(SqlValue::from(count as i64));
    list_params.push(SqlValue::from(start as i64));
    let sql = format!(
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                (SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = t.album_id) > 1
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE {}
         ORDER BY t.title COLLATE NOCASE
         LIMIT ?{lim} OFFSET ?{off}",
        where_clause.sql,
        lim = where_clause.params.len() + 1,
        off = where_clause.params.len() + 2,
    );
    let mut stmt = ctx.conn.prepare_cached(&sql)?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(rusqlite::params_from_iter(&list_params), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                TrackRow {
                    album_id: r.get(1)?,
                    title: r.get(2)?,
                    artist: r.get(3)?,
                    genre: r.get(4)?,
                    track_num: r.get(5)?,
                    disc_num: r.get(6)?,
                    duration_ms: r.get(7)?,
                    sample_rate: r.get(8)?,
                    bit_depth: r.get(9)?,
                    channels: r.get(10)?,
                    bitrate: r.get(11)?,
                    mime_type: r.get(12)?,
                    file_size: r.get(13)?,
                    album: r.get(14)?,
                    multi_disc: r.get::<_, i64>(15)? != 0,
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

// ── Predicate → SQL ───────────────────────────────────────────────────────

#[derive(Default)]
struct WhereClause {
    sql: String,
    params: Vec<SqlValue>,
}

impl WhereClause {
    fn is_empty(&self) -> bool {
        self.sql.is_empty()
    }
}

/// Walk a predicate tree, mapping each `Property` to a SQL column via
/// `column_for`. Returns the WHERE-clause SQL plus its positional params.
/// `column_for` returning `None` drops that leaf (e.g. genre on the albums
/// table); the surrounding AND/OR is simplified accordingly.
fn walk(p: &Predicate, column_for: &dyn Fn(&Property) -> Option<&'static str>) -> WhereClause {
    let mut w = WhereClause::default();
    walk_inner(p, column_for, &mut w);
    w
}

fn walk_inner(
    p: &Predicate,
    column_for: &dyn Fn(&Property) -> Option<&'static str>,
    out: &mut WhereClause,
) {
    match p {
        Predicate::Contains { prop, value, .. } => {
            if let Some(col) = column_for(prop) {
                let placeholder = out.params.len() + 1;
                out.sql
                    .push_str(&format!("{} LIKE ?{} COLLATE NOCASE", col, placeholder));
                out.params.push(SqlValue::from(format!("%{}%", value)));
            }
        }
        Predicate::And(children) => emit_join(children, "AND", column_for, out),
        Predicate::Or(children) => emit_join(children, "OR", column_for, out),
        Predicate::DerivedFrom(_) | Predicate::True => {
            // Should have been stripped / collapsed before reaching here.
        }
    }
}

fn emit_join(
    children: &[Predicate],
    sep: &str,
    column_for: &dyn Fn(&Property) -> Option<&'static str>,
    out: &mut WhereClause,
) {
    let mut parts: Vec<(String, Vec<SqlValue>)> = Vec::with_capacity(children.len());
    for c in children {
        let mut sub = WhereClause::default();
        // Re-base placeholders during a second pass after we know the final
        // ordering; for now collect each child's raw fragments and renumber
        // below.
        walk_with_local_indices(c, column_for, &mut sub);
        if !sub.is_empty() {
            parts.push((sub.sql, sub.params));
        }
    }
    if parts.is_empty() {
        return;
    }
    let mut renumbered = Vec::with_capacity(parts.len());
    for (sub_sql, sub_params) in parts {
        let start = out.params.len();
        // Append params first so we know the final placeholder indices.
        for v in &sub_params {
            out.params.push(v.clone());
        }
        let renum = renumber_placeholders(&sub_sql, start);
        renumbered.push(renum);
    }
    out.sql
        .push_str(&format!("({})", renumbered.join(&format!(" {} ", sep))));
}

/// Like `walk_inner` but writes placeholders starting at index 1 with no
/// awareness of the outer context. Used during the two-pass build in
/// `emit_join` so we can renumber later.
fn walk_with_local_indices(
    p: &Predicate,
    column_for: &dyn Fn(&Property) -> Option<&'static str>,
    out: &mut WhereClause,
) {
    walk_inner(p, column_for, out);
}

/// Shift every `?N` placeholder in `sql` by `base`. Naive but adequate —
/// our SQL never contains a literal `?`.
fn renumber_placeholders(sql: &str, base: usize) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 4);
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'?' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            let n: usize = sql[i + 1..j].parse().unwrap_or(0);
            out.push_str(&format!("?{}", n + base));
            i = j;
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db::{albums, schema, tracks};
    use crate::upnp::search::parse_criteria;

    /// Seed of 3 tracks / 2 albums: Beatles' Abbey Road (2 tracks) + Various
    /// Artists' Hits (1 track with artist "Some Singer", genre "Rock").
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

    // ── Album class (Linn's Album field) ──────────────────────────────────

    #[test]
    fn st1_album_class_finds_album_by_title() {
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Abbey""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers.len(), 1);
        assert!(r.didl.items.is_empty());
        assert_eq!(r.didl.containers[0].title, "Abbey Road");
        // Album containers carry the right ObjectID prefix and upnp class.
        assert!(r.didl.containers[0].id.starts_with("alb:"));
        assert_eq!(
            r.didl.containers[0].upnp_class,
            "object.container.album.musicAlbum"
        );
    }

    // ── Artist class (Linn's Artist field) ────────────────────────────────

    #[test]
    fn st2_artist_class_finds_album_artist_by_title() {
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Beatles""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "The Beatles");
        assert!(r.didl.containers[0].id.starts_with("aa:"));
        assert_eq!(
            r.didl.containers[0].upnp_class,
            "object.container.person.musicArtist"
        );
    }

    #[test]
    fn st2b_artist_class_returns_distinct_artists() {
        let conn = seed_db();
        // Both "The Beatles" and "Various Artists" exist; partial "ar" matches
        // "Various Artists" (substring of "Artists") not the Beatles row.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Various""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Various Artists");
    }

    // ── Track class with OR composition (Linn's Track / global field) ─────

    #[test]
    fn st3_track_class_or_matches_any_field() {
        let conn = seed_db();
        // "Some Singer" matches via the artist OR branch only.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and ( dc:title contains "Some Singer" or upnp:album contains "Some Singer" or upnp:artist contains "Some Singer" or upnp:genre contains "Some Singer" )"#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.items.len(), 1);
        assert!(r.didl.containers.is_empty());
        assert_eq!(r.didl.items[0].title, "VA Track");
    }

    #[test]
    fn st3b_track_class_or_matches_track_title() {
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and ( dc:title contains "Together" or upnp:album contains "Together" or upnp:artist contains "Together" or upnp:genre contains "Together" )"#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.items[0].title, "Come Together");
    }

    #[test]
    fn st3c_track_class_or_matches_genre() {
        let conn = seed_db();
        // All seeded tracks have genre = "Rock", so a genre-only hit returns all 3.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and ( dc:title contains "Rock" or upnp:album contains "Rock" or upnp:artist contains "Rock" or upnp:genre contains "Rock" )"#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
    }

    // ── Composer (role attribute — parsed, currently behaves like artist) ─

    #[test]
    fn st4_composer_role_attribute_falls_through_to_artist_search() {
        let conn = seed_db();
        // role="Composer" is parsed and stored on the Predicate, but the SQL
        // layer ignores it (no composer column yet). The search behaves as a
        // plain artist search. The Various Artists row matches.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and upnp:artist[@role="Composer"] contains "Various""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Various Artists");
    }

    // ── No-op / empty ─────────────────────────────────────────────────────

    #[test]
    fn st5_wildcard_returns_empty_didl() {
        let conn = seed_db();
        let e = parse_criteria("*");
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
        assert!(r.didl.items.is_empty());
    }

    #[test]
    fn st6_derivedfrom_only_returns_empty() {
        let conn = seed_db();
        let e = parse_criteria(r#"upnp:class derivedfrom "object.item.audioItem""#);
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        // No usable predicate → empty (we don't list all tracks just because
        // a class was specified).
        assert_eq!(r.total_matches, 0);
    }

    // ── Pagination still works ────────────────────────────────────────────

    #[test]
    fn st7_pagination_offset_count() {
        let conn = seed_db();
        // 2 Abbey Road tracks; offset=1, limit=1 returns one.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and ( dc:title contains "Together" or upnp:album contains "Abbey" or upnp:artist contains "zzz" or upnp:genre contains "zzz" )"#,
        );
        let r = search_tracks(&ctx(&conn), &e, 1, 1).unwrap();
        // Both Abbey Road tracks match via upnp:album OR branch; total = 2.
        assert_eq!(r.total_matches, 2);
        assert_eq!(r.didl.items.len(), 1);
    }
}

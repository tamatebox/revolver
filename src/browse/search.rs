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
//! Comparisons use `LIKE '%X%'` against NFKD-folded shadow columns
//! (`*_norm`, populated at upsert / migrate; see `crate::normalize`). Both
//! the column value and the search input flow through the same
//! [`crate::normalize::for_search`] so accent / halfwidth / hiragana
//! drift folds away (#6). `COLLATE NOCASE` is no longer needed — the
//! normalize step lowercases.
//!
//! Linn's role attribute `upnp:artist[@role="Composer"]` (or `Conductor` /
//! `Performer`) is routed to the matching tracks column (#9). For
//! Artist-class searches the result switches container type to match
//! (`cm:` / `cn:` / `pf:`).

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
            nodes: vec![],
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
         ORDER BY album_norm
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
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

fn predicate_to_sql_albums(p: &Predicate) -> WhereClause {
    // Map title→album_norm, album→album_norm, artist→effective_album_artist_norm.
    // Genre is a track-level attribute on revolver's schema and doesn't appear
    // on albums; if it shows up here we drop that branch.
    walk(p, &|prop, _role| match prop {
        Property::Title | Property::Album => Some("album_norm"),
        Property::Artist => Some("effective_album_artist_norm"),
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
    // #9: if the predicate carries `[@role="Composer"]` (etc.) Linn is asking
    // for that classical facet, not the regular Album Artist list. Route to
    // the appropriate column and return matching cm/cn/pf containers.
    if let Some(role) = first_role(predicate) {
        if let Some((column, prefix, id_builder)) = role_to_column(role) {
            return search_classical_facet(
                ctx, predicate, column, prefix, id_builder, start, count,
            );
        }
    }
    // For Artist-class searches Linn sends `dc:title contains "X"` — meaning
    // the artist name. Map title→effective_album_artist_norm. We also accept
    // upnp:artist for the same reason.
    let where_clause = walk(predicate, &|prop, _role| match prop {
        Property::Title | Property::Artist => Some("effective_album_artist_norm"),
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
         ORDER BY effective_album_artist_norm
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
            nodes: vec![],
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
    // #9: if `upnp:artist[@role="Composer"]` (etc.) appears, route that leaf
    // to the matching tracks column rather than `t.artist`.
    let where_clause = walk(predicate, &|prop, role| match prop {
        Property::Title => Some("t.title_norm"),
        Property::Album => Some("a.album_norm"),
        Property::Artist => match role {
            Some("Composer") => Some("t.composer_norm"),
            Some("Conductor") => Some("t.conductor_norm"),
            Some("Performer") => Some("t.performer_norm"),
            _ => Some("t.artist_norm"),
        },
        Property::Genre => Some("t.genre_norm"),
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
                (SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = t.album_id) > 1,
                t.composer, t.conductor, t.performer
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE {}
         ORDER BY t.title_norm
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
                    composer: r.get(16)?,
                    conductor: r.get(17)?,
                    performer: r.get(18)?,
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
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

// ── #9 helpers: route role-tagged Artist-class searches to composer / etc. ─

/// First non-empty `role` attribute found anywhere in the predicate tree.
/// Used to decide whether an Artist-class search is asking for a classical
/// facet (Composer / Conductor / Performer) instead of Album Artist.
fn first_role(p: &Predicate) -> Option<&str> {
    match p {
        Predicate::Contains { role, .. } => role.as_deref(),
        Predicate::And(children) | Predicate::Or(children) => children.iter().find_map(first_role),
        _ => None,
    }
}

type RoleRoute = (&'static str, &'static str, fn(String) -> ObjectId);

/// `role="Composer"` → (column, parent-cat id, ObjectId constructor). Returns
/// `None` for unrecognized roles (search falls back to the default
/// effective_album_artist path so unknown roles don't break the response).
fn role_to_column(role: &str) -> Option<RoleRoute> {
    match role {
        "Composer" => Some(("composer", "cat:cm", ObjectId::Composer)),
        "Conductor" => Some(("conductor", "cat:cn", ObjectId::Conductor)),
        "Performer" => Some(("performer", "cat:pf", ObjectId::Performer)),
        _ => None,
    }
}

/// #9: search a classical facet column. Mirrors `search_artists` but queries
/// `DISTINCT t.{column} FROM tracks` and returns the facet's container kind.
///
/// LIKE matches go against `{column}_norm` (#6). The raw column is still
/// the source of the container `title` so the user sees the original tag
/// value, not the folded form.
fn search_classical_facet(
    ctx: &BrowseContext,
    predicate: &Predicate,
    column: &'static str,
    parent_cat: &'static str,
    make_id: fn(String) -> ObjectId,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    let norm_col = match_column_norm(column);
    // Accept dc:title and upnp:artist in the predicate; map both to the facet's
    // norm column. role is already known (`first_role`) and not re-checked per leaf.
    let where_clause = walk(predicate, &|prop, _role| match prop {
        Property::Title | Property::Artist => Some(norm_col),
        _ => None,
    });
    if where_clause.is_empty() {
        return Ok(empty());
    }

    let count_sql = format!(
        "SELECT COUNT(*) FROM (SELECT DISTINCT {col} FROM tracks
         WHERE {col} IS NOT NULL AND {col} != '' AND {where_})",
        col = column,
        where_ = where_clause.sql,
    );
    let total: i64 = ctx.conn.query_row(
        &count_sql,
        rusqlite::params_from_iter(&where_clause.params),
        |r| r.get(0),
    )?;

    let mut list_params = where_clause.params.clone();
    list_params.push(SqlValue::from(count as i64));
    list_params.push(SqlValue::from(start as i64));
    let sql = format!(
        "SELECT DISTINCT {col} FROM tracks
         WHERE {col} IS NOT NULL AND {col} != '' AND {where_}
         ORDER BY {norm_col}
         LIMIT ?{lim} OFFSET ?{off}",
        col = column,
        norm_col = norm_col,
        where_ = where_clause.sql,
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
            let id = make_id(name.clone());
            Container {
                id: object_id::encode(&id),
                parent_id: parent_cat.to_string(),
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
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

/// Map the raw facet column to its `*_norm` shadow column (#6). Caller
/// passes a literal so the match is exhaustive.
fn match_column_norm(col: &'static str) -> &'static str {
    match col {
        "composer" => "composer_norm",
        "conductor" => "conductor_norm",
        "performer" => "performer_norm",
        _ => "composer_norm",
    }
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

/// Walk a predicate tree, mapping each `Property` (with its optional `role`
/// attribute) to a SQL column via `column_for`. Returns the WHERE-clause SQL
/// plus its positional params. `column_for` returning `None` drops that leaf
/// (e.g. genre on the albums table); the surrounding AND/OR is simplified
/// accordingly.
///
/// The `role` argument lets Track-class search route
/// `upnp:artist[@role="Composer"]` to the `t.composer` column instead of
/// `t.artist` (#9).
fn walk(
    p: &Predicate,
    column_for: &dyn Fn(&Property, Option<&str>) -> Option<&'static str>,
) -> WhereClause {
    let mut w = WhereClause::default();
    walk_inner(p, column_for, &mut w);
    w
}

fn walk_inner(
    p: &Predicate,
    column_for: &dyn Fn(&Property, Option<&str>) -> Option<&'static str>,
    out: &mut WhereClause,
) {
    match p {
        Predicate::Contains { prop, value, role } => {
            if let Some(col) = column_for(prop, role.as_deref()) {
                // #6: the column is a `*_norm` shadow, so the search value
                // must run through the same pipeline. NOCASE is unnecessary
                // because `for_search` already lowercases.
                let normalized = crate::normalize::for_search(value);
                let placeholder = out.params.len() + 1;
                out.sql.push_str(&format!("{} LIKE ?{}", col, placeholder));
                out.params.push(SqlValue::from(format!("%{}%", normalized)));
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
    column_for: &dyn Fn(&Property, Option<&str>) -> Option<&'static str>,
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
    column_for: &dyn Fn(&Property, Option<&str>) -> Option<&'static str>,
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
        // #9: third track also carries Composer / Conductor / Performer tags so
        // the role-tagged search tests have something to match.
        for (album_id, path, title, artist, composer, conductor, performer) in [
            (
                beatles_id,
                "/m/come_together.flac",
                "Come Together",
                "The Beatles",
                None,
                None,
                None,
            ),
            (
                beatles_id,
                "/m/something.flac",
                "Something",
                "The Beatles",
                None,
                None,
                None,
            ),
            (
                va_id,
                "/m/va_track.mp3",
                "VA Track",
                "Some Singer",
                Some("J.S. Bach"),
                Some("Karajan"),
                Some("Berlin Philharmonic"),
            ),
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
                    composer,
                    conductor,
                    performer,
                    year: None,
                    rg_track_gain: None,
                    rg_track_peak: None,
                    rg_album_gain: None,
                    rg_album_peak: None,
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

    // ── Composer / Conductor / Performer (role attribute, #9) ─────────────

    #[test]
    fn st4_composer_role_returns_composer_container() {
        let conn = seed_db();
        // role="Composer" routes to the composer column. Seeded composer is
        // "J.S. Bach"; substring "Bach" should match.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and upnp:artist[@role="Composer"] contains "Bach""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "J.S. Bach");
        assert!(r.didl.containers[0].id.starts_with("cm:"));
        assert_eq!(r.didl.containers[0].parent_id, "cat:cm");
    }

    #[test]
    fn st4b_conductor_role_returns_conductor_container() {
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and upnp:artist[@role="Conductor"] contains "Karajan""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Karajan");
        assert!(r.didl.containers[0].id.starts_with("cn:"));
    }

    #[test]
    fn st4c_performer_role_returns_performer_container() {
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and upnp:artist[@role="Performer"] contains "Berlin""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Berlin Philharmonic");
        assert!(r.didl.containers[0].id.starts_with("pf:"));
    }

    #[test]
    fn st4d_track_class_role_composer_routes_to_composer_column() {
        let conn = seed_db();
        // Track-class search with role attribute: should hit composer column
        // (matches the seeded "J.S. Bach"), not artist.
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.item.audioItem" and upnp:artist[@role="Composer"] contains "Bach""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.items[0].title, "VA Track");
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

    // ── #6: fuzzy matching (NFKD + halfwidth/fullwidth + katakana→hiragana) ──

    /// Seed of 4 albums with accented / fullwidth / katakana names so the
    /// fuzzy-Search tests have unambiguous targets. Each artist appears on
    /// exactly one album so a hit can be attributed to the fuzzy fold.
    fn seed_fuzzy_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        for (aa, album) in [
            ("Sigur Rós", "Takk..."),         // accent fold: café/cafe family
            ("Björk", "Debut"),               // diaeresis
            ("Ｂｅａｔｌｅｓ", "Ｈｅｌｐ！"), // fullwidth → halfwidth
            ("ミユキ", "ﾌｧｲﾅﾙ"),              // katakana / halfwidth-katakana → hiragana
        ] {
            let aid = albums::upsert(
                &conn,
                &albums::AlbumKey {
                    effective_album_artist: aa,
                    album,
                    compilation: false,
                },
                Some(aa),
                0,
            )
            .unwrap();
            tracks::upsert(
                &conn,
                &tracks::TrackRow {
                    album_id: aid,
                    path: &format!("/m/{aa}-{album}.flac"),
                    title: Some(album),
                    artist: Some(aa),
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
                    added_at: 0,
                    mtime: 0,
                    composer: None,
                    conductor: None,
                    performer: None,
                    year: None,
                    rg_track_gain: None,
                    rg_track_peak: None,
                    rg_album_gain: None,
                    rg_album_peak: None,
                },
            )
            .unwrap();
        }
        albums::recalc_counts(&conn).unwrap();
        conn
    }

    #[test]
    fn fz1_album_search_strips_accents() {
        // User types ASCII "Sigur Ros"; tag is "Sigur Rós" → fuzzy match.
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Takk""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Takk...");
    }

    #[test]
    fn fz2_artist_search_strips_diacritics_either_direction() {
        // Tag "Björk", query "Bjork" — and vice versa.
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Bjork""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Björk");

        // Query with the diacritic also matches the same tag.
        let e2 = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Björk""#,
        );
        let r2 = search_tracks(&ctx(&conn), &e2, 0, 100).unwrap();
        assert_eq!(r2.total_matches, 1);
    }

    #[test]
    fn fz3_fullwidth_search_input_matches_fullwidth_tag() {
        // Tag "Ｂｅａｔｌｅｓ" (fullwidth), query plain ASCII "Beatles".
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Beatles""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Ｂｅａｔｌｅｓ");
    }

    #[test]
    fn fz4_hiragana_search_input_matches_katakana_tag() {
        // Tag "ミユキ" (katakana), query "みゆき" (hiragana) — and the reverse.
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "みゆき""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "ミユキ");

        // Query with katakana matches the same tag.
        let e2 = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "ミユキ""#,
        );
        let r2 = search_tracks(&ctx(&conn), &e2, 0, 100).unwrap();
        assert_eq!(r2.total_matches, 1);
    }

    #[test]
    fn fz5_halfwidth_katakana_search_input_matches_katakana_tag() {
        // Tag album "ﾌｧｲﾅﾙ" (halfwidth katakana), query "ファイナル" (fullwidth).
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "ファイナル""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "ﾌｧｲﾅﾙ");
    }

    #[test]
    fn fz6_case_is_folded_at_query_and_column() {
        // Mixed-case search input matches uppercase tag (legacy COLLATE NOCASE
        // behavior preserved by `for_search` lowercasing both sides).
        let conn = seed_fuzzy_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "TAKK""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
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

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
    // Linn's Album field always sends a single-leaf `dc:title contains "X"`.
    // For that shape we switch to a relevance-ranked ORDER BY (exact album →
    // partial album → artist's own album → compilation guest). Compound
    // predicates (AND / OR, `upnp:album`, …) stay on the simple `album_norm`
    // ascending path because the ranking doesn't generalize cleanly when
    // multiple LIKE values are in play.
    if let Predicate::Contains {
        prop: Property::Title,
        value,
        role: _,
    } = predicate
    {
        return search_albums_ranked(ctx, value, start, count);
    }

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

/// Ranked Album-search for the single `dc:title contains "X"` case. The
/// WHERE clause matches the same 3-way OR as `predicate_to_sql_albums`
/// Title branch; ORDER BY layers a 4-bucket CASE on top so the most
/// "obvious" hits come first:
///
/// | Rank | Match                                                |
/// |------|------------------------------------------------------|
/// | 0    | album name == X (exact, normalized)                  |
/// | 1    | album name contains X                                |
/// | 2    | effective_album_artist contains X (artist's own)     |
/// | 3    | only a track-level `tracks.artist` carries X (comp)  |
///
/// Same `?1` (the `%X%` LIKE value) is referenced from both the WHERE
/// clause and the CASE; `?2` is the exact-match value (no `%`).
fn search_albums_ranked(
    ctx: &BrowseContext,
    value: &str,
    start: usize,
    count: usize,
) -> Result<SearchResult> {
    let norm = crate::normalize::for_search(value);
    let like_pat = format!("%{}%", norm);

    let where_sql = "album_norm LIKE ?1
         OR effective_album_artist_norm LIKE ?1
         OR EXISTS (SELECT 1 FROM tracks WHERE album_id = albums.id AND artist_norm LIKE ?1)";

    let total: i64 = ctx.conn.query_row(
        &format!("SELECT COUNT(*) FROM albums WHERE {where_sql}"),
        rusqlite::params![&like_pat],
        |r| r.get(0),
    )?;

    let sql = format!(
        "SELECT id, album, effective_album_artist, track_count
         FROM albums
         WHERE {where_sql}
         ORDER BY
           CASE
             WHEN album_norm = ?2 THEN 0
             WHEN album_norm LIKE ?1 THEN 1
             WHEN effective_album_artist_norm LIKE ?1 THEN 2
             ELSE 3
           END,
           album_norm
         LIMIT ?3 OFFSET ?4"
    );
    let mut stmt = ctx.conn.prepare_cached(&sql)?;
    let rows: Vec<(i64, String, String, i64)> = stmt
        .query_map(
            rusqlite::params![&like_pat, &norm, count as i64, start as i64],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?
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
    // Map title→(album_norm OR effective_album_artist_norm OR EXISTS tracks.artist_norm);
    // album→album_norm; artist→effective_album_artist_norm. Genre is a
    // track-level attribute on revolver's schema and doesn't appear on
    // albums; if it shows up here we drop that branch.
    //
    // The Title fan-out (#21) is what makes "type an artist name into the
    // Album field" find both the artist's own albums and compilations where
    // they appear only at the track level. `upnp:album` stays album-only so
    // explicit album-name predicates from non-Linn clients aren't widened.
    walk(p, &|prop, _role| match prop {
        Property::Title => &[
            "album_norm LIKE ?",
            "effective_album_artist_norm LIKE ?",
            "EXISTS (SELECT 1 FROM tracks WHERE album_id = albums.id AND artist_norm LIKE ?)",
        ],
        Property::Album => &["album_norm LIKE ?"],
        Property::Artist => &["effective_album_artist_norm LIKE ?"],
        Property::Genre => &[],
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
    // the artist name. #22: search the UNION of album_artist (curated, from
    // `cat:aa`) and track-level artist (noisy, from `cat:ar`) so guests on
    // compilations are discoverable by Search even though they only live in
    // `tracks.artist`. Hits coming from albums emit `aa:{X}`; track-only
    // hits emit `ar:{X}` (whose Browse handler already exists). Names
    // present in both columns are deduped to a single `aa:` container —
    // album_artist wins because it's the curated identity.
    let where_aa = walk(predicate, &|prop, _role| match prop {
        Property::Title | Property::Artist => &["effective_album_artist_norm LIKE ?"],
        _ => &[],
    });
    let where_tr = walk(predicate, &|prop, _role| match prop {
        Property::Title | Property::Artist => &["artist_norm LIKE ?"],
        _ => &[],
    });
    if where_aa.is_empty() && where_tr.is_empty() {
        return Ok(empty());
    }

    let n_aa = where_aa.params.len();
    let where_tr_shifted = renumber_placeholders(&where_tr.sql, n_aa);
    let mut params: Vec<SqlValue> = Vec::with_capacity(n_aa + where_tr.params.len() + 2);
    params.extend(where_aa.params.iter().cloned());
    params.extend(where_tr.params.iter().cloned());

    let total: i64 = {
        let sql = format!(
            "SELECT COUNT(*) FROM (
               SELECT effective_album_artist AS name FROM albums WHERE {aa_where}
               UNION
               SELECT artist AS name FROM tracks
                 WHERE artist IS NOT NULL AND artist != '' AND {tr_where}
             )",
            aa_where = where_aa.sql,
            tr_where = where_tr_shifted,
        );
        ctx.conn
            .query_row(&sql, rusqlite::params_from_iter(&params), |r| r.get(0))?
    };

    // UNION ALL + GROUP BY so we can keep `is_aa` per row and decide the
    // container kind. MIN(name_norm) is identical across rows with the same
    // name, so it's a stable sort key.
    let lim_idx = params.len() + 1;
    let off_idx = params.len() + 2;
    let sql = format!(
        "SELECT name, MAX(is_aa) AS is_aa
         FROM (
           SELECT effective_album_artist AS name,
                  effective_album_artist_norm AS name_norm,
                  1 AS is_aa
             FROM albums WHERE {aa_where}
           UNION ALL
           SELECT artist AS name, artist_norm AS name_norm, 0 AS is_aa
             FROM tracks
             WHERE artist IS NOT NULL AND artist != '' AND {tr_where}
         )
         GROUP BY name
         ORDER BY MIN(name_norm)
         LIMIT ?{lim} OFFSET ?{off}",
        aa_where = where_aa.sql,
        tr_where = where_tr_shifted,
        lim = lim_idx,
        off = off_idx,
    );
    params.push(SqlValue::from(count as i64));
    params.push(SqlValue::from(start as i64));

    let mut stmt = ctx.conn.prepare_cached(&sql)?;
    let rows: Vec<(String, i64)> = stmt
        .query_map(rusqlite::params_from_iter(&params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();

    let containers: Vec<Container> = rows
        .into_iter()
        .map(|(name, is_aa)| {
            let (id, parent) = if is_aa == 1 {
                (ObjectId::AlbumArtist(name.clone()), "cat:aa")
            } else {
                (ObjectId::Artist(name.clone()), "cat:ar")
            };
            Container {
                id: object_id::encode(&id),
                parent_id: parent.to_string(),
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
        Property::Title => &["t.title_norm LIKE ?"],
        Property::Album => &["a.album_norm LIKE ?"],
        Property::Artist => match role {
            Some("Composer") => &["t.composer_norm LIKE ?"],
            Some("Conductor") => &["t.conductor_norm LIKE ?"],
            Some("Performer") => &["t.performer_norm LIKE ?"],
            _ => &["t.artist_norm LIKE ?"],
        },
        Property::Genre => &["t.genre_norm LIKE ?"],
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
    let (norm_col, template): (&'static str, &'static [&'static str]) = match column {
        "composer" => ("composer_norm", &["composer_norm LIKE ?"]),
        "conductor" => ("conductor_norm", &["conductor_norm LIKE ?"]),
        "performer" => ("performer_norm", &["performer_norm LIKE ?"]),
        _ => ("composer_norm", &["composer_norm LIKE ?"]),
    };
    // Accept dc:title and upnp:artist in the predicate; map both to the facet's
    // norm column. role is already known (`first_role`) and not re-checked per leaf.
    let where_clause = walk(predicate, &|prop, _role| match prop {
        Property::Title | Property::Artist => template,
        _ => &[],
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
/// attribute) to one or more SQL match-expression templates via `expr_for`.
/// Each template carries exactly one `?` placeholder that the leaf substitutes
/// with the actual positional index. Returns the WHERE-clause SQL plus its
/// params. An empty slice from `expr_for` drops that leaf (e.g. genre on the
/// albums table); the surrounding AND/OR is simplified accordingly. Multiple
/// templates from one leaf are OR-combined and share a single placeholder
/// (and therefore one param). This is how the Album-class `dc:title` predicate
/// fans out across `album_norm`, `effective_album_artist_norm`, and the
/// `EXISTS (tracks.artist_norm)` subquery.
///
/// The `role` argument lets Track-class search route
/// `upnp:artist[@role="Composer"]` to the `t.composer` column instead of
/// `t.artist` (#9).
fn walk(
    p: &Predicate,
    expr_for: &dyn Fn(&Property, Option<&str>) -> &'static [&'static str],
) -> WhereClause {
    let mut w = WhereClause::default();
    walk_inner(p, expr_for, &mut w);
    w
}

fn walk_inner(
    p: &Predicate,
    expr_for: &dyn Fn(&Property, Option<&str>) -> &'static [&'static str],
    out: &mut WhereClause,
) {
    match p {
        Predicate::Contains { prop, value, role } => {
            let templates = expr_for(prop, role.as_deref());
            if templates.is_empty() {
                return;
            }
            // #6: the column is a `*_norm` shadow, so the search value must
            // run through the same pipeline. NOCASE is unnecessary because
            // `for_search` already lowercases.
            let normalized = crate::normalize::for_search(value);
            let placeholder = out.params.len() + 1;
            out.params.push(SqlValue::from(format!("%{}%", normalized)));
            let placeholder_str = format!("?{}", placeholder);
            let parts: Vec<String> = templates
                .iter()
                .map(|t| t.replacen('?', &placeholder_str, 1))
                .collect();
            if parts.len() == 1 {
                out.sql.push_str(&parts[0]);
            } else {
                out.sql.push_str(&format!("({})", parts.join(" OR ")));
            }
        }
        Predicate::And(children) => emit_join(children, "AND", expr_for, out),
        Predicate::Or(children) => emit_join(children, "OR", expr_for, out),
        Predicate::DerivedFrom(_) | Predicate::True => {
            // Should have been stripped / collapsed before reaching here.
        }
    }
}

fn emit_join(
    children: &[Predicate],
    sep: &str,
    expr_for: &dyn Fn(&Property, Option<&str>) -> &'static [&'static str],
    out: &mut WhereClause,
) {
    let mut parts: Vec<(String, Vec<SqlValue>)> = Vec::with_capacity(children.len());
    for c in children {
        let mut sub = WhereClause::default();
        // Re-base placeholders during a second pass after we know the final
        // ordering; for now collect each child's raw fragments and renumber
        // below.
        walk_with_local_indices(c, expr_for, &mut sub);
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
    expr_for: &dyn Fn(&Property, Option<&str>) -> &'static [&'static str],
    out: &mut WhereClause,
) {
    walk_inner(p, expr_for, out);
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
                    artist_sort: None,
                    album_artist_sort: None,
                    album_sort: None,
                    title_sort: None,
                    composer_sort: None,
                    original_year: None,
                    mb_recording_id: None,
                    mb_release_id: None,
                    mb_release_group_id: None,
                    mb_artist_id: None,
                    mb_release_artist_id: None,
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

    // #21: Album-class `dc:title` also fans out to `effective_album_artist`
    // and (via EXISTS) `tracks.artist`. Typing an artist name into Linn's
    // Album field should reveal both the artist's own albums and any
    // compilation where they appear only at the track level.

    #[test]
    fn st1b_album_class_title_matches_album_artist() {
        // "Beatles" appears nowhere in album titles but is the album_artist
        // of Abbey Road. With the #21 fan-out the album shows up.
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Beatles""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Abbey Road");
    }

    #[test]
    fn st1c_album_class_title_matches_track_level_artist_via_exists() {
        // "Some Singer" is a track-level artist on the VA compilation but is
        // not an album_artist anywhere. The EXISTS branch surfaces the comp.
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Some Singer""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Hits");
    }

    #[test]
    fn st1d_album_class_upnp_album_stays_album_name_only() {
        // Regression guard: `upnp:album` is *album name* per UPnP, so it
        // must not pick up artist matches. "Beatles" is the album_artist
        // but not an album title — this should be zero.
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and upnp:album contains "Beatles""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
    }

    #[test]
    fn st1e_album_class_dc_title_orders_by_rank_buckets() {
        // 4 albums covering each rank bucket of the ranked Album-search path:
        //   rank 0 — album name == "Foo" exactly
        //   rank 1 — album name contains "Foo" (partial)
        //   rank 2 — album_artist contains "Foo" (artist's own record)
        //   rank 3 — only a track-level artist carries "Foo" (compilation)
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        crate::db::schema::migrate(&conn).unwrap();
        let mk_album = |aa: &str, album: &str, comp: bool| -> i64 {
            crate::db::albums::upsert(
                &conn,
                &crate::db::albums::AlbumKey {
                    effective_album_artist: aa,
                    album,
                    compilation: comp,
                },
                Some(aa),
                0,
            )
            .unwrap()
        };
        let exact_id = mk_album("Other Artist", "Foo", false);
        let partial_id = mk_album("Other Artist", "Foo Extra", false);
        let artist_id = mk_album("Foo Person", "Solo", false);
        let comp_id = mk_album("Various Artists", "Mix Tape", true);
        // Each album needs at least one track. Compilation track artist is "Foo".
        for (album_id, path, track_artist) in [
            (exact_id, "/m/exact.flac", "Other Artist"),
            (partial_id, "/m/partial.flac", "Other Artist"),
            (artist_id, "/m/artist.flac", "Foo Person"),
            (comp_id, "/m/comp.flac", "Foo"),
        ] {
            crate::db::tracks::upsert(
                &conn,
                &crate::db::tracks::TrackRow {
                    album_id,
                    path,
                    title: Some("t"),
                    artist: Some(track_artist),
                    genre: None,
                    track_num: Some(1),
                    disc_num: Some(1),
                    duration_ms: Some(1),
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    channels: Some(2),
                    bitrate: Some(1),
                    codec: "flac",
                    mime_type: "audio/flac",
                    file_size: 1,
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
                    artist_sort: None,
                    album_artist_sort: None,
                    album_sort: None,
                    title_sort: None,
                    composer_sort: None,
                    original_year: None,
                    mb_recording_id: None,
                    mb_release_id: None,
                    mb_release_group_id: None,
                    mb_artist_id: None,
                    mb_release_artist_id: None,
                },
            )
            .unwrap();
        }
        crate::db::albums::recalc_counts(&conn).unwrap();

        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Foo""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 4);
        let titles: Vec<&str> = r.didl.containers.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, vec!["Foo", "Foo Extra", "Solo", "Mix Tape"]);
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

    // #22: Artist-class search also hits track-level `tracks.artist`. A guest
    // who never appears as an album_artist still surfaces — as an `ar:`
    // container so its existing Browse handler (`albums_by_artist_children`)
    // takes over from there.

    #[test]
    fn st2c_artist_class_finds_track_only_artist_as_ar_container() {
        // "Some Singer" is a track-level artist on the VA compilation, never
        // an album_artist. Pre-#22 this returned 0 results.
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Some Singer""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert_eq!(r.didl.containers[0].title, "Some Singer");
        assert!(r.didl.containers[0].id.starts_with("ar:"));
        assert_eq!(r.didl.containers[0].parent_id, "cat:ar");
    }

    #[test]
    fn st2d_artist_class_dedupes_album_and_track_artist_to_aa() {
        // "The Beatles" appears as both an album_artist (Abbey Road) and
        // every Abbey Road track's artist. UNION + GROUP BY collapses to one
        // row; MAX(is_aa) routes it to `aa:`, not `ar:`.
        let conn = seed_db();
        let e = parse_criteria(
            r#"upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "Beatles""#,
        );
        let r = search_tracks(&ctx(&conn), &e, 0, 100).unwrap();
        assert_eq!(r.total_matches, 1);
        assert!(r.didl.containers[0].id.starts_with("aa:"));
        assert_eq!(r.didl.containers[0].parent_id, "cat:aa");
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
                    artist_sort: None,
                    album_artist_sort: None,
                    album_sort: None,
                    title_sort: None,
                    composer_sort: None,
                    original_year: None,
                    mb_recording_id: None,
                    mb_release_id: None,
                    mb_release_group_id: None,
                    mb_artist_id: None,
                    mb_release_artist_id: None,
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

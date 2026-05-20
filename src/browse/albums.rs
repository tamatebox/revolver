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

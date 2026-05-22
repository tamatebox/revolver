//! `at:{name}` Browse — "All tracks by X" flat shortcut (#23).
//!
//! Listed as the first child of `aa:{X}` / `ar:{X}` when at least one track
//! row carries `artist_norm = for_search(X)`. Browsing it returns track items
//! across every album where X appears at the track level (so e.g. a guest
//! on a Various Artists comp is reachable without descending into the comp
//! and skipping past unrelated tracks).
//!
//! The match is **exact** on the normalized column (`= ?`), not `LIKE`, so
//! the shortcut surfaces the same exact-name set on both the `aa:` and `ar:`
//! sides — partial matches are a Search concern, not a Browse one.

use rusqlite::params;

use super::tracks::{build_track_item, TrackRow};
use super::{single, BrowseContext, ChildrenResult, DidlOutput};
use crate::error::Result;
use crate::upnp::didl::{Container, Item};
use crate::upnp::object_id::{self, ObjectId};

/// BrowseMetadata (`at:{name}`). Returns the synthetic "All tracks (N)"
/// container. `parent_id` resolves to `aa:{X}` when X exists as an
/// album_artist, otherwise `ar:{X}`. (Linn rarely queries metadata on
/// children, so this is mostly defensive — `browse_children` is the hot path.)
pub fn artist_tracks_metadata(ctx: &BrowseContext, artist_name: &str) -> Result<DidlOutput> {
    let norm = crate::normalize::for_search(artist_name);
    let track_count: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE artist_norm = ?1",
        params![norm],
        |r| r.get(0),
    )?;
    let parent_id = parent_for(ctx, artist_name)?;
    Ok(single(build_at_container(
        ctx,
        artist_name,
        track_count,
        parent_id,
    )))
}

/// BrowseDirectChildren (`at:{name}`). Returns the flat track list for X
/// ordered by album / disc / track.
pub fn artist_tracks_children(
    ctx: &BrowseContext,
    artist_name: &str,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    let norm = crate::normalize::for_search(artist_name);
    let total: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE artist_norm = ?1",
        params![norm],
        |r| r.get(0),
    )?;

    let mut stmt = ctx.conn.prepare_cached(
        "SELECT t.id, t.album_id, t.title, t.artist, t.genre, t.track_num, t.disc_num,
                t.duration_ms, t.sample_rate, t.bit_depth, t.channels,
                t.bitrate, t.mime_type, t.file_size, a.album,
                (SELECT IFNULL(MAX(disc_num), 0) FROM tracks WHERE album_id = t.album_id) > 1,
                t.composer, t.conductor, t.performer
         FROM tracks t JOIN albums a ON t.album_id = a.id
         WHERE t.artist_norm = ?1
         ORDER BY a.album_norm, t.disc_num, t.track_num
         LIMIT ?2 OFFSET ?3",
    )?;
    let rows: Vec<(i64, TrackRow)> = stmt
        .query_map(params![norm, count as i64, start as i64], |r| {
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

    Ok(ChildrenResult {
        didl: DidlOutput {
            containers: vec![],
            items,
            nodes: vec![],
        },
        total_matches: total as usize,
    })
}

/// Build the `at:{name}` shortcut container shown inside `aa:{X}` / `ar:{X}`.
/// Public so [`super::albums`] can prepend it before the album list. Carries
/// the bespoke `cat-at` icon (same `/icon/cat/{slug}` mechanism as root
/// facets — see [`super::categories::cat_with_icon`]) so the "All tracks" row
/// is visually distinguishable from the sibling album thumbnails.
pub(crate) fn build_at_container(
    ctx: &BrowseContext,
    name: &str,
    track_count: i64,
    parent_id: String,
) -> Container {
    let host_base = ctx.art_base_url.trim_end_matches("/art");
    Container {
        id: object_id::encode(&ObjectId::ArtistTracks(name.to_string())),
        parent_id,
        title: format!("All tracks ({track_count})"),
        upnp_class: "object.container",
        child_count: Some(track_count),
        artist: None,
        album_art_uri: Some(format!("{host_base}/icon/cat/at")),
    }
}

/// Counts tracks where `tracks.artist_norm = for_search(name)`. Returned so
/// callers can decide whether to emit the shortcut and what `childCount` to
/// advertise. Zero means "X has no track-level rows", in which case no
/// shortcut is shown.
pub(crate) fn track_count_for_artist(ctx: &BrowseContext, name: &str) -> Result<i64> {
    let norm = crate::normalize::for_search(name);
    let n: i64 = ctx.conn.query_row(
        "SELECT COUNT(*) FROM tracks WHERE artist_norm = ?1",
        params![norm],
        |r| r.get(0),
    )?;
    Ok(n)
}

fn parent_for(ctx: &BrowseContext, artist_name: &str) -> Result<String> {
    let exists_as_aa: bool = ctx.conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM albums WHERE effective_album_artist = ?1)",
        params![artist_name],
        |r| r.get::<_, i64>(0).map(|v| v != 0),
    )?;
    let id = if exists_as_aa {
        ObjectId::AlbumArtist(artist_name.to_string())
    } else {
        ObjectId::Artist(artist_name.to_string())
    };
    Ok(object_id::encode(&id))
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db::{albums, schema, tracks as db_tracks};

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

    /// Seed of 3 tracks across 2 albums where "Some Singer" appears on a
    /// compilation track and on a solo album track — exactly the cross-album
    /// case the shortcut exists for.
    fn seed_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();

        let solo_id = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "Some Singer",
                album: "Solo",
                compilation: false,
            },
            Some("Some Singer"),
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

        for (album_id, path, title, artist, track_num) in [
            (solo_id, "/m/solo1.flac", "Solo Song 1", "Some Singer", 1),
            (solo_id, "/m/solo2.flac", "Solo Song 2", "Some Singer", 2),
            (va_id, "/m/va.flac", "Guest Track", "Some Singer", 5),
            (va_id, "/m/other.flac", "Other Artist Song", "Other", 6),
        ] {
            db_tracks::upsert(
                &conn,
                &db_tracks::TrackRow {
                    album_id,
                    path,
                    title: Some(title),
                    artist: Some(artist),
                    genre: Some("Rock"),
                    track_num: Some(track_num),
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
    fn at1_children_returns_only_x_tracks_across_albums() {
        let conn = seed_db();
        let ctx = ctx(&conn);
        let r = artist_tracks_children(&ctx, "Some Singer", 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
        assert_eq!(r.didl.items.len(), 3);
        // The "Other" track on the same compilation must not appear.
        for it in &r.didl.items {
            assert_ne!(it.title, "Other Artist Song");
        }
    }

    #[test]
    fn at2_children_ordered_by_album_then_track() {
        // Solo album sorts before "Hits" (S vs H? actually H sorts before S
        // lexicographically), so the comp track should appear first and the
        // solo tracks after, in track order.
        let conn = seed_db();
        let ctx = ctx(&conn);
        let r = artist_tracks_children(&ctx, "Some Singer", 0, 100).unwrap();
        let titles: Vec<&str> = r.didl.items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles, vec!["Guest Track", "Solo Song 1", "Solo Song 2"]);
    }

    #[test]
    fn at3_metadata_picks_aa_parent_when_album_artist_exists() {
        // "Some Singer" is an album_artist on Solo → parent_id = aa:{...}.
        let conn = seed_db();
        let ctx = ctx(&conn);
        let r = artist_tracks_metadata(&ctx, "Some Singer").unwrap();
        assert_eq!(r.containers.len(), 1);
        assert!(r.containers[0].parent_id.starts_with("aa:"));
        assert_eq!(r.containers[0].title, "All tracks (3)");
        assert_eq!(r.containers[0].child_count, Some(3));
        // bespoke cat-at icon URL is reconstructed off art_base_url.
        assert_eq!(
            r.containers[0].album_art_uri.as_deref(),
            Some("http://x/icon/cat/at")
        );
        assert!(crate::upnp::icon::category_icon("at").is_some());
    }

    #[test]
    fn at3b_metadata_falls_back_to_ar_for_track_only_artist() {
        // "Other" is only a track-level artist — never an album_artist.
        let conn = seed_db();
        let ctx = ctx(&conn);
        let r = artist_tracks_metadata(&ctx, "Other").unwrap();
        assert_eq!(r.containers.len(), 1);
        assert!(r.containers[0].parent_id.starts_with("ar:"));
        assert_eq!(r.containers[0].child_count, Some(1));
    }

    #[test]
    fn at4_track_count_helper_returns_zero_when_no_tracks() {
        let conn = seed_db();
        let ctx = ctx(&conn);
        assert_eq!(track_count_for_artist(&ctx, "Nobody").unwrap(), 0);
        assert_eq!(track_count_for_artist(&ctx, "Some Singer").unwrap(), 3);
    }

    #[test]
    fn at5_children_normalizes_input_for_fuzzy_match() {
        // tag stored as "Some Singer"; query with fullwidth/case variant —
        // for_search normalization on both sides should match.
        let conn = seed_db();
        let ctx = ctx(&conn);
        let r = artist_tracks_children(&ctx, "SOME SINGER", 0, 100).unwrap();
        assert_eq!(r.total_matches, 3);
    }
}

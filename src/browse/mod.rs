//! ObjectID → SQL → DIDL Container/Item mapping (SPEC §6.4).
//! Only `BrowseMetadata` and `BrowseDirectChildren` dispatch live here.
//! Implementation is split into submodules:
//! - [`categories`]: Root / under `cat:*` facets
//! - [`albums`]: `alb:*` metadata + album list under each facet
//! - [`tracks`]: `trk:*` metadata + track list under an album + DIDL Item builder
//! - [`search`]: query for the ContentDirectory `Search` action

use rusqlite::Connection;

use crate::error::Result;
use crate::random::RandomState;
use crate::state::BrowseSettings;
use crate::upnp::didl::Container;
use crate::upnp::object_id::ObjectId;

pub mod albums;
pub mod categories;
pub mod played;
pub mod quality;
pub mod random;
pub mod recent;
pub mod search;
pub mod tracks;

#[cfg(test)]
pub(crate) mod test_helpers;

pub struct DidlOutput {
    pub containers: Vec<Container>,
    pub items: Vec<crate::upnp::didl::Item>,
}

pub struct ChildrenResult {
    pub didl: DidlOutput,
    pub total_matches: usize,
}

pub struct BrowseContext<'a> {
    pub conn: &'a Connection,
    /// e.g. `"http://192.168.1.10:8200/art"`
    pub art_base_url: &'a str,
    /// e.g. `"http://192.168.1.10:8200/stream"`
    pub stream_base_url: &'a str,
    /// Shuffled album_id array for the `cat:random` view (SPEC §6.6).
    /// Not referenced by other views.
    pub random_state: &'a RandomState,
    /// Current time as unix seconds (used for SPEC §6.7 time-range menu calculation).
    /// `SystemTime::now()` is called by the caller (`content_directory.rs` /
    /// test helpers that build a `BrowseContext`) so tests can inject a fixed value.
    pub now_secs: i64,
    /// Tuning values from `config.toml [browse]` (`recently_added_limit` /
    /// `random_albums_limit` / `quality_categories`).
    pub settings: &'a BrowseSettings,
}

/// BrowseMetadata dispatch. Returns a single object.
pub fn browse_metadata(ctx: &BrowseContext, id: &ObjectId) -> Result<DidlOutput> {
    use categories::{genre_container, person_container, plain_cat, root_container};
    match id {
        ObjectId::Root => Ok(single(root_container())),
        ObjectId::CatAa => Ok(single(plain_cat("cat:aa", "0", "Album Artist"))),
        ObjectId::CatAr => Ok(single(plain_cat("cat:ar", "0", "Artist"))),
        ObjectId::CatAl => Ok(single(plain_cat("cat:al", "0", "Album"))),
        ObjectId::CatGn => Ok(single(plain_cat("cat:gn", "0", "Genre"))),
        ObjectId::CatRecent => Ok(single(plain_cat("cat:recent", "0", "Recently Added"))),
        ObjectId::CatRecentRange(r) => Ok(single(plain_cat(
            &crate::upnp::object_id::encode(&ObjectId::CatRecentRange(*r)),
            "cat:recent",
            recent_range_title(*r).as_str(),
        ))),
        ObjectId::CatPlayed => Ok(single(plain_cat("cat:played", "0", "Recently Played"))),
        ObjectId::CatRandom => Ok(single(plain_cat("cat:random", "0", "Random Albums"))),
        ObjectId::CatHires => Ok(single(plain_cat("cat:hires", "0", "Hi-Res Albums"))),
        ObjectId::CatLossy => Ok(single(plain_cat("cat:lossy", "0", "Lossy Albums"))),
        ObjectId::CatMixed => Ok(single(plain_cat("cat:mixed", "0", "Mixed Quality"))),
        ObjectId::AlbumArtist(name) => Ok(single(person_container(id, "cat:aa", name))),
        ObjectId::Artist(name) => Ok(single(person_container(id, "cat:ar", name))),
        ObjectId::Genre(name) => Ok(single(genre_container(id, "cat:gn", name))),
        ObjectId::Album(album_id) => albums::album_metadata(ctx, *album_id),
        ObjectId::Track(track_id) => tracks::track_metadata(ctx, *track_id),
    }
}

/// BrowseDirectChildren dispatch. Returns the children list + total_matches.
pub fn browse_children(
    ctx: &BrowseContext,
    id: &ObjectId,
    start: usize,
    count: usize,
) -> Result<ChildrenResult> {
    match id {
        ObjectId::Root => Ok(categories::root_children(ctx)),
        ObjectId::CatAa => categories::album_artists_children(ctx, start, count),
        ObjectId::CatAr => categories::artists_children(ctx, start, count),
        ObjectId::CatAl => categories::albums_children(ctx, start, count),
        ObjectId::CatGn => categories::genres_children(ctx, start, count),
        ObjectId::CatRecent => recent::recent_root_children(ctx),
        ObjectId::CatRecentRange(r) => recent::recent_range_children(ctx, *r, start, count),
        ObjectId::CatPlayed => played::played_albums_children(ctx, start, count),
        ObjectId::CatRandom => random::random_albums_children(ctx, start, count),
        ObjectId::CatHires => {
            quality::quality_albums_children(ctx, "hires", "cat:hires", start, count)
        }
        ObjectId::CatLossy => {
            quality::quality_albums_children(ctx, "lossy", "cat:lossy", start, count)
        }
        ObjectId::CatMixed => {
            quality::quality_albums_children(ctx, "mixed", "cat:mixed", start, count)
        }
        ObjectId::AlbumArtist(name) => albums::albums_by_aa_children(ctx, name, start, count),
        ObjectId::Artist(name) => albums::albums_by_artist_children(ctx, name, start, count),
        ObjectId::Genre(name) => albums::albums_by_genre_children(ctx, name, start, count),
        ObjectId::Album(album_id) => tracks::album_tracks_children(ctx, *album_id, start, count),
        ObjectId::Track(_) => Ok(ChildrenResult {
            didl: DidlOutput {
                containers: vec![],
                items: vec![],
            },
            total_matches: 0,
        }),
    }
}

/// `cat:recent:<range>` container title for `BrowseMetadata` (SPEC §6.3).
fn recent_range_title(r: crate::upnp::object_id::RecentRange) -> String {
    use crate::upnp::object_id::RecentRange::*;
    match r {
        Day => "Last day".to_string(),
        Week => "Last week".to_string(),
        Month => "Last month".to_string(),
        ThreeMonths => "Last 3 months".to_string(),
        Year(y) => y.to_string(),
        All => "Show All".to_string(),
    }
}

pub(crate) fn single(c: Container) -> DidlOutput {
    DidlOutput {
        containers: vec![c],
        items: vec![],
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::db::{albums, schema, tracks};

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

        tracks::upsert(
            &conn,
            &tracks::TrackRow {
                album_id: beatles_id,
                path: "/m/come_together.flac",
                title: Some("Come Together"),
                artist: Some("The Beatles"),
                genre: Some("Rock"),
                track_num: Some(1),
                disc_num: Some(1),
                duration_ms: Some(259_000),
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
        tracks::upsert(
            &conn,
            &tracks::TrackRow {
                album_id: beatles_id,
                path: "/m/something.flac",
                title: Some("Something"),
                artist: Some("The Beatles"),
                genre: Some("Rock"),
                track_num: Some(2),
                disc_num: Some(1),
                duration_ms: Some(183_000),
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
        tracks::upsert(
            &conn,
            &tracks::TrackRow {
                album_id: va_id,
                path: "/m/va_track.mp3",
                title: Some("VA Track"),
                artist: Some("Some Singer"),
                genre: Some("Pop"),
                track_num: Some(1),
                disc_num: Some(1),
                duration_ms: Some(200_000),
                sample_rate: Some(44100),
                bit_depth: None,
                channels: Some(2),
                bitrate: Some(320),
                codec: "mp3",
                mime_type: "audio/mpeg",
                file_size: 4567,
                added_at: 100,
                mtime: 200,
            },
        )
        .unwrap();

        albums::recalc_counts(&conn).unwrap();
        conn
    }

    fn ctx(conn: &Connection) -> BrowseContext<'_> {
        static RS: std::sync::OnceLock<crate::random::RandomState> = std::sync::OnceLock::new();
        static BS: std::sync::OnceLock<BrowseSettings> = std::sync::OnceLock::new();
        BrowseContext {
            conn,
            art_base_url: "http://x/art",
            stream_base_url: "http://x/stream",
            random_state: RS.get_or_init(crate::random::RandomState::new),
            now_secs: 0,
            settings: BS.get_or_init(BrowseSettings::default),
        }
    }

    #[test]
    fn br1_root_children_returns_ten_categories() {
        let conn = seed_db();
        let result = browse_children(&ctx(&conn), &ObjectId::Root, 0, 100).unwrap();
        assert_eq!(result.total_matches, 10);
        assert_eq!(result.didl.containers.len(), 10);
        let ids: Vec<String> = result
            .didl
            .containers
            .iter()
            .map(|c| c.id.clone())
            .collect();
        for expected in [
            "cat:aa",
            "cat:ar",
            "cat:al",
            "cat:gn",
            "cat:recent",
            "cat:played",
            "cat:random",
            "cat:hires",
            "cat:lossy",
            "cat:mixed",
        ] {
            assert!(ids.contains(&expected.to_string()), "missing {}", expected);
        }
    }

    #[test]
    fn br2_cat_aa_children_returns_distinct_album_artists() {
        let conn = seed_db();
        let result = browse_children(&ctx(&conn), &ObjectId::CatAa, 0, 100).unwrap();
        assert_eq!(result.total_matches, 2);
        let titles: Vec<&str> = result
            .didl
            .containers
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert!(titles.contains(&"The Beatles"));
        assert!(titles.contains(&"Various Artists"));
    }

    #[test]
    fn br3_aa_name_children_returns_artist_albums() {
        let conn = seed_db();
        let result = browse_children(
            &ctx(&conn),
            &ObjectId::AlbumArtist("The Beatles".to_string()),
            0,
            100,
        )
        .unwrap();
        assert_eq!(result.total_matches, 1);
        assert_eq!(result.didl.containers.len(), 1);
        assert_eq!(result.didl.containers[0].title, "Abbey Road");
        assert_eq!(result.didl.containers[0].child_count, Some(2));
    }

    #[test]
    fn br4_alb_id_children_returns_tracks_in_order() {
        let conn = seed_db();
        let album_id: i64 = conn
            .query_row(
                "SELECT id FROM albums WHERE album = 'Abbey Road'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let result = browse_children(&ctx(&conn), &ObjectId::Album(album_id), 0, 100).unwrap();
        assert_eq!(result.total_matches, 2);
        assert_eq!(result.didl.items.len(), 2);
        assert_eq!(result.didl.items[0].title, "Come Together");
        assert_eq!(result.didl.items[1].title, "Something");
        assert!(result.didl.items[0].res.url.contains("/stream/"));
    }

    #[test]
    fn br5_cat_al_children_returns_all_albums() {
        let conn = seed_db();
        let result = browse_children(&ctx(&conn), &ObjectId::CatAl, 0, 100).unwrap();
        assert_eq!(result.total_matches, 2);
        let titles: Vec<&str> = result
            .didl
            .containers
            .iter()
            .map(|c| c.title.as_str())
            .collect();
        assert!(titles.contains(&"Abbey Road"));
        assert!(titles.contains(&"Hits"));
    }

    #[test]
    fn br6_browse_metadata_for_root() {
        let conn = seed_db();
        let result = browse_metadata(&ctx(&conn), &ObjectId::Root).unwrap();
        assert_eq!(result.containers.len(), 1);
        assert_eq!(result.containers[0].id, "0");
        assert_eq!(result.containers[0].title, "Music Library");
    }

    fn empty_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn br7_empty_db_categories_return_zero_total() {
        // Browsing each cat:* on a 0-item library returns empty, not an error.
        let conn = empty_conn();
        for id in [
            ObjectId::CatAa,
            ObjectId::CatAr,
            ObjectId::CatAl,
            ObjectId::CatGn,
        ] {
            let r = browse_children(&ctx(&conn), &id, 0, 100).unwrap();
            assert_eq!(r.total_matches, 0, "category {:?} should be empty", id);
            assert!(r.didl.containers.is_empty());
            assert!(r.didl.items.is_empty());
        }
    }

    #[test]
    fn br8_unknown_album_metadata_returns_err() {
        // A nonexistent album_id raises a DB query error
        // (rusqlite::Error::QueryReturnedNoRows). The caller (SOAP handler)
        // converts it to NoSuchObject.
        let conn = empty_conn();
        let result = browse_metadata(&ctx(&conn), &ObjectId::Album(99999));
        assert!(result.is_err());
    }

    #[test]
    fn br9_unknown_track_metadata_returns_err() {
        let conn = empty_conn();
        let result = browse_metadata(&ctx(&conn), &ObjectId::Track(99999));
        assert!(result.is_err());
    }

    #[test]
    fn br10_unknown_aa_name_children_returns_zero() {
        // Browsing a nonexistent AA name: SQL returns 0 rows, not an error.
        let conn = seed_db();
        let r = browse_children(
            &ctx(&conn),
            &ObjectId::AlbumArtist("Nobody".to_string()),
            0,
            100,
        )
        .unwrap();
        assert_eq!(r.total_matches, 0);
    }

    #[test]
    fn br11_browse_track_object_returns_empty_children() {
        // Per spec, DirectChildren of ObjectId::Track is 0 (Track is a leaf).
        let conn = seed_db();
        let r = browse_children(&ctx(&conn), &ObjectId::Track(1), 0, 100).unwrap();
        assert_eq!(r.total_matches, 0);
        assert!(r.didl.containers.is_empty());
        assert!(r.didl.items.is_empty());
    }
}

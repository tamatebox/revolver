//! Shared test helpers for the browse submodules (ops §P2).
//!
//! Previously `recent/played/quality/random` each duplicated 40-60 lines of
//! `seed_*` and `ctx_*` (~200 lines total). The common parts are extracted:
//! - [`open_in_memory_migrated`] — connection + PRAGMA + migrate
//! - [`default_track_row`] — typical values for the `TrackRow` passed to `tracks::upsert`
//! - [`default_ctx`] — typical `BrowseContext` build (fixed art/stream base URLs)
//!
//! View-specific seeding (specific `added_at` / `last_played_at` / mixed quality)
//! still lives in each test file — over-abstraction hurts test readability.

use rusqlite::Connection;

use crate::db::tracks::TrackRow;
use crate::random::RandomState;
use crate::state::BrowseSettings;

use super::BrowseContext;

/// Returns a Connection that is in-memory + FK on + migrated.
pub fn open_in_memory_migrated() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
    crate::db::schema::migrate(&conn).unwrap();
    conn
}

/// A typical `TrackRow` (44.1/16 FLAC, 3 min, disc=1 track=1). `added_at` is
/// taken as an argument since callers commonly want to vary it. Other fields
/// should be overwritten directly (e.g. `row.bit_depth = ...;`) instead of via
/// a builder.
pub fn default_track_row<'a>(album_id: i64, path: &'a str, added_at: i64) -> TrackRow<'a> {
    TrackRow {
        album_id,
        path,
        title: Some("T"),
        artist: Some("AA"),
        genre: None,
        track_num: Some(1),
        disc_num: Some(1),
        duration_ms: Some(180_000),
        sample_rate: Some(44100),
        bit_depth: Some(16),
        channels: Some(2),
        bitrate: Some(1000),
        codec: "flac",
        mime_type: "audio/flac",
        file_size: 1,
        added_at,
        mtime: 0,
        composer: None,
        conductor: None,
        performer: None,
    }
}

/// Typical `BrowseContext` build. `now_secs = 0`, test-only art/stream base URLs.
pub fn default_ctx<'a>(
    conn: &'a Connection,
    random_state: &'a RandomState,
    settings: &'a BrowseSettings,
    now_secs: i64,
) -> BrowseContext<'a> {
    BrowseContext {
        conn,
        art_base_url: "http://x/art",
        stream_base_url: "http://x/stream",
        random_state,
        now_secs,
        settings,
    }
}

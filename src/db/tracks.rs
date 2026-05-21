use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// Minimal per-row info needed by the `/stream/{track_id}` handler.
pub struct TrackPath {
    pub path: PathBuf,
    pub file_size: u64,
    pub mime_type: String,
}

/// Fetch path / file_size / mime_type by id. Returns `None` if not present.
pub fn lookup_by_id(conn: &Connection, id: i64) -> Result<Option<TrackPath>> {
    let row = conn
        .query_row(
            "SELECT path, file_size, mime_type FROM tracks WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(path, size, mime)| TrackPath {
        path: PathBuf::from(path),
        file_size: size as u64,
        mime_type: mime,
    }))
}

/// Result of `upsert` (used to drive scan-report inserted/updated counters).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsertOutcome {
    Inserted,
    Updated,
}

/// One row of values to INSERT/UPDATE in the tracks table.
pub struct TrackRow<'a> {
    pub album_id: i64,
    pub path: &'a str,
    pub title: Option<&'a str>,
    pub artist: Option<&'a str>,
    pub genre: Option<&'a str>,
    pub track_num: Option<u32>,
    pub disc_num: Option<u32>,
    pub duration_ms: Option<u64>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub channels: Option<u8>,
    pub bitrate: Option<u32>,
    pub codec: &'a str,
    pub mime_type: &'a str,
    pub file_size: u64,
    pub added_at: i64,
    pub mtime: i64,
    /// #9: classical-library tags. NULL when absent.
    pub composer: Option<&'a str>,
    pub conductor: Option<&'a str>,
    pub performer: Option<&'a str>,
    /// #2: release year. NULL when absent or unparseable.
    pub year: Option<i32>,
}

/// Upsert into tracks (SPEC §4.3). On `path` UNIQUE conflict, update everything
/// **except `added_at`**. `added_at` is set only on INSERT.
///
/// Returns whether INSERT or UPDATE happened.
///
/// perf §P1: the old impl pre-checked existence with `SELECT 1` and then ran
/// an ON CONFLICT upsert — always 2 queries/track. The new impl uses
/// `INSERT OR IGNORE` and its `changes()` (insert count) to detect existence:
/// 1 query for a brand-new path, INSERT IGNORE + UPDATE for an existing one.
/// Wins on incremental scans where **most rows are mtime-skipped with a few
/// inserts**.
pub fn upsert(conn: &Connection, row: &TrackRow) -> Result<UpsertOutcome> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO tracks (
           album_id, path, title, artist, genre,
           track_num, disc_num, duration_ms,
           sample_rate, bit_depth, channels, bitrate,
           codec, mime_type, file_size,
           added_at, mtime,
           composer, conductor, performer,
           year
         )
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
        params![
            row.album_id,
            row.path,
            row.title,
            row.artist,
            row.genre,
            row.track_num,
            row.disc_num,
            row.duration_ms.map(|x| x as i64),
            row.sample_rate,
            row.bit_depth,
            row.channels,
            row.bitrate,
            row.codec,
            row.mime_type,
            row.file_size as i64,
            row.added_at,
            row.mtime,
            row.composer,
            row.conductor,
            row.performer,
            row.year,
        ],
    )?;
    if inserted == 1 {
        return Ok(UpsertOutcome::Inserted);
    }
    // Existing path: overwrite everything except `added_at`
    conn.execute(
        "UPDATE tracks SET
           album_id    = ?1,
           title       = ?2,
           artist      = ?3,
           genre       = ?4,
           track_num   = ?5,
           disc_num    = ?6,
           duration_ms = ?7,
           sample_rate = ?8,
           bit_depth   = ?9,
           channels    = ?10,
           bitrate     = ?11,
           codec       = ?12,
           mime_type   = ?13,
           file_size   = ?14,
           mtime       = ?15,
           composer    = ?16,
           conductor   = ?17,
           performer   = ?18,
           year        = ?19
         WHERE path = ?20",
        params![
            row.album_id,
            row.title,
            row.artist,
            row.genre,
            row.track_num,
            row.disc_num,
            row.duration_ms.map(|x| x as i64),
            row.sample_rate,
            row.bit_depth,
            row.channels,
            row.bitrate,
            row.codec,
            row.mime_type,
            row.file_size as i64,
            row.mtime,
            row.composer,
            row.conductor,
            row.performer,
            row.year,
            row.path,
        ],
    )?;
    Ok(UpsertOutcome::Updated)
}

/// DELETE tracks whose path is not in the enumerated set (SPEC §4.1 step 7).
/// Returns the DELETE count. FK ON DELETE CASCADE does not propagate to albums;
/// orphaned albums are swept separately by `albums::delete_orphans`.
///
/// **Implementation**: bulk-insert enumerated paths into a TEMP TABLE, then a single
/// `DELETE FROM tracks WHERE path NOT IN (...)`. Avoids N+1 on 50K-200K-track DBs
/// (the old impl pulled `SELECT path FROM tracks` into Rust and ran a per-row
/// `DELETE WHERE path = ?` — 50K round-trips for 50K tracks).
pub fn detect_deleted(conn: &Connection, enumerated: &HashSet<&str>) -> Result<usize> {
    // TEMP TABLE is visible only on the same connection. Caller is expected to
    // invoke this inside a transaction, but it also works fine without one
    // (temp tables are connection-scoped).
    conn.execute(
        "CREATE TEMP TABLE IF NOT EXISTS enumerated_paths (path TEXT PRIMARY KEY)",
        [],
    )?;
    conn.execute("DELETE FROM enumerated_paths", [])?;

    {
        let mut ins =
            conn.prepare_cached("INSERT OR IGNORE INTO enumerated_paths (path) VALUES (?1)")?;
        for p in enumerated.iter() {
            ins.execute(params![p])?;
        }
    }

    let n = conn.execute(
        "DELETE FROM tracks WHERE path NOT IN (SELECT path FROM enumerated_paths)",
        [],
    )?;

    // Cleanup: the TEMP TABLE lives until the connection ends. We don't DROP it
    // because we want to empty it before reuse (next call does CREATE IF NOT
    // EXISTS + DELETE to reset).
    Ok(n)
}

/// Snapshot of the current DB as `path → mtime` (used for mtime-skip detection).
pub fn get_mtimes(conn: &Connection) -> Result<HashMap<String, i64>> {
    let mut stmt = conn.prepare("SELECT path, mtime FROM tracks")?;
    let map: HashMap<String, i64> = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{albums, schema::migrate};

    fn open_with_album() -> (Connection, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&conn).unwrap();
        let aid = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            100,
        )
        .unwrap();
        (conn, aid)
    }

    fn sample(album_id: i64, path: &str, added_at: i64, mtime: i64) -> TrackRow<'_> {
        TrackRow {
            album_id,
            path,
            title: Some("T"),
            artist: Some("A"),
            genre: None,
            track_num: Some(1),
            disc_num: None,
            duration_ms: Some(180_000),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            bitrate: Some(1411),
            codec: "flac",
            mime_type: "audio/flac",
            file_size: 1234,
            added_at,
            mtime,
            composer: None,
            conductor: None,
            performer: None,
            year: None,
        }
    }

    #[test]
    fn t1_insert_new() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn t2_added_at_is_preserved_on_conflict() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        upsert(&conn, &sample(aid, "/m/a.flac", 999, 300)).unwrap();
        let added_at: i64 = conn
            .query_row(
                "SELECT added_at FROM tracks WHERE path = '/m/a.flac'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(added_at, 100, "added_at must never be overwritten");
    }

    #[test]
    fn t3_tag_and_mtime_updated_on_conflict() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        let mut row = sample(aid, "/m/a.flac", 999, 300);
        row.title = Some("NewTitle");
        upsert(&conn, &row).unwrap();
        let (title, mtime): (String, i64) = conn
            .query_row(
                "SELECT title, mtime FROM tracks WHERE path = '/m/a.flac'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title, "NewTitle");
        assert_eq!(mtime, 300);
    }

    #[test]
    fn t4_album_id_can_change_on_conflict() {
        let (conn, aid1) = open_with_album();
        let aid2 = albums::upsert(
            &conn,
            &albums::AlbumKey {
                effective_album_artist: "AA2",
                album: "Alb2",
                compilation: false,
            },
            None,
            100,
        )
        .unwrap();
        assert_ne!(aid1, aid2);

        upsert(&conn, &sample(aid1, "/m/a.flac", 100, 200)).unwrap();
        upsert(&conn, &sample(aid2, "/m/a.flac", 999, 300)).unwrap();

        let aid: i64 = conn
            .query_row(
                "SELECT album_id FROM tracks WHERE path = '/m/a.flac'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(aid, aid2);
    }

    #[test]
    fn u1_first_upsert_returns_inserted() {
        let (conn, aid) = open_with_album();
        let outcome = upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        assert_eq!(outcome, UpsertOutcome::Inserted);
    }

    #[test]
    fn u2_second_upsert_on_same_path_returns_updated() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        let outcome = upsert(&conn, &sample(aid, "/m/a.flac", 999, 300)).unwrap();
        assert_eq!(outcome, UpsertOutcome::Updated);
    }

    #[test]
    fn d1_detect_deleted_removes_paths_not_in_enumerated() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/keep.flac", 100, 200)).unwrap();
        upsert(&conn, &sample(aid, "/m/gone.flac", 100, 200)).unwrap();

        let mut enumerated = HashSet::new();
        enumerated.insert("/m/keep.flac");

        let deleted = detect_deleted(&conn, &enumerated).unwrap();
        assert_eq!(deleted, 1);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[test]
    fn d2_detect_deleted_keeps_everything_when_all_enumerated() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 200)).unwrap();
        upsert(&conn, &sample(aid, "/m/b.flac", 100, 200)).unwrap();

        let enumerated: HashSet<&str> = ["/m/a.flac", "/m/b.flac"].into_iter().collect();
        let deleted = detect_deleted(&conn, &enumerated).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn d3_detect_deleted_handles_large_enumerated_set() {
        // Regression check for the TEMP TABLE path: even at 1000 rows, a single DELETE suffices
        let (conn, aid) = open_with_album();
        for i in 0..1000 {
            upsert(
                &conn,
                &sample(
                    aid,
                    Box::leak(format!("/m/file{}.flac", i).into_boxed_str()),
                    100,
                    200,
                ),
            )
            .unwrap();
        }
        // Keep only half in `enumerated`
        let kept: Vec<String> = (0..500).map(|i| format!("/m/file{}.flac", i)).collect();
        let enumerated: HashSet<&str> = kept.iter().map(|s| s.as_str()).collect();
        let deleted = detect_deleted(&conn, &enumerated).unwrap();
        assert_eq!(deleted, 500);
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 500);
    }

    // SPEC §4.2: a file move is treated as a path change, becoming DELETE + INSERT.
    // The new path's `added_at` is "when the server first saw this path" = now.
    // (CLAUDE.md explicitly notes this as intended behavior.)
    #[test]
    fn fm1_file_move_is_delete_plus_insert_with_fresh_added_at() {
        let (conn, aid) = open_with_album();
        // Insert a track at the original path (added_at=100)
        upsert(&conn, &sample(aid, "/m/old.flac", 100, 200)).unwrap();
        let original_id: i64 = conn
            .query_row(
                "SELECT id FROM tracks WHERE path = '/m/old.flac'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Scan now sees /m/new.flac instead of /m/old.flac (a move)
        let enumerated: HashSet<&str> = ["/m/new.flac"].into_iter().collect();
        let deleted = detect_deleted(&conn, &enumerated).unwrap();
        assert_eq!(deleted, 1, "old path must be deleted");

        // Upsert at the new path (added_at=999 = "now")
        upsert(&conn, &sample(aid, "/m/new.flac", 999, 200)).unwrap();

        // New row has a new id, added_at = 999 (treated as a new addition)
        let (new_id, new_added): (i64, i64) = conn
            .query_row(
                "SELECT id, added_at FROM tracks WHERE path = '/m/new.flac'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_ne!(new_id, original_id, "moved file gets new track.id");
        assert_eq!(
            new_added, 999,
            "moved file's added_at is the new-scan time (SPEC §4.2)"
        );
    }

    #[test]
    fn g1_get_mtimes_returns_path_to_mtime_map() {
        let (conn, aid) = open_with_album();
        upsert(&conn, &sample(aid, "/m/a.flac", 100, 222)).unwrap();
        upsert(&conn, &sample(aid, "/m/b.flac", 100, 333)).unwrap();

        let map = get_mtimes(&conn).unwrap();
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("/m/a.flac"), Some(&222));
        assert_eq!(map.get("/m/b.flac"), Some(&333));
    }
}

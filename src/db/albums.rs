use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

/// UNIQUE key for the albums table (SPEC §3.1).
pub struct AlbumKey<'a> {
    pub effective_album_artist: &'a str,
    pub album: &'a str,
    pub compilation: bool,
}

/// Upsert into albums (SPEC §4.3). `first_seen_at` is set only on INSERT and is
/// never overwritten on ON CONFLICT; only `album_artist_raw` is updated.
/// Returns the album id.
pub fn upsert(
    conn: &Connection,
    key: &AlbumKey,
    album_artist_raw: Option<&str>,
    first_seen_at: i64,
) -> Result<i64> {
    let id: i64 = conn.query_row(
        "INSERT INTO albums
           (effective_album_artist, album, compilation, album_artist_raw, first_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(effective_album_artist, album, compilation) DO UPDATE SET
           album_artist_raw = excluded.album_artist_raw
         RETURNING id",
        params![
            key.effective_album_artist,
            key.album,
            key.compilation as i64,
            album_artist_raw,
            first_seen_at,
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Return the path of the representative track used for album-art extraction (SPEC §8.3).
/// Selection order: `disc_num` ASC → `track_num` ASC → `path` ASC LIMIT 1.
/// SQLite sorts NULLs first by default, so a row is returned stably even when disc/track
/// are unset (ties broken by path lexicographically).
pub fn get_representative_track_path(conn: &Connection, album_id: i64) -> Result<Option<PathBuf>> {
    let row = conn
        .query_row(
            "SELECT path FROM tracks
             WHERE album_id = ?1
             ORDER BY disc_num, track_num, path
             LIMIT 1",
            params![album_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(row.map(PathBuf::from))
}

/// Delete orphan albums (albums with no tracks) (SPEC §4.1 step 9).
/// Decides based on the live tracks table rather than the cached `track_count`.
pub fn delete_orphans(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM albums WHERE id NOT IN (SELECT DISTINCT album_id FROM tracks)",
        [],
    )?;
    Ok(n)
}

/// Bulk-recompute albums.quality from tracks' codec / sample_rate / bit_depth
/// (SPEC §4.6).
///
/// Per-track tier:
/// - `flac` / `alac` / `pcm` + (`sample_rate > 48000` or `bit_depth > 16`) → `hires`
/// - `flac` / `alac` / `pcm`, otherwise → `lossless`
/// - `mp3` / `aac` → `lossy`
/// - anything else → `unknown` (excluded from aggregation)
///
/// Per-album aggregation:
/// - all tracks share one tier (excluding unknown) → that tier
/// - mixed tiers → `mixed`
/// - all tracks `unknown` (only unrecognised codecs) → `unknown`
///
/// The query in SPEC §4.6 references a subquery alias in its WHERE clause, which
/// SQLite does not support, so we inline `codec IN (...)` in the WHERE to drop
/// unknowns directly.
pub fn recalc_quality(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE albums SET quality = COALESCE((
           SELECT CASE
             WHEN COUNT(DISTINCT tier) > 1 THEN 'mixed'
             ELSE MAX(tier)
           END
           FROM (
             SELECT
               CASE
                 WHEN codec IN ('flac','alac','pcm')
                      AND (COALESCE(sample_rate,0) > 48000 OR COALESCE(bit_depth,0) > 16) THEN 'hires'
                 WHEN codec IN ('flac','alac','pcm') THEN 'lossless'
                 WHEN codec IN ('mp3','aac') THEN 'lossy'
               END AS tier
             FROM tracks
             WHERE album_id = albums.id
               AND codec IN ('flac','alac','pcm','mp3','aac')
           )
         ), 'unknown')",
        [],
    )?;
    Ok(())
}

/// Bulk-recompute albums.track_count / total_duration_ms from tracks
/// (SPEC §4.1 step 8).
pub fn recalc_counts(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE albums SET
           track_count = (SELECT COUNT(*) FROM tracks WHERE tracks.album_id = albums.id),
           total_duration_ms = COALESCE(
             (SELECT SUM(duration_ms) FROM tracks WHERE tracks.album_id = albums.id),
             0
           )",
        [],
    )?;
    Ok(())
}

/// Bulk-recompute albums.last_added_at = MAX(tracks.added_at).
/// Invoked at scan completion. Browse (`cat:recent`) skips GROUP BY and reads this directly.
pub fn recalc_last_added_at(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE albums SET last_added_at = (
           SELECT MAX(added_at) FROM tracks WHERE tracks.album_id = albums.id
         )",
        [],
    )?;
    Ok(())
}

/// Bulk-recompute albums.last_played_at = MAX(tracks.last_played_at).
/// Normally unused: the stream handler updates a single album per playback via
/// `bump_album_last_played_at`. Kept around for initial-migration backfill and
/// manual maintenance.
pub fn recalc_last_played_at(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE albums SET last_played_at = (
           SELECT MAX(last_played_at) FROM tracks WHERE tracks.album_id = albums.id
         )",
        [],
    )?;
    Ok(())
}

/// Update a single album's `last_played_at` (stream handler hot path).
/// `now` is unix seconds, pre-computed by the caller.
pub fn bump_album_last_played_at(conn: &Connection, album_id: i64, now: i64) -> Result<()> {
    conn.execute(
        "UPDATE albums SET last_played_at = ?1 WHERE id = ?2",
        params![now, album_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn a1_insert_returns_positive_id() {
        let conn = open();
        let id = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            Some("AA"),
            100,
        )
        .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn a2_same_key_returns_same_id() {
        let conn = open();
        let id1 = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            Some("AA"),
            100,
        )
        .unwrap();
        let id2 = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            Some("AA-updated"),
            200,
        )
        .unwrap();
        assert_eq!(id1, id2);
    }

    #[test]
    fn a3_compilation_flag_differentiates_albums() {
        let conn = open();
        let id1 = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "Various Artists",
                album: "Hits",
                compilation: true,
            },
            None,
            100,
        )
        .unwrap();
        let id2 = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "Various Artists",
                album: "Hits",
                compilation: false,
            },
            None,
            100,
        )
        .unwrap();
        assert_ne!(id1, id2);
    }

    fn insert_track(conn: &Connection, album_id: i64, path: &str, duration_ms: i64) {
        conn.execute(
            "INSERT INTO tracks
               (album_id, path, duration_ms, added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, ?2, ?3, 0, 0, 'flac', 'audio/flac', 0)",
            params![album_id, path, duration_ms],
        )
        .unwrap();
    }

    #[test]
    fn o1_delete_orphans_removes_albums_without_tracks() {
        let conn = open();
        let with_tracks = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Has",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        let orphan = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Empty",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track(&conn, with_tracks, "/m/a.flac", 1000);

        let deleted = delete_orphans(&conn).unwrap();
        assert_eq!(deleted, 1);

        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE id = ?1",
                params![with_tracks],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "album with tracks must survive");

        let gone: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM albums WHERE id = ?1",
                params![orphan],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(gone, 0, "orphan album must be gone");
    }

    #[test]
    fn o2_delete_orphans_keeps_everything_when_all_have_tracks() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track(&conn, aid, "/m/a.flac", 1000);

        let deleted = delete_orphans(&conn).unwrap();
        assert_eq!(deleted, 0);
    }

    #[test]
    fn r1_recalc_counts_sets_track_count() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track(&conn, aid, "/m/a.flac", 1000);
        insert_track(&conn, aid, "/m/b.flac", 2000);

        recalc_counts(&conn).unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT track_count FROM albums WHERE id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn r2_recalc_counts_sets_total_duration_ms() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track(&conn, aid, "/m/a.flac", 1000);
        insert_track(&conn, aid, "/m/b.flac", 2500);

        recalc_counts(&conn).unwrap();

        let total: i64 = conn
            .query_row(
                "SELECT total_duration_ms FROM albums WHERE id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 3500);
    }

    fn insert_track_with_audio(
        conn: &Connection,
        album_id: i64,
        path: &str,
        codec: &str,
        sample_rate: u32,
        bit_depth: u32,
    ) {
        conn.execute(
            "INSERT INTO tracks
               (album_id, path, codec, sample_rate, bit_depth, duration_ms,
                added_at, mtime, mime_type, file_size)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, 'x', 0)",
            params![album_id, path, codec, sample_rate, bit_depth],
        )
        .unwrap();
    }

    fn quality_of(conn: &Connection, album_id: i64) -> String {
        conn.query_row(
            "SELECT quality FROM albums WHERE id = ?1",
            params![album_id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn q1_all_lossless_cd_quality_is_lossless() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "CD",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.flac", "flac", 44100, 16);
        insert_track_with_audio(&conn, aid, "/m/b.flac", "flac", 44100, 16);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "lossless");
    }

    #[test]
    fn q2_all_hires_24_96_quality_is_hires() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "HD",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.flac", "flac", 96000, 24);
        insert_track_with_audio(&conn, aid, "/m/b.flac", "flac", 96000, 24);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "hires");
    }

    #[test]
    fn q3_all_mp3_quality_is_lossy() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Pop",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.mp3", "mp3", 44100, 0);
        insert_track_with_audio(&conn, aid, "/m/b.mp3", "mp3", 44100, 0);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "lossy");
    }

    #[test]
    fn q4_flac_plus_mp3_is_mixed() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Bonus",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.flac", "flac", 44100, 16);
        insert_track_with_audio(&conn, aid, "/m/b.mp3", "mp3", 44100, 0);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "mixed");
    }

    #[test]
    fn q5_hires_plus_lossless_is_mixed() {
        // A mix of 24/96 and 16/44.1 is also `mixed` (remaster-mixed releases)
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Mix",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.flac", "flac", 96000, 24);
        insert_track_with_audio(&conn, aid, "/m/b.flac", "flac", 44100, 16);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "mixed");
    }

    #[test]
    fn q6_unknown_codec_only_falls_back_to_unknown() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Weird",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        insert_track_with_audio(&conn, aid, "/m/a.dsf", "dsf", 0, 0);
        recalc_quality(&conn).unwrap();
        assert_eq!(quality_of(&conn, aid), "unknown");
    }

    #[test]
    fn rep1_get_representative_track_path_returns_lowest_disc_track() {
        let (conn, aid) = {
            let conn = Connection::open_in_memory().unwrap();
            migrate(&conn).unwrap();
            let aid = upsert(
                &conn,
                &AlbumKey {
                    effective_album_artist: "AA",
                    album: "Alb",
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap();
            (conn, aid)
        };
        // Insert disc=1 track=2 / disc=1 track=1 / disc=2 track=1
        conn.execute(
            "INSERT INTO tracks (album_id, path, disc_num, track_num, duration_ms,
                                 added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, '/m/d2t1.flac', 2, 1, 0, 0, 0, 'flac', 'audio/flac', 0)",
            params![aid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, disc_num, track_num, duration_ms,
                                 added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, '/m/d1t2.flac', 1, 2, 0, 0, 0, 'flac', 'audio/flac', 0)",
            params![aid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, disc_num, track_num, duration_ms,
                                 added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, '/m/d1t1.flac', 1, 1, 0, 0, 0, 'flac', 'audio/flac', 0)",
            params![aid],
        )
        .unwrap();
        let p = get_representative_track_path(&conn, aid).unwrap().unwrap();
        assert_eq!(p, std::path::PathBuf::from("/m/d1t1.flac"));
    }

    #[test]
    fn rep2_get_representative_track_path_unknown_album_returns_none() {
        let conn = open();
        assert!(get_representative_track_path(&conn, 9999)
            .unwrap()
            .is_none());
    }

    #[test]
    fn la1_recalc_last_added_at_uses_max_of_tracks() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        // 3 tracks with added_at = 100, 500, 300
        for (path, added) in [("/m/a.flac", 100i64), ("/m/b.flac", 500), ("/m/c.flac", 300)] {
            conn.execute(
                "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                     duration_ms)
                 VALUES (?1, ?2, ?3, 0, 'flac', 'audio/flac', 0, 0)",
                params![aid, path, added],
            )
            .unwrap();
        }
        recalc_last_added_at(&conn).unwrap();
        let max: i64 = conn
            .query_row(
                "SELECT last_added_at FROM albums WHERE id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(max, 500);
    }

    #[test]
    fn la2_recalc_last_added_at_is_null_for_album_without_tracks() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Empty",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        recalc_last_added_at(&conn).unwrap();
        let v: Option<i64> = conn
            .query_row(
                "SELECT last_added_at FROM albums WHERE id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(v.is_none());
    }

    #[test]
    fn lp1_bump_album_last_played_at_sets_value() {
        let conn = open();
        let aid = upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        bump_album_last_played_at(&conn, aid, 9999).unwrap();
        let v: i64 = conn
            .query_row(
                "SELECT last_played_at FROM albums WHERE id = ?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, 9999);
    }

    // ── ObjectID stability: re-upsert with the same key keeps the id (SPEC §10.4)
    #[test]
    fn os1_album_and_track_ids_stable_across_rescan() {
        let conn = open();
        let key = AlbumKey {
            effective_album_artist: "AA",
            album: "Alb",
            compilation: false,
        };
        let aid1 = upsert(&conn, &key, None, 100).unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                 duration_ms)
             VALUES (?1, '/m/stable.flac', 100, 100, 'flac', 'audio/flac', 0, 0)
             ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
            params![aid1],
        )
        .unwrap();
        let tid1: i64 = conn
            .query_row(
                "SELECT id FROM tracks WHERE path = '/m/stable.flac'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Simulate a second rescan
        let aid2 = upsert(&conn, &key, None, 999).unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                 duration_ms)
             VALUES (?1, '/m/stable.flac', 100, 200, 'flac', 'audio/flac', 0, 0)
             ON CONFLICT(path) DO UPDATE SET mtime = excluded.mtime",
            params![aid2],
        )
        .unwrap();
        let tid2: i64 = conn
            .query_row(
                "SELECT id FROM tracks WHERE path = '/m/stable.flac'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(aid1, aid2, "album.id must survive rescan (ObjectId alb:N)");
        assert_eq!(tid1, tid2, "track.id must survive rescan (ObjectId trk:N)");

        // Also assert that the encoded ObjectId is unchanged
        use crate::upnp::object_id::{encode, ObjectId};
        assert_eq!(
            encode(&ObjectId::Album(aid1)),
            encode(&ObjectId::Album(aid2))
        );
        assert_eq!(
            encode(&ObjectId::Track(tid1)),
            encode(&ObjectId::Track(tid2))
        );
    }

    #[test]
    fn a4_first_seen_at_is_preserved_across_upserts() {
        let conn = open();
        upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            100,
        )
        .unwrap();
        upsert(
            &conn,
            &AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            999,
        )
        .unwrap();
        let first_seen: i64 = conn
            .query_row(
                "SELECT first_seen_at FROM albums WHERE album = 'Alb'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            first_seen, 100,
            "first_seen_at must not be overwritten on conflict"
        );
    }
}

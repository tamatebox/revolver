use rusqlite::{Connection, OptionalExtension};

use crate::db::state_kv;
use crate::error::Result;

/// Schema version expected by the current binary. `migrate()` writes it into
/// `server_state.schema_version`. `ensure_compatible_or_err()` compares the DB value
/// and, if **DB > binary**, treats it as a downgrade and refuses to start (ops §P1:
/// closes the path where running an older binary after a newer one added columns
/// would silently corrupt data).
///
/// Bump by +1 whenever a column is added or removed.
pub const SCHEMA_VERSION: u32 = 5;

/// Table definitions from SPEC §3.1. Idempotent via `CREATE ... IF NOT EXISTS`.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS albums (
  id                     INTEGER PRIMARY KEY AUTOINCREMENT,
  effective_album_artist TEXT    NOT NULL,
  album                  TEXT    NOT NULL,
  compilation            INTEGER NOT NULL DEFAULT 0,
  album_artist_raw       TEXT,
  first_seen_at          INTEGER NOT NULL,
  track_count            INTEGER NOT NULL DEFAULT 0,
  total_duration_ms      INTEGER NOT NULL DEFAULT 0,
  quality                TEXT    NOT NULL DEFAULT 'unknown',
  UNIQUE(effective_album_artist, album, compilation)
);

CREATE INDEX IF NOT EXISTS idx_alb_aa      ON albums(effective_album_artist);
CREATE INDEX IF NOT EXISTS idx_alb_first   ON albums(first_seen_at DESC);
CREATE INDEX IF NOT EXISTS idx_alb_quality ON albums(quality);
-- albums.last_added_at / last_played_at: indexes are created after migrate() runs ALTER
-- (avoids CREATE INDEX failing on older DBs that lack the column; same pattern as play_count).

CREATE TABLE IF NOT EXISTS tracks (
  id             INTEGER PRIMARY KEY AUTOINCREMENT,
  album_id       INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
  path           TEXT    NOT NULL UNIQUE,
  title          TEXT,
  artist         TEXT,
  genre          TEXT,
  track_num      INTEGER,
  disc_num       INTEGER,
  duration_ms    INTEGER,
  sample_rate    INTEGER,
  bit_depth      INTEGER,
  channels       INTEGER,
  bitrate        INTEGER,
  codec          TEXT,
  mime_type      TEXT,
  file_size      INTEGER,
  added_at       INTEGER NOT NULL,
  mtime          INTEGER NOT NULL,
  play_count     INTEGER NOT NULL DEFAULT 0,
  last_played_at INTEGER,
  composer       TEXT,
  conductor      TEXT,
  performer      TEXT,
  year           INTEGER
);

CREATE INDEX IF NOT EXISTS idx_trk_album  ON tracks(album_id);
CREATE INDEX IF NOT EXISTS idx_trk_artist ON tracks(artist);
CREATE INDEX IF NOT EXISTS idx_trk_genre  ON tracks(genre);
CREATE INDEX IF NOT EXISTS idx_trk_added  ON tracks(added_at DESC);
-- idx_trk_played is created post-ALTER (CREATE INDEX would fail on older DBs that
-- don't have last_played_at yet, so it runs at the end of migrate()).

CREATE TABLE IF NOT EXISTS server_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

-- User-editable settings layered over `config.toml` defaults (#13).
-- `value` holds a JSON-encoded scalar/array so non-string types round-trip.
CREATE TABLE IF NOT EXISTS config_overrides (
  key        TEXT    PRIMARY KEY,
  value      TEXT    NOT NULL,
  updated_at INTEGER NOT NULL
);
"#;

pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;
    // Upgrade existing DBs to the play_count / last_played_at columns added in
    // Phase 2 step 16c (SPEC §6.8). `CREATE TABLE IF NOT EXISTS` is a no-op when
    // the table already exists, so we add the columns via ALTER TABLE.
    ensure_column(conn, "tracks", "play_count", "INTEGER NOT NULL DEFAULT 0")?;
    ensure_column(conn, "tracks", "last_played_at", "INTEGER")?;
    // Phase 3 denormalize: cache MAX(tracks.added_at) / MAX(tracks.last_played_at)
    // onto albums, eliminating GROUP BY from the Recently Added / Played Browse path.
    // Maintained after scan and on play_count updates (db/albums.rs::recalc_last_*).
    ensure_column(conn, "albums", "last_added_at", "INTEGER")?;
    ensure_column(conn, "albums", "last_played_at", "INTEGER")?;
    // #9: Composer / Conductor / Performer columns for the classical-music facets.
    ensure_column(conn, "tracks", "composer", "TEXT")?;
    ensure_column(conn, "tracks", "conductor", "TEXT")?;
    ensure_column(conn, "tracks", "performer", "TEXT")?;
    // #2: release year (parsed from DATE / YEAR tag), for cat:yr / cat:dec facets.
    ensure_column(conn, "tracks", "year", "INTEGER")?;
    // Create indexes only after the columns are guaranteed to exist.
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_trk_played ON tracks(last_played_at DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_alb_last_added ON albums(last_added_at DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_alb_last_played ON albums(last_played_at DESC)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_trk_composer ON tracks(composer)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_trk_conductor ON tracks(conductor)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_trk_performer ON tracks(performer)",
        [],
    )?;
    conn.execute(
        "CREATE INDEX IF NOT EXISTS idx_trk_year ON tracks(year)",
        [],
    )?;
    // Write schema_version only after all ALTERs succeed. By **not writing on
    // pre-init or failure**, we keep the invariant: "migrate completed" ⇒ "schema
    // matches SCHEMA_VERSION".
    state_kv::set(conn, "schema_version", &SCHEMA_VERSION.to_string())?;
    Ok(())
}

/// Called at startup from `db::pool()` **before** migrate(). Returns Err and refuses
/// to start when the DB's `schema_version` is **greater than** the binary's
/// `SCHEMA_VERSION` (a downgrade).
///
/// Behavior by state:
/// - no `server_state` table: fresh DB (about to migrate) → Ok
/// - `server_state` present, `schema_version` unrecorded: old DB → Ok (migrate fills it in)
/// - recorded, ≤ binary: Ok
/// - recorded, > binary: Err (downgrade refused, ops §P1)
pub fn ensure_compatible_or_err(conn: &Connection) -> Result<()> {
    // No server_state ⇒ fresh DB (pre-migrate). Skip all checks.
    let server_state_exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='server_state'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !server_state_exists {
        return Ok(());
    }

    let recorded: Option<u32> = state_kv::get(conn, "schema_version")?.and_then(|s| s.parse().ok());
    if let Some(v) = recorded {
        if v > SCHEMA_VERSION {
            return Err(crate::error::Error::Sqlite(
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(format!(
                    "DB schema_version {} is newer than this binary's SCHEMA_VERSION {}; \
                     refusing to start (would silently corrupt data on downgrade)",
                    v, SCHEMA_VERSION
                )))),
            ));
        }
    }
    Ok(())
}

/// Run `ALTER TABLE ADD COLUMN` if `table.column` does not exist. Idempotent.
fn ensure_column(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let exists = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .filter_map(|r| r.ok())
        .any(|name| name == column);
    if !exists {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn open_in_memory_with_fk() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn s1_tables_created() {
        let conn = open_in_memory_with_fk();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(names.contains(&"albums".to_string()));
        assert!(names.contains(&"tracks".to_string()));
        assert!(names.contains(&"server_state".to_string()));
    }

    #[test]
    fn s2_migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
    }

    #[test]
    fn s4_play_count_and_last_played_at_columns_exist() {
        // migrate() creates play_count / last_played_at on a fresh DB
        let conn = open_in_memory_with_fk();
        let mut stmt = conn.prepare("PRAGMA table_info(tracks)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(cols.contains(&"play_count".to_string()), "got: {:?}", cols);
        assert!(
            cols.contains(&"last_played_at".to_string()),
            "got: {:?}",
            cols
        );
    }

    #[test]
    fn s6_albums_denormalized_columns_exist() {
        // migrate() creates albums.last_added_at / last_played_at on a fresh DB
        let conn = open_in_memory_with_fk();
        let mut stmt = conn.prepare("PRAGMA table_info(albums)").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(
            cols.contains(&"last_added_at".to_string()),
            "albums.last_added_at missing: {:?}",
            cols
        );
        assert!(
            cols.contains(&"last_played_at".to_string()),
            "albums.last_played_at missing: {:?}",
            cols
        );
    }

    #[test]
    fn s7_migrate_upgrades_existing_pre_denormalize_db() {
        // Old schema (albums without last_added_at / last_played_at): migrate() adds them via ALTER
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE albums (
               id INTEGER PRIMARY KEY,
               effective_album_artist TEXT NOT NULL,
               album TEXT NOT NULL,
               compilation INTEGER NOT NULL DEFAULT 0,
               album_artist_raw TEXT,
               first_seen_at INTEGER NOT NULL,
               track_count INTEGER NOT NULL DEFAULT 0,
               total_duration_ms INTEGER NOT NULL DEFAULT 0,
               quality TEXT NOT NULL DEFAULT 'unknown'
             );
             CREATE TABLE tracks (
               id INTEGER PRIMARY KEY,
               album_id INTEGER NOT NULL,
               path TEXT NOT NULL UNIQUE,
               title TEXT, artist TEXT, genre TEXT,
               track_num INTEGER, disc_num INTEGER, duration_ms INTEGER,
               sample_rate INTEGER, bit_depth INTEGER, channels INTEGER,
               bitrate INTEGER, codec TEXT, mime_type TEXT, file_size INTEGER,
               added_at INTEGER NOT NULL,
               mtime INTEGER NOT NULL
             );",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // New columns were added by ALTER (NULL-able)
        conn.execute(
            "INSERT INTO albums (id, effective_album_artist, album, first_seen_at, last_added_at)
             VALUES (1, 'AA', 'Alb', 0, 12345)",
            [],
        )
        .unwrap();
        let (la, lp): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_added_at, last_played_at FROM albums WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(la, 12345);
        assert_eq!(lp, None);
    }

    #[test]
    fn s5_migrate_upgrades_existing_pre_play_count_db() {
        // Hand-build a pre-Phase-2 schema (no play_count / last_played_at) and verify
        // migrate() adds them via ALTER TABLE.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE albums (
               id INTEGER PRIMARY KEY,
               effective_album_artist TEXT NOT NULL,
               album TEXT NOT NULL,
               compilation INTEGER NOT NULL DEFAULT 0,
               album_artist_raw TEXT,
               first_seen_at INTEGER NOT NULL,
               track_count INTEGER NOT NULL DEFAULT 0,
               total_duration_ms INTEGER NOT NULL DEFAULT 0,
               quality TEXT NOT NULL DEFAULT 'unknown'
             );
             CREATE TABLE tracks (
               id INTEGER PRIMARY KEY,
               album_id INTEGER NOT NULL,
               path TEXT NOT NULL UNIQUE,
               title TEXT, artist TEXT, genre TEXT,
               track_num INTEGER, disc_num INTEGER, duration_ms INTEGER,
               sample_rate INTEGER, bit_depth INTEGER, channels INTEGER,
               bitrate INTEGER, codec TEXT, mime_type TEXT, file_size INTEGER,
               added_at INTEGER NOT NULL,
               mtime INTEGER NOT NULL
             );",
        )
        .unwrap();

        migrate(&conn).unwrap();

        // Existing rows are preserved (play_count=0, last_played_at=NULL)
        conn.execute(
            "INSERT INTO albums (id, effective_album_artist, album, first_seen_at) VALUES (1, 'AA', 'Alb', 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO tracks (album_id, path, added_at, mtime) VALUES (1, '/m/a', 0, 0)",
            [],
        )
        .unwrap();

        let (pc, lp): (i64, Option<i64>) = conn
            .query_row("SELECT play_count, last_played_at FROM tracks", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(pc, 0);
        assert_eq!(lp, None);
    }

    #[test]
    fn sv1_migrate_writes_schema_version() {
        let conn = open_in_memory_with_fk();
        let v = state_kv::get(&conn, "schema_version").unwrap().unwrap();
        assert_eq!(v.parse::<u32>().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn sv2_ensure_compatible_passes_on_fresh_db() {
        let conn = open_in_memory_with_fk();
        ensure_compatible_or_err(&conn).expect("fresh DB should be compatible");
    }

    #[test]
    fn sv3_ensure_compatible_passes_when_recorded_is_missing() {
        // Empty DB without running migrate (just hand-create the server_state table)
        // → schema_version unrecorded → treated as compatible (migrate will follow).
        let conn = Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE server_state (key TEXT PRIMARY KEY, value TEXT NOT NULL)",
            [],
        )
        .unwrap();
        ensure_compatible_or_err(&conn).expect("missing schema_version should pass");
    }

    #[test]
    fn sv4_ensure_compatible_rejects_newer_db() {
        // Simulate opening a DB written by a future binary with an older one:
        // schema_version exceeds ours, so reject (ops §P1).
        let conn = open_in_memory_with_fk();
        state_kv::set(&conn, "schema_version", &(SCHEMA_VERSION + 1).to_string()).unwrap();
        let err = ensure_compatible_or_err(&conn).expect_err("must reject newer schema");
        assert!(format!("{}", err).contains("newer"));
    }

    #[test]
    fn s3_foreign_key_enforced() {
        let conn = open_in_memory_with_fk();
        let result = conn.execute(
            "INSERT INTO tracks (album_id, path, added_at, mtime) VALUES (?1, ?2, ?3, ?4)",
            params![999i64, "/tmp/x.flac", 0i64, 0i64],
        );
        assert!(result.is_err(), "FK violation should error");
    }
}

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
pub const SCHEMA_VERSION: u32 = 9;

/// Table definitions from SPEC §3.1. Idempotent via `CREATE ... IF NOT EXISTS`.
const SCHEMA_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS albums (
  id                          INTEGER PRIMARY KEY AUTOINCREMENT,
  effective_album_artist      TEXT    NOT NULL,
  album                       TEXT    NOT NULL,
  compilation                 INTEGER NOT NULL DEFAULT 0,
  album_artist_raw            TEXT,
  first_seen_at               INTEGER NOT NULL,
  track_count                 INTEGER NOT NULL DEFAULT 0,
  total_duration_ms           INTEGER NOT NULL DEFAULT 0,
  quality                     TEXT    NOT NULL DEFAULT 'unknown',
  -- #6: NFKD-folded shadow columns used by Search (kept in sync via upsert
  -- + bulk backfill on migrate). Allowed NULL only as a transient state
  -- during ALTER → backfill; populated rows always carry a value.
  album_norm                  TEXT,
  effective_album_artist_norm TEXT,
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
  year           INTEGER,
  -- #6: NFKD-folded shadow columns. See note on albums.album_norm above.
  title_norm     TEXT,
  artist_norm    TEXT,
  genre_norm     TEXT,
  composer_norm  TEXT,
  conductor_norm TEXT,
  performer_norm TEXT,
  -- #11: ReplayGain tags. Gain values are dB (signed); peak values are
  -- linear amplitude (0..~1.x). Nullable — most libraries are partial.
  rg_track_gain  REAL,
  rg_track_peak  REAL,
  rg_album_gain  REAL,
  rg_album_peak  REAL,
  -- v8: capture-only sort / original-year / MusicBrainz fields. Read at scan
  -- time so a future PR can wire queries / DIDL without re-tagging 100k files.
  artist_sort         TEXT,
  album_artist_sort   TEXT,
  album_sort          TEXT,
  title_sort          TEXT,
  composer_sort       TEXT,
  original_year       INTEGER,
  mb_recording_id     TEXT,
  mb_release_id       TEXT,
  mb_release_group_id TEXT,
  mb_artist_id        TEXT,
  mb_release_artist_id TEXT
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

/// #28: FTS5 trigram-tokenizer virtual tables shadowing the `*_norm` columns
/// on `albums` and `tracks`, plus the AFTER INSERT/UPDATE/DELETE triggers that
/// keep them in sync. External-content tables (`content='albums'` etc.) share
/// the source `id` as rowid, so the trigger payload only needs to forward the
/// indexed columns. Bound to `tokenize='trigram'` for substring matching with
/// 1–2 character typo tolerance — used by `browse::search` when the query is
/// at least 3 chars long.
const FTS5_SQL: &str = r#"
CREATE VIRTUAL TABLE IF NOT EXISTS albums_fts USING fts5(
  album_norm,
  effective_album_artist_norm,
  content='albums',
  content_rowid='id',
  tokenize='trigram'
);

CREATE VIRTUAL TABLE IF NOT EXISTS tracks_fts USING fts5(
  title_norm,
  artist_norm,
  composer_norm,
  conductor_norm,
  performer_norm,
  genre_norm,
  content='tracks',
  content_rowid='id',
  tokenize='trigram'
);

CREATE TRIGGER IF NOT EXISTS albums_ai AFTER INSERT ON albums BEGIN
  INSERT INTO albums_fts(rowid, album_norm, effective_album_artist_norm)
  VALUES (new.id, new.album_norm, new.effective_album_artist_norm);
END;
CREATE TRIGGER IF NOT EXISTS albums_ad AFTER DELETE ON albums BEGIN
  INSERT INTO albums_fts(albums_fts, rowid, album_norm, effective_album_artist_norm)
  VALUES('delete', old.id, old.album_norm, old.effective_album_artist_norm);
END;
CREATE TRIGGER IF NOT EXISTS albums_au AFTER UPDATE ON albums BEGIN
  INSERT INTO albums_fts(albums_fts, rowid, album_norm, effective_album_artist_norm)
  VALUES('delete', old.id, old.album_norm, old.effective_album_artist_norm);
  INSERT INTO albums_fts(rowid, album_norm, effective_album_artist_norm)
  VALUES (new.id, new.album_norm, new.effective_album_artist_norm);
END;

CREATE TRIGGER IF NOT EXISTS tracks_ai AFTER INSERT ON tracks BEGIN
  INSERT INTO tracks_fts(rowid, title_norm, artist_norm, composer_norm, conductor_norm, performer_norm, genre_norm)
  VALUES (new.id, new.title_norm, new.artist_norm, new.composer_norm, new.conductor_norm, new.performer_norm, new.genre_norm);
END;
CREATE TRIGGER IF NOT EXISTS tracks_ad AFTER DELETE ON tracks BEGIN
  INSERT INTO tracks_fts(tracks_fts, rowid, title_norm, artist_norm, composer_norm, conductor_norm, performer_norm, genre_norm)
  VALUES('delete', old.id, old.title_norm, old.artist_norm, old.composer_norm, old.conductor_norm, old.performer_norm, old.genre_norm);
END;
CREATE TRIGGER IF NOT EXISTS tracks_au AFTER UPDATE ON tracks BEGIN
  INSERT INTO tracks_fts(tracks_fts, rowid, title_norm, artist_norm, composer_norm, conductor_norm, performer_norm, genre_norm)
  VALUES('delete', old.id, old.title_norm, old.artist_norm, old.composer_norm, old.conductor_norm, old.performer_norm, old.genre_norm);
  INSERT INTO tracks_fts(rowid, title_norm, artist_norm, composer_norm, conductor_norm, performer_norm, genre_norm)
  VALUES (new.id, new.title_norm, new.artist_norm, new.composer_norm, new.conductor_norm, new.performer_norm, new.genre_norm);
END;
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
    // #6: NFKD-folded shadow columns for fuzzy Search. Filled in by
    // `backfill_search_norms` below on first migration to this version.
    ensure_column(conn, "tracks", "title_norm", "TEXT")?;
    ensure_column(conn, "tracks", "artist_norm", "TEXT")?;
    ensure_column(conn, "tracks", "genre_norm", "TEXT")?;
    ensure_column(conn, "tracks", "composer_norm", "TEXT")?;
    ensure_column(conn, "tracks", "conductor_norm", "TEXT")?;
    ensure_column(conn, "tracks", "performer_norm", "TEXT")?;
    ensure_column(conn, "albums", "album_norm", "TEXT")?;
    ensure_column(conn, "albums", "effective_album_artist_norm", "TEXT")?;
    // #11: ReplayGain (per-track gain/peak, optional per-album gain/peak).
    // Nullable REAL — no defaults, NULL means "tag absent".
    ensure_column(conn, "tracks", "rg_track_gain", "REAL")?;
    ensure_column(conn, "tracks", "rg_track_peak", "REAL")?;
    ensure_column(conn, "tracks", "rg_album_gain", "REAL")?;
    ensure_column(conn, "tracks", "rg_album_peak", "REAL")?;
    // Capture-only fields (schema v8): read at scan time, no queries / DIDL
    // wiring yet. Future PRs will denormalize to `albums` and switch ORDER BY
    // for cat:aa / cat:al / cat:ar / cat:yr to use the sort / original_year
    // columns; the MB ids enable dedup + external lookups when wired up.
    ensure_column(conn, "tracks", "artist_sort", "TEXT")?;
    ensure_column(conn, "tracks", "album_artist_sort", "TEXT")?;
    ensure_column(conn, "tracks", "album_sort", "TEXT")?;
    ensure_column(conn, "tracks", "title_sort", "TEXT")?;
    ensure_column(conn, "tracks", "composer_sort", "TEXT")?;
    ensure_column(conn, "tracks", "original_year", "INTEGER")?;
    ensure_column(conn, "tracks", "mb_recording_id", "TEXT")?;
    ensure_column(conn, "tracks", "mb_release_id", "TEXT")?;
    ensure_column(conn, "tracks", "mb_release_group_id", "TEXT")?;
    ensure_column(conn, "tracks", "mb_artist_id", "TEXT")?;
    ensure_column(conn, "tracks", "mb_release_artist_id", "TEXT")?;
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
    // #6: backfill the shadow columns from existing rows. One-off cost on
    // upgrade; idempotent (only rows where the source field is non-null and
    // the norm field is still null are touched).
    backfill_search_norms(conn)?;
    // #28: FTS5 trigram virtual tables + sync triggers. Must run AFTER the
    // `*_norm` columns exist (CREATE references them) AND after the backfill
    // (so 'rebuild' has the populated norm values to index). Idempotent via
    // `IF NOT EXISTS` on every CREATE; the trigger set is fixed-shape so a
    // second migrate() is a no-op.
    ensure_search_fts(conn)?;
    // #28: register per-connection UDFs (jaccard_trigram). Production paths
    // already register these via `db::pool`'s `with_init` hook, but tests
    // build a `Connection::open_in_memory()` directly and rely on migrate()
    // to surface a fully usable DB. `create_scalar_function` is idempotent
    // (re-registers replace the binding), so double-registration is fine.
    crate::db::udf::register(conn)?;
    // Write schema_version only after all ALTERs succeed. By **not writing on
    // pre-init or failure**, we keep the invariant: "migrate completed" ⇒ "schema
    // matches SCHEMA_VERSION".
    state_kv::set(conn, "schema_version", &SCHEMA_VERSION.to_string())?;
    Ok(())
}

/// #28: Create the FTS5 virtual tables and sync triggers, then `'rebuild'`
/// each index from the backing source so existing rows become searchable
/// immediately on upgrade. Rebuild is the official FTS5 way to repopulate
/// an external-content table (`https://sqlite.org/fts5.html#the_rebuild_command`)
/// and is fast at the 88k-track baseline (whole rebuild well under a second).
///
/// Idempotent: on a fresh DB the `CREATE VIRTUAL TABLE` lays out the indexes,
/// then `'rebuild'` runs against zero rows (no-op). On a second migrate the
/// `IF NOT EXISTS` short-circuits and 'rebuild' just refreshes — also fine.
fn ensure_search_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch(FTS5_SQL)?;
    // Repopulate from the source tables. Safe to run on every migrate: the
    // triggers above keep the index live, so the only cost here is a single
    // bulk scan on upgrade. Two separate statements (FTS5's rebuild command
    // only addresses one virtual table at a time).
    conn.execute("INSERT INTO albums_fts(albums_fts) VALUES('rebuild')", [])?;
    conn.execute("INSERT INTO tracks_fts(tracks_fts) VALUES('rebuild')", [])?;
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

/// #6: One-time backfill of NFKD shadow columns for upgraded DBs. Runs
/// inside `migrate()` after `ensure_column` has guaranteed the targets
/// exist. Touches only rows whose norm value is still NULL while the
/// source column is populated, so it is safe to call repeatedly.
fn backfill_search_norms(conn: &Connection) -> Result<()> {
    backfill_one(
        conn,
        "tracks",
        &[
            ("title", "title_norm"),
            ("artist", "artist_norm"),
            ("genre", "genre_norm"),
            ("composer", "composer_norm"),
            ("conductor", "conductor_norm"),
            ("performer", "performer_norm"),
        ],
    )?;
    backfill_one(
        conn,
        "albums",
        &[
            ("album", "album_norm"),
            ("effective_album_artist", "effective_album_artist_norm"),
        ],
    )?;
    Ok(())
}

/// SELECT id + the listed columns, normalize each `(src, dst)` pair in Rust,
/// and write back via batched UPDATEs. Caller-controlled column literals
/// (never user input) make the dynamic SQL safe.
fn backfill_one(conn: &Connection, table: &str, pairs: &[(&str, &str)]) -> Result<()> {
    // Skip rows that already have every norm column set or have nothing to
    // normalize (all source columns NULL). The WHERE condition lets the
    // common case — a fresh DB or a fully backfilled DB — short-circuit
    // without scanning the table.
    let any_null = pairs
        .iter()
        .map(|(src, dst)| format!("({src} IS NOT NULL AND {dst} IS NULL)"))
        .collect::<Vec<_>>()
        .join(" OR ");
    let src_cols = pairs.iter().map(|(s, _)| *s).collect::<Vec<_>>().join(", ");
    let select_sql = format!("SELECT id, {src_cols} FROM {table} WHERE {any_null}");
    let mut stmt = conn.prepare(&select_sql)?;
    let n_pairs = pairs.len();
    let rows: Vec<(i64, Vec<Option<String>>)> = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let mut srcs = Vec::with_capacity(n_pairs);
            for i in 0..n_pairs {
                srcs.push(row.get::<_, Option<String>>(i + 1)?);
            }
            Ok((id, srcs))
        })?
        .filter_map(|r| r.ok())
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    // `COALESCE(dst, ?N)` keeps any pre-existing value the caller wrote in by
    // hand and only fills NULLs. Makes the backfill safely re-runnable and
    // lets ops manually pin a column without losing it across upgrades.
    let set_clause = pairs
        .iter()
        .enumerate()
        .map(|(i, (_, dst))| format!("{dst} = COALESCE({dst}, ?{})", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let update_sql = format!(
        "UPDATE {table} SET {set_clause} WHERE id = ?{}",
        n_pairs + 1
    );
    let mut update = conn.prepare(&update_sql)?;
    for (id, srcs) in rows {
        let normed: Vec<Option<String>> = srcs
            .into_iter()
            .map(|s| s.map(|t| crate::normalize::for_search(&t)))
            .collect();
        let mut params: Vec<rusqlite::types::Value> = normed
            .into_iter()
            .map(|v| match v {
                Some(s) => rusqlite::types::Value::Text(s),
                None => rusqlite::types::Value::Null,
            })
            .collect();
        params.push(rusqlite::types::Value::Integer(id));
        update.execute(rusqlite::params_from_iter(&params))?;
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

    // ── #6: NFKD shadow columns + backfill ──────────────────────────────

    #[test]
    fn s8_norm_columns_exist_after_migrate() {
        let conn = open_in_memory_with_fk();
        for (table, column) in [
            ("tracks", "title_norm"),
            ("tracks", "artist_norm"),
            ("tracks", "genre_norm"),
            ("tracks", "composer_norm"),
            ("tracks", "conductor_norm"),
            ("tracks", "performer_norm"),
            ("albums", "album_norm"),
            ("albums", "effective_album_artist_norm"),
        ] {
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .unwrap();
            let cols: Vec<String> = stmt
                .query_map([], |r| r.get::<_, String>(1))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            assert!(
                cols.contains(&column.to_string()),
                "{table}.{column} missing: {cols:?}"
            );
        }
    }

    #[test]
    fn s9_backfill_populates_norm_from_existing_rows() {
        // Hand-build a pre-#6 DB (no *_norm), insert data, then migrate.
        // The backfill should fill the shadow columns from the source values.
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
             );
             INSERT INTO albums (id, effective_album_artist, album, first_seen_at)
               VALUES (1, 'Björk', 'Café', 0);
             INSERT INTO tracks
               (album_id, path, title, artist, added_at, mtime)
               VALUES (1, '/m/a.flac', 'Ｈｉｔ', 'ミユキ', 0, 0);",
        )
        .unwrap();

        migrate(&conn).unwrap();

        let (album_norm, aa_norm): (String, String) = conn
            .query_row(
                "SELECT album_norm, effective_album_artist_norm FROM albums WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(album_norm, "cafe");
        assert_eq!(aa_norm, "bjork");

        let (title_norm, artist_norm): (String, String) = conn
            .query_row(
                "SELECT title_norm, artist_norm FROM tracks WHERE path = '/m/a.flac'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(title_norm, "hit");
        assert_eq!(artist_norm, "みゆき");
    }

    #[test]
    fn s12_v8_capture_only_columns_exist_after_migrate() {
        // v8 added sort / original_year / MusicBrainz columns on tracks.
        // All nullable; verify they exist with the expected affinity.
        let conn = open_in_memory_with_fk();
        let mut stmt = conn.prepare("PRAGMA table_info(tracks)").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for (name, ty) in [
            ("artist_sort", "TEXT"),
            ("album_artist_sort", "TEXT"),
            ("album_sort", "TEXT"),
            ("title_sort", "TEXT"),
            ("composer_sort", "TEXT"),
            ("original_year", "INTEGER"),
            ("mb_recording_id", "TEXT"),
            ("mb_release_id", "TEXT"),
            ("mb_release_group_id", "TEXT"),
            ("mb_artist_id", "TEXT"),
            ("mb_release_artist_id", "TEXT"),
        ] {
            let row = cols.iter().find(|(n, _)| n == name);
            assert!(row.is_some(), "tracks.{name} missing");
            assert_eq!(row.unwrap().1, ty, "{name} affinity mismatch");
        }
    }

    #[test]
    fn s13_v8_columns_added_to_pre_v8_db_via_alter() {
        // Hand-build a pre-v8 schema (one without the capture-only columns) and
        // verify migrate() adds them via ALTER, preserving existing rows.
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
             );
             INSERT INTO albums (id, effective_album_artist, album, first_seen_at)
               VALUES (1, 'AA', 'Alb', 0);
             INSERT INTO tracks (album_id, path, added_at, mtime)
               VALUES (1, '/m/a.flac', 0, 0);",
        )
        .unwrap();
        migrate(&conn).unwrap();
        // New columns exist and existing row reads them back as NULL.
        let (asort, oy, mb_rec): (Option<String>, Option<i32>, Option<String>) = conn
            .query_row(
                "SELECT artist_sort, original_year, mb_recording_id FROM tracks WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(asort, None);
        assert_eq!(oy, None);
        assert_eq!(mb_rec, None);
    }

    #[test]
    fn s11_rg_columns_exist_after_migrate() {
        // #11: tracks gains 4 nullable REAL columns for ReplayGain values.
        let conn = open_in_memory_with_fk();
        let mut stmt = conn.prepare("PRAGMA table_info(tracks)").unwrap();
        let cols: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        for expected in [
            "rg_track_gain",
            "rg_track_peak",
            "rg_album_gain",
            "rg_album_peak",
        ] {
            let row = cols.iter().find(|(n, _)| n == expected);
            assert!(row.is_some(), "tracks.{expected} missing");
            assert_eq!(row.unwrap().1, "REAL", "{expected} should be REAL");
        }
    }

    #[test]
    fn s10_backfill_is_idempotent_and_skips_already_set() {
        // Second migrate() call must not overwrite — guarantees a future binary
        // can swap in a different normalize variant by re-running backfill only
        // for the new column without disturbing the others.
        let conn = open_in_memory_with_fk();
        conn.execute(
            "INSERT INTO albums (effective_album_artist, album, first_seen_at)
             VALUES ('AA', 'Alb', 0)",
            [],
        )
        .unwrap();
        // Manually trample one norm value to simulate a custom override; the
        // backfill should leave it alone (filter is `WHERE *_norm IS NULL`).
        conn.execute(
            "UPDATE albums SET album_norm = 'CUSTOM' WHERE album = 'Alb'",
            [],
        )
        .unwrap();
        // Re-run.
        migrate(&conn).unwrap();
        let v: String = conn
            .query_row(
                "SELECT album_norm FROM albums WHERE album = 'Alb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, "CUSTOM");
    }
}

use std::path::Path;

use r2d2_sqlite::SqliteConnectionManager;

use crate::error::Result;

pub mod albums;
pub mod config_overrides;
pub mod schema;
pub mod state_kv;
pub mod tracks;
pub mod udf;

/// r2d2 connection pool shared by HTTP handlers.
pub type Pool = r2d2::Pool<SqliteConnectionManager>;

/// Build the pool, install PRAGMAs (SPEC §3.3) on every connection via the init
/// hook, and run migration once.
///
/// PRAGMA rationale:
/// - `journal_mode = WAL`: Browse stays available during scan (concurrent reader/writer).
/// - `synchronous = NORMAL`: lowers fsync frequency to speed up scan (tail of WAL may
///   be lost on crash — acceptable).
/// - `foreign_keys = ON`: enables ON DELETE CASCADE from tracks to albums.
/// - `cache_size = -64000`: 64MB page cache (not excessive for a single LAN-server process).
/// - `busy_timeout = 5000`: waits 5s when another connection holds the writer, so we
///   never need to handle `SQLITE_BUSY` in code.
/// - `wal_autocheckpoint = 1000`: auto-checkpoint once WAL hits 1000 pages (~4MB),
///   keeping startup fast after a write-heavy scan.
pub fn pool(path: &Path) -> Result<Pool> {
    let manager = SqliteConnectionManager::file(path).with_init(|conn| {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA cache_size = -64000;
             PRAGMA busy_timeout = 5000;
             PRAGMA wal_autocheckpoint = 1000;",
        )?;
        // #28: per-connection UDFs used by fuzzy Search (jaccard_trigram).
        udf::register(conn)
    });
    let pool = r2d2::Pool::builder().max_size(8).build(manager)?;
    {
        let conn = pool.get()?;
        // ops §P1: downgrade guard. Must run **before** migrate()
        // (otherwise migrate() would overwrite the recorded value with its own SCHEMA_VERSION).
        schema::ensure_compatible_or_err(&conn)?;
        schema::migrate(&conn)?;
    }
    Ok(pool)
}

//! CRUD for the `config_overrides` table (issue #13).
//!
//! Each row stores a JSON-encoded value for a known catalog key
//! (see [`crate::config_catalog`]). The table layers user edits over
//! `config.toml` defaults at startup; the toml file remains a bootstrap default.

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM config_overrides WHERE key = ?1",
            params![key],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(value)
}

pub fn list_all(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT key, value FROM config_overrides")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .filter_map(|r| r.ok())
        .collect();
    Ok(rows)
}

pub fn set(conn: &Connection, key: &str, value: &str, now_secs: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO config_overrides (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![key, value, now_secs],
    )?;
    Ok(())
}

pub fn delete(conn: &Connection, key: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM config_overrides WHERE key = ?1", params![key])?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::migrate;

    fn open_in_memory() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    #[test]
    fn co1_get_missing_returns_none() {
        let conn = open_in_memory();
        assert_eq!(get(&conn, "browse.recently_added_limit").unwrap(), None);
    }

    #[test]
    fn co2_set_then_get() {
        let conn = open_in_memory();
        set(&conn, "browse.recently_added_limit", "75", 100).unwrap();
        assert_eq!(
            get(&conn, "browse.recently_added_limit")
                .unwrap()
                .as_deref(),
            Some("75")
        );
    }

    #[test]
    fn co3_set_overwrites_and_updates_timestamp() {
        let conn = open_in_memory();
        set(&conn, "k", "v1", 100).unwrap();
        set(&conn, "k", "v2", 200).unwrap();
        assert_eq!(get(&conn, "k").unwrap().as_deref(), Some("v2"));
        let updated_at: i64 = conn
            .query_row(
                "SELECT updated_at FROM config_overrides WHERE key = 'k'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(updated_at, 200);
    }

    #[test]
    fn co4_delete_returns_true_when_present() {
        let conn = open_in_memory();
        set(&conn, "k", "v", 0).unwrap();
        assert!(delete(&conn, "k").unwrap());
        assert_eq!(get(&conn, "k").unwrap(), None);
    }

    #[test]
    fn co5_delete_returns_false_when_missing() {
        let conn = open_in_memory();
        assert!(!delete(&conn, "nope").unwrap());
    }

    #[test]
    fn co6_list_all_returns_all_rows() {
        let conn = open_in_memory();
        set(&conn, "a", "1", 0).unwrap();
        set(&conn, "b", "2", 0).unwrap();
        let mut rows = list_all(&conn).unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("a".to_string(), "1".to_string()),
                ("b".to_string(), "2".to_string())
            ]
        );
    }
}

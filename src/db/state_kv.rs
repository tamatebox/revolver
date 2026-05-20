use rusqlite::{params, Connection, OptionalExtension};

use crate::error::Result;

pub fn get(conn: &Connection, key: &str) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM server_state WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value)
}

pub fn set(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO server_state (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
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
    fn k1_get_missing_returns_none() {
        let conn = open_in_memory();
        assert_eq!(get(&conn, "nope").unwrap(), None);
    }

    #[test]
    fn k2_set_then_get() {
        let conn = open_in_memory();
        set(&conn, "uuid", "abc").unwrap();
        assert_eq!(get(&conn, "uuid").unwrap().as_deref(), Some("abc"));
    }

    #[test]
    fn k3_set_overwrites() {
        let conn = open_in_memory();
        set(&conn, "k", "v1").unwrap();
        set(&conn, "k", "v2").unwrap();
        assert_eq!(get(&conn, "k").unwrap().as_deref(), Some("v2"));
    }
}

//! Shuffle state for the Random Albums view (SPEC §6.6).
//!
//! Holds an album_id array in `Mutex<Vec<i64>>` and fully reshuffles on startup,
//! scan completion, and `POST /admin/reshuffle`.
//!
//! Stale album_ids left in the array during transient states (e.g. mid-scan) do
//! not break anything: `browse::random` skips them on individual `SELECT`.

use std::sync::Mutex;

use rand::seq::SliceRandom;
use rusqlite::Connection;

use crate::error::Result;

pub struct RandomState {
    album_ids: Mutex<Vec<i64>>,
}

impl Default for RandomState {
    fn default() -> Self {
        Self::new()
    }
}

impl RandomState {
    pub fn new() -> Self {
        Self {
            album_ids: Mutex::new(Vec::new()),
        }
    }

    /// Refetch all album_ids, shuffle them, truncate to `limit`, and replace the
    /// internal state. Returns the new length.
    ///
    /// `limit` is the saved `browse.random_albums_limit` (SPEC §6.6). `None`
    /// means no cap — the entire shuffled population is kept. Capping at
    /// reshuffle time means `len()` / `page()` naturally reflect the user's cap
    /// without each caller having to clamp — and a re-roll picks a different
    /// random subset of `limit` albums out of the library each time.
    pub fn reshuffle(&self, conn: &Connection, limit: Option<usize>) -> Result<usize> {
        let mut stmt = conn.prepare_cached("SELECT id FROM albums")?;
        let mut ids: Vec<i64> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        ids.shuffle(&mut rand::thread_rng());
        if let Some(n) = limit {
            ids.truncate(n);
        }
        let len = ids.len();
        let mut guard = self.album_ids.lock().unwrap_or_else(|e| e.into_inner());
        *guard = ids;
        Ok(len)
    }

    /// Return a cloned `Vec` of the `[start, start+count)` slice of the array.
    /// Returns an empty `Vec` when out of range.
    pub fn page(&self, start: usize, count: usize) -> Vec<i64> {
        let guard = self.album_ids.lock().unwrap_or_else(|e| e.into_inner());
        if start >= guard.len() {
            return Vec::new();
        }
        let end = (start + count).min(guard.len());
        guard[start..end].to_vec()
    }

    /// Current array length (used as TotalMatches for `cat:random`).
    pub fn len(&self) -> usize {
        self.album_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Equivalent to `Vec::is_empty`. Provided to silence clippy.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{albums, schema};

    fn open() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON;").unwrap();
        schema::migrate(&conn).unwrap();
        conn
    }

    fn seed_n_albums(conn: &Connection, n: usize) {
        for i in 0..n {
            albums::upsert(
                conn,
                &albums::AlbumKey {
                    effective_album_artist: &format!("AA{}", i),
                    album: &format!("Alb{}", i),
                    compilation: false,
                },
                None,
                0,
            )
            .unwrap();
        }
    }

    #[test]
    fn ra1_new_page_returns_empty() {
        let s = RandomState::new();
        assert!(s.is_empty());
        assert_eq!(s.page(0, 10), Vec::<i64>::new());
    }

    #[test]
    fn ra2_after_reshuffle_page_contains_all_ids() {
        let conn = open();
        seed_n_albums(&conn, 5);
        let s = RandomState::new();
        let n = s.reshuffle(&conn, Some(1000)).unwrap();
        assert_eq!(n, 5);
        let mut got = s.page(0, 100);
        got.sort();
        assert_eq!(got, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn ra3_reshuffle_produces_different_order_with_high_probability() {
        // For 100 items, two shuffles producing the same order has probability 1/100! ≈ 0.
        let conn = open();
        seed_n_albums(&conn, 100);
        let s = RandomState::new();
        s.reshuffle(&conn, Some(1000)).unwrap();
        let first = s.page(0, 100);
        s.reshuffle(&conn, Some(1000)).unwrap();
        let second = s.page(0, 100);
        assert_ne!(first, second, "two shuffles should differ");
    }

    #[test]
    fn ra4_page_out_of_range_returns_empty_no_panic() {
        let conn = open();
        seed_n_albums(&conn, 3);
        let s = RandomState::new();
        s.reshuffle(&conn, Some(1000)).unwrap();
        assert_eq!(s.page(100, 10), Vec::<i64>::new());
        // Partial out-of-range is fine (start+count > len).
        let partial = s.page(1, 100);
        assert_eq!(partial.len(), 2);
    }

    #[test]
    fn ra5_empty_library_reshuffle_returns_zero() {
        let conn = open();
        let s = RandomState::new();
        let n = s.reshuffle(&conn, Some(1000)).unwrap();
        assert_eq!(n, 0);
        assert!(s.is_empty());
    }

    #[test]
    fn ra6_reshuffle_truncates_to_limit() {
        // 10 albums in DB, limit = 3 → state holds exactly 3 ids, drawn from the
        // shuffled population. `len()` reports the truncated size so `cat:random`
        // total_matches surfaces the cap.
        let conn = open();
        seed_n_albums(&conn, 10);
        let s = RandomState::new();
        let n = s.reshuffle(&conn, Some(3)).unwrap();
        assert_eq!(n, 3);
        assert_eq!(s.len(), 3);
        let ids = s.page(0, 100);
        assert_eq!(ids.len(), 3);
        // All retained ids must be valid album_ids (1..=10).
        for id in &ids {
            assert!((1..=10).contains(id), "stray id {} not in [1,10]", id);
        }
    }

    #[test]
    fn ra7_limit_greater_than_population_returns_full_population() {
        // Limit >> library size → just returns everything (no padding, no panic).
        let conn = open();
        seed_n_albums(&conn, 4);
        let s = RandomState::new();
        let n = s.reshuffle(&conn, Some(1000)).unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn ra8_none_limit_keeps_full_population() {
        // None = no cap; the full shuffled set should be retained.
        let conn = open();
        seed_n_albums(&conn, 7);
        let s = RandomState::new();
        let n = s.reshuffle(&conn, None).unwrap();
        assert_eq!(n, 7);
        assert_eq!(s.len(), 7);
    }
}

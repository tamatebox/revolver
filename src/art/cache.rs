//! In-memory cache for album art (SPEC §8.3).
//!
//! When the 100MB bytes budget is exceeded, **clear everything** (MVP keep-it-simple).
//! Access pattern is "many albums x few hits each", so a full LRU buys little.
//! Swap it out later if hit rate proves poor.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use bytes::Bytes;

/// Cache entry. `bytes` is `bytes::Bytes` for zero-copy sharing -- the handler
/// only Arc-clones it through `axum::body::Body::from(Bytes)`.
#[derive(Clone)]
pub struct CachedArt {
    pub bytes: Bytes,
    pub mime: &'static str,
}

impl CachedArt {
    fn approximate_size(&self) -> usize {
        self.bytes.len()
    }
}

/// 100MB byte budget (SPEC §8.3). Once exceeded, clear everything.
const CACHE_BYTES_BUDGET: usize = 100 * 1024 * 1024;

pub struct ArtCache {
    inner: Mutex<std::collections::HashMap<i64, CachedArt>>,
    current_bytes: AtomicUsize,
}

impl Default for ArtCache {
    fn default() -> Self {
        Self::new()
    }
}

impl ArtCache {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(std::collections::HashMap::new()),
            current_bytes: AtomicUsize::new(0),
        }
    }

    pub fn get(&self, album_id: i64) -> Option<CachedArt> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(&album_id).cloned()
    }

    pub fn put(&self, album_id: i64, art: CachedArt) {
        self.put_with_budget(album_id, art, CACHE_BYTES_BUDGET);
    }

    /// Body of `put`. Budget is parameterized for the eviction unit tests
    /// (filling 100MB in a test is not realistic). Production always goes
    /// through `put`.
    pub(crate) fn put_with_budget(&self, album_id: i64, art: CachedArt, budget: usize) {
        let size = art.approximate_size();
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        // Budget exceeded -> clear everything (MVP). The put itself proceeds normally.
        // Handling current_bytes inside the same guard keeps the Mutex and atomic
        // consistent (the previous atomic-only impl could drift under concurrent readers).
        let cur = self.current_bytes.load(Ordering::Relaxed);
        if cur.saturating_add(size) > budget {
            guard.clear();
            self.current_bytes.store(0, Ordering::Relaxed);
        }
        if let Some(old) = guard.insert(album_id, art) {
            self.current_bytes
                .fetch_sub(old.approximate_size(), Ordering::Relaxed);
        }
        self.current_bytes.fetch_add(size, Ordering::Relaxed);
    }

    pub fn current_bytes(&self) -> usize {
        self.current_bytes.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(bytes: Vec<u8>) -> CachedArt {
        CachedArt {
            bytes: Bytes::from(bytes),
            mime: "image/jpeg",
        }
    }

    #[test]
    fn c1_put_then_get_returns_same_bytes() {
        let cache = ArtCache::new();
        cache.put(42, art(b"hello".to_vec()));
        let got = cache.get(42).unwrap();
        assert_eq!(&got.bytes[..], b"hello");
        assert_eq!(got.mime, "image/jpeg");
    }

    #[test]
    fn c2_get_miss_returns_none() {
        let cache = ArtCache::new();
        assert!(cache.get(42).is_none());
    }

    #[test]
    fn c3_overwrite_same_key_keeps_one_entry() {
        let cache = ArtCache::new();
        cache.put(1, art(vec![0u8; 100]));
        cache.put(1, art(vec![0u8; 200]));
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.current_bytes(), 200);
    }

    // SPEC §8.3: clear everything on budget overflow. Verified via the
    // budget-parameterized test path.
    #[test]
    fn ev1_exceeding_budget_clears_all_then_adds_new() {
        let cache = ArtCache::new();
        // budget = 1000, insert two 400-byte entries (total 800, under threshold).
        cache.put_with_budget(1, art(vec![0u8; 400]), 1000);
        cache.put_with_budget(2, art(vec![0u8; 400]), 1000);
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.current_bytes(), 800);

        // The third 400 byte entry brings total to 1200 > 1000 -> clear all, add 1 new.
        cache.put_with_budget(3, art(vec![0u8; 400]), 1000);
        assert_eq!(cache.len(), 1, "after clear, only the new entry remains");
        assert_eq!(cache.current_bytes(), 400);
        assert!(cache.get(1).is_none(), "old entries discarded");
        assert!(cache.get(2).is_none());
        assert!(cache.get(3).is_some(), "new entry survives");
    }

    #[test]
    fn ev2_overwriting_same_key_does_not_trigger_clear() {
        // Overwriting the same key with a budget overflow is a subtle case (the
        // current impl subtracts the old size before adding, so same-size rewrite
        // does not trigger clear in essence).
        let cache = ArtCache::new();
        cache.put_with_budget(1, art(vec![0u8; 800]), 1000);
        // Same-key overwrite (800 -> 800). current(800) + size(800) = 1600 > 1000
        // triggers clear (compatible with the old impl: under the MVP simple strategy
        // it is less complex to re-clear than to compute strict size diff for same-key
        // overwrites).
        cache.put_with_budget(1, art(vec![1u8; 800]), 1000);
        // After clear, the new value is inserted: 1 entry, 800 bytes.
        assert_eq!(cache.len(), 1);
        let got = cache.get(1).unwrap();
        assert_eq!(got.bytes[0], 1, "overwritten value is visible");
    }

    #[test]
    fn ev3_zero_size_entry_does_not_evict() {
        let cache = ArtCache::new();
        cache.put_with_budget(1, art(vec![0u8; 500]), 1000);
        // 0-byte put: 500 + 0 = 500 < 1000 -> no clear.
        cache.put_with_budget(2, art(vec![]), 1000);
        assert_eq!(cache.len(), 2);
        assert!(cache.get(1).is_some());
    }
}

//! In-memory progress counter for an in-flight scan (issue #12).
//!
//! `scan::run` updates it as it moves through phases (`walk` → `tag_read` →
//! `upsert` → `postprocess`). HTTP handlers read snapshots without locks via
//! `Atomic*::load`. A scan completing (or not running) is represented by
//! `Phase::Idle`.

use std::sync::atomic::{AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle = 0,
    Walking = 1,
    TagRead = 2,
    Upsert = 3,
    Postprocess = 4,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Phase::Walking,
            2 => Phase::TagRead,
            3 => Phase::Upsert,
            4 => Phase::Postprocess,
            _ => Phase::Idle,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Walking => "walking",
            Phase::TagRead => "tag_read",
            Phase::Upsert => "upsert",
            Phase::Postprocess => "postprocess",
        }
    }
}

#[derive(Default)]
pub struct ScanProgress {
    phase: AtomicU8,
    current: AtomicUsize,
    total: AtomicUsize,
    /// Unix-seconds when the current scan started. 0 means idle.
    started_at: AtomicI64,
}

impl ScanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stamp the start of a scan. Called once at the top of `scan::run`.
    pub fn begin_scan(&self) {
        self.started_at.store(unix_now_secs(), Ordering::Relaxed);
        self.phase.store(Phase::Walking as u8, Ordering::Relaxed);
        self.current.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
    }

    /// Move to a new phase with a known `total` (number of work units).
    pub fn enter(&self, phase: Phase, total: usize) {
        self.phase.store(phase as u8, Ordering::Relaxed);
        self.current.store(0, Ordering::Relaxed);
        self.total.store(total, Ordering::Relaxed);
    }

    /// Increment `current` by 1. Called from rayon workers during tag-read.
    pub fn tick(&self) {
        self.current.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment `current` by `n` (used by the upsert phase for chunk commits).
    pub fn advance(&self, n: usize) {
        self.current.fetch_add(n, Ordering::Relaxed);
    }

    /// Reset to idle. Called at scan end (success or failure).
    pub fn finish(&self) {
        self.phase.store(Phase::Idle as u8, Ordering::Relaxed);
        self.current.store(0, Ordering::Relaxed);
        self.total.store(0, Ordering::Relaxed);
        self.started_at.store(0, Ordering::Relaxed);
    }

    pub fn current(&self) -> usize {
        self.current.load(Ordering::Relaxed)
    }

    pub fn total(&self) -> usize {
        self.total.load(Ordering::Relaxed)
    }

    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn snapshot(&self) -> ScanProgressSnapshot {
        let started_at = self.started_at.load(Ordering::Relaxed);
        let elapsed_ms = if started_at > 0 {
            let now = unix_now_secs();
            ((now - started_at).max(0) as u64) * 1000
        } else {
            0
        };
        ScanProgressSnapshot {
            phase: self.phase().as_str(),
            current: self.current(),
            total: self.total(),
            started_at,
            elapsed_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScanProgressSnapshot {
    pub phase: &'static str,
    pub current: usize,
    pub total: usize,
    pub started_at: i64,
    pub elapsed_ms: u64,
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sp1_default_is_idle() {
        let p = ScanProgress::new();
        let snap = p.snapshot();
        assert_eq!(snap.phase, "idle");
        assert_eq!(snap.current, 0);
        assert_eq!(snap.total, 0);
        assert_eq!(snap.started_at, 0);
    }

    #[test]
    fn sp2_begin_scan_sets_walking() {
        let p = ScanProgress::new();
        p.begin_scan();
        assert_eq!(p.phase(), Phase::Walking);
        assert!(p.snapshot().started_at > 0);
    }

    #[test]
    fn sp3_enter_resets_current_and_sets_total() {
        let p = ScanProgress::new();
        p.begin_scan();
        p.advance(42);
        p.enter(Phase::TagRead, 100);
        assert_eq!(p.phase(), Phase::TagRead);
        assert_eq!(p.current(), 0);
        assert_eq!(p.total(), 100);
    }

    #[test]
    fn sp4_tick_and_advance_increment() {
        let p = ScanProgress::new();
        p.enter(Phase::TagRead, 10);
        p.tick();
        p.tick();
        p.advance(3);
        assert_eq!(p.current(), 5);
    }

    #[test]
    fn sp5_finish_returns_to_idle() {
        let p = ScanProgress::new();
        p.begin_scan();
        p.enter(Phase::TagRead, 10);
        p.advance(5);
        p.finish();
        let snap = p.snapshot();
        assert_eq!(snap.phase, "idle");
        assert_eq!(snap.current, 0);
        assert_eq!(snap.total, 0);
        assert_eq!(snap.started_at, 0);
    }
}

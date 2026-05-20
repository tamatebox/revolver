use std::time::{SystemTime, UNIX_EPOCH};

/// Compute the `effective_album_artist` used as the albums-table matching
/// key from each track's (compilation, album_artist, artist) (SPEC §3.2).
///
/// Priority:
/// 1. compilation flag set → "Various Artists"
/// 2. album_artist tag non-empty → album_artist
/// 3. otherwise artist non-empty → artist
/// 4. otherwise → "Unknown Artist"
pub fn effective_album_artist(
    compilation: bool,
    album_artist: Option<&str>,
    artist: Option<&str>,
) -> String {
    if compilation {
        return "Various Artists".to_string();
    }
    if let Some(s) = album_artist.filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    if let Some(s) = artist.filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    "Unknown Artist".to_string()
}

/// Determine `added_at` (SPEC §4.2).
///
/// - Initial scan: use `min(btime, mtime)` (or `now` if absent). Future
///   timestamps (corrupt tags or clock skew) are clamped to `now`.
/// - Subsequent scans (newly discovered path): `now`.
///
/// Returns Unix epoch seconds (the storage format of `tracks.added_at`).
pub fn determine_added_at(
    file_btime: Option<SystemTime>,
    file_mtime: Option<SystemTime>,
    is_initial_scan: bool,
    now: SystemTime,
) -> i64 {
    let now_secs = to_unix_secs(now);
    if !is_initial_scan {
        return now_secs;
    }
    let candidate = match (file_btime, file_mtime) {
        (Some(b), Some(m)) => b.min(m),
        (Some(b), None) => b,
        (None, Some(m)) => m,
        (None, None) => now,
    };
    to_unix_secs(candidate).min(now_secs)
}

fn to_unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ── effective_album_artist ───────────────────────────────────────────

    #[test]
    fn m1_compilation_overrides_everything() {
        assert_eq!(
            effective_album_artist(true, Some("Real AA"), Some("Real A")),
            "Various Artists"
        );
    }

    #[test]
    fn m2_album_artist_takes_precedence_over_artist() {
        assert_eq!(effective_album_artist(false, Some("AA"), Some("A")), "AA");
    }

    #[test]
    fn m3_falls_back_to_artist_when_album_artist_empty_or_missing() {
        assert_eq!(effective_album_artist(false, None, Some("A")), "A");
        assert_eq!(effective_album_artist(false, Some(""), Some("A")), "A");
    }

    #[test]
    fn m4_unknown_when_all_empty_or_missing() {
        assert_eq!(effective_album_artist(false, None, None), "Unknown Artist");
        assert_eq!(
            effective_album_artist(false, Some(""), Some("")),
            "Unknown Artist"
        );
    }

    // ── determine_added_at ───────────────────────────────────────────────

    fn ts(now: SystemTime, t: SystemTime) -> i64 {
        let _ = now;
        t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64
    }

    #[test]
    fn m5_initial_with_both_returns_min() {
        let now = SystemTime::now();
        let btime = now - Duration::from_secs(1000);
        let mtime = now - Duration::from_secs(500);
        let got = determine_added_at(Some(btime), Some(mtime), true, now);
        assert_eq!(got, ts(now, btime), "expected min(btime, mtime) = btime");
    }

    #[test]
    fn m6_initial_with_only_btime() {
        let now = SystemTime::now();
        let btime = now - Duration::from_secs(1000);
        let got = determine_added_at(Some(btime), None, true, now);
        assert_eq!(got, ts(now, btime));
    }

    #[test]
    fn m7_not_initial_returns_now() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(10_000);
        let got = determine_added_at(Some(old), Some(old), false, now);
        assert_eq!(got, ts(now, now));
    }

    #[test]
    fn m8_initial_future_btime_is_clamped_to_now() {
        let now = SystemTime::now();
        let future = now + Duration::from_secs(86_400);
        let got = determine_added_at(Some(future), None, true, now);
        assert_eq!(got, ts(now, now), "future timestamp must clamp to now");
    }
}

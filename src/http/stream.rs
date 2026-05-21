use std::io::SeekFrom;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::db;
use crate::http::HttpError;
use crate::state::AppState;

/// Parsed Range. Not a half-open interval over a `total`-byte resource, but
/// **inclusive** offset / length (SPEC §8.2).
#[derive(Debug, PartialEq, Eq)]
struct ResolvedRange {
    start: u64,
    /// Bytes to deliver (= `end - start + 1`).
    length: u64,
}

/// Result of parsing the Range header. `Ok(None)` means "no header, deliver whole file".
#[derive(Debug, PartialEq, Eq)]
enum RangeParse {
    /// No Range header → 200 with the whole file.
    None,
    /// Valid range. 206 partial content.
    Ok(ResolvedRange),
    /// Out of range (`N >= total`, etc.). 416 + `Content-Range: bytes */TOTAL`.
    Unsatisfiable,
    /// Syntactically invalid. 400.
    Malformed,
}

/// SPEC §8.2: three forms `bytes=N-M` / `bytes=N-` / `bytes=-N` plus malformed and unsatisfiable.
fn parse_range(header: Option<&str>, total: u64) -> RangeParse {
    let raw = match header {
        None => return RangeParse::None,
        Some(s) => s.trim(),
    };
    let spec = match raw.strip_prefix("bytes=") {
        Some(s) => s.trim(),
        None => return RangeParse::Malformed,
    };
    // multipart range (`bytes=0-1,5-9`) is not accepted in MVP.
    if spec.contains(',') {
        return RangeParse::Malformed;
    }
    let (lhs, rhs) = match spec.split_once('-') {
        Some(pair) => pair,
        None => return RangeParse::Malformed,
    };
    let lhs = lhs.trim();
    let rhs = rhs.trim();

    match (lhs.is_empty(), rhs.is_empty()) {
        // `bytes=-N` suffix range
        (true, false) => {
            let n: u64 = match rhs.parse() {
                Ok(v) => v,
                Err(_) => return RangeParse::Malformed,
            };
            if n == 0 || total == 0 {
                return RangeParse::Unsatisfiable;
            }
            let n = n.min(total);
            RangeParse::Ok(ResolvedRange {
                start: total - n,
                length: n,
            })
        }
        // `bytes=N-` open-ended
        (false, true) => {
            let start: u64 = match lhs.parse() {
                Ok(v) => v,
                Err(_) => return RangeParse::Malformed,
            };
            if start >= total {
                return RangeParse::Unsatisfiable;
            }
            RangeParse::Ok(ResolvedRange {
                start,
                length: total - start,
            })
        }
        // `bytes=N-M` closed
        (false, false) => {
            let start: u64 = match lhs.parse() {
                Ok(v) => v,
                Err(_) => return RangeParse::Malformed,
            };
            let end: u64 = match rhs.parse() {
                Ok(v) => v,
                Err(_) => return RangeParse::Malformed,
            };
            if start > end {
                return RangeParse::Malformed;
            }
            if start >= total {
                return RangeParse::Unsatisfiable;
            }
            let end = end.min(total - 1);
            RangeParse::Ok(ResolvedRange {
                start,
                length: end - start + 1,
            })
        }
        // `bytes=-` (both sides empty)
        (true, true) => RangeParse::Malformed,
    }
}

/// Decide whether a Range counts as a "play start" (SPEC §6.8).
///
/// - `None` → count (no Range = whole-file delivery).
/// - `Ok(r)` with `r.start == 0` → count (`bytes=0-N` / `bytes=0-`).
/// - Otherwise (`bytes=N-` with N>0 / `bytes=-N` suffix / Malformed / Unsatisfiable) → do not count.
fn is_play_start(parsed: &RangeParse) -> bool {
    matches!(parsed, RangeParse::None) || matches!(parsed, RangeParse::Ok(r) if r.start == 0)
}

/// Increment `tracks.play_count` and set `tracks.last_played_at = now` (SPEC §6.8).
/// Also bump the corresponding `albums.last_played_at` to remove the GROUP BY
/// from `cat:played` Browse (perf §P0).
/// Failures do not break stream delivery (`tracing::warn!` only).
fn bump_play_stats(state: &AppState, track_id: i64) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let pool = state.db_pool.clone();
    match pool.get() {
        Ok(conn) => {
            // Run two statements on one connection. Wrapping both in a transaction
            // is not required: a single failure only logs a warning, so impact is limited.
            let updated = conn.execute(
                "UPDATE tracks SET play_count = play_count + 1, last_played_at = ?1 WHERE id = ?2",
                rusqlite::params![now, track_id],
            );
            match updated {
                Ok(_) => {
                    // Fetch album_id and bump albums.last_played_at as well.
                    let album_id: Result<i64, _> = conn.query_row(
                        "SELECT album_id FROM tracks WHERE id = ?1",
                        rusqlite::params![track_id],
                        |r| r.get(0),
                    );
                    if let Ok(aid) = album_id {
                        if let Err(e) =
                            crate::db::albums::bump_album_last_played_at(&conn, aid, now)
                        {
                            tracing::warn!(
                                album_id = aid,
                                error = %e,
                                "failed to bump album last_played_at"
                            );
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "failed to update play stats"),
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to get db conn for play stats"),
    }
}

/// `GET /stream/{track_id}` — Range-capable audio file delivery (SPEC §8.2).
#[tracing::instrument(name = "stream", skip(state, headers), fields(track_id))]
pub async fn stream(
    Path(track_id): Path<i64>,
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, HttpError> {
    let track = {
        let conn = state.db_pool.get()?;
        db::tracks::lookup_by_id(&conn, track_id)?
    };
    let track = match track {
        Some(t) => t,
        None => return Err(HttpError::NotFound),
    };

    // Defense-in-depth: the walker also clamps symlink targets inside library_root,
    // but a TOCTOU rewrite after scan could repoint the target outside.
    // Canonicalize before opening and verify the library_root prefix (security §1).
    if !path_within_library(&track.path, &state.library_root).await {
        tracing::warn!(
            path = %track.path.display(),
            "track path resolves outside library_root; refusing to serve"
        );
        return Err(HttpError::NotFound);
    }

    let total = track.file_size;
    let mime = track.mime_type.clone();

    let range_header = headers.get("range").and_then(|v| v.to_str().ok());
    let parsed = parse_range(range_header, total);

    // SPEC §6.8: treat requests with no Range or start=0 as "play start" and
    // update play_count / last_played_at. Linn pre-fetches (suffix / N-) are excluded.
    if is_play_start(&parsed) {
        bump_play_stats(&state, track_id);
    }

    match parsed {
        RangeParse::Malformed => Ok((
            StatusCode::BAD_REQUEST,
            [
                ("accept-ranges", "bytes".to_string()),
                ("content-type", mime),
            ],
            "malformed range header",
        )
            .into_response()),
        RangeParse::Unsatisfiable => Ok((
            StatusCode::RANGE_NOT_SATISFIABLE,
            [
                ("accept-ranges", "bytes".to_string()),
                ("content-type", mime),
                ("content-range", format!("bytes */{}", total)),
            ],
            "range not satisfiable",
        )
            .into_response()),
        RangeParse::None => {
            let body = open_body(&track.path, 0, total).await?;
            Ok((
                StatusCode::OK,
                [
                    ("accept-ranges", "bytes".to_string()),
                    ("content-type", mime),
                    ("content-length", total.to_string()),
                ],
                body,
            )
                .into_response())
        }
        RangeParse::Ok(r) => {
            let body = open_body(&track.path, r.start, r.length).await?;
            let end = r.start + r.length - 1;
            Ok((
                StatusCode::PARTIAL_CONTENT,
                [
                    ("accept-ranges", "bytes".to_string()),
                    ("content-type", mime),
                    ("content-length", r.length.to_string()),
                    (
                        "content-range",
                        format!("bytes {}-{}/{}", r.start, end, total),
                    ),
                ],
                body,
            )
                .into_response())
        }
    }
}

/// Canonicalize `path` and check it falls inside the already-canonicalized
/// `library_root` prefix. `library_root` is canonicalized once at `AppState`
/// construction (`main.rs` / each test helper), so here we canonicalize only
/// the track path (ops §P1, halving per-request syscalls).
/// Returns `false` on failure (canonicalize error / out of range).
async fn path_within_library(path: &std::path::Path, library_root: &std::path::Path) -> bool {
    let canonical_path = match tokio::fs::canonicalize(path).await {
        Ok(p) => p,
        Err(_) => return false,
    };
    canonical_path.starts_with(library_root)
}

/// Open `path` and return a `Body` for `length` bytes starting at `start`.
async fn open_body(path: &std::path::Path, start: u64, length: u64) -> Result<Body, HttpError> {
    let mut file = tokio::fs::File::open(path)
        .await
        .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))?;
    if start > 0 {
        file.seek(SeekFrom::Start(start))
            .await
            .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))?;
    }
    let limited = file.take(length);
    Ok(Body::from_stream(ReaderStream::new(limited)))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rusqlite::params;
    use tempfile::{NamedTempFile, TempDir};
    use tower::ServiceExt;

    use crate::state::test_helpers::test_state;
    use crate::state::AppState;

    /// Create a fixed 100-byte file and insert `(album_id=1, path, ...)` into tracks.
    /// Returns (state, db tmpdir guard, NamedTempFile guard, track_id).
    /// To keep the path_within_library check from triggering in the working tree,
    /// the audio file is created **inside the same tmpdir as library_root**.
    fn setup() -> (AppState, TempDir, NamedTempFile, i64) {
        let (state, dbdir) = test_state();

        // Write the fixed byte sequence 0..100. Placing it under library_root (= dbdir)
        // ensures the stream handler's path_within_library check passes.
        let mut tmpfile = tempfile::Builder::new()
            .prefix("audio")
            .suffix(".bin")
            .tempfile_in(dbdir.path())
            .unwrap();
        let payload: Vec<u8> = (0..100u8).collect();
        tmpfile.write_all(&payload).unwrap();
        tmpfile.flush().unwrap();
        let path_str = tmpfile.path().to_string_lossy().into_owned();

        {
            let conn = state.db_pool.get().unwrap();
            // Insert one parent album (required by FK constraint).
            conn.execute(
                "INSERT INTO albums (effective_album_artist, album, compilation, first_seen_at)
                 VALUES ('AA', 'Alb', 0, 0)",
                [],
            )
            .unwrap();
            let album_id: i64 = conn
                .query_row("SELECT id FROM albums LIMIT 1", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO tracks (
                   album_id, path, codec, mime_type, file_size, added_at, mtime
                 ) VALUES (?1, ?2, 'flac', 'audio/flac', 100, 0, 0)",
                params![album_id, path_str],
            )
            .unwrap();
        }
        let track_id: i64 = {
            let conn = state.db_pool.get().unwrap();
            conn.query_row("SELECT id FROM tracks LIMIT 1", [], |r| r.get(0))
                .unwrap()
        };

        (state, dbdir, tmpfile, track_id)
    }

    async fn body_bytes(resp: axum::http::Response<Body>) -> Vec<u8> {
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec()
    }

    #[tokio::test]
    async fn st1_no_range_returns_200_full_body() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("accept-ranges").unwrap(), "bytes");
        assert_eq!(resp.headers().get("content-type").unwrap(), "audio/flac");
        assert_eq!(resp.headers().get("content-length").unwrap(), "100");
        let body = body_bytes(resp).await;
        assert_eq!(body.len(), 100);
        assert_eq!(body[0], 0);
        assert_eq!(body[99], 99);
    }

    #[tokio::test]
    async fn st2_closed_range_returns_206() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=0-9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            "bytes 0-9/100"
        );
        assert_eq!(resp.headers().get("content-length").unwrap(), "10");
        let body = body_bytes(resp).await;
        assert_eq!(body, (0..10u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn st3_open_ended_range_returns_206() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=10-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            "bytes 10-99/100"
        );
        let body = body_bytes(resp).await;
        assert_eq!(body.len(), 90);
        assert_eq!(body[0], 10);
        assert_eq!(body[89], 99);
    }

    #[tokio::test]
    async fn st4_suffix_range_returns_last_n_bytes() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=-10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(
            resp.headers().get("content-range").unwrap(),
            "bytes 90-99/100"
        );
        let body = body_bytes(resp).await;
        assert_eq!(body, (90..100u8).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn st5_out_of_range_returns_416() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=100-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(resp.headers().get("content-range").unwrap(), "bytes */100");
    }

    #[tokio::test]
    async fn st6_unknown_track_returns_404() {
        let (state, _db, _f, _tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/stream/99999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn st8_path_outside_library_root_returns_404() {
        // When tracks.path contains an absolute path outside library_root,
        // the stream handler must return 404 (security §1, path traversal prevention).
        let (state, dbdir) = test_state();

        // Mimic the attack scenario: create the audio file **outside** library_root.
        let outside = NamedTempFile::new().unwrap(); // OS TMPDIR, outside library_root.
        let payload: Vec<u8> = (0..50u8).collect();
        std::fs::write(outside.path(), &payload).unwrap();
        let outside_path = outside.path().to_string_lossy().into_owned();

        let track_id: i64 = {
            let conn = state.db_pool.get().unwrap();
            conn.execute(
                "INSERT INTO albums (effective_album_artist, album, compilation, first_seen_at)
                 VALUES ('AA','Alb',0,0)",
                [],
            )
            .unwrap();
            let album_id: i64 = conn
                .query_row("SELECT id FROM albums LIMIT 1", [], |r| r.get(0))
                .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, codec, mime_type, file_size, added_at, mtime)
                 VALUES (?1, ?2, 'flac', 'audio/flac', 50, 0, 0)",
                rusqlite::params![album_id, outside_path],
            )
            .unwrap();
            conn.query_row("SELECT id FROM tracks LIMIT 1", [], |r| r.get(0))
                .unwrap()
        };

        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", track_id))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Path is outside library_root → 404 (the file exists, but we don't serve it).
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Keep dbdir / outside alive until they are dropped.
        drop(dbdir);
        drop(outside);
    }

    #[test]
    fn range_edge_total_zero_makes_any_range_unsatisfiable() {
        // Against an empty file (total=0), every Range is unsatisfiable.
        assert!(matches!(
            super::parse_range(Some("bytes=0-9"), 0),
            super::RangeParse::Unsatisfiable
        ));
        assert!(matches!(
            super::parse_range(Some("bytes=0-"), 0),
            super::RangeParse::Unsatisfiable
        ));
        assert!(matches!(
            super::parse_range(Some("bytes=-10"), 0),
            super::RangeParse::Unsatisfiable
        ));
    }

    #[test]
    fn range_edge_multi_range_is_rejected() {
        // SPEC §6.8: multipart range (`bytes=0-9,20-29`) is not accepted in MVP.
        // Linn only uses a single range, so accidentally appearing to support
        // it would confuse partial-delivery logic. Always reject as Malformed
        // before actual audio delivery.
        assert!(matches!(
            super::parse_range(Some("bytes=0-9,20-29"), 100),
            super::RangeParse::Malformed
        ));
        // Three ranges, mixed suffix, or whitespace-laden are all Malformed.
        assert!(matches!(
            super::parse_range(Some("bytes=0-9,20-29,50-59"), 100),
            super::RangeParse::Malformed
        ));
        assert!(matches!(
            super::parse_range(Some("bytes=0-9, -10"), 100),
            super::RangeParse::Malformed
        ));
    }

    #[test]
    fn range_edge_missing_bytes_prefix_is_malformed() {
        // Units other than `bytes=` (`items=`, `chunks=`, etc.) are Malformed.
        assert!(matches!(
            super::parse_range(Some("items=0-9"), 100),
            super::RangeParse::Malformed
        ));
        assert!(matches!(
            super::parse_range(Some("0-9"), 100),
            super::RangeParse::Malformed
        ));
        assert!(matches!(
            super::parse_range(Some(""), 100),
            super::RangeParse::Malformed
        ));
    }

    #[test]
    fn range_edge_end_beyond_total_is_clamped() {
        // bytes=0-999999 with total=100 → end clamps to 99, length=100.
        if let super::RangeParse::Ok(r) = super::parse_range(Some("bytes=0-999999"), 100) {
            assert_eq!(r.start, 0);
            assert_eq!(r.length, 100);
        } else {
            panic!("expected Ok with clamped end");
        }
    }

    #[test]
    fn range_edge_suffix_larger_than_total_clamps_to_full_file() {
        // bytes=-999 with total=100 → return all 100 bytes (start=0, length=100).
        if let super::RangeParse::Ok(r) = super::parse_range(Some("bytes=-999"), 100) {
            assert_eq!(r.start, 0);
            assert_eq!(r.length, 100);
        } else {
            panic!("expected Ok with full-file suffix");
        }
    }

    #[tokio::test]
    async fn st7_malformed_range_returns_400() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=abc")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    // ── Recently Played: stream hit counting (SPEC §6.8) ──────────────────

    fn play_count_of(state: &AppState, track_id: i64) -> (i64, Option<i64>) {
        let conn = state.db_pool.get().unwrap();
        conn.query_row(
            "SELECT play_count, last_played_at FROM tracks WHERE id = ?1",
            rusqlite::params![track_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn pc1_no_range_increments_play_count() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let (pc, lp) = play_count_of(&state, tid);
        assert_eq!(pc, 1);
        assert!(lp.is_some(), "last_played_at must be set");
    }

    #[tokio::test]
    async fn pc2_range_starting_at_zero_increments_play_count() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state.clone());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=0-9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let (pc, _) = play_count_of(&state, tid);
        assert_eq!(pc, 1);
    }

    #[tokio::test]
    async fn pc3_non_zero_start_range_does_not_count() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state.clone());
        // `bytes=N-` request mimicking Linn's gapless pre-fetch.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=10-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let (pc, lp) = play_count_of(&state, tid);
        assert_eq!(pc, 0, "non-zero start range must not count as play start");
        assert!(lp.is_none());
    }

    #[tokio::test]
    async fn pc4_suffix_range_does_not_count() {
        let (state, _db, _f, tid) = setup();
        let app = crate::http::router(state.clone());
        // `bytes=-N` request mimicking Linn's suffix pre-fetch.
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(format!("/stream/{}", tid))
                    .header("range", "bytes=-10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PARTIAL_CONTENT);
        let (pc, _) = play_count_of(&state, tid);
        assert_eq!(pc, 0, "suffix range must not count as play start");
    }

    #[tokio::test]
    async fn pc5_multiple_plays_increment_correctly() {
        let (state, _db, _f, tid) = setup();
        // Three plays (one is a pre-fetch and does not count).
        for range in [None, Some("bytes=0-99"), Some("bytes=50-")] {
            let app = crate::http::router(state.clone());
            let mut builder = Request::builder().uri(format!("/stream/{}", tid));
            if let Some(r) = range {
                builder = builder.header("range", r);
            }
            let resp = app
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert!(resp.status().is_success() || resp.status() == StatusCode::PARTIAL_CONTENT);
        }
        let (pc, _) = play_count_of(&state, tid);
        assert_eq!(pc, 2, "2 plays should count (no Range + bytes=0-99)");
    }

    // ── proptest: parse_range never panics on any input, and Ok bounds hold the invariants ──
    proptest::proptest! {
        /// `parse_range` does not panic on any Range header string.
        #[test]
        fn rp1_parse_range_never_panics(header in proptest::option::of(".*"), total in 0u64..=u64::MAX) {
            let _ = super::parse_range(header.as_deref(), total);
        }

        /// When `RangeParse::Ok` is returned, `start + length <= total` and `length > 0`.
        /// Breaking this could cause an out-of-bounds read or an empty-body 206 in stream.
        #[test]
        fn rp2_ok_range_bounds_invariant(
            header in "bytes=[0-9]{0,6}-[0-9]{0,6}",
            total in 1u64..1_000_000
        ) {
            // None/Unsatisfiable/Malformed are out of scope.
            if let super::RangeParse::Ok(r) = super::parse_range(Some(&header), total) {
                proptest::prop_assert!(r.length > 0);
                proptest::prop_assert!(r.start.saturating_add(r.length) <= total);
            }
        }
    }
}

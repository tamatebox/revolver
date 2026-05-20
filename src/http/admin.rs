use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::db::state_kv;
use crate::http::HttpError;
use crate::scan::report::ScanReport;
use crate::state::AppState;

/// Single-page HTML for the Web admin UI (SPEC §8.4). CSS / JS are inline, no dependencies.
/// Embedded in the binary, so no separate static file server is needed.
const ADMIN_UI_HTML: &str = include_str!("admin_ui.html");

/// `GET /admin/ui` and `GET /admin/` — return the admin UI as a single HTML page
/// (SPEC §8.4). Polls `/admin/stats` to display state and triggers scan / reshuffle
/// via buttons.
pub async fn ui() -> Response {
    (
        StatusCode::OK,
        [("content-type", "text/html; charset=utf-8")],
        ADMIN_UI_HTML,
    )
        .into_response()
}

/// `GET /admin/scan-report` — return the JSON saved in
/// `server_state.last_scan_report` as-is. 404 if no scan has ever run.
pub async fn scan_report(State(state): State<AppState>) -> Result<Response, HttpError> {
    let conn = state.db_pool.get()?;
    let json = state_kv::get(&conn, "last_scan_report")?;
    drop(conn);

    match json {
        None => Err(HttpError::NotFound),
        Some(text) => {
            Ok((StatusCode::OK, [("content-type", "application/json")], text).into_response())
        }
    }
}

/// `POST /admin/rescan` — start a scan, wait for completion, return the ScanReport JSON.
/// 409 if `scan_lock` is already held.
pub async fn rescan(State(state): State<AppState>) -> Result<Json<ScanReport>, HttpError> {
    let permit = state
        .scan_lock
        .clone()
        .try_acquire_owned()
        .map_err(|_| HttpError::Conflict("scan already running"))?;

    let library_root = state.library_root.clone();
    let extensions = state.extensions.clone();
    let parallel = state.scan_parallel;
    let pool = state.db_pool.clone();

    let report = tokio::task::spawn_blocking(move || -> crate::error::Result<ScanReport> {
        let _permit = permit; // Hold the permit until the task completes.
        let mut conn = pool.get()?;
        crate::scan::run(&mut conn, &library_root, &extensions, parallel)
    })
    .await
    .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))??;

    // SPEC §5.1 / §9.6: if there was a structural change, deliver a SystemUpdateID
    // propchange NOTIFY to CD subscribers (the value is already bumped inside scan::run).
    if crate::scan::should_bump_system_update_id(&report.stats) {
        let conn = state.db_pool.get()?;
        let id = state_kv::get(&conn, "system_update_id")?.unwrap_or_else(|| "1".to_string());
        drop(conn);
        crate::upnp::gena::broadcast_propchange(
            &state.notify_client,
            &state.subscriptions,
            &state.notify_tasks,
            crate::upnp::gena::ServiceId::ContentDirectory,
            &[("SystemUpdateID", &id)],
        )
        .await;

        // On structural changes, reorder Random as well (SPEC §6.6).
        let conn = state.db_pool.get()?;
        let n = state.random_state.reshuffle(&conn)?;
        tracing::info!(albums = n, "post-rescan random reshuffle complete");
    }

    Ok(Json(report))
}

#[derive(Serialize)]
pub struct ReshuffleResponse {
    pub shuffled: usize,
}

/// Response body for `/admin/stats` (SPEC §8.5).
#[derive(Serialize)]
pub struct StatsResponse {
    pub albums_total: i64,
    pub tracks_total: i64,
    pub total_duration_ms: i64,
    pub quality_breakdown: QualityBreakdown,
    pub plays_total: i64,
    pub played_albums_total: i64,
    pub last_scan: Option<LastScan>,
    pub system_update_id: u32,
    pub uptime_seconds: i64,
    // ── Observability (ops §P1): distinguish "Linn can't see us" from "album count dropped".
    /// Current number of held GENA subscriptions.
    pub gena_subscribers: usize,
    /// Whether the SSDP listener task is running (used to detect port 1900 conflicts).
    pub ssdp_listener_active: bool,
    /// Whether the SSDP advertiser task is running.
    pub ssdp_advertiser_active: bool,
    /// Art cache state. Look here when observing evictions.
    pub art_cache: ArtCacheStats,
    /// Length of the random albums array (updated by post-scan reshuffle).
    pub random_albums_buffered: usize,
}

#[derive(Serialize)]
pub struct ArtCacheStats {
    pub entries: usize,
    pub bytes: usize,
}

#[derive(Serialize, Default)]
pub struct QualityBreakdown {
    pub hires: i64,
    pub lossless: i64,
    pub lossy: i64,
    pub mixed: i64,
    pub unknown: i64,
}

#[derive(Serialize)]
pub struct LastScan {
    pub scan_id: String,
    pub completed_at: i64,
}

/// `GET /admin/stats` — return server-wide summary as JSON (SPEC §8.5).
/// Underlying endpoint that the subsequent Web admin UI (step 18) calls.
pub async fn stats(State(state): State<AppState>) -> Result<Json<StatsResponse>, HttpError> {
    let pool = state.db_pool.clone();
    let started_at = state.started_at;
    // Pull observability metrics on the tokio thread, then move them into the blocking task.
    let gena_subscribers = state.subscriptions.len();
    let ssdp_listener_active = state
        .ssdp_listener_active
        .load(std::sync::atomic::Ordering::Relaxed);
    let ssdp_advertiser_active = state
        .ssdp_advertiser_active
        .load(std::sync::atomic::Ordering::Relaxed);
    let art_cache_entries = state.art_cache.len();
    let art_cache_bytes = state.art_cache.current_bytes();
    let random_albums_buffered = state.random_state.len();
    let response = tokio::task::spawn_blocking(move || -> crate::error::Result<StatsResponse> {
        let conn = pool.get()?;

        let albums_total: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))?;
        let tracks_total: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))?;
        let total_duration_ms: i64 = conn.query_row(
            "SELECT COALESCE(SUM(duration_ms), 0) FROM tracks",
            [],
            |r| r.get(0),
        )?;

        // quality_breakdown: single query for {quality: count}, dispatched to matching fields.
        let mut breakdown = QualityBreakdown::default();
        let mut stmt = conn.prepare("SELECT quality, COUNT(*) FROM albums GROUP BY quality")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows.flatten() {
            match row.0.as_str() {
                "hires" => breakdown.hires = row.1,
                "lossless" => breakdown.lossless = row.1,
                "lossy" => breakdown.lossy = row.1,
                "mixed" => breakdown.mixed = row.1,
                _ => breakdown.unknown += row.1, // "unknown" + any unexpected label folded into unknown.
            }
        }

        let plays_total: i64 =
            conn.query_row("SELECT COALESCE(SUM(play_count), 0) FROM tracks", [], |r| {
                r.get(0)
            })?;
        let played_albums_total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM (
               SELECT album_id FROM tracks WHERE last_played_at IS NOT NULL
               GROUP BY album_id
             )",
            [],
            |r| r.get(0),
        )?;

        // last_scan: extract scan_id / completed_at from the server_state.last_scan_report JSON.
        // Missing values or JSON parse failures are treated as null (don't 500 the whole stats endpoint).
        let last_scan = state_kv::get(&conn, "last_scan_report")?.and_then(|json| {
            #[derive(serde::Deserialize)]
            struct Minimal {
                scan_id: String,
                completed_at: i64,
            }
            serde_json::from_str::<Minimal>(&json)
                .ok()
                .map(|m| LastScan {
                    scan_id: m.scan_id,
                    completed_at: m.completed_at,
                })
        });

        let system_update_id: u32 = state_kv::get(&conn, "system_update_id")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(started_at);
        let uptime_seconds = (now_secs - started_at).max(0);

        Ok(StatsResponse {
            albums_total,
            tracks_total,
            total_duration_ms,
            quality_breakdown: breakdown,
            plays_total,
            played_albums_total,
            last_scan,
            system_update_id,
            uptime_seconds,
            gena_subscribers,
            ssdp_listener_active,
            ssdp_advertiser_active,
            art_cache: ArtCacheStats {
                entries: art_cache_entries,
                bytes: art_cache_bytes,
            },
            random_albums_buffered,
        })
    })
    .await
    .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))??;
    Ok(Json(response))
}

/// `POST /admin/reshuffle` — reshuffle the order of `cat:random` (SPEC §6.6).
/// Fully reshuffles `state.random_state` and returns the new array length as JSON.
pub async fn reshuffle(
    State(state): State<AppState>,
) -> Result<Json<ReshuffleResponse>, HttpError> {
    let pool = state.db_pool.clone();
    let rs = state.random_state.clone();
    let shuffled = tokio::task::spawn_blocking(move || -> crate::error::Result<usize> {
        let conn = pool.get()?;
        rs.reshuffle(&conn)
    })
    .await
    .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))??;
    Ok(Json(ReshuffleResponse { shuffled }))
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::db;
    use crate::state::test_helpers::test_state_with_library;

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn h1_scan_report_returns_404_when_no_report() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/scan-report")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn h2_scan_report_returns_saved_json() {
        let (state, _db, _lib) = test_state_with_library();
        // Manually seed last_scan_report.
        {
            let conn = state.db_pool.get().unwrap();
            db::state_kv::set(&conn, "last_scan_report", r#"{"scan_id":"abc"}"#).unwrap();
        }
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/scan-report")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "application/json"
        );
        let body = body_string(resp).await;
        assert!(
            body.contains("scan_id"),
            "body should contain scan_id: {}",
            body
        );
    }

    #[tokio::test]
    async fn h3_rescan_runs_and_returns_report() {
        let (state, _db, _lib) = test_state_with_library();
        let lock = state.scan_lock.clone();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/rescan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("scan_id"));
        // The permit must be released.
        assert!(
            lock.try_acquire().is_ok(),
            "scan_lock should be released after rescan completes"
        );
    }

    #[tokio::test]
    async fn h5_reshuffle_returns_200_with_count() {
        // Even with an empty library, reshuffle itself succeeds (shuffled = 0).
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reshuffle")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("\"shuffled\":0"), "body was: {}", body);
    }

    #[tokio::test]
    async fn h6_reshuffle_does_not_bump_system_update_id() {
        // SPEC §5.1: reshuffle is not a structural change, so don't bump system_update_id
        // (avoid needlessly invalidating Linn's Browse cache).
        let (state, _db, _lib) = test_state_with_library();
        // Seed the initial value.
        {
            let conn = state.db_pool.get().unwrap();
            db::state_kv::set(&conn, "system_update_id", "42").unwrap();
        }
        let pool = state.db_pool.clone();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/reshuffle")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // After reshuffle, system_update_id remains 42.
        let conn = pool.get().unwrap();
        let id = db::state_kv::get(&conn, "system_update_id").unwrap().unwrap();
        assert_eq!(id, "42", "reshuffle must not bump system_update_id");
    }

    // ── /admin/stats (SPEC §8.5) ─────────────────────────────────────

    async fn fetch_stats_json(app: axum::Router) -> serde_json::Value {
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/admin/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        serde_json::from_str(&body).unwrap()
    }

    #[tokio::test]
    async fn st1_empty_library_returns_zeros() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        assert_eq!(v["albums_total"], 0);
        assert_eq!(v["tracks_total"], 0);
        assert_eq!(v["total_duration_ms"], 0);
        assert_eq!(v["plays_total"], 0);
        assert_eq!(v["played_albums_total"], 0);
        assert_eq!(v["quality_breakdown"]["hires"], 0);
        assert_eq!(v["quality_breakdown"]["unknown"], 0);
        assert!(v["last_scan"].is_null());
    }

    #[tokio::test]
    async fn st2_counts_match_after_seed() {
        let (state, _db, _lib) = test_state_with_library();
        {
            let conn = state.db_pool.get().unwrap();
            conn.execute(
                "INSERT INTO albums (effective_album_artist, album, compilation, first_seen_at,
                                     track_count, total_duration_ms, quality)
                 VALUES ('AA', 'A', 0, 0, 2, 300000, 'lossless')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO albums (effective_album_artist, album, compilation, first_seen_at,
                                     track_count, total_duration_ms, quality)
                 VALUES ('AA', 'B', 0, 0, 1, 200000, 'hires')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, duration_ms, added_at, mtime, codec,
                                     mime_type, file_size)
                 VALUES (1, '/m/a1.flac', 150000, 0, 0, 'flac', 'audio/flac', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, duration_ms, added_at, mtime, codec,
                                     mime_type, file_size)
                 VALUES (1, '/m/a2.flac', 150000, 0, 0, 'flac', 'audio/flac', 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, duration_ms, added_at, mtime, codec,
                                     mime_type, file_size)
                 VALUES (2, '/m/b1.flac', 200000, 0, 0, 'flac', 'audio/flac', 0)",
                [],
            )
            .unwrap();
        }
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        assert_eq!(v["albums_total"], 2);
        assert_eq!(v["tracks_total"], 3);
        assert_eq!(v["total_duration_ms"], 500000);
        assert_eq!(v["quality_breakdown"]["lossless"], 1);
        assert_eq!(v["quality_breakdown"]["hires"], 1);
    }

    #[tokio::test]
    async fn st3_play_stats_reflect_in_response() {
        let (state, _db, _lib) = test_state_with_library();
        {
            let conn = state.db_pool.get().unwrap();
            conn.execute(
                "INSERT INTO albums (effective_album_artist, album, compilation, first_seen_at)
                 VALUES ('AA', 'A', 0, 0)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                     play_count, last_played_at)
                 VALUES (1, '/m/x.flac', 0, 0, 'flac', 'audio/flac', 0, 3, 1000)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO tracks (album_id, path, added_at, mtime, codec, mime_type, file_size,
                                     play_count, last_played_at)
                 VALUES (1, '/m/y.flac', 0, 0, 'flac', 'audio/flac', 0, 2, 2000)",
                [],
            )
            .unwrap();
        }
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        assert_eq!(v["plays_total"], 5);
        assert_eq!(v["played_albums_total"], 1);
    }

    #[tokio::test]
    async fn st4_last_scan_extracted_from_state_kv() {
        let (state, _db, _lib) = test_state_with_library();
        {
            let conn = state.db_pool.get().unwrap();
            db::state_kv::set(
                &conn,
                "last_scan_report",
                r#"{"scan_id":"xyz","completed_at":12345,"started_at":0,"duration_ms":0,"is_initial":false,"stats":{"files_enumerated":0,"tracks_inserted":0,"tracks_updated":0,"tracks_unchanged":0,"tracks_deleted":0,"albums_inserted":0,"albums_deleted":0,"tag_read_failed":0},"issues":[],"skipped":[]}"#,
            )
            .unwrap();
            db::state_kv::set(&conn, "system_update_id", "42").unwrap();
        }
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        assert_eq!(v["last_scan"]["scan_id"], "xyz");
        assert_eq!(v["last_scan"]["completed_at"], 12345);
        assert_eq!(v["system_update_id"], 42);
    }

    #[tokio::test]
    async fn st5_malformed_last_scan_json_returns_null() {
        let (state, _db, _lib) = test_state_with_library();
        {
            let conn = state.db_pool.get().unwrap();
            db::state_kv::set(&conn, "last_scan_report", "{ not valid json").unwrap();
        }
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        // Invalid JSON fails to parse → return null (the overall stats endpoint stays 200).
        assert!(v["last_scan"].is_null());
    }

    #[tokio::test]
    async fn st6b_observability_fields_present() {
        // ops §P1: gena_subscribers / ssdp_*_active / art_cache / random_albums_buffered
        // must appear in the response (so a debugger can isolate "Linn can't see us").
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        assert!(v.get("gena_subscribers").is_some());
        assert!(v.get("ssdp_listener_active").is_some());
        assert!(v.get("ssdp_advertiser_active").is_some());
        assert!(v.get("art_cache").is_some());
        assert!(v["art_cache"].get("entries").is_some());
        assert!(v["art_cache"].get("bytes").is_some());
        assert!(v.get("random_albums_buffered").is_some());
        // Initial values are zero / false.
        assert_eq!(v["gena_subscribers"], 0);
        assert_eq!(v["ssdp_listener_active"], false);
        assert_eq!(v["art_cache"]["entries"], 0);
        assert_eq!(v["random_albums_buffered"], 0);
    }

    #[tokio::test]
    async fn st6_uptime_is_non_negative() {
        let (mut state, _db, _lib) = test_state_with_library();
        // Seed started_at = 1 (UNIX seconds), so the difference from now is a large positive.
        state.started_at = 1;
        let app = crate::http::router(state);
        let v = fetch_stats_json(app).await;
        let uptime = v["uptime_seconds"].as_i64().unwrap();
        assert!(uptime >= 0, "uptime should be non-negative");
        assert!(uptime > 0, "with started_at=1, uptime should be large");
    }

    // ── /admin/ui (SPEC §8.4) ──────────────────────────────────────────

    async fn fetch_admin_ui(app: axum::Router, uri: &str) -> axum::http::Response<Body> {
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn ui1_admin_ui_returns_html() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = fetch_admin_ui(app, "/admin/ui").await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "expected text/html, got {:?}",
            resp.headers().get("content-type")
        );
        let body = body_string(resp).await;
        assert!(body.contains("<title>revolver admin</title>"));
        assert!(body.contains("Rescan library"));
    }

    #[tokio::test]
    async fn ui2_admin_trailing_slash_also_returns_ui() {
        // The UI is returned even with a trailing slash (a courtesy for browser address completion).
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = fetch_admin_ui(app, "/admin/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.contains("<title>revolver admin</title>"));
    }

    #[tokio::test]
    async fn ui3_html_references_backend_endpoints() {
        // Verify that the endpoint paths called from the UI's JS are present in the HTML
        // (early detection of dead links).
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = fetch_admin_ui(app, "/admin/ui").await;
        let body = body_string(resp).await;
        for path in [
            "/admin/stats",
            "/admin/rescan",
            "/admin/reshuffle",
            "/admin/scan-report",
        ] {
            assert!(body.contains(path), "missing endpoint reference: {}", path);
        }
    }

    #[tokio::test]
    async fn h4_rescan_returns_conflict_when_lock_held() {
        let (state, _db, _lib) = test_state_with_library();
        // Acquire the permit beforehand to create a state where scan cannot run.
        let _held = state.scan_lock.clone().try_acquire_owned().unwrap();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/rescan")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }
}

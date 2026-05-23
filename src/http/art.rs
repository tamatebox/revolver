//! `GET /art/{album_id}` — return album art (SPEC §8.3, Phase 2 step 15).
//!
//! - Cache hit: return immediately.
//! - Miss: embedded picture of representative track → folder image → 404.
//! - Response is `image/jpeg` or `image/png` + `Cache-Control`.
//!
//! `?v=` query is accepted and ignored (reserved for future URL-based cache
//! busting; when SystemUpdateID changes, Linn's cache is invalidated anyway).

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde::Deserialize;

use crate::art::extract;
use crate::art::CachedArt;
use crate::db;
use crate::http::HttpError;
use crate::state::AppState;

#[derive(Debug, Deserialize)]
pub struct ArtQuery {
    /// Cache buster. MVP just accepts and ignores it.
    #[allow(dead_code)]
    pub v: Option<String>,
}

/// Tri-state outcome of the per-request extraction:
/// - `Found` — embedded picture or folder image was extracted; serve + cache.
/// - `NoArt` — album row exists and has tracks, but no art was found; serve
///   the fallback placeholder so Linn shows a branded thumbnail instead of a
///   raw 404 blank.
/// - `NoSuchAlbum` — the album id has no tracks at all (invalid id or empty
///   row); stays a 404 so client bugs are still visible.
enum ArtOutcome {
    Found(CachedArt),
    NoArt,
    NoSuchAlbum,
}

pub async fn handler(
    State(state): State<AppState>,
    Path(album_id): Path<i64>,
    Query(_q): Query<ArtQuery>,
) -> Result<Response, HttpError> {
    // 1. Cache lookup.
    if let Some(art) = state.art_cache.get(album_id) {
        return Ok(response_for(art));
    }

    // 2. Get representative track path, then extract from embedded picture or folder.
    //    Extraction is blocking I/O, so offload to spawn_blocking (file read + lofty parse).
    let pool = state.db_pool.clone();
    let art_cache = state.art_cache.clone();
    let outcome: ArtOutcome =
        tokio::task::spawn_blocking(move || -> Result<ArtOutcome, HttpError> {
            let conn = pool.get()?;
            let Some(track_path) = db::albums::get_representative_track_path(&conn, album_id)?
            else {
                return Ok(ArtOutcome::NoSuchAlbum);
            };
            drop(conn);

            if let Some((bytes, mime)) = extract::extract_embedded(&track_path) {
                return Ok(ArtOutcome::Found(CachedArt {
                    bytes: Bytes::from(bytes),
                    mime,
                }));
            }
            if let Some(parent) = track_path.parent() {
                if let Some((bytes, mime, _src)) = extract::extract_folder(parent) {
                    return Ok(ArtOutcome::Found(CachedArt {
                        bytes: Bytes::from(bytes),
                        mime,
                    }));
                }
            }
            Ok(ArtOutcome::NoArt)
        })
        .await
        .map_err(|e| HttpError::Internal(anyhow::Error::new(e)))??;

    match outcome {
        ArtOutcome::Found(art) => {
            art_cache.put(album_id, art.clone());
            Ok(response_for(art))
        }
        ArtOutcome::NoArt => Ok(placeholder_response()),
        ArtOutcome::NoSuchAlbum => Err(HttpError::NotFound),
    }
}

fn response_for(art: CachedArt) -> Response {
    // bytes::Bytes is internally Arc-shared; Body::from(Bytes) is zero-copy (single Arc::clone).
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, art.mime),
            // 24h (Linn uses URL-based caching, reusing the response until `?v=` is changed).
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        Body::from(art.bytes),
    )
        .into_response()
}

/// Response served when an album exists but no art could be extracted.
/// Short `max-age` (5 min, vs 24h for real art) so that adding a folder image
/// or re-tagging a file refreshes the thumbnail within minutes rather than
/// being cached as the placeholder for a full day.
fn placeholder_response() -> Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        Body::from(crate::upnp::icon::ALBUM_FALLBACK_PNG),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::test_helpers::test_state_with_library;
    use axum::body::to_bytes;
    use axum::extract::{Path as AxPath, Query as AxQuery, State as AxState};
    use rusqlite::params;
    use std::fs;

    fn insert_album_with_track(state: &AppState, track_filename: &str) -> i64 {
        let conn = state.db_pool.get().unwrap();
        let aid = crate::db::albums::upsert(
            &conn,
            &crate::db::albums::AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            0,
        )
        .unwrap();
        let track_path = state.library_root.join(track_filename);
        conn.execute(
            "INSERT INTO tracks (album_id, path, disc_num, track_num, duration_ms,
                                 added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, ?2, 1, 1, 0, 0, 0, 'flac', 'audio/flac', 0)",
            params![aid, track_path.to_str().unwrap()],
        )
        .unwrap();
        aid
    }

    #[tokio::test]
    async fn ah1_get_art_returns_200_with_folder_jpg() {
        let (state, _dbdir, libdir) = test_state_with_library();
        fs::write(libdir.path().join("track.flac"), b"fake-flac").unwrap();
        fs::write(libdir.path().join("cover.jpg"), b"COVERBYTES").unwrap();
        let aid = insert_album_with_track(&state, "track.flac");

        let res = handler(
            AxState(state.clone()),
            AxPath(aid),
            AxQuery(ArtQuery { v: None }),
        )
        .await
        .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/jpeg"
        );
        let body = to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(&body[..], b"COVERBYTES");
    }

    #[tokio::test]
    async fn ah2_get_art_unknown_album_returns_404() {
        let (state, _dbdir, _libdir) = test_state_with_library();
        let err = handler(AxState(state), AxPath(9999), AxQuery(ArtQuery { v: None }))
            .await
            .expect_err("must error");
        assert!(matches!(err, HttpError::NotFound));
    }

    #[tokio::test]
    async fn ah3_get_art_no_picture_serves_placeholder() {
        // Album exists with a track, but no embedded picture and no folder image:
        // we now serve the bundled fallback PNG instead of 404 so Linn renders a
        // branded thumbnail. Cache-Control deliberately short (5 min) so newly
        // added art shows up quickly on the next browse.
        let (state, _dbdir, libdir) = test_state_with_library();
        fs::write(libdir.path().join("track.flac"), b"fake-flac").unwrap();
        let aid = insert_album_with_track(&state, "track.flac");

        let res = handler(AxState(state), AxPath(aid), AxQuery(ArtQuery { v: None }))
            .await
            .unwrap();

        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(
            res.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/png"
        );
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "public, max-age=300"
        );
        let body = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], crate::upnp::icon::ALBUM_FALLBACK_PNG);
    }

    #[tokio::test]
    async fn ah5_non_numeric_album_id_in_path_returns_4xx() {
        // `/art/{album_id}` is `Path<i64>`. Non-numeric paths are rejected with 4xx
        // by axum's deserialization (handler is not called). **The path is harmless,
        // but pinning axum's behavior as a test catches regressions on version bumps.**
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let (state, _dbdir, _libdir) = test_state_with_library();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/art/not-a-number")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // axum 0.8 returns 400 (not 404) when path deserialization fails.
        assert!(
            resp.status().is_client_error(),
            "expected 4xx for malformed Path, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn ah4_second_call_serves_from_cache() {
        let (state, _dbdir, libdir) = test_state_with_library();
        fs::write(libdir.path().join("track.flac"), b"fake-flac").unwrap();
        fs::write(libdir.path().join("cover.jpg"), b"FIRST_BYTES").unwrap();
        let aid = insert_album_with_track(&state, "track.flac");

        // First call: extract and populate cache.
        let _ = handler(
            AxState(state.clone()),
            AxPath(aid),
            AxQuery(ArtQuery { v: None }),
        )
        .await
        .unwrap();
        // Rewrite the file. If the cache works, the first call's bytes are returned.
        fs::write(libdir.path().join("cover.jpg"), b"REPLACED_BYTES_!!").unwrap();

        let res = handler(
            AxState(state.clone()),
            AxPath(aid),
            AxQuery(ArtQuery { v: None }),
        )
        .await
        .unwrap();
        let body = to_bytes(res.into_body(), 1024).await.unwrap();
        assert_eq!(
            &body[..],
            b"FIRST_BYTES",
            "second call must come from cache, not re-extract"
        );
    }
}

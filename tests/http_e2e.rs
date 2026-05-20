//! Integration tests that go through a real TCP socket. `tower::oneshot` does
//! not exercise the TCP layer (HTTP/1.1 chunking, propagation of Content-Range
//! headers for Range requests, Connection: close handling), so we cover that here.

use std::io::Write;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use rusqlite::params;
use tempfile::{NamedTempFile, TempDir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

use revolver::art::ArtCache;
use revolver::db;
use revolver::random::RandomState;
use revolver::state::{build_notify_client, AppState, BrowseSettings};
use revolver::upnp::gena::{NotifyTasks, Subscriptions};

/// Start a server with one track that holds a fixed 100-byte payload (0..100).
/// The returned TempDir / NamedTempFile must be kept alive until the test ends.
async fn spawn_server() -> (SocketAddr, TempDir, NamedTempFile, i64) {
    let dbdir = TempDir::new().unwrap();
    let pool = db::pool(&dbdir.path().join("test.db")).unwrap();

    // Place the audio file under library_root (= dbdir) so the stream handler's
    // path_within_library check passes (see security §1).
    let mut tmpfile = tempfile::Builder::new()
        .prefix("audio")
        .suffix(".bin")
        .tempfile_in(dbdir.path())
        .unwrap();
    let payload: Vec<u8> = (0..100u8).collect();
    tmpfile.write_all(&payload).unwrap();
    tmpfile.flush().unwrap();
    // Canonicalize the path too: library_root is canonical, so the path emitted
    // by the scan walker is also canonical (e.g. macOS /tmp -> /private/tmp).
    // If DB-side paths stay non-canonical, detect_deleted treats them as
    // "no matching path" and rescan removes the track (the rescan-time stream-404
    // path exercised by e2e5).
    let path_str = std::fs::canonicalize(tmpfile.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();

    {
        let conn = pool.get().unwrap();
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
            "INSERT INTO tracks (album_id, path, codec, mime_type, file_size, added_at, mtime)
             VALUES (?1, ?2, 'flac', 'audio/flac', 100, 0, 0)",
            params![album_id, path_str],
        )
        .unwrap();
    }
    let track_id: i64 = {
        let conn = pool.get().unwrap();
        conn.query_row("SELECT id FROM tracks LIMIT 1", [], |r| r.get(0))
            .unwrap()
    };

    // Canonicalize library_root just like production
    // (so path_within_library checks do not break on macOS /tmp -> /private/tmp).
    let library_root =
        std::fs::canonicalize(dbdir.path()).unwrap_or_else(|_| dbdir.path().to_path_buf());
    let state = AppState {
        db_pool: pool,
        library_root: Arc::new(library_root),
        // The test audio file is `.bin` (raw 0..100 byte sequence).
        // Allow `bin` here so rescan does not treat the track as "out of scope" and delete it.
        extensions: Arc::new(vec!["flac".to_string(), "bin".to_string()]),
        scan_parallel: 1,
        scan_lock: Arc::new(Semaphore::new(1)),
        browse: Arc::new(BrowseSettings::default()),
        uuid: Arc::new("E2E-UUID".to_string()),
        friendly_name: Arc::new("E2E Server".to_string()),
        http_port: 0,
        local_ip: std::net::Ipv4Addr::LOCALHOST,
        subscriptions: Arc::new(Subscriptions::new()),
        notify_tasks: Arc::new(NotifyTasks::new()),
        notify_client: build_notify_client(),
        art_cache: Arc::new(ArtCache::new()),
        random_state: Arc::new(RandomState::new()),
        started_at: 0,
        ssdp_listener_active: Arc::new(AtomicBool::new(false)),
        ssdp_advertiser_active: Arc::new(AtomicBool::new(false)),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = revolver::http::router(state);
    tokio::spawn(async move {
        // The server task is aborted at test teardown, so ignore the error.
        let _ = axum::serve(listener, app).await;
    });
    (addr, dbdir, tmpfile, track_id)
}

/// Send a single HTTP request over raw TCP and return status line + headers + body.
/// Assumes `Connection: close`, so the read stops at EOF once the server is done writing.
///
/// Note: closing the write half via `stream.shutdown()` can cause hyper 0.14+ to
/// treat the request as incomplete, so we do not shutdown. Instead we ask the
/// server to close after the response via the `Connection: close` header.
async fn raw_request(addr: SocketAddr, request: &str) -> Vec<u8> {
    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(addr))
        .await
        .expect("connect timeout")
        .expect("connect failed");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.flush().await.ok();
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut buf))
        .await
        .expect("read timeout")
        .expect("read failed");
    buf
}

fn split_response(raw: &[u8]) -> (String, Vec<u8>) {
    let sep = b"\r\n\r\n";
    let pos = raw
        .windows(sep.len())
        .position(|w| w == sep)
        .expect("response missing header-body separator");
    let headers = String::from_utf8_lossy(&raw[..pos]).to_string();
    let body = raw[pos + sep.len()..].to_vec();
    (headers, body)
}

#[tokio::test]
async fn e2e1_description_xml_returns_200_with_uuid() {
    let (addr, _db, _f, _tid) = spawn_server().await;
    let raw = raw_request(
        addr,
        "GET /description.xml HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (headers, body) = split_response(&raw);
    assert!(headers.starts_with("HTTP/1.1 200"), "headers: {}", headers);
    let body_str = String::from_utf8_lossy(&body);
    assert!(body_str.contains("E2E-UUID"));
}

#[tokio::test]
async fn e2e2_stream_with_range_returns_206_and_exact_bytes() {
    let (addr, _db, _f, tid) = spawn_server().await;
    let req = format!(
        "GET /stream/{} HTTP/1.1\r\nHost: x\r\nRange: bytes=10-19\r\nConnection: close\r\n\r\n",
        tid
    );
    let raw = raw_request(addr, &req).await;
    let (headers, body) = split_response(&raw);
    assert!(headers.starts_with("HTTP/1.1 206"), "headers: {}", headers);
    assert!(
        headers
            .to_lowercase()
            .contains("content-range: bytes 10-19/100"),
        "Content-Range missing: {}",
        headers
    );
    // Payload is 0..100, so bytes 10..20 are returned.
    assert_eq!(body, (10..20u8).collect::<Vec<_>>());
}

#[tokio::test]
async fn e2e3_stream_suffix_range_returns_last_n_bytes() {
    // Verify `bytes=-N` (used by Linn gapless) over real TCP.
    let (addr, _db, _f, tid) = spawn_server().await;
    let req = format!(
        "GET /stream/{} HTTP/1.1\r\nHost: x\r\nRange: bytes=-10\r\nConnection: close\r\n\r\n",
        tid
    );
    let raw = raw_request(addr, &req).await;
    let (headers, body) = split_response(&raw);
    assert!(headers.starts_with("HTTP/1.1 206"), "headers: {}", headers);
    assert!(
        headers
            .to_lowercase()
            .contains("content-range: bytes 90-99/100"),
        "Content-Range missing: {}",
        headers
    );
    assert_eq!(body, (90..100u8).collect::<Vec<_>>());
}

#[tokio::test]
async fn e2e5_stream_during_rescan_still_succeeds() {
    // Run POST /admin/rescan in parallel with GET /stream. Under WAL mode + the
    // r2d2 pool, readers can use a separate connection while the scan holds the
    // DB writer. Regression check for "playback never stops during rescan".
    let (addr, _db, _f, tid) = spawn_server().await;

    // Kick off rescan in the background (library is an empty tempdir, so it finishes instantly).
    let rescan_task = tokio::spawn(async move {
        let _ = raw_request(
            addr,
            "POST /admin/rescan HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        )
        .await;
    });

    // Fire /stream 8 times in a row while rescan is in flight. All must return 206.
    let mut handles = Vec::new();
    for i in 0..8 {
        let req = format!(
            "GET /stream/{} HTTP/1.1\r\nHost: x\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
            tid,
            i * 5,
            i * 5 + 4
        );
        handles.push(tokio::spawn(async move {
            let raw = raw_request(addr, &req).await;
            let (headers, body) = split_response(&raw);
            (headers, body)
        }));
    }

    for h in handles {
        let (headers, body) = h.await.unwrap();
        assert!(
            headers.starts_with("HTTP/1.1 206"),
            "stream during rescan should succeed: {}",
            headers
        );
        assert_eq!(body.len(), 5);
    }

    let _ = rescan_task.await;
}

#[tokio::test]
async fn e2e6_admin_scan_report_404_before_any_scan() {
    // Right after startup (no scan has run yet) /admin/scan-report -> 404.
    let (addr, _db, _f, _tid) = spawn_server().await;
    let raw = raw_request(
        addr,
        "GET /admin/scan-report HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    )
    .await;
    let (headers, _body) = split_response(&raw);
    assert!(headers.starts_with("HTTP/1.1 404"), "got: {}", headers);
}

#[tokio::test]
async fn e2e4_concurrent_range_requests_do_not_corrupt_each_other() {
    // Fire four parallel Range requests against the same track. Each response
    // body must match its expected byte range (no file-handle or cache crosstalk).
    let (addr, _db, _f, tid) = spawn_server().await;
    let mk_req = |s: u8, e: u8| {
        format!(
            "GET /stream/{} HTTP/1.1\r\nHost: x\r\nRange: bytes={}-{}\r\nConnection: close\r\n\r\n",
            tid, s, e
        )
    };
    let pairs = [(0u8, 9u8), (20, 29), (50, 59), (80, 89)];
    let mut handles = Vec::new();
    for (s, e) in pairs {
        let req = mk_req(s, e);
        handles.push(tokio::spawn(async move {
            let raw = raw_request(addr, &req).await;
            let (_headers, body) = split_response(&raw);
            (s, e, body)
        }));
    }
    for h in handles {
        let (s, e, body) = h.await.unwrap();
        let expected: Vec<u8> = (s..=e).collect();
        assert_eq!(body, expected, "range {}-{} got wrong bytes", s, e);
    }
}

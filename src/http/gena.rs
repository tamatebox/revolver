use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::db::state_kv;
use crate::state::AppState;
use crate::upnp::gena::{build_propertyset, spawn_initial_notify, ServiceId, Subscriptions};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(1800);
const MIN_TIMEOUT_SECS: u64 = 60;
const MAX_TIMEOUT_SECS: u64 = 3600;

/// `/event/cd` (ContentDirectory) — SUBSCRIBE / UNSUBSCRIBE endpoint.
pub async fn event_cd(State(state): State<AppState>, req: Request<Body>) -> Response {
    handle_event(state, req, ServiceId::ContentDirectory).await
}

/// `/event/cm` (ConnectionManager) — no evented variables in MVP, accept SUBSCRIBE only.
pub async fn event_cm(State(state): State<AppState>, req: Request<Body>) -> Response {
    handle_event(state, req, ServiceId::ConnectionManager).await
}

async fn handle_event(state: AppState, req: Request<Body>, service: ServiceId) -> Response {
    let method = req.method().as_str().to_string();
    let headers = req.headers().clone();
    match method.as_str() {
        "SUBSCRIBE" => handle_subscribe(state, headers, service).await,
        "UNSUBSCRIBE" => handle_unsubscribe(&state.subscriptions, &headers),
        _ => (StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response(),
    }
}

async fn handle_subscribe(state: AppState, headers: HeaderMap, service: ServiceId) -> Response {
    let timeout = parse_timeout(header_str(&headers, "timeout"));

    // Presence of SID header means refresh request (SPEC §9.5).
    if let Some(sid) = header_str(&headers, "sid") {
        if state.subscriptions.refresh(sid, timeout) {
            return subscribe_ok(sid, timeout);
        }
        return (StatusCode::PRECONDITION_FAILED, "unknown SID").into_response();
    }

    // New SUBSCRIBE: extract URL from CALLBACK header.
    let callback_url = match header_str(&headers, "callback").and_then(parse_first_callback) {
        Some(u) => u,
        None => return (StatusCode::BAD_REQUEST, "invalid CALLBACK header").into_response(),
    };

    let sid = match state
        .subscriptions
        .register(callback_url.clone(), service, timeout)
    {
        Some(s) => s,
        None => {
            // Limit reached (security §3, DoS protection).
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "subscription limit reached",
            )
                .into_response();
        }
    };

    // SPEC §9.6: initial NOTIFY is required. Handler returns 200 immediately and
    // sends via background task through the tracker (abortable on shutdown).
    let initial_body = build_initial_propertyset(&state, service);
    let seq = state.subscriptions.take_next_seq(&sid).unwrap_or(0);
    spawn_initial_notify(
        state.notify_client.clone(),
        &state.notify_tasks,
        callback_url,
        sid.clone(),
        seq,
        initial_body,
    );

    subscribe_ok(&sid, timeout)
}

fn handle_unsubscribe(subs: &Subscriptions, headers: &HeaderMap) -> Response {
    let sid = match header_str(headers, "sid") {
        Some(s) => s,
        None => return (StatusCode::BAD_REQUEST, "missing SID").into_response(),
    };
    if subs.unsubscribe(sid) {
        StatusCode::OK.into_response()
    } else {
        (StatusCode::PRECONDITION_FAILED, "unknown SID").into_response()
    }
}

fn subscribe_ok(sid: &str, timeout: Duration) -> Response {
    let timeout_header = format!("Second-{}", timeout.as_secs());
    (
        StatusCode::OK,
        [("sid", sid.to_string()), ("timeout", timeout_header)],
    )
        .into_response()
}

fn build_initial_propertyset(state: &AppState, service: ServiceId) -> String {
    match service {
        ServiceId::ContentDirectory => {
            let id = read_system_update_id(state).unwrap_or(1);
            build_propertyset(&[("SystemUpdateID", &id.to_string())])
        }
        ServiceId::ConnectionManager => {
            // No evented variables in MVP. Send an empty propertyset.
            build_propertyset(&[])
        }
    }
}

fn read_system_update_id(state: &AppState) -> Option<u32> {
    let conn = state.db_pool.get().ok()?;
    state_kv::get(&conn, "system_update_id")
        .ok()
        .flatten()
        .and_then(|s| s.parse().ok())
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Extract the first URL in `<http://host:port/path>` form. Multiple URLs may
/// arrive space-separated, but taking the first one is sufficient for MVP (SPEC §9.4).
fn parse_first_callback(raw: &str) -> Option<String> {
    let start = raw.find('<')?;
    let end_offset = raw[start + 1..].find('>')?;
    let url = &raw[start + 1..start + 1 + end_offset];
    if url.is_empty() {
        None
    } else {
        Some(url.to_string())
    }
}

/// Parse `Second-N` or `Second-infinite`. Out-of-range values clamp to default.
fn parse_timeout(raw: Option<&str>) -> Duration {
    let s = match raw {
        Some(v) => v,
        None => return DEFAULT_TIMEOUT,
    };
    let rest = match s.strip_prefix("Second-") {
        Some(r) => r,
        None => return DEFAULT_TIMEOUT,
    };
    if rest.eq_ignore_ascii_case("infinite") {
        return DEFAULT_TIMEOUT;
    }
    match rest.parse::<u64>() {
        Ok(n) => Duration::from_secs(n.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS)),
        Err(_) => DEFAULT_TIMEOUT,
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tempfile::TempDir;
    use tower::ServiceExt;

    use super::*;
    use crate::state::test_helpers::test_state;
    use crate::state::AppState;

    /// gena tests need an initial system_update_id, so this is a thin wrapper
    /// over test_state() that also writes the kv entry.
    fn gena_test_state() -> (AppState, TempDir) {
        let (state, dbdir) = test_state();
        {
            let conn = state.db_pool.get().unwrap();
            state_kv::set(&conn, "system_update_id", "1").unwrap();
        }
        (state, dbdir)
    }

    #[test]
    fn pc1_parse_first_callback_single_url() {
        assert_eq!(
            parse_first_callback("<http://192.168.0.1:9999/cb>"),
            Some("http://192.168.0.1:9999/cb".to_string())
        );
    }

    #[test]
    fn pc2_parse_first_callback_takes_first_of_many() {
        assert_eq!(
            parse_first_callback("<http://a/cb1> <http://b/cb2>"),
            Some("http://a/cb1".to_string())
        );
    }

    #[test]
    fn pc3_parse_first_callback_rejects_malformed() {
        assert_eq!(parse_first_callback("not a url"), None);
        assert_eq!(parse_first_callback("<>"), None);
        assert_eq!(parse_first_callback("<http://no-close"), None);
    }

    #[test]
    fn pt1_parse_timeout_normal() {
        assert_eq!(
            parse_timeout(Some("Second-1800")),
            Duration::from_secs(1800)
        );
    }

    #[test]
    fn pt2_parse_timeout_clamped() {
        assert_eq!(
            parse_timeout(Some("Second-10")),
            Duration::from_secs(MIN_TIMEOUT_SECS)
        );
        assert_eq!(
            parse_timeout(Some("Second-99999")),
            Duration::from_secs(MAX_TIMEOUT_SECS)
        );
    }

    #[test]
    fn pt3_parse_timeout_infinite_falls_back_to_default() {
        assert_eq!(parse_timeout(Some("Second-infinite")), DEFAULT_TIMEOUT);
    }

    #[test]
    fn pt4_parse_timeout_missing_or_malformed_falls_back() {
        assert_eq!(parse_timeout(None), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(Some("garbage")), DEFAULT_TIMEOUT);
        assert_eq!(parse_timeout(Some("Second-abc")), DEFAULT_TIMEOUT);
    }

    #[tokio::test]
    async fn h_sub1_new_subscribe_returns_200_with_sid_and_timeout() {
        let (state, _db) = gena_test_state();
        let app = crate::http::router(state);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri("/event/cd")
                    .header("CALLBACK", "<http://127.0.0.1:9/cb>")
                    .header("NT", "upnp:event")
                    .header("TIMEOUT", "Second-1800")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sid = resp.headers().get("sid").unwrap().to_str().unwrap();
        assert!(sid.starts_with("uuid:"));
        let timeout = resp.headers().get("timeout").unwrap().to_str().unwrap();
        assert_eq!(timeout, "Second-1800");
    }

    #[tokio::test]
    async fn h_sub2_refresh_with_existing_sid_returns_same_sid() {
        let (state, _db) = gena_test_state();
        // Register directly to create a SID (going through SUBSCRIBE spawns an
        // initial NOTIFY task, but we only need to test refresh, so this is faster).
        let sid = state
            .subscriptions
            .register(
                "http://127.0.0.1:9/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri("/event/cd")
                    .header("SID", &sid)
                    .header("TIMEOUT", "Second-1800")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("sid").unwrap().to_str().unwrap(), sid);
    }

    #[tokio::test]
    async fn h_sub3_refresh_unknown_sid_returns_412() {
        let (state, _db) = gena_test_state();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri("/event/cd")
                    .header("SID", "uuid:nope")
                    .header("TIMEOUT", "Second-1800")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PRECONDITION_FAILED);
    }

    #[tokio::test]
    async fn h_unsub1_existing_sid_removes_from_registry() {
        let (state, _db) = gena_test_state();
        let sid = state
            .subscriptions
            .register(
                "http://127.0.0.1:9/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(state.subscriptions.len(), 1);

        let subs = state.subscriptions.clone();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("UNSUBSCRIBE")
                    .uri("/event/cd")
                    .header("SID", &sid)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(subs.len(), 0);
    }

    #[tokio::test]
    async fn h_sub4_missing_callback_returns_400() {
        let (state, _db) = gena_test_state();
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("SUBSCRIBE")
                    .uri("/event/cd")
                    // No CALLBACK header, no SID header → invalid new SUBSCRIBE.
                    .header("NT", "upnp:event")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}

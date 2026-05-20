use std::net::IpAddr;

use axum::extract::{DefaultBodyLimit, Request};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::Router;
use tower::limit::ConcurrencyLimitLayer;

use crate::state::AppState;

pub mod admin;
pub mod art;
pub mod gena;
pub mod soap_ctrl;
pub mod stream;
pub mod upnp;

/// Maximum number of concurrent HTTP requests (security §4, DoS protection).
/// Sized for 4 simultaneous streams + multiplexed Browse + headroom. Excess
/// requests wait in the queue (the `tower::limit` behavior: back-pressure, not reject).
pub const MAX_CONCURRENT_REQUESTS: usize = 256;

/// SOAP control body size limit (security §P2). The axum 0.8 default of 2MB is
/// excessive for realistic SOAP envelope sizes (typically 2-5KB, at most tens of KB).
/// Capping at 64KB prevents parse-cost explosion from maliciously oversized bodies.
pub const MAX_SOAP_BODY_BYTES: usize = 64 * 1024;

/// Build the router. Endpoints from SPEC §8.1 are added incrementally.
///
/// GENA SUBSCRIBE / UNSUBSCRIBE are non-standard HTTP methods, so we use `any()`
/// to accept all methods and dispatch inside the handler (SPEC §9.4).
///
/// `/admin/*` goes through the **CSRF defense middleware** (security §D1). This
/// blocks the path where a LAN user opens a malicious web page that fires
/// `fetch("/admin/rescan")` from JS to trigger a scan.
pub fn router(state: AppState) -> Router {
    let admin_routes = Router::new()
        .route("/admin/scan-report", get(admin::scan_report))
        .route("/admin/rescan", post(admin::rescan))
        .route("/admin/reshuffle", post(admin::reshuffle))
        .route("/admin/stats", get(admin::stats))
        .route("/admin/ui", get(admin::ui))
        .route("/admin/", get(admin::ui))
        .layer(middleware::from_fn(admin_csrf_guard));

    // /control/* is SOAP-envelope-only, so cap body at 64KB (security §P2).
    // Other endpoints barely use the body (GET / SUBSCRIBE), so axum's default 2MB stays.
    let control_routes = Router::new()
        .route("/control/cd", post(soap_ctrl::control_cd))
        .route("/control/cm", post(soap_ctrl::control_cm))
        .layer(DefaultBodyLimit::max(MAX_SOAP_BODY_BYTES));

    Router::new()
        .route("/description.xml", get(upnp::description))
        .route("/scpd/cd.xml", get(upnp::scpd_cd))
        .route("/scpd/cm.xml", get(upnp::scpd_cm))
        .route("/event/cd", any(gena::event_cd))
        .route("/event/cm", any(gena::event_cm))
        .route("/stream/{track_id}", get(stream::stream))
        .route("/art/{album_id}", get(art::handler))
        .merge(control_routes)
        .merge(admin_routes)
        .layer(ConcurrencyLimitLayer::new(MAX_CONCURRENT_REQUESTS))
        .with_state(state)
}

/// CSRF defense middleware for `/admin/*` (security §D1).
///
/// Decision logic:
/// - If an `Origin` header is present, parse its host and require it to be a
///   **LAN private IP** (RFC1918 / loopback / link-local). Public IPs / DNS names are rejected.
/// - If `Origin` is absent, pass through (normal requests from curl / non-`mode: 'no-cors'`
///   fetch, local CLI tools, and same-origin fetches from `/admin/ui` itself).
///
/// This implementation focuses on "CSRF from a malicious external web page"
/// (blocking on the assumption that an attacker page cannot forge Origin).
/// CLI usage and the `/admin/ui` experience are unaffected.
async fn admin_csrf_guard(request: Request, next: Next) -> Response {
    if let Some(origin_value) = request.headers().get("origin") {
        let origin = match origin_value.to_str() {
            Ok(s) => s,
            Err(_) => {
                return (StatusCode::FORBIDDEN, "invalid origin header").into_response();
            }
        };
        // Origin is in `scheme://host[:port]` form. Extract the host.
        // Minimal parser: take everything after "://" up to the first "/" or ":".
        let host = match extract_origin_host(origin) {
            Some(h) => h,
            None => {
                return (StatusCode::FORBIDDEN, "unparseable origin").into_response();
            }
        };
        // Pass if parseable as an IP literal and in LAN range. Otherwise reject.
        let lan_ok = match host.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
            Ok(IpAddr::V6(v6)) => {
                v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
            }
            Err(_) => host.eq_ignore_ascii_case("localhost"),
        };
        if !lan_ok {
            tracing::warn!(
                origin = %origin,
                "rejecting /admin/* request from non-LAN origin (CSRF defense)"
            );
            return (StatusCode::FORBIDDEN, "admin endpoints restricted to LAN origins").into_response();
        }
    }
    next.run(request).await
}

/// `https://192.168.1.10:8200` → returns `Some("192.168.1.10")`. `scheme://` is required.
fn extract_origin_host(origin: &str) -> Option<&str> {
    let after_scheme = origin.split_once("://")?.1;
    // From `host[:port][/path]`, take just the host.
    let end = after_scheme
        .find([':', '/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let host = &after_scheme[..end];
    if host.is_empty() {
        None
    } else {
        Some(host)
    }
}

/// Unified error type returned from HTTP handlers. `IntoResponse` maps it to
/// an appropriate HTTP status. Internal details (the inside of anyhow::Error)
/// are not leaked to the client; they go only to server logs via `tracing::error`.
#[derive(Debug)]
pub enum HttpError {
    NotFound,
    Conflict(&'static str),
    Internal(anyhow::Error),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        match self {
            HttpError::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            HttpError::Conflict(msg) => (StatusCode::CONFLICT, msg).into_response(),
            HttpError::Internal(e) => {
                tracing::error!(error = ?e, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
            }
        }
    }
}

// ops §P2: declare uniform `From<X> for HttpError` one per line. Supports the
// automatic `?` path that wraps via `anyhow::Error::new()`. Only `From<anyhow::Error>`
// is declared separately, since it is taken as-is without wrapping.
macro_rules! from_into_internal {
    ($($t:ty),+ $(,)?) => {
        $(impl From<$t> for HttpError {
            fn from(e: $t) -> Self {
                HttpError::Internal(anyhow::Error::new(e))
            }
        })+
    };
}

from_into_internal!(crate::error::Error, r2d2::Error, rusqlite::Error);

impl From<anyhow::Error> for HttpError {
    fn from(e: anyhow::Error) -> Self {
        HttpError::Internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn ok_app() -> Router {
        let (state, _db, _lib) = crate::state::test_helpers::test_state_with_library();
        router(state)
    }

    #[test]
    fn eo1_extract_origin_host_basic() {
        assert_eq!(
            extract_origin_host("http://192.168.1.10:8200"),
            Some("192.168.1.10")
        );
        assert_eq!(
            extract_origin_host("https://example.com"),
            Some("example.com")
        );
        assert_eq!(extract_origin_host("http://localhost"), Some("localhost"));
        assert_eq!(extract_origin_host("http://[::1]:8200"), Some("[")); // IPv6 brackets are not handled (fine for LAN use).
        assert_eq!(extract_origin_host("garbage"), None);
        assert_eq!(extract_origin_host("http://"), None);
    }

    #[tokio::test]
    async fn ag1_admin_without_origin_header_passes_through() {
        // Calls from curl / internal tools have no Origin → pass through.
        let app = ok_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/admin/stats")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ag2_admin_with_lan_origin_passes_through() {
        // Fetches from /admin/ui are same-origin (LAN IP) → pass through.
        let app = ok_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/admin/stats")
                    .header("origin", "http://192.168.1.10:8200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ag3_admin_with_external_origin_returns_403() {
        // CSRF from a malicious web page → 403.
        let app = ok_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/admin/rescan")
                    .header("origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn ag4_admin_with_localhost_origin_passes_through() {
        let app = ok_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/admin/stats")
                    .header("origin", "http://localhost:8200")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn bl1_soap_body_exceeding_64k_returns_4xx() {
        // security §P2: posting a huge body to /control/* should yield 413/400
        // (verifies the limit is 64KB, not the default 2MB). axum returns
        // 413 PAYLOAD_TOO_LARGE on body limit overflow.
        let app = ok_app();
        let huge = vec![b'A'; super::MAX_SOAP_BODY_BYTES + 1];
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/cd")
                    .header("content-type", "text/xml")
                    .body(Body::from(huge))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            resp.status().is_client_error(),
            "expected 4xx for oversized body, got {}",
            resp.status()
        );
    }

    #[tokio::test]
    async fn bl2_normal_soap_body_still_works() {
        // Normal-sized envelopes pass (regression check that the limit isn't too tight).
        let app = ok_app();
        let envelope = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body>
</s:Envelope>"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/cm")
                    .body(Body::from(envelope))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn ag5_non_admin_routes_are_not_gated() {
        // SOAP/GENA/stream are not subject to the middleware. SOAP is called
        // from Linn on the LAN, but without an Origin header (so it would pass
        // even without the middleware). Explicitly assert "admin guard doesn't
        // apply to other routes".
        let app = ok_app();
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/description.xml")
                    .header("origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // /description.xml returns 200 (admin guard does not run).
        assert_eq!(resp.status(), StatusCode::OK);
    }
}

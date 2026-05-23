use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;
use crate::upnp::{device, icon, scpd};

const XML_CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";

/// `GET /description.xml` — UPnP Device Description (SPEC §5.2).
pub async fn description(State(state): State<AppState>) -> Response {
    let xml = device::description_xml(&state.uuid, &state.friendly_name);
    (StatusCode::OK, [("content-type", XML_CONTENT_TYPE)], xml).into_response()
}

/// `GET /scpd/cd.xml` — ContentDirectory:1 SCPD.
pub async fn scpd_cd() -> Response {
    (
        StatusCode::OK,
        [("content-type", XML_CONTENT_TYPE)],
        scpd::CONTENT_DIRECTORY,
    )
        .into_response()
}

/// `GET /scpd/cm.xml` — ConnectionManager:1 SCPD.
pub async fn scpd_cm() -> Response {
    (
        StatusCode::OK,
        [("content-type", XML_CONTENT_TYPE)],
        scpd::CONNECTION_MANAGER,
    )
        .into_response()
}

/// `GET /icon/48.png` — small icon referenced by Device Description `<iconList>`.
pub async fn icon_48() -> Response {
    (
        StatusCode::OK,
        [("content-type", icon::MIME)],
        icon::ICON_48_PNG,
    )
        .into_response()
}

/// `GET /icon/120.png` — large icon referenced by Device Description `<iconList>`.
pub async fn icon_120() -> Response {
    (
        StatusCode::OK,
        [("content-type", icon::MIME)],
        icon::ICON_120_PNG,
    )
        .into_response()
}

/// `GET /icon/512.png` — extra-large icon for retina-class control points,
/// also advertised in the Device Description `<iconList>`.
pub async fn icon_512() -> Response {
    (
        StatusCode::OK,
        [("content-type", icon::MIME)],
        icon::ICON_512_PNG,
    )
        .into_response()
}

/// `GET /icon/1024.png` — highest-resolution icon for high-DPI device-picker
/// tiles (e.g. iPad Pro / 4K UI). Advertised first in `<iconList>` so Linn-
/// style "pick the first entry" clients receive the sharpest asset, and
/// available to any other control point that requests it by URL.
pub async fn icon_1024() -> Response {
    (
        StatusCode::OK,
        [("content-type", icon::MIME)],
        icon::ICON_1024_PNG,
    )
        .into_response()
}

/// `GET /icon.svg` — vector icon used as the admin UI favicon. Not part of
/// the UPnP `<iconList>` (control points stick to the PNG sizes).
pub async fn icon_svg() -> Response {
    (
        StatusCode::OK,
        [("content-type", icon::SVG_MIME)],
        icon::ICON_SVG,
    )
        .into_response()
}

/// `GET /icon/cat/{slug}` — per-category icon for root facets (#24).
/// Returns the embedded PNG bytes with `Content-Type: image/png`, or 404 for
/// unknown slugs. The URL deliberately omits the `.png` extension because
/// axum disallows mixing a path parameter with literal text inside one
/// segment; control points key off the response Content-Type, not the URL.
pub async fn icon_category(Path(slug): Path<String>) -> Response {
    match icon::category_icon(&slug) {
        Some(bytes) => (StatusCode::OK, [("content-type", icon::MIME)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::state::test_helpers::test_state_with_library;

    #[tokio::test]
    async fn uh1_description_xml_returns_200_with_uuid() {
        let (mut state, _db, _lib) = test_state_with_library();
        state.uuid = Arc::new("TEST-UUID-1234".to_string());
        let app = crate::http::router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/description.xml")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("xml"), "content-type was {}", ct);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("TEST-UUID-1234"));
        assert!(body.contains("Test Server"));
    }
}

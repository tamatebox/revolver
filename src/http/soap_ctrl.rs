use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;
use crate::upnp::{connection_manager, content_directory, soap};

const XML_CONTENT_TYPE: &str = "text/xml; charset=\"utf-8\"";

/// `POST /control/cd` — SOAP action dispatch for ContentDirectory:1.
/// Both SOAP parse and DB access are sync, so we run inside `spawn_blocking`.
pub async fn control_cd(State(state): State<AppState>, body: String) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        let pool = state.db_pool.clone();
        let request = match soap::parse_envelope(&body) {
            Some(r) => r,
            None => return Err(soap::SoapFault::invalid_args()),
        };
        content_directory::handle(&pool, &state, &request)
    })
    .await
    .expect("spawn_blocking join");

    match result {
        Ok(body) => (StatusCode::OK, [("content-type", XML_CONTENT_TYPE)], body).into_response(),
        Err(fault) => fault_response(fault),
    }
}

/// `POST /control/cm` — SOAP action dispatch for ConnectionManager:1.
/// No DB access, but we keep the same pattern as CD (sync SOAP parse, structured
/// result). `spawn_blocking` is unnecessary, so it completes inside the handler.
pub async fn control_cm(body: String) -> Response {
    let request = match soap::parse_envelope(&body) {
        Some(r) => r,
        None => return fault_response(soap::SoapFault::invalid_args()),
    };
    match connection_manager::handle(&request) {
        Ok(body) => (StatusCode::OK, [("content-type", XML_CONTENT_TYPE)], body).into_response(),
        Err(fault) => fault_response(fault),
    }
}

fn fault_response(fault: soap::SoapFault) -> Response {
    let body = soap::build_fault_body(&fault);
    // UPnP convention: SOAP faults are returned with HTTP status 500.
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [("content-type", XML_CONTENT_TYPE)],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::state::test_helpers::test_state_with_library;

    async fn body_string(resp: axum::http::Response<Body>) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn sc1_control_cd_browse_root_returns_categories() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let envelope = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <ObjectID>0</ObjectID>
      <BrowseFlag>BrowseDirectChildren</BrowseFlag>
      <Filter>*</Filter>
      <StartingIndex>0</StartingIndex>
      <RequestedCount>10</RequestedCount>
      <SortCriteria></SortCriteria>
    </u:Browse>
  </s:Body>
</s:Envelope>"#;
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/control/cd")
                    .header("content-type", "text/xml")
                    .body(Body::from(envelope))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let body = body_string(resp).await;
        assert!(body.contains("<u:BrowseResponse"));
        assert!(body.contains("<TotalMatches>10</TotalMatches>"));
        assert!(body.contains("cat:aa"));
    }

    #[tokio::test]
    async fn sc2_control_cm_get_protocol_info_returns_source_list() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
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
        assert_eq!(resp.status(), 200);
        let body = body_string(resp).await;
        assert!(body.contains("<u:GetProtocolInfoResponse"));
        assert!(body.contains("audio/flac"));
        assert!(body.contains("audio/mp4"));
    }

    #[tokio::test]
    async fn sc3_control_cm_unknown_action_returns_invalid_action() {
        let (state, _db, _lib) = test_state_with_library();
        let app = crate::http::router(state);
        let envelope = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:PrepareForConnection xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body>
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
        assert_eq!(resp.status(), 500);
        let body = body_string(resp).await;
        assert!(body.contains("<UPnPError"));
        assert!(body.contains("<errorCode>401</errorCode>"));
    }
}

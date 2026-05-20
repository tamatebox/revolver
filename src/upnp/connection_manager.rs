//! Minimal ConnectionManager:1 implementation (SPEC §5.5).
//!
//! Only the three mandatory actions (`GetProtocolInfo` / `GetCurrentConnectionIDs` /
//! `GetCurrentConnectionInfo`) are implemented; other actions (optional
//! `PrepareForConnection` / `ConnectionComplete`) return SoapFault 401.
//!
//! All values are **fixed** — assumes the MediaServer has a single connection
//! (connection_id = 0), no Sink, and protocolInfo enumerates all 5 formats from §7.3.

use crate::upnp::soap;

const CM_SERVICE_TYPE: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";

/// Source protocolInfo covering all formats from SPEC §7.3.
/// No per-client profile (SPEC §10.3, fixed wildcards).
const SOURCE_PROTOCOL_INFO: &str = concat!(
    "http-get:*:audio/flac:*,",
    "http-get:*:audio/x-wav:*,",
    "http-get:*:audio/x-aiff:*,",
    "http-get:*:audio/mp4:*,",
    "http-get:*:audio/mpeg:*",
);

pub fn handle(request: &soap::SoapRequest) -> Result<String, soap::SoapFault> {
    match request.action.as_str() {
        "GetProtocolInfo" => Ok(soap::build_response_body(
            "GetProtocolInfo",
            CM_SERVICE_TYPE,
            &[("Source", SOURCE_PROTOCOL_INFO), ("Sink", "")],
        )),
        "GetCurrentConnectionIDs" => Ok(soap::build_response_body(
            "GetCurrentConnectionIDs",
            CM_SERVICE_TYPE,
            &[("ConnectionIDs", "0")],
        )),
        "GetCurrentConnectionInfo" => Ok(soap::build_response_body(
            "GetCurrentConnectionInfo",
            CM_SERVICE_TYPE,
            &[
                ("RcsID", "-1"),
                ("AVTransportID", "-1"),
                ("ProtocolInfo", ""),
                ("PeerConnectionManager", ""),
                ("PeerConnectionID", "-1"),
                ("Direction", "Output"),
                ("Status", "OK"),
            ],
        )),
        _ => Err(soap::SoapFault::invalid_action()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn req(action: &str) -> soap::SoapRequest {
        soap::SoapRequest {
            action: action.to_string(),
            args: HashMap::new(),
        }
    }

    #[test]
    fn cm1_get_protocol_info_lists_all_five_mimes() {
        let body = handle(&req("GetProtocolInfo")).unwrap();
        assert!(body.contains("<u:GetProtocolInfoResponse"));
        assert!(body.contains("audio/flac"));
        assert!(body.contains("audio/x-wav"));
        assert!(body.contains("audio/x-aiff"));
        assert!(body.contains("audio/mp4"));
        assert!(body.contains("audio/mpeg"));
    }

    #[test]
    fn cm2_get_protocol_info_has_empty_sink() {
        let body = handle(&req("GetProtocolInfo")).unwrap();
        // Either empty `<Sink></Sink>` or `<Sink/>`. Just verify no MIME leaks in
        // (rule out errors like `<Sink>http-get...`).
        assert!(body.contains("<Sink></Sink>") || body.contains("<Sink/>"));
        assert!(!body.contains("<Sink>http"));
    }

    #[test]
    fn cm3_get_current_connection_ids_returns_zero() {
        let body = handle(&req("GetCurrentConnectionIDs")).unwrap();
        assert!(body.contains("<u:GetCurrentConnectionIDsResponse"));
        assert!(body.contains("<ConnectionIDs>0</ConnectionIDs>"));
    }

    #[test]
    fn cm4_get_current_connection_info_returns_all_fixed_fields() {
        let body = handle(&req("GetCurrentConnectionInfo")).unwrap();
        assert!(body.contains("<u:GetCurrentConnectionInfoResponse"));
        assert!(body.contains("<RcsID>-1</RcsID>"));
        assert!(body.contains("<AVTransportID>-1</AVTransportID>"));
        assert!(body.contains("<PeerConnectionID>-1</PeerConnectionID>"));
        assert!(body.contains("<Direction>Output</Direction>"));
        assert!(body.contains("<Status>OK</Status>"));
    }

    #[test]
    fn cm5_unknown_action_returns_invalid_action_fault() {
        let result = handle(&req("PrepareForConnection"));
        assert!(matches!(result, Err(ref f) if f.code == 401));
    }

    #[test]
    fn cm6_connection_complete_returns_invalid_action_fault() {
        // SPEC §5.5: reject all optional actions with 401 InvalidAction (SinkProtocolInfo /
        // PrepareForConnection / ConnectionComplete / GetFeatureList, etc.). Blocks the path
        // where Linn / generic CPs might silently call them and trigger misbehavior.
        let result = handle(&req("ConnectionComplete"));
        assert!(matches!(result, Err(ref f) if f.code == 401));
    }

    #[test]
    fn cm7_arbitrary_unknown_action_returns_401() {
        // Safety net for the fall-through path: arbitrary unsupported action also returns 401.
        let result = handle(&req("SomeBogusAction"));
        assert!(matches!(result, Err(ref f) if f.code == 401));
    }
}

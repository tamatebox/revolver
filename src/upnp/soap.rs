//! SOAP envelope parse / encode.
//!
//! Parsing extracts the first child element inside `<s:Body>` as the action and
//! collects its children as key-value pairs. Complex SOAP features
//! (header / namespace handling) are ignored.

use std::collections::HashMap;

use quick_xml::escape::resolve_predefined_entity;
use quick_xml::events::{BytesRef, Event};
use quick_xml::Reader;

use crate::upnp::xml::escape_text as xml_escape;

const SOAP_ENV_NS: &str = "http://schemas.xmlsoap.org/soap/envelope/";

#[derive(Debug)]
pub struct SoapRequest {
    pub action: String,
    pub args: HashMap<String, String>,
}

/// Parse a SOAP envelope. Returns `None` if malformed.
pub fn parse_envelope(body: &str) -> Option<SoapRequest> {
    // No `trim_text`: since quick-xml 0.38 an argument's text arrives split
    // around each entity reference (`Text` / `GeneralRef` / `Text`), so
    // per-event trimming would eat the spaces in
    // `dc:title contains &quot;The Beatles&quot;`. The assembled value is
    // trimmed once at `End` instead.
    let mut reader = Reader::from_str(body);

    let mut depth: i32 = 0;
    let mut body_depth: Option<i32> = None;
    let mut action_depth: Option<i32> = None;
    let mut action: Option<String> = None;
    let mut args: HashMap<String, String> = HashMap::new();
    let mut current_arg: Option<String> = None;
    let mut current_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                depth += 1;
                let local = std::str::from_utf8(e.local_name().as_ref())
                    .ok()?
                    .to_string();

                if body_depth.is_none() && local == "Body" {
                    body_depth = Some(depth);
                } else if action_depth.is_none() && body_depth == Some(depth - 1) {
                    // First element directly under <s:Body> is the action.
                    action = Some(local);
                    action_depth = Some(depth);
                } else if action_depth == Some(depth - 1) {
                    // Children directly under the action element are arguments.
                    current_arg = Some(local);
                    current_text.clear();
                }
            }
            // Pick up argument-less actions (`<u:GetProtocolInfo/>`, etc.) and
            // empty arguments (`<RcsID/>`, etc.) as Empty events.
            Ok(Event::Empty(e)) => {
                let local = std::str::from_utf8(e.local_name().as_ref())
                    .ok()?
                    .to_string();
                if action_depth.is_none() && body_depth == Some(depth) {
                    // Case: <s:Body><u:Foo/></s:Body>.
                    action = Some(local);
                    // action_depth would conceptually be depth+1, but Empty has no
                    // children — leaving it unset is fine (no further args will arrive).
                } else if action_depth == Some(depth) {
                    // Argument without a value → insert as an empty string.
                    args.insert(local, String::new());
                }
            }
            Ok(Event::Text(t)) if current_arg.is_some() => {
                if let Ok(text) = t.xml10_content() {
                    current_text.push_str(&text);
                }
            }
            Ok(Event::GeneralRef(r)) if current_arg.is_some() => {
                if let Some(resolved) = resolve_ref(&r) {
                    current_text.push_str(&resolved);
                }
            }
            Ok(Event::End(_)) => {
                if let Some(arg) = current_arg.take() {
                    args.insert(arg, current_text.trim().to_string());
                    current_text.clear();
                }
                depth -= 1;
            }
            Ok(Event::Eof) => break,
            Err(_) => return None,
            _ => {}
        }
    }

    action.map(|a| SoapRequest { action: a, args })
}

/// Resolve `&#NN;` / `&#xNN;` and the five predefined XML entities. Unknown
/// entities (no DTD is honored) resolve to nothing.
fn resolve_ref(r: &BytesRef<'_>) -> Option<String> {
    if let Ok(Some(c)) = r.resolve_char_ref() {
        return Some(c.to_string());
    }
    let name = r.decode().ok()?;
    resolve_predefined_entity(&name).map(str::to_string)
}

/// Build the SOAP body for a successful response. `args` values are auto XML-escaped.
pub fn build_response_body(action: &str, service_type: &str, args: &[(&str, &str)]) -> String {
    let mut s = String::new();
    s.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    s.push_str(&format!(
        r#"<s:Envelope xmlns:s="{SOAP_ENV_NS}" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">"#,
    ));
    s.push_str("<s:Body>");
    s.push_str(&format!(r#"<u:{action}Response xmlns:u="{service_type}">"#));
    for (key, value) in args {
        s.push_str(&format!("<{key}>{}</{key}>", xml_escape(value)));
    }
    s.push_str(&format!("</u:{action}Response>"));
    s.push_str("</s:Body>");
    s.push_str("</s:Envelope>");
    s
}

#[derive(Debug, Clone)]
pub struct SoapFault {
    pub code: u16,
    pub description: &'static str,
}

impl SoapFault {
    pub fn invalid_action() -> Self {
        Self {
            code: 401,
            description: "Invalid Action",
        }
    }
    pub fn invalid_args() -> Self {
        Self {
            code: 402,
            description: "Invalid Args",
        }
    }
    pub fn no_such_object() -> Self {
        Self {
            code: 701,
            description: "No such object",
        }
    }
    pub fn internal_error() -> Self {
        Self {
            code: 500,
            description: "Internal Server Error",
        }
    }
}

/// Build a UPnP SOAP fault (the `<UPnPError>` form embedded in `<s:Fault>`).
pub fn build_fault_body(fault: &SoapFault) -> String {
    let mut s = String::new();
    s.push_str(r#"<?xml version="1.0" encoding="utf-8"?>"#);
    s.push_str(&format!(
        r#"<s:Envelope xmlns:s="{SOAP_ENV_NS}" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">"#,
    ));
    s.push_str("<s:Body>");
    s.push_str("<s:Fault>");
    s.push_str("<faultcode>s:Client</faultcode>");
    s.push_str("<faultstring>UPnPError</faultstring>");
    s.push_str("<detail>");
    s.push_str(r#"<UPnPError xmlns="urn:schemas-upnp-org:control-1-0">"#);
    s.push_str(&format!("<errorCode>{}</errorCode>", fault.code));
    s.push_str(&format!(
        "<errorDescription>{}</errorDescription>",
        xml_escape(fault.description)
    ));
    s.push_str("</UPnPError>");
    s.push_str("</detail>");
    s.push_str("</s:Fault>");
    s.push_str("</s:Body>");
    s.push_str("</s:Envelope>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sp1_parse_browse_envelope() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
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
        let req = parse_envelope(body).expect("must parse");
        assert_eq!(req.action, "Browse");
        assert_eq!(req.args.get("ObjectID").map(String::as_str), Some("0"));
        assert_eq!(
            req.args.get("BrowseFlag").map(String::as_str),
            Some("BrowseDirectChildren")
        );
        assert_eq!(
            req.args.get("RequestedCount").map(String::as_str),
            Some("10")
        );
    }

    #[test]
    fn sp2_malformed_returns_none() {
        assert!(parse_envelope("not xml at all").is_none());
        assert!(parse_envelope("<unclosed").is_none());
    }

    #[test]
    fn sp_self_closing_action_with_no_args() {
        // Argument-less actions may arrive as `<u:Foo/>` (e.g., GetProtocolInfo /
        // GetCurrentConnectionIDs from SPEC §5.5). Must parse correctly.
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:GetProtocolInfo xmlns:u="urn:schemas-upnp-org:service:ConnectionManager:1"/></s:Body>
</s:Envelope>"#;
        let req = parse_envelope(body).expect("self-closing action must parse");
        assert_eq!(req.action, "GetProtocolInfo");
        assert!(req.args.is_empty());
    }

    #[test]
    fn sp4_entities_in_args_are_resolved_and_inner_spaces_kept() {
        // Linn's SearchCriteria arrives entity-escaped; the spaces next to
        // `&quot;` must survive (quick-xml ≥ 0.38 splits text around refs).
        let body = r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body>
    <u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
      <SearchCriteria> upnp:class derivedfrom &quot;object.container.album&quot; and dc:title contains &quot;Rock &amp; Roll&quot; </SearchCriteria>
      <Filter>&#x41;&#66;&lt;&gt;&apos;&unknown;</Filter>
    </u:Search>
  </s:Body>
</s:Envelope>"#;
        let req = parse_envelope(body).expect("must parse");
        assert_eq!(
            req.args.get("SearchCriteria").map(String::as_str),
            Some(
                r#"upnp:class derivedfrom "object.container.album" and dc:title contains "Rock & Roll""#
            )
        );
        assert_eq!(req.args.get("Filter").map(String::as_str), Some("AB<>'"));
    }

    #[test]
    fn sp3_build_response_body_escapes_values() {
        let body = build_response_body(
            "Browse",
            "urn:schemas-upnp-org:service:ContentDirectory:1",
            &[("Result", "<DIDL/>"), ("NumberReturned", "3")],
        );
        assert!(body.contains("<u:BrowseResponse"));
        assert!(body.contains(r#"xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1""#));
        assert!(body.contains("<Result>&lt;DIDL/&gt;</Result>"));
        assert!(body.contains("<NumberReturned>3</NumberReturned>"));
    }

    // ── proptest: parse_envelope must not panic on arbitrary input ───────────────
    proptest::proptest! {
        /// Any UTF-8-valid byte sequence passed to parse_envelope must not panic
        /// and must return `None`/`Some`. XXE / billion laughs are deferred to
        /// quick-xml's default behavior (DTDs are ignored).
        #[test]
        fn sp_parse_envelope_never_panics(input in ".*") {
            let _ = parse_envelope(&input);
        }

        /// Given an XML-valid envelope shape, parsing must either extract the action
        /// or return None — never panic in between.
        #[test]
        fn sp_parse_envelope_with_action_name(
            action in "[A-Za-z][A-Za-z0-9]{0,20}"
        ) {
            let body = format!(
                r#"<?xml version="1.0"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
  <s:Body><u:{a} xmlns:u="x"/></s:Body>
</s:Envelope>"#,
                a = action
            );
            let req = parse_envelope(&body).expect("valid envelope must parse");
            proptest::prop_assert_eq!(req.action, action);
        }
    }
}

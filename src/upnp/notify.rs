use std::net::IpAddr;

use reqwest::header::{HeaderName, HeaderValue};
use reqwest::Method;

// Migrated from the previous hand-written HTTP raw-socket version to a shared
// `reqwest::Client` pool (perf §P0). What used to open a new TCP connection per
// NOTIFY is now reused via keep-alive. FD release no longer dominates during
// the Linn re-fetch rush right after a SystemUpdateID bump.
//
// `parse_http_url` is kept solely for URL format validation: we need to decompose
// the URL once to pass the host to `is_lan_host`. `reqwest::Url` could parse it
// too, but we keep the simple parser to avoid exposing that crate.

/// Minimal `http://host[:port]/path` parser. Query / fragment are ignored.
/// Accepts only the CALLBACK URL format from SPEC §9.4.
fn parse_http_url(url: &str) -> Option<(String, u16, String)> {
    let rest = url.strip_prefix("http://")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (authority.to_string(), 80u16),
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path.to_string()))
}

/// SSRF defense: verify the CALLBACK URL host is in LAN range (private / loopback /
/// link-local) (security §2).
///
/// - IP literals are judged directly.
/// - Hostnames are rejected on the safe side (to avoid DNS rebinding). Typical CPs
///   like Linn / BubbleUPnP register callbacks with IP literals, so no real impact.
fn is_lan_host(host: &str) -> bool {
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        Ok(IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

/// Default client (for tests). Production AppState.notify_client is built by
/// `crate::state::build_notify_client`.
#[cfg(test)]
fn default_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(std::time::Duration::from_secs(60))
        .build()
        .expect("reqwest client (no TLS) should build")
}

/// Send the NOTIFY HTTP request from SPEC §9.6. `client` is a `reqwest::Client`
/// sharing a keep-alive pool (`AppState.notify_client`).
///
/// On failure the caller only warn-logs; no subscription removal or retry
/// (SPEC §9.6 "drop after consecutive failures" is a separate commit). However,
/// **non-2xx responses** are returned as Err so the caller can observe them (ops §P1).
pub async fn send_notify(
    client: &reqwest::Client,
    callback_url: &str,
    sid: &str,
    seq: u32,
    body: &str,
) -> Result<(), String> {
    let (host, _port, _path) = parse_http_url(callback_url)
        .ok_or_else(|| format!("invalid callback url: {}", callback_url))?;

    // SSRF defense: reject callbacks outside the LAN range (security §2).
    if !is_lan_host(&host) {
        return Err(format!(
            "callback host {} is not in LAN range; refusing to send NOTIFY",
            host
        ));
    }

    // NOTIFY is a non-standard HTTP method. Build it via reqwest::Method::from_bytes.
    let method = Method::from_bytes(b"NOTIFY").map_err(|e| format!("invalid method: {e}"))?;

    let resp = client
        .request(method, callback_url)
        .header(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("text/xml; charset=\"utf-8\""),
        )
        .header(HeaderName::from_static("nt"), HeaderValue::from_static("upnp:event"))
        .header(
            HeaderName::from_static("nts"),
            HeaderValue::from_static("upnp:propchange"),
        )
        .header(
            HeaderName::from_static("sid"),
            HeaderValue::from_str(sid).map_err(|e| format!("invalid sid: {e}"))?,
        )
        .header(
            HeaderName::from_static("seq"),
            HeaderValue::from_str(&seq.to_string()).expect("seq u32 always valid"),
        )
        .body(body.to_string())
        .send()
        .await
        .map_err(|e| format!("notify send error: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(format!(
            "notify got non-2xx status {} from {}",
            status.as_u16(),
            callback_url
        ));
    }
    Ok(())
}

/// Kept for test compatibility: callable without a client (internally builds the
/// default one). Production passes the shared AppState.notify_client, so this is unused.
#[cfg(test)]
pub async fn send_notify_default(
    callback_url: &str,
    sid: &str,
    seq: u32,
    body: &str,
) -> Result<(), String> {
    let client = default_client();
    send_notify(&client, callback_url, sid, seq, body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn n1_parse_http_url_with_port_and_path() {
        let (h, p, path) = parse_http_url("http://192.168.0.1:9999/cb/path").unwrap();
        assert_eq!(h, "192.168.0.1");
        assert_eq!(p, 9999);
        assert_eq!(path, "/cb/path");
    }

    #[test]
    fn n2_parse_http_url_without_port_defaults_80() {
        let (h, p, path) = parse_http_url("http://example.com/").unwrap();
        assert_eq!(h, "example.com");
        assert_eq!(p, 80);
        assert_eq!(path, "/");
    }

    #[test]
    fn n3_parse_http_url_without_path() {
        let (h, p, path) = parse_http_url("http://host:8080").unwrap();
        assert_eq!(h, "host");
        assert_eq!(p, 8080);
        assert_eq!(path, "/");
    }

    #[test]
    fn n4_parse_http_url_rejects_invalid() {
        assert!(parse_http_url("https://x/").is_none()); // https not accepted (MVP)
        assert!(parse_http_url("ftp://x/").is_none());
        assert!(parse_http_url("http://:8080/").is_none()); // empty host
        assert!(parse_http_url("http://host:abc/").is_none()); // port parse fail
    }

    #[tokio::test]
    async fn n5_send_notify_to_local_listener_succeeds() {
        // Spin up a lightweight HTTP/1.1 server to observe the request.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}/cb", addr.port());

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut accumulated: Vec<u8> = Vec::new();
            loop {
                let mut buf = [0u8; 4096];
                let n = tokio::time::timeout(Duration::from_secs(3), sock.read(&mut buf))
                    .await
                    .expect("read timeout")
                    .expect("read err");
                if n == 0 {
                    break;
                }
                accumulated.extend_from_slice(&buf[..n]);
                // Once headers + body are fully received, return 200 and close the connection.
                if accumulated.ends_with(b"<body/>") {
                    break;
                }
            }
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&accumulated).to_string()
        });

        send_notify_default(&url, "uuid:abc", 42, "<body/>")
            .await
            .unwrap();
        let req = server.await.unwrap();

        assert!(req.starts_with("NOTIFY /cb HTTP/1.1\r\n"));
        // reqwest emits headers in mixed case, so check case-insensitively.
        let req_lc = req.to_ascii_lowercase();
        assert!(req_lc.contains("nt: upnp:event"));
        assert!(req_lc.contains("nts: upnp:propchange"));
        assert!(req_lc.contains("sid: uuid:abc"));
        assert!(req_lc.contains("seq: 42"));
        assert!(req.ends_with("<body/>"));
    }

    #[tokio::test]
    async fn n6_send_notify_returns_err_on_invalid_url() {
        let err = send_notify_default("not a url", "uuid:x", 0, "<body/>").await;
        assert!(err.is_err());
    }

    #[test]
    fn n7_is_lan_host_accepts_private_ranges() {
        assert!(is_lan_host("192.168.1.1"));
        assert!(is_lan_host("10.0.0.1"));
        assert!(is_lan_host("172.16.0.1"));
        assert!(is_lan_host("127.0.0.1"));
        assert!(is_lan_host("169.254.0.1"));
        assert!(is_lan_host("::1"));
        assert!(is_lan_host("fd12:3456::1"));
        assert!(is_lan_host("fe80::1"));
    }

    #[test]
    fn n7b_is_lan_host_accepts_aws_metadata_address_explicitly() {
        // 169.254.169.254 belongs to the link-local range (169.254.0.0/16), so
        // `is_lan_host` **accepts it**. This is by spec since LAN-only deployment is assumed.
        // (For cloud deployments warn in README; extra metadata-route reject is out of scope.)
        // Failure of this test = regression: the link-local detection range has shrunk.
        assert!(is_lan_host("169.254.169.254"));
    }

    #[test]
    fn n8_is_lan_host_rejects_public_and_hostnames() {
        assert!(!is_lan_host("8.8.8.8"));
        assert!(!is_lan_host("1.1.1.1"));
        assert!(!is_lan_host("example.com"));
        assert!(!is_lan_host("attacker.local"));
        assert!(!is_lan_host(""));
        // Note: 169.254.169.254 (AWS metadata) is link-local, so **it is accepted**.
        // By spec under the LAN-only deployment assumption. For cloud deployments warn in README.
    }

    #[tokio::test]
    async fn n9_send_notify_rejects_public_callback() {
        let err = send_notify_default("http://8.8.8.8/cb", "uuid:x", 0, "<body/>")
            .await
            .unwrap_err();
        assert!(err.contains("not in LAN range"), "got: {}", err);
    }

    #[tokio::test]
    async fn n10_send_notify_returns_err_on_404_response() {
        // ops §P1: non-2xx must surface as Err (the path used to detect dead subscriptions).
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://127.0.0.1:{}/cb", addr.port());

        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Discard incoming bytes (just wait long enough to consume NOTIFY headers + body).
            let mut buf = vec![0u8; 4096];
            let _ = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf)).await;
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        let err = send_notify_default(&url, "uuid:gone", 0, "<body/>")
            .await
            .unwrap_err();
        assert!(err.contains("404"), "got: {}", err);
    }
}

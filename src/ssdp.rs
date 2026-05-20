//! SSDP discovery -- UDP multicast listener and alive/byebye advertiser
//! (SPEC §9.1-9.3).

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::state::AppState;
use crate::upnp::usn::NtTarget;

const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const MULTICAST_PORT: u16 = 1900;
const MAX_AGE: u32 = 1800;
const REANNOUNCE_INTERVAL: Duration = Duration::from_secs(900);

/// Upper bound for `MX` (SPEC §9.2: 1-5 is typical). The spec allows up to 120,
/// but we clamp here to prevent a malicious large value from pinning a task
/// (security §2).
const MX_CAP_SECS: u8 = 5;

fn multicast_target() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(MULTICAST_GROUP, MULTICAST_PORT))
}

/// Bind UDP port 1900 with SO_REUSEADDR / SO_REUSEPORT, join the SSDP multicast
/// group, and return a tokio socket. **Call from main.rs at startup** so failures
/// are visible immediately and `AppState.ssdp_*_active` flags can be set right
/// away (ops §P1).
pub fn ssdp_socket() -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    let bind_addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), MULTICAST_PORT);
    socket.bind(&bind_addr.into())?;
    socket.join_multicast_v4(&MULTICAST_GROUP, &Ipv4Addr::UNSPECIFIED)?;
    let std_socket: std::net::UdpSocket = socket.into();
    UdpSocket::from_std(std_socket)
}

/// Detect the outbound IP to put in the LOCATION header of SSDP responses.
/// `connect()` a UDP socket to 8.8.8.8 and read local_addr (no traffic is sent).
pub fn detect_local_ip() -> Ipv4Addr {
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                if let IpAddr::V4(ip) = addr.ip() {
                    if !ip.is_loopback() {
                        return ip;
                    }
                }
            }
        }
    }
    warn!("could not detect local IPv4, falling back to 127.0.0.1 (LAN clients won't see this server)");
    Ipv4Addr::new(127, 0, 0, 1)
}

/// Parse result of M-SEARCH (only the minimum: ST and MX).
#[derive(Debug, PartialEq, Eq)]
pub struct MSearch {
    pub st: String,
    /// Response delay upper bound (seconds). Clamped to 0..=5.
    pub mx: u8,
}

/// Parse an M-SEARCH packet. Split the HTTP-like text by lines and extract
/// `ST` and `MX`. Returns None if `ST` is missing. `MX` defaults to 0 when
/// missing or unparseable.
pub fn parse_msearch(data: &[u8]) -> Option<MSearch> {
    let text = std::str::from_utf8(data).ok()?;
    let mut lines = text.lines();
    let first = lines.next()?;
    if !first.starts_with("M-SEARCH") {
        return None;
    }
    let mut st: Option<String> = None;
    let mut mx: u8 = 0;
    for line in lines {
        let Some(colon) = line.find(':') else {
            continue;
        };
        let key = line[..colon].trim().to_ascii_uppercase();
        let val = line[colon + 1..].trim();
        match key.as_str() {
            "ST" => st = Some(val.to_string()),
            "MX" => mx = val.parse::<u8>().unwrap_or(0).min(MX_CAP_SECS),
            _ => {}
        }
    }
    Some(MSearch { st: st?, mx })
}

fn format_msearch_response(target: NtTarget, state: &AppState) -> String {
    let location = format!(
        "http://{}:{}/description.xml",
        state.local_ip, state.http_port
    );
    format!(
        "HTTP/1.1 200 OK\r\n\
         CACHE-CONTROL: max-age={MAX_AGE}\r\n\
         EXT:\r\n\
         LOCATION: {location}\r\n\
         SERVER: revolver/0.1.0 UPnP/1.0\r\n\
         ST: {st}\r\n\
         USN: {usn}\r\n\
         \r\n",
        st = target.nt(&state.uuid),
        usn = target.usn(&state.uuid),
    )
}

fn format_alive_notify(target: NtTarget, state: &AppState) -> String {
    let location = format!(
        "http://{}:{}/description.xml",
        state.local_ip, state.http_port
    );
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         CACHE-CONTROL: max-age={MAX_AGE}\r\n\
         LOCATION: {location}\r\n\
         NT: {nt}\r\n\
         NTS: ssdp:alive\r\n\
         SERVER: revolver/0.1.0 UPnP/1.0\r\n\
         USN: {usn}\r\n\
         \r\n",
        nt = target.nt(&state.uuid),
        usn = target.usn(&state.uuid),
    )
}

fn format_byebye_notify(target: NtTarget, state: &AppState) -> String {
    format!(
        "NOTIFY * HTTP/1.1\r\n\
         HOST: 239.255.255.250:1900\r\n\
         NT: {nt}\r\n\
         NTS: ssdp:byebye\r\n\
         USN: {usn}\r\n\
         \r\n",
        nt = target.nt(&state.uuid),
        usn = target.usn(&state.uuid),
    )
}

/// M-SEARCH listener. Receives port 1900 multicast and replies via unicast.
/// **The caller pre-binds the socket and passes it in** so failures surface
/// immediately in main.rs (ops §P1).
pub async fn listener_task(
    state: AppState,
    socket: UdpSocket,
    mut shutdown: broadcast::Receiver<()>,
) {
    let socket = Arc::new(socket);
    state.ssdp_listener_active.store(true, Ordering::Relaxed);
    info!("ssdp listener started on 0.0.0.0:1900");

    let mut buf = vec![0u8; 2048];
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("ssdp listener shutting down");
                break;
            }
            result = socket.recv_from(&mut buf) => {
                match result {
                    Ok((n, peer)) => {
                        let Some(m) = parse_msearch(&buf[..n]) else { continue };
                        debug!(st = %m.st, mx = m.mx, peer = %peer, "received M-SEARCH");
                        // SPEC §9.2: respond with a random delay within MX. Smooths
                        // the response peak when many CPs M-SEARCH simultaneously and
                        // prevents being used as an amplification reflector
                        // (security §2). Spawn each response in its own task so the
                        // main recv loop is never blocked.
                        let socket_c = socket.clone();
                        let state_c = state.clone();
                        tokio::spawn(async move {
                            delayed_respond_msearch(&socket_c, &state_c, &m, peer).await;
                        });
                    }
                    Err(e) => {
                        warn!(error = %e, "ssdp recv error");
                    }
                }
            }
        }
    }
    state.ssdp_listener_active.store(false, Ordering::Relaxed);
}

async fn delayed_respond_msearch(
    socket: &UdpSocket,
    state: &AppState,
    m: &MSearch,
    peer: SocketAddr,
) {
    if m.mx > 0 {
        let delay_ms = {
            let mut rng = rand::thread_rng();
            rng.gen_range(0..=(m.mx as u64 * 1000))
        };
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    respond_msearch(socket, state, m, peer).await;
}

async fn respond_msearch(socket: &UdpSocket, state: &AppState, m: &MSearch, peer: SocketAddr) {
    for target in NtTarget::ALL {
        if target.matches_st(&m.st, &state.uuid) {
            let response = format_msearch_response(target, state);
            if let Err(e) = socket.send_to(response.as_bytes(), peer).await {
                warn!(error = %e, peer = %peer, "ssdp response send failed");
            }
        }
    }
}

/// Multicast `ssdp:alive` at startup and on a 900s interval.
/// On shutdown, send `ssdp:byebye` and terminate the task.
/// The caller pre-binds the socket and passes it in (ops §P1).
pub async fn advertiser_task(
    state: AppState,
    socket: UdpSocket,
    mut shutdown: broadcast::Receiver<()>,
) {
    state.ssdp_advertiser_active.store(true, Ordering::Relaxed);
    info!("ssdp advertiser started");
    let target = multicast_target();

    // Initial alive burst.
    for nt in NtTarget::ALL {
        let msg = format_alive_notify(nt, &state);
        if let Err(e) = socket.send_to(msg.as_bytes(), target).await {
            warn!(error = %e, "initial ssdp:alive send failed");
        }
    }

    let mut ticker = tokio::time::interval(REANNOUNCE_INTERVAL);
    ticker.tick().await; // skip the first tick (interval returns immediately)

    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                info!("ssdp advertiser sending byebye");
                for nt in NtTarget::ALL {
                    let msg = format_byebye_notify(nt, &state);
                    let _ = socket.send_to(msg.as_bytes(), target).await;
                }
                break;
            }
            _ = ticker.tick() => {
                debug!("ssdp re-announcing alive");
                for nt in NtTarget::ALL {
                    let msg = format_alive_notify(nt, &state);
                    let _ = socket.send_to(msg.as_bytes(), target).await;
                }
            }
        }
    }
    state.ssdp_advertiser_active.store(false, Ordering::Relaxed);
}

/// Bind two sockets (listener / advertiser) at startup and return them.
/// On failure, log an error and return `None` (the server still starts with
/// HTTP only).
pub fn try_bind_pair() -> (Option<UdpSocket>, Option<UdpSocket>) {
    let listener = ssdp_socket()
        .map_err(|e| {
            error!(error = %e, "ssdp listener bind failed; M-SEARCH won't be answered");
            e
        })
        .ok();
    let advertiser = ssdp_socket()
        .map_err(|e| {
            error!(error = %e, "ssdp advertiser bind failed; ssdp:alive won't be sent");
            e
        })
        .ok();
    (listener, advertiser)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn p1_parse_valid_msearch() {
        let data = b"M-SEARCH * HTTP/1.1\r\n\
                     HOST: 239.255.255.250:1900\r\n\
                     MAN: \"ssdp:discover\"\r\n\
                     MX: 3\r\n\
                     ST: urn:schemas-upnp-org:device:MediaServer:1\r\n\
                     \r\n";
        let m = parse_msearch(data).unwrap();
        assert_eq!(m.st, "urn:schemas-upnp-org:device:MediaServer:1");
        assert_eq!(m.mx, 3);
    }

    #[test]
    fn p2_parse_msearch_without_mx_defaults_to_zero() {
        let data = b"M-SEARCH * HTTP/1.1\r\nST: ssdp:all\r\n\r\n";
        let m = parse_msearch(data).unwrap();
        assert_eq!(m.st, "ssdp:all");
        assert_eq!(m.mx, 0);
    }

    #[test]
    fn p3_parse_msearch_clamps_mx_to_cap() {
        // security §2: do not let a huge MX pin a response task for a long time.
        let data = b"M-SEARCH * HTTP/1.1\r\nMX: 60\r\nST: ssdp:all\r\n\r\n";
        let m = parse_msearch(data).unwrap();
        assert_eq!(m.mx, MX_CAP_SECS);
    }
}

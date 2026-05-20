use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::upnp::notify::send_notify;

/// Return a `MutexGuard` recovered from a poisoned state.
/// Poisoning is a rare condition that only occurs when another thread panics.
/// In this project, discarding the contents and recovering is safer than letting
/// `.unwrap()` cascade into another panic.
fn lock_recover<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Subscribable services (SPEC §9.4). MVP supports only these two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceId {
    ContentDirectory,
    ConnectionManager,
}

/// A single GENA subscription (SPEC §9.4).
#[derive(Debug, Clone)]
pub struct Subscription {
    pub sid: String,
    pub callback_url: String,
    pub service: ServiceId,
    pub expires_at: Instant,
    pub seq: u32,
}

/// Upper bound on concurrently held subscriptions (security §3, DoS defense). Has
/// plenty of headroom even for a few CPs on the same LAN. When exceeded, `register`
/// returns `None` and the HTTP handler replies 503.
pub const MAX_SUBSCRIPTIONS: usize = 1024;

/// Registry holding all subscriptions (process memory only; cleared on restart).
/// Shared from `AppState` via `Arc<Subscriptions>`.
pub struct Subscriptions {
    inner: Mutex<HashMap<String, Subscription>>,
}

impl Subscriptions {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// New SUBSCRIBE. Issues an SID with `uuid:` prefix, registers it, and returns it.
    /// Returns `None` if the cap (`MAX_SUBSCRIPTIONS`) is reached (security §3).
    pub fn register(
        &self,
        callback_url: String,
        service: ServiceId,
        timeout: Duration,
    ) -> Option<String> {
        let mut g = lock_recover(&self.inner);
        if g.len() >= MAX_SUBSCRIPTIONS {
            return None;
        }
        let sid = format!("uuid:{}", Uuid::new_v4());
        let sub = Subscription {
            sid: sid.clone(),
            callback_url,
            service,
            expires_at: Instant::now() + timeout,
            seq: 0,
        };
        g.insert(sid.clone(), sub);
        Some(sid)
    }

    /// Update the `expires_at` of an existing SID. Returns `false` if no such SID.
    pub fn refresh(&self, sid: &str, timeout: Duration) -> bool {
        let mut g = lock_recover(&self.inner);
        if let Some(sub) = g.get_mut(sid) {
            sub.expires_at = Instant::now() + timeout;
            true
        } else {
            false
        }
    }

    /// Explicit UNSUBSCRIBE.
    pub fn unsubscribe(&self, sid: &str) -> bool {
        lock_recover(&self.inner).remove(sid).is_some()
    }

    /// Sweep expired subscriptions. Returns the number removed.
    pub fn sweep_expired(&self) -> usize {
        let now = Instant::now();
        let mut g = lock_recover(&self.inner);
        let before = g.len();
        g.retain(|_, sub| sub.expires_at > now);
        before - g.len()
    }

    /// Take the current seq for a single SID, increment it by 1, and return the prior value.
    /// Used for the initial NOTIFY (first call returns 0 and advances to 1 for next time).
    pub fn take_next_seq(&self, sid: &str) -> Option<u32> {
        let mut g = lock_recover(&self.inner);
        let sub = g.get_mut(sid)?;
        let cur = sub.seq;
        sub.seq = sub.seq.saturating_add(1);
        Some(cur)
    }

    /// Return `(sid, callback, seq)` for all subscriptions of the given service.
    /// Each seq is incremented by 1 at the time of retrieval (used for propchange fan-out).
    pub fn snapshot_and_increment(&self, service: ServiceId) -> Vec<(String, String, u32)> {
        let mut g = lock_recover(&self.inner);
        g.values_mut()
            .filter(|s| s.service == service)
            .map(|s| {
                let seq = s.seq;
                s.seq = s.seq.saturating_add(1);
                (s.sid.clone(), s.callback_url.clone(), seq)
            })
            .collect()
    }

    /// Current number of held subscriptions. Used for `/admin/stats` observability (ops §P1).
    pub fn len(&self) -> usize {
        lock_recover(&self.inner).len()
    }

    pub fn is_empty(&self) -> bool {
        lock_recover(&self.inner).is_empty()
    }

    #[cfg(test)]
    pub fn get(&self, sid: &str) -> Option<Subscription> {
        lock_recover(&self.inner).get(sid).cloned()
    }
}

impl Default for Subscriptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Tracker for in-flight NOTIFY tasks. On shutdown, `shutdown_abort` aborts them
/// to release immediately. Shared from AppState via `Arc<NotifyTasks>`.
/// Uses `tokio::sync::Mutex` (not `std::sync::Mutex`) because the guard is held
/// across `.await`.
pub struct NotifyTasks {
    inner: AsyncMutex<JoinSet<()>>,
}

impl NotifyTasks {
    pub fn new() -> Self {
        Self {
            inner: AsyncMutex::new(JoinSet::new()),
        }
    }

    /// Reap completed tasks (prevents accumulation). Low frequency is fine.
    pub async fn reap(&self) {
        let mut g = self.inner.lock().await;
        while g.try_join_next().is_some() {}
    }

    /// For shutdown: abort all remaining tasks and wait for them to finish.
    pub async fn shutdown_abort(&self) {
        let mut g = self.inner.lock().await;
        g.abort_all();
        while g.join_next().await.is_some() {}
    }
}

impl Default for NotifyTasks {
    fn default() -> Self {
        Self::new()
    }
}

/// Fan out a NOTIFY of `properties` to all subscriptions of `service`.
/// Each send is spawned into the `JoinSet` for tracking (abortable on shutdown).
/// This function returns as soon as the spawns are done (no per-target delivery
/// confirmation, within SPEC §9.6 MVP scope).
///
/// `client` is the `reqwest::Client` from `AppState.notify_client` (shared keep-alive pool).
pub async fn broadcast_propchange(
    client: &reqwest::Client,
    subscriptions: &Arc<Subscriptions>,
    tasks: &Arc<NotifyTasks>,
    service: ServiceId,
    properties: &[(&str, &str)],
) {
    let snap = subscriptions.snapshot_and_increment(service);
    if snap.is_empty() {
        return;
    }
    let body = build_propertyset(properties);
    let mut g = tasks.inner.lock().await;
    // Reap completed tasks while we're here.
    while g.try_join_next().is_some() {}
    for (sid, callback, seq) in snap {
        let body_clone = body.clone();
        let client_clone = client.clone();
        g.spawn(async move {
            if let Err(e) = send_notify(&client_clone, &callback, &sid, seq, &body_clone).await {
                tracing::warn!(sid = %sid, error = %e, "propchange NOTIFY failed");
            }
        });
    }
}

/// Spawn a single NOTIFY through the tracker (used on initial SUBSCRIBE).
pub fn spawn_initial_notify(
    client: reqwest::Client,
    tasks: &Arc<NotifyTasks>,
    callback: String,
    sid: String,
    seq: u32,
    body: String,
) {
    let tasks = tasks.clone();
    // Calling `JoinSet::spawn` requires acquiring the async lock, so wrap the
    // whole acquire+spawn in `tokio::spawn` (lets the handler return immediately).
    tokio::spawn(async move {
        let mut g = tasks.inner.lock().await;
        g.spawn(async move {
            // Short delay so the NOTIFY doesn't arrive before Linn receives the 200.
            tokio::time::sleep(Duration::from_millis(50)).await;
            if let Err(e) = send_notify(&client, &callback, &sid, seq, &body).await {
                tracing::warn!(sid = %sid, error = %e, "initial NOTIFY failed");
            }
        });
    });
}

/// Build the `<e:propertyset>` XML from SPEC §9.6.
/// Values are XML-escaped minimally (`& < > " '`).
pub fn build_propertyset(properties: &[(&str, &str)]) -> String {
    let mut s = String::from(
        "<?xml version=\"1.0\"?>\n\
         <e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\n",
    );
    for (name, value) in properties {
        s.push_str("  <e:property><");
        s.push_str(name);
        s.push('>');
        s.push_str(&xml_escape(value));
        s.push_str("</");
        s.push_str(name);
        s.push_str("></e:property>\n");
    }
    s.push_str("</e:propertyset>\n");
    s
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gn1_register_creates_uuid_sid_and_zero_seq() {
        let subs = Subscriptions::new();
        let sid = subs
            .register(
                "http://192.168.0.1:9999/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(1800),
            )
            .unwrap();
        assert!(sid.starts_with("uuid:"));
        let sub = subs.get(&sid).unwrap();
        assert_eq!(sub.seq, 0);
        assert_eq!(sub.service, ServiceId::ContentDirectory);
        assert_eq!(sub.callback_url, "http://192.168.0.1:9999/cb");
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn gn2_refresh_extends_expires_at_and_returns_true() {
        let subs = Subscriptions::new();
        let sid = subs
            .register(
                "http://x/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        let original = subs.get(&sid).unwrap().expires_at;
        // Wait a moment, then refresh — just verify that expires_at has advanced.
        std::thread::sleep(Duration::from_millis(5));
        assert!(subs.refresh(&sid, Duration::from_secs(120)));
        let extended = subs.get(&sid).unwrap().expires_at;
        assert!(extended > original);

        // Unknown SID returns false.
        assert!(!subs.refresh("uuid:nonexistent", Duration::from_secs(60)));
    }

    #[test]
    fn gn3_sweep_expired_removes_past_subscriptions() {
        let subs = Subscriptions::new();
        // Inject an entry directly with an expires_at in the past.
        {
            let mut g = lock_recover(&subs.inner);
            g.insert(
                "uuid:past".to_string(),
                Subscription {
                    sid: "uuid:past".to_string(),
                    callback_url: "http://x/cb".to_string(),
                    service: ServiceId::ContentDirectory,
                    expires_at: Instant::now() - Duration::from_secs(1),
                    seq: 0,
                },
            );
        }
        let sid_alive = subs
            .register(
                "http://x/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(subs.len(), 2);

        let removed = subs.sweep_expired();
        assert_eq!(removed, 1);
        assert_eq!(subs.len(), 1);
        assert!(subs.get(&sid_alive).is_some());
    }

    #[test]
    fn gn4_propertyset_xml_has_system_update_id() {
        let xml = build_propertyset(&[("SystemUpdateID", "123")]);
        assert!(xml.contains("<e:propertyset"));
        assert!(xml.contains("urn:schemas-upnp-org:event-1-0"));
        assert!(xml.contains("<SystemUpdateID>123</SystemUpdateID>"));
    }

    #[test]
    fn gn4b_propertyset_xml_escapes_values() {
        let xml = build_propertyset(&[("Foo", "a & b < c")]);
        assert!(xml.contains("a &amp; b &lt; c"));
    }

    #[test]
    fn gn_take_next_seq_increments() {
        let subs = Subscriptions::new();
        let sid = subs
            .register(
                "http://x/cb".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(subs.take_next_seq(&sid), Some(0));
        assert_eq!(subs.take_next_seq(&sid), Some(1));
        assert_eq!(subs.take_next_seq(&sid), Some(2));
        assert_eq!(subs.take_next_seq("uuid:nope"), None);
    }

    #[test]
    fn gn_register_returns_none_at_cap() {
        // After filling to MAX_SUBSCRIPTIONS, register must return None (security §3).
        let subs = Subscriptions::new();
        for i in 0..MAX_SUBSCRIPTIONS {
            let sid = subs
                .register(
                    format!("http://x/{}", i),
                    ServiceId::ContentDirectory,
                    Duration::from_secs(60),
                )
                .unwrap();
            assert!(!sid.is_empty());
        }
        assert_eq!(subs.len(), MAX_SUBSCRIPTIONS);

        let overflow = subs.register(
            "http://x/overflow".to_string(),
            ServiceId::ContentDirectory,
            Duration::from_secs(60),
        );
        assert!(overflow.is_none(), "should refuse beyond MAX_SUBSCRIPTIONS");
        assert_eq!(subs.len(), MAX_SUBSCRIPTIONS);
    }

    #[test]
    fn gn_snapshot_filters_by_service_and_increments() {
        let subs = Subscriptions::new();
        let sid_cd = subs
            .register(
                "http://x/cd".to_string(),
                ServiceId::ContentDirectory,
                Duration::from_secs(60),
            )
            .unwrap();
        let sid_cm = subs
            .register(
                "http://x/cm".to_string(),
                ServiceId::ConnectionManager,
                Duration::from_secs(60),
            )
            .unwrap();

        let snap = subs.snapshot_and_increment(ServiceId::ContentDirectory);
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].0, sid_cd);
        assert_eq!(snap[0].2, 0);

        // Only the CD seq advances.
        assert_eq!(subs.get(&sid_cd).unwrap().seq, 1);
        assert_eq!(subs.get(&sid_cm).unwrap().seq, 0);
    }
}

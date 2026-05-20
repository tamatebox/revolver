use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use tokio::sync::Semaphore;

use crate::art::ArtCache;
use crate::random::RandomState;
use crate::upnp::gena::{NotifyTasks, Subscriptions};

/// Browse-side tuning values pulled from `[browse]` in `config.toml`. Flowed
/// from AppState to each view via `BrowseContext`.
#[derive(Debug, Clone)]
pub struct BrowseSettings {
    /// Max items returned under `cat:recent` (SPEC §6.7).
    /// Caps SOAP `RequestedCount` even when the client asks for more.
    pub recently_added_limit: usize,
    /// Hide albums older than this many days from `cat:recent`. `None` (the
    /// default) means no age cap — show all albums by recency.
    pub recently_added_max_age_days: Option<u32>,
    /// Max items per page for `cat:random` (SPEC §6.6). Same role.
    pub random_albums_limit: usize,
    /// Whether to expose `cat:hires` / `cat:lossy` / `cat:mixed` in the root
    /// container (SPEC §6.2). When false, all three are hidden from the root.
    pub quality_categories: bool,
}

/// Runtime state shared by HTTP handlers and SSDP tasks (ARCHITECTURE.md §1).
/// Injected into each handler via `axum::extract::State<AppState>`.
#[derive(Clone)]
pub struct AppState {
    pub db_pool: crate::db::Pool,
    /// Absolute path already `canonicalize`d in `main.rs`. Used by the stream
    /// handler's `path_within_library` check; canonicalized once at startup to
    /// avoid the per-request syscall (ops §P1).
    pub library_root: Arc<PathBuf>,
    pub extensions: Arc<Vec<String>>,
    pub scan_parallel: usize,
    /// `Semaphore::new(1)`. Prevents concurrent scans via `try_acquire_owned`.
    pub scan_lock: Arc<Semaphore>,

    /// Tuning values from `[browse]`. Wrapped in an `RwLock` so the config API
    /// (#13) can swap in updated values without restarting the process. Read in
    /// the SOAP path on every Browse / Search; cloned to a local snapshot to
    /// keep the lock window tiny.
    pub browse: Arc<RwLock<BrowseSettings>>,

    /// Snapshot of toml defaults for every catalog key (`config_catalog::CATALOG`),
    /// captured at startup. The config API uses it to report `"default"` /
    /// `"source"` alongside effective values. Immutable after startup.
    pub config_defaults: Arc<crate::config_catalog::DefaultsMap>,

    // ── UPnP / SSDP ─────────────────────────────────────────────
    /// Device UUID (persisted in `server_state.uuid`, generated on first run).
    pub uuid: Arc<String>,
    pub friendly_name: Arc<String>,
    pub http_port: u16,
    /// Used in the `LOCATION` header of SSDP responses (not for send).
    pub local_ip: Ipv4Addr,

    /// GENA subscriptions (in-process memory only; lost across restarts). SPEC §9.4.
    pub subscriptions: Arc<Subscriptions>,

    /// Tracker for in-flight NOTIFY tasks (aborted on shutdown).
    pub notify_tasks: Arc<NotifyTasks>,

    /// Shared HTTP client (reqwest) for sending NOTIFY. Holds a keep-alive pool
    /// so connect/teardown does not dominate during the Linn re-fetch rush after
    /// a SystemUpdateID bump (perf §P0).
    pub notify_client: reqwest::Client,

    /// In-memory album-art cache (SPEC §8.3).
    pub art_cache: Arc<ArtCache>,

    /// Shuffled album_id array for the `cat:random` view (SPEC §6.6).
    /// Reshuffled on startup, scan completion, and `POST /admin/reshuffle`.
    pub random_state: Arc<RandomState>,

    /// Live progress counter for an in-flight scan (#12). `Phase::Idle` when no
    /// scan is running. Read by `/admin/scan-progress` without locks.
    pub scan_progress: Arc<crate::scan::progress::ScanProgress>,

    /// Process start time (unix seconds). Used to compute uptime in
    /// `/admin/stats` (SPEC §8.5).
    pub started_at: i64,

    /// Whether the SSDP listener bound successfully and is running (for
    /// `/admin/stats` debugging). Helps diagnose "Linn says no music" when
    /// port 1900 is contended (ops §P1).
    pub ssdp_listener_active: Arc<AtomicBool>,
    /// Same as above, for the advertiser.
    pub ssdp_advertiser_active: Arc<AtomicBool>,
}

impl BrowseSettings {
    /// Build from values already loaded from `config.toml`. Structured to be
    /// config-independent so it can be reused from both production and tests.
    pub fn from_parts(
        recently_added_limit: usize,
        recently_added_max_age_days: Option<u32>,
        random_albums_limit: usize,
        quality_categories: bool,
    ) -> Self {
        Self {
            recently_added_limit: recently_added_limit.max(1),
            recently_added_max_age_days,
            random_albums_limit: random_albums_limit.max(1),
            quality_categories,
        }
    }
}

impl Default for BrowseSettings {
    fn default() -> Self {
        Self {
            recently_added_limit: 1000,
            recently_added_max_age_days: None,
            random_albums_limit: 1000,
            quality_categories: true,
        }
    }
}

/// Build a reqwest::Client with modest settings, suitable for tests. Production
/// uses the same builder via `main.rs`, which sets pool size / timeout there.
pub fn build_notify_client() -> reqwest::Client {
    reqwest::Client::builder()
        // SPEC §9.6: short timeouts on both read/write are enough for NOTIFY (LAN only).
        .connect_timeout(std::time::Duration::from_secs(5))
        .timeout(std::time::Duration::from_secs(10))
        // Keep-alive is reqwest's default (HTTP/1.1 keep-alive). Size
        // pool_max_idle_per_host to cover one Linn plus a handful of other CPs.
        .pool_max_idle_per_host(8)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
        .expect("reqwest client build (basic config without TLS should not fail)")
}

#[cfg(test)]
pub mod test_helpers {
    //! `AppState` builder shared across the test suite. Centralized to avoid
    //! repeating the same struct construction in every handler test file.
    //!
    //! The caller must retain the returned `tempfile::TempDir` (dropping it
    //! deletes the DB file).

    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, RwLock};

    use tempfile::TempDir;
    use tokio::sync::Semaphore;

    use super::{build_notify_client, AppState, BrowseSettings};
    use crate::art::ArtCache;
    use crate::db;
    use crate::random::RandomState;
    use crate::upnp::gena::{NotifyTasks, Subscriptions};

    /// The most typical test_state: empty library + temp DB + LOCALHOST.
    /// The returned TempDir must be kept alive until the test ends.
    pub fn test_state() -> (AppState, TempDir) {
        let dbdir = TempDir::new().unwrap();
        let pool = db::pool(&dbdir.path().join("test.db")).unwrap();
        // Canonicalize library_root just like production
        // (so path_within_library tests do not break on macOS /tmp -> /private/tmp).
        let library_root =
            std::fs::canonicalize(dbdir.path()).unwrap_or_else(|_| dbdir.path().to_path_buf());
        let state = AppState {
            db_pool: pool,
            library_root: Arc::new(library_root),
            extensions: Arc::new(vec!["flac".to_string()]),
            scan_parallel: 1,
            scan_lock: Arc::new(Semaphore::new(1)),
            browse: Arc::new(RwLock::new(BrowseSettings::default())),
            config_defaults: Arc::new(std::collections::HashMap::new()),
            uuid: Arc::new("TEST-UUID".to_string()),
            friendly_name: Arc::new("Test Server".to_string()),
            http_port: 8200,
            local_ip: std::net::Ipv4Addr::LOCALHOST,
            subscriptions: Arc::new(Subscriptions::new()),
            notify_tasks: Arc::new(NotifyTasks::new()),
            notify_client: build_notify_client(),
            art_cache: Arc::new(ArtCache::new()),
            random_state: Arc::new(RandomState::new()),
            scan_progress: Arc::new(crate::scan::progress::ScanProgress::new()),
            started_at: 0,
            ssdp_listener_active: Arc::new(AtomicBool::new(false)),
            ssdp_advertiser_active: Arc::new(AtomicBool::new(false)),
        };
        (state, dbdir)
    }

    /// For tests that want the library root in a separate TempDir (scan / stream).
    /// Returns (state, db_tmp, library_tmp).
    pub fn test_state_with_library() -> (AppState, TempDir, TempDir) {
        let (mut state, dbdir) = test_state();
        let libdir = TempDir::new().unwrap();
        let canonical_lib =
            std::fs::canonicalize(libdir.path()).unwrap_or_else(|_| PathBuf::from(libdir.path()));
        state.library_root = Arc::new(canonical_lib);
        (state, dbdir, libdir)
    }
}

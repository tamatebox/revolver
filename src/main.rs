use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use tokio::sync::{broadcast, Semaphore};
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

use revolver::art::ArtCache;
use revolver::db::state_kv;
use revolver::random::RandomState;
use revolver::state::{build_notify_client, AppState};
use revolver::{config, config_catalog, db, error, http, scan, ssdp, upnp};

#[derive(Debug, Parser)]
#[command(name = "revolver", version, about = "UPnP MediaServer for Linn DSM/2")]
struct Args {
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let cfg = config::Config::load(&args.config)?;
    tracing::info!(
        config = %args.config.display(),
        root = %cfg.library.root.display(),
        "loaded config"
    );

    // SPEC §1: canonicalize library_root once at startup and reuse it
    // (ops §P1: fixes the issue where the stream handler canonicalized per-request).
    // Abort on failure (a non-existent root is a config error).
    let library_root = std::fs::canonicalize(&cfg.library.root).with_context(|| {
        format!(
            "cannot canonicalize library.root (missing path / insufficient permissions?): {}",
            cfg.library.root.display()
        )
    })?;

    // ops §P1: inside `db::pool()`, `ensure_compatible_or_err()` runs before `migrate()`.
    // On downgrade (DB schema_version greater than binary), `pool()` returns Err and
    // startup is aborted.
    let pool = db::pool(&cfg.server.db_path)?;
    tracing::info!(db = %cfg.server.db_path.display(), "opened database pool");

    // Initialize UUID and system_update_id in server_state (SPEC §5.1).
    let uuid = {
        let conn = pool.get()?;

        // Initial system_update_id is 1 (SPEC §5.1).
        if state_kv::get(&conn, "system_update_id")?.is_none() {
            state_kv::set(&conn, "system_update_id", "1")?;
            tracing::info!("initialized system_update_id = 1");
        }

        // UUID: reuse existing value if present, otherwise generate
        // (use the config value if it is not "auto").
        match state_kv::get(&conn, "uuid")? {
            Some(u) => u,
            None => {
                let new_uuid = if cfg.server.uuid == "auto" {
                    Uuid::new_v4().to_string()
                } else {
                    cfg.server.uuid.clone()
                };
                state_kv::set(&conn, "uuid", &new_uuid)?;
                tracing::info!(uuid = %new_uuid, "generated and persisted device UUID");
                new_uuid
            }
        }
    };

    let local_ip = ssdp::detect_local_ip();
    tracing::info!(local_ip = %local_ip, "detected local IP");

    let subscriptions = Arc::new(upnp::gena::Subscriptions::new());
    let notify_tasks = Arc::new(upnp::gena::NotifyTasks::new());
    let notify_client = build_notify_client();

    let ssdp_listener_active = Arc::new(AtomicBool::new(false));
    let ssdp_advertiser_active = Arc::new(AtomicBool::new(false));

    // Layer saved config_overrides (#13) on top of the toml defaults so the
    // process starts with the user's last-saved values.
    let config_defaults = Arc::new(config_catalog::precompute_defaults(&cfg));
    let browse_settings = {
        let conn = pool.get()?;
        Arc::new(RwLock::new(config_catalog::build_browse_settings(
            &config_defaults,
            &conn,
        )?))
    };

    let state = AppState {
        db_pool: pool.clone(),
        library_root: Arc::new(library_root),
        extensions: Arc::new(cfg.library.extensions.clone()),
        scan_parallel: cfg.scan.parallel,
        scan_lock: Arc::new(Semaphore::new(1)),
        browse: browse_settings,
        config_defaults: config_defaults.clone(),
        uuid: Arc::new(uuid),
        friendly_name: Arc::new(cfg.server.friendly_name.clone()),
        http_port: cfg.server.http_port,
        local_ip,
        subscriptions: subscriptions.clone(),
        notify_tasks: notify_tasks.clone(),
        notify_client: notify_client.clone(),
        art_cache: Arc::new(ArtCache::new()),
        random_state: Arc::new(RandomState::new()),
        started_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        ssdp_listener_active: ssdp_listener_active.clone(),
        ssdp_advertiser_active: ssdp_advertiser_active.clone(),
    };

    // Startup random shuffle (SPEC §6.6). Order vs. scan does not matter, but run once
    // upfront so `cat:random` is not empty under no-scan / scan-disabled startups.
    // If scan runs, another reshuffle follows after it -- double shuffle is acceptable.
    {
        let conn = pool.get()?;
        let n = state.random_state.reshuffle(&conn)?;
        tracing::info!(albums = n, "initial random reshuffle complete");
    }

    // Startup scan (SPEC §4.4 / config.scan.on_startup).
    if cfg.scan.on_startup {
        let library_root = state.library_root.clone();
        let extensions = state.extensions.clone();
        let parallel = state.scan_parallel;
        // ops §P1: replace expect() chain with anyhow to avoid process abort.
        // A closed semaphore is exceptional, but bubbling it up explicitly as a
        // startup failure is easier to trace from ops logs than a panic.
        let permit = state
            .scan_lock
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("scan_lock semaphore closed unexpectedly during startup"))?;
        let scan_pool = pool.clone();
        let report =
            tokio::task::spawn_blocking(move || -> error::Result<scan::report::ScanReport> {
                let _permit = permit;
                let mut conn = scan_pool.get()?;
                scan::run(&mut conn, &library_root, &extensions, parallel)
            })
            .await
            .map_err(|e| anyhow!("startup scan spawn_blocking join failed: {e}"))??;
        tracing::info!(
            scan_id = %report.scan_id,
            duration_ms = report.duration_ms,
            "startup scan complete"
        );

        // If the startup scan produced a structural change (insert/delete/update),
        // send propchange to already-SUBSCRIBED control points (SPEC §5.1).
        // Right after startup there are typically 0 subscribers, so this is often a no-op.
        if scan::should_bump_system_update_id(&report.stats) {
            let conn = pool.get()?;
            let id = state_kv::get(&conn, "system_update_id")?.unwrap_or_else(|| "1".to_string());
            drop(conn);
            upnp::gena::broadcast_propchange(
                &notify_client,
                &subscriptions,
                &notify_tasks,
                upnp::gena::ServiceId::ContentDirectory,
                &[("SystemUpdateID", &id)],
            )
            .await;

            // On structural change, also reshuffle Random (SPEC §6.6).
            // Full reshuffle every time to avoid new releases being buried at the tail.
            let conn = pool.get()?;
            let n = state.random_state.reshuffle(&conn)?;
            tracing::info!(albums = n, "post-startup-scan random reshuffle complete");
        }
    }

    // Start the SSDP listener / advertiser.
    // ops §P1: hoist bind() up to main so failures are visible immediately (the old
    // impl warn'd inside the task and silently died). Only spawn tasks whose socket bound.
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let (listener_socket, advertiser_socket) = ssdp::try_bind_pair();
    let ssdp_listener = listener_socket.map(|sock| {
        tokio::spawn(ssdp::listener_task(
            state.clone(),
            sock,
            shutdown_tx.subscribe(),
        ))
    });
    let ssdp_advertiser = advertiser_socket.map(|sock| {
        tokio::spawn(ssdp::advertiser_task(
            state.clone(),
            sock,
            shutdown_tx.subscribe(),
        ))
    });

    // Expiry sweep for GENA subscriptions. 60s interval is sufficient (SPEC §9.5).
    let sweep_subs = subscriptions.clone();
    let mut sweep_shutdown = shutdown_tx.subscribe();
    let sweep_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    let removed = sweep_subs.sweep_expired();
                    if removed > 0 {
                        tracing::info!(removed, "expired subscriptions swept");
                    }
                }
                _ = sweep_shutdown.recv() => break,
            }
        }
    });

    let addr = format!("{}:{}", cfg.server.bind_address, cfg.server.http_port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(listen = %addr, "http server listening");

    // ops §P1: stash the scan_lock handle before moving state into the router so
    // we can wait for in-flight scans at shutdown.
    let scan_lock_for_shutdown = state.scan_lock.clone();

    let app = http::router(state);
    let shutdown_clone = shutdown_tx.clone();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(shutdown_clone))
        .await?;

    // ops §P1: wait for any in-flight scan to complete (prevents WAL tail loss).
    // Acquire is immediate when no scan is running; otherwise it blocks until the
    // scan finishes. Avoids long-scan + Ctrl-C truncating the WAL mid-flight.
    tracing::info!("waiting for in-flight scan to complete (if any)");
    if let Ok(_permit) = scan_lock_for_shutdown.acquire().await {
        // Drop the permit immediately upon acquisition.
    }
    tracing::info!("scan_lock acquired; proceeding to shutdown");

    // Wait for SSDP tasks to finish sending byebye.
    if let Some(h) = ssdp_listener {
        let _ = h.await;
    }
    if let Some(h) = ssdp_advertiser {
        let _ = h.await;
    }
    let _ = sweep_task.await;

    // Abort any remaining in-flight NOTIFY tasks (graceful shutdown).
    notify_tasks.shutdown_abort().await;

    Ok(())
}

/// ops §P1: also trigger graceful shutdown on SIGTERM (so systemd / docker stop
/// finishes sending byebye before exit). Unix only; on Windows ctrl_c only.
async fn shutdown_signal(shutdown_tx: broadcast::Sender<()>) {
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to install SIGTERM handler; falling back to ctrl_c only");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("shutdown signal: ctrl-c"),
        _ = term => tracing::info!("shutdown signal: SIGTERM"),
    }
    let _ = shutdown_tx.send(());
}

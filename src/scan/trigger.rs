//! Detached scan launcher shared by every trigger path: startup (#15),
//! `POST /admin/rescan` (#18), and the filesystem watcher (#30).
//!
//! The caller acquires the `scan_lock` permit (so it decides between
//! "wait" and "409 Conflict") and generates the `scan_id` (so the value it
//! reports is the one `scan::run` persists). Everything after that — the
//! blocking scan, the SystemUpdateID propchange NOTIFY, and the Random
//! reshuffle — is identical across triggers and lives here.

use tokio::sync::OwnedSemaphorePermit;
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::db::state_kv;
use crate::scan::report::ScanReport;
use crate::state::AppState;

/// Spawn the scan + post-scan side effects on a detached task. The permit is
/// held until the blocking scan returns, so shutdown's `scan_lock.acquire()`
/// still waits for it (WAL safety).
pub fn spawn_detached(
    state: AppState,
    permit: OwnedSemaphorePermit,
    scan_id: String,
    span: tracing::Span,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            let library_root = state.library_root.clone();
            let extensions = state.extensions.clone();
            let parallel = state.scan_parallel;
            let pool = state.db_pool.clone();
            let progress = state.scan_progress.clone();

            let scan_result =
                tokio::task::spawn_blocking(move || -> crate::error::Result<ScanReport> {
                    let _permit = permit; // Hold the permit until the scan completes.
                    let mut conn = pool.get()?;
                    crate::scan::run(
                        &mut conn,
                        &library_root,
                        &extensions,
                        parallel,
                        progress,
                        scan_id,
                    )
                })
                .await;

            let report = match scan_result {
                Ok(Ok(r)) => r,
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "background scan failed");
                    return;
                }
                Err(e) => {
                    tracing::error!(error = %e, "background scan join failed");
                    return;
                }
            };
            tracing::info!(duration_ms = report.duration_ms, "background scan complete");

            if crate::scan::should_bump_system_update_id(&report.stats) {
                after_structural_change(&state).await;
            }
        }
        .instrument(span),
    )
}

/// SPEC §5.1 / §9.6: deliver a SystemUpdateID propchange NOTIFY to CD
/// subscribers (the value is already bumped inside `scan::run`), then reorder
/// Random (SPEC §6.6) so new releases are not buried at the tail.
async fn after_structural_change(state: &AppState) {
    let conn = match state.db_pool.get() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "post-scan: failed to get DB conn");
            return;
        }
    };
    let id = state_kv::get(&conn, "system_update_id")
        .ok()
        .flatten()
        .unwrap_or_else(|| "1".to_string());
    drop(conn);
    crate::upnp::gena::broadcast_propchange(
        &state.notify_client,
        &state.subscriptions,
        &state.notify_tasks,
        crate::upnp::gena::ServiceId::ContentDirectory,
        &[("SystemUpdateID", &id)],
    )
    .await;

    if let Ok(conn) = state.db_pool.get() {
        // Lock poison is rare; degrade to `None` (= no cap) rather than
        // skipping the reshuffle entirely. Worse than the configured cap,
        // but preserves the user-visible reshuffle behavior.
        let limit = state
            .browse
            .read()
            .map(|s| s.random_albums_limit)
            .unwrap_or(None);
        match state.random_state.reshuffle(&conn, limit) {
            Ok(n) => tracing::info!(albums = n, "post-scan random reshuffle complete"),
            Err(e) => tracing::error!(error = %e, "post-scan reshuffle failed"),
        }
    }
}

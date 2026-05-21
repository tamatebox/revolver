use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rusqlite::Connection;
use tracing::{debug, info, warn};

use crate::db::{albums, state_kv, tracks};
use crate::error::Result;
use crate::scan::progress::{Phase, ScanProgress};
use crate::scan::report::{Issue, ScanReport, ScanStats, SkippedEntry};

pub mod matcher;
pub mod progress;
pub mod report;
pub mod tagger;
pub mod walker;

/// Per-path pre-processing result (FS metadata already fetched).
struct EnumeratedTrack {
    path: PathBuf,
    path_str: String,
    fs_btime: Option<SystemTime>,
    fs_mtime: SystemTime,
}

/// Track for which tag read succeeded.
struct TaggedTrack<'a> {
    enumerated: &'a EnumeratedTrack,
    tags: tagger::TrackTags,
}

/// Full orchestrator per SPEC §4.1. Steps 1-12 + scan report persistence (§4.7).
///
/// ops §P1: on mid-scan failure, overwrite `last_scan_report` with a minimal
/// partial report so the failure trace is visible via `/admin/scan-report`.
#[tracing::instrument(
    name = "scan",
    skip_all,
    fields(scan_id = tracing::field::Empty, root = %root.display()),
)]
pub fn run(
    conn: &mut Connection,
    root: &Path,
    extensions: &[String],
    parallel: usize,
    progress: Arc<ScanProgress>,
) -> Result<ScanReport> {
    let scan_id = ScanReport::new_id();
    tracing::Span::current().record("scan_id", scan_id.as_str());
    let started = SystemTime::now();
    let started_secs = to_unix_secs(started);

    info!("scan started");
    progress.begin_scan();

    let result = run_inner(
        conn,
        root,
        extensions,
        parallel,
        &scan_id,
        started,
        started_secs,
        &progress,
    );
    if let Err(ref e) = result {
        write_failure_report(conn, &scan_id, started, started_secs, &e.to_string());
    }
    progress.finish();
    result
}

/// On mid-scan failure, write a partial report to `server_state.last_scan_report`.
/// Internal failures (JSON serialization or DB write) are logged at warn level
/// without swallowing the original Err.
pub(crate) fn write_failure_report(
    conn: &Connection,
    scan_id: &str,
    started: SystemTime,
    started_secs: i64,
    error: &str,
) {
    let completed = SystemTime::now();
    let duration_ms = completed
        .duration_since(started)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let failed = ScanReport {
        scan_id: scan_id.to_string(),
        started_at: started_secs,
        completed_at: to_unix_secs(completed),
        duration_ms,
        is_initial: false,
        stats: ScanStats::default(),
        issues: vec![],
        skipped: vec![],
        error: Some(error.to_string()),
    };
    tracing::warn!(error = %error, "scan failed; writing partial report");
    match serde_json::to_string(&failed) {
        Ok(json) => {
            if let Err(e2) = state_kv::set(conn, "last_scan_report", &json) {
                tracing::error!(error = %e2, "failed to persist scan failure report");
            }
        }
        Err(e) => tracing::error!(error = %e, "failed to serialize scan failure report"),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    conn: &mut Connection,
    root: &Path,
    extensions: &[String],
    parallel: usize,
    scan_id: &str,
    started: SystemTime,
    started_secs: i64,
    progress: &Arc<ScanProgress>,
) -> Result<ScanReport> {
    // ── 1-2. walker ─────────────────────────────────────────────────────
    let walk_result = walker::walk(root, extensions);
    let companion_files_seen = walk_result.companion_files_seen;
    let files_enumerated =
        walk_result.audio_files.len() + walk_result.skipped.len() + companion_files_seen;

    // security §6: normalize scan report paths to be library_root-relative.
    // Exposing absolute host paths in JSON would leak filesystem layout.
    let skipped: Vec<SkippedEntry> = walk_result
        .skipped
        .iter()
        .map(|s| SkippedEntry {
            path: relativize_path(&s.path, root),
            reason: s.reason.as_str(),
        })
        .collect();

    // ── 3. Collect FS metadata for each path ─────────────────────────
    let enumerated: Vec<EnumeratedTrack> = walk_result
        .audio_files
        .iter()
        .map(|p| {
            let meta = std::fs::metadata(p).ok();
            EnumeratedTrack {
                path: p.clone(),
                path_str: p.to_string_lossy().into_owned(),
                fs_btime: meta.as_ref().and_then(|m| m.created().ok()),
                fs_mtime: meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or_else(SystemTime::now),
            }
        })
        .collect();

    let is_initial = is_initial_scan(conn)?;
    debug!(is_initial, "scan mode");

    // ── 4. DB mtime snapshot ────────────────────────────────────────
    let db_mtimes = tracks::get_mtimes(conn)?;

    // ── 5. Partition by mtime diff (SPEC §4.5) ──────────────────────
    let path_mtime_pairs: Vec<(&str, i64)> = enumerated
        .iter()
        .map(|e| (e.path_str.as_str(), to_unix_secs(e.fs_mtime)))
        .collect();
    let (needs_tag_read_idx, tracks_unchanged) = partition_by_mtime(&path_mtime_pairs, &db_mtimes);

    // ── 6. Parallel tag read (only paths that changed) ─────────────
    let tagged = parallel_tag_read(
        &enumerated,
        &needs_tag_read_idx,
        parallel,
        Arc::clone(progress),
    );
    let tag_read_failed = needs_tag_read_idx.len() - tagged.len();

    // ── Pre-count for albums_inserted ──────────────────────────────
    let albums_before: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))?;

    // ── 7. upsert + deletion detection + orphan cleanup + recompute ──
    let mut tracks_inserted = 0usize;
    let mut tracks_updated = 0usize;
    let mut issues: Vec<Issue> = Vec::new();

    // SPEC §4.1: wrapping a large-scale track scan in one giant transaction
    // (a) loses everything on mid-scan crash, (b) bloats the WAL and prolongs
    // checkpoint waits. Use batch commits of 1000 rows. 1000 is the typical
    // SQLite batch-efficiency peak (amortized prepare cost + single WAL fsync).
    const TX_CHUNK: usize = 1000;
    progress.enter(Phase::Upsert, tagged.len());
    let total_chunks = tagged.len().div_ceil(TX_CHUNK);
    for (chunk_idx, chunk) in tagged.chunks(TX_CHUNK).enumerate() {
        let tx = conn.transaction()?;
        for t in chunk {
            // Issue detection (SPEC §4.7). Paths in the report are library_root-relative
            // (security §6, to prevent absolute host path leakage).
            let path_str = t.enumerated.path_str.as_str();
            let rel = relativize_path(&t.enumerated.path, root);
            if t.tags.album.as_deref().unwrap_or("").is_empty() {
                issues.push(Issue {
                    path: rel.clone(),
                    issue: "missing_album",
                });
            }
            if !t.tags.compilation && t.tags.album_artist.as_deref().unwrap_or("").is_empty() {
                issues.push(Issue {
                    path: rel.clone(),
                    issue: "missing_album_artist",
                });
            }
            if t.tags.duration_ms.is_none() {
                issues.push(Issue {
                    path: rel,
                    issue: "no_duration",
                });
            }

            let eff_aa = matcher::effective_album_artist(
                t.tags.compilation,
                t.tags.album_artist.as_deref(),
                t.tags.artist.as_deref(),
            );
            let album_name = t
                .tags
                .album
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Unknown Album");

            let album_id = albums::upsert(
                &tx,
                &albums::AlbumKey {
                    effective_album_artist: &eff_aa,
                    album: album_name,
                    compilation: t.tags.compilation,
                },
                t.tags.album_artist.as_deref(),
                started_secs,
            )?;

            let added_at = matcher::determine_added_at(
                t.enumerated.fs_btime,
                Some(t.enumerated.fs_mtime),
                is_initial,
                started,
            );

            let outcome = tracks::upsert(
                &tx,
                &tracks::TrackRow {
                    album_id,
                    path: path_str,
                    title: t.tags.title.as_deref(),
                    artist: t.tags.artist.as_deref(),
                    genre: t.tags.genre.as_deref(),
                    track_num: t.tags.track_num,
                    disc_num: t.tags.disc_num,
                    duration_ms: t.tags.duration_ms,
                    sample_rate: t.tags.sample_rate,
                    bit_depth: t.tags.bit_depth,
                    channels: t.tags.channels,
                    bitrate: t.tags.bitrate,
                    codec: &t.tags.codec,
                    mime_type: &t.tags.mime_type,
                    file_size: t.tags.file_size,
                    added_at,
                    mtime: to_unix_secs(t.enumerated.fs_mtime),
                    composer: t.tags.composer.as_deref(),
                    conductor: t.tags.conductor.as_deref(),
                    performer: t.tags.performer.as_deref(),
                    year: t.tags.year,
                    rg_track_gain: t.tags.rg_track_gain,
                    rg_track_peak: t.tags.rg_track_peak,
                    rg_album_gain: t.tags.rg_album_gain,
                    rg_album_peak: t.tags.rg_album_peak,
                    artist_sort: t.tags.artist_sort.as_deref(),
                    album_artist_sort: t.tags.album_artist_sort.as_deref(),
                    album_sort: t.tags.album_sort.as_deref(),
                    title_sort: t.tags.title_sort.as_deref(),
                    composer_sort: t.tags.composer_sort.as_deref(),
                    original_year: t.tags.original_year,
                    mb_recording_id: t.tags.mb_recording_id.as_deref(),
                    mb_release_id: t.tags.mb_release_id.as_deref(),
                    mb_release_group_id: t.tags.mb_release_group_id.as_deref(),
                    mb_artist_id: t.tags.mb_artist_id.as_deref(),
                    mb_release_artist_id: t.tags.mb_release_artist_id.as_deref(),
                },
            )?;
            match outcome {
                tracks::UpsertOutcome::Inserted => tracks_inserted += 1,
                tracks::UpsertOutcome::Updated => tracks_updated += 1,
            }
        }
        tx.commit()?;
        progress.advance(chunk.len());
        info!(
            chunk = chunk_idx + 1,
            total_chunks,
            committed = progress.current(),
            total = progress.total(),
            "upsert chunk committed"
        );
    }

    progress.enter(Phase::Postprocess, 0);

    // Wrap post-processing (deletion detection / orphan cleanup / recompute)
    // in a separate transaction. Post-processing itself does a full-table scan
    // of albums + tracks at high water mark, but it's small enough vs. upsert
    // chunks that a single tx is fine.
    let post_tx = conn.transaction()?;
    let enumerated_set: HashSet<&str> = enumerated.iter().map(|e| e.path_str.as_str()).collect();
    let tracks_deleted = tracks::detect_deleted(&post_tx, &enumerated_set)?;
    let albums_deleted = albums::delete_orphans(&post_tx)?;
    albums::recalc_counts(&post_tx)?;
    albums::recalc_quality(&post_tx)?;
    // Phase 3 denormalize: to remove GROUP BY from cat:recent / cat:played Browse,
    // refresh albums.last_added_at and albums.last_played_at in bulk here.
    // last_played_at is normally bumped one row at a time by the stream handler
    // on the hot path; this bulk recalc reconciles the post-scan state after
    // orphan deletion or track reshuffling.
    albums::recalc_last_added_at(&post_tx)?;
    albums::recalc_last_played_at(&post_tx)?;
    post_tx.commit()?;

    // ── albums_inserted = (after - before) + deleted ───────────────────
    let albums_after: i64 = conn.query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))?;
    let albums_inserted = ((albums_after - albums_before) + albums_deleted as i64).max(0) as usize;

    // ── 8. Build and persist ScanReport ─────────────────────────────
    let completed = SystemTime::now();
    let duration_ms = completed
        .duration_since(started)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let stats = ScanStats {
        files_enumerated,
        tracks_inserted,
        tracks_updated,
        tracks_unchanged,
        tracks_deleted,
        albums_inserted,
        albums_deleted,
        tag_read_failed,
        companion_files_seen,
    };

    // SPEC §5.1: bump system_update_id by 1 on structural change.
    // Quality-only changes (e.g. bitrate) should not bump it; this impl
    // includes tracks_updated so it over-bumps, but the worst case is just
    // a redundant Linn-side re-fetch (no real harm).
    if should_bump_system_update_id(&stats) {
        bump_system_update_id(conn)?;
    }

    info!(
        duration_ms,
        files_enumerated = stats.files_enumerated,
        tracks_inserted = stats.tracks_inserted,
        tracks_updated = stats.tracks_updated,
        tracks_unchanged = stats.tracks_unchanged,
        tracks_deleted = stats.tracks_deleted,
        albums_inserted = stats.albums_inserted,
        albums_deleted = stats.albums_deleted,
        tag_read_failed = stats.tag_read_failed,
        companion_files_seen = stats.companion_files_seen,
        issues = issues.len(),
        skipped = skipped.len(),
        "scan complete"
    );

    let report = ScanReport {
        scan_id: scan_id.to_string(),
        started_at: started_secs,
        completed_at: to_unix_secs(completed),
        duration_ms,
        is_initial,
        stats,
        issues,
        skipped,
        error: None,
    };

    let json = serde_json::to_string(&report)?;
    state_kv::set(conn, "last_scan_report", &json)?;

    // Refresh planner stats so post-scan Browse/Search hit SEARCH plans, then
    // truncate the WAL the scan grew. `optimize` short-circuits internally when
    // stats are fresh, and `wal_checkpoint(TRUNCATE)` is a no-op when the WAL
    // is already small — both are cheap on a no-op rescan. Failures are logged
    // but never abort: a hot WAL or unrefreshed stats are mere annoyances, not
    // a reason to lose the scan report we just wrote.
    if let Err(e) = conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);") {
        tracing::warn!(error = %e, "post-scan optimize/checkpoint failed");
    }

    Ok(report)
}

/// SPEC §4.5: enumerated indices whose `(path, mtime)` matches `db_mtimes`
/// skip the tag re-read. Returns (indices needing re-read, count skipped due
/// to mtime match).
///
/// Extracted as a standalone pure function for testability — the full scan
/// is I/O-heavy via lofty, but the partition step can be verified in isolation.
pub(crate) fn partition_by_mtime(
    enumerated: &[(&str, i64)],
    db_mtimes: &std::collections::HashMap<String, i64>,
) -> (Vec<usize>, usize) {
    let mut needs = Vec::new();
    let mut unchanged = 0usize;
    for (i, (path, mtime)) in enumerated.iter().enumerate() {
        match db_mtimes.get(*path) {
            Some(&db_mtime) if db_mtime == *mtime => unchanged += 1,
            _ => needs.push(i),
        }
    }
    (needs, unchanged)
}

fn parallel_tag_read<'a>(
    enumerated: &'a [EnumeratedTrack],
    needs_idx: &[usize],
    parallel: usize,
    progress: Arc<ScanProgress>,
) -> Vec<TaggedTrack<'a>> {
    // perf §P2: previously built `ThreadPoolBuilder::build()` per scan; now
    // memoized via `OnceLock`. Pool construction is sub-ms (not dominant),
    // but one pool per process is sufficient. The `parallel` parameter is
    // pinned to its first value (changing it across rescans has no effect —
    // an accepted limitation since the config value isn't mutated at runtime).
    use std::sync::OnceLock;
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(parallel.max(1))
            .build()
            .expect("rayon thread pool")
    });

    progress.enter(Phase::TagRead, needs_idx.len());

    // Ticker thread logs progress every 5s (#12). Stopped via `AtomicBool`
    // after the rayon work completes. Shutdown takes up to one 5s tick — fine
    // because this only runs during multi-minute scans.
    let ticker_stop = Arc::new(AtomicBool::new(false));
    let ticker_handle = spawn_progress_ticker(Arc::clone(&progress), Arc::clone(&ticker_stop));

    let tagged = pool.install(|| {
        needs_idx
            .par_iter()
            .filter_map(|&i| {
                let et = &enumerated[i];
                let outcome = match tagger::read(&et.path) {
                    Ok(tags) => Some(TaggedTrack {
                        enumerated: et,
                        tags,
                    }),
                    Err(e) => {
                        warn!(path = %et.path.display(), error = %e, "tag read failed");
                        None
                    }
                };
                progress.tick();
                outcome
            })
            .collect()
    });

    ticker_stop.store(true, Ordering::Relaxed);
    if let Some(h) = ticker_handle {
        let _ = h.join();
    }

    tagged
}

/// Background thread that logs tag-read progress every 5s. Returns `None`
/// when there is no work to track (avoids spawning a useless thread for
/// trivially-empty rescans).
///
/// Polls `stop` every 500ms so shutdown latency stays bounded even if the
/// scan completes between log windows (a fast rescan would otherwise block
/// up to 5s in the join below).
fn spawn_progress_ticker(
    progress: Arc<ScanProgress>,
    stop: Arc<AtomicBool>,
) -> Option<std::thread::JoinHandle<()>> {
    if progress.total() == 0 {
        return None;
    }
    Some(std::thread::spawn(move || {
        let log_interval = Duration::from_secs(5);
        let poll_interval = Duration::from_millis(500);
        let mut last_log = std::time::Instant::now();
        loop {
            std::thread::sleep(poll_interval);
            if stop.load(Ordering::Relaxed) {
                break;
            }
            if last_log.elapsed() < log_interval {
                continue;
            }
            last_log = std::time::Instant::now();
            let current = progress.current();
            let total = progress.total();
            if let Some(pct) = (current * 100).checked_div(total) {
                info!(
                    phase = progress.phase().as_str(),
                    current,
                    total,
                    percent = pct,
                    "scan progress"
                );
            }
        }
    }))
}

/// Return the relative path of `path` against the `root` prefix (security §6).
/// If they do not share a prefix (mainly absolute paths post-C1 reject),
/// return only the file name to hide host fs structure.
fn relativize_path(path: &Path, root: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(root) {
        return rel.to_string_lossy().into_owned();
    }
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<unknown>".to_string())
}

fn is_initial_scan(conn: &Connection) -> Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM tracks", [], |row| row.get(0))?;
    Ok(count == 0)
}

/// SPEC §5.1: bump on structural change. MVP also bumps on `tracks_updated > 0`
/// (quality-only changes aren't separated from album_id changes — slightly
/// over-bumps but acceptable).
pub fn should_bump_system_update_id(stats: &ScanStats) -> bool {
    stats.tracks_inserted > 0
        || stats.tracks_deleted > 0
        || stats.tracks_updated > 0
        || stats.albums_inserted > 0
}

/// Bump `server_state.system_update_id` by 1 (initialize to 1 if absent).
/// Returns the new value.
fn bump_system_update_id(conn: &Connection) -> Result<u32> {
    let cur: u32 = state_kv::get(conn, "system_update_id")?
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let next = cur.saturating_add(1);
    state_kv::set(conn, "system_update_id", &next.to_string())?;
    Ok(next)
}

fn to_unix_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;
    use tempfile::TempDir;

    /// Build a DB pre-seeded with one album + track.
    fn seed_db_with_one_track(conn: &Connection) -> i64 {
        let aid = albums::upsert(
            conn,
            &albums::AlbumKey {
                effective_album_artist: "AA",
                album: "Alb",
                compilation: false,
            },
            None,
            100,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tracks
               (album_id, path, duration_ms, added_at, mtime, codec, mime_type, file_size)
             VALUES (?1, '/nonexistent/orphan.flac', 100000, 50, 60, 'flac', 'audio/flac', 0)",
            params![aid],
        )
        .unwrap();
        aid
    }

    #[test]
    fn i1_orchestrator_deletes_orphaned_tracks_and_albums() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("scan.db");
        let pool = crate::db::pool(&db_path).unwrap();
        let mut conn = pool.get().unwrap();

        seed_db_with_one_track(&conn);

        // Point at an empty music root
        let music = TempDir::new().unwrap();
        let extensions = vec!["flac".to_string()];

        let report = run(
            &mut conn,
            music.path(),
            &extensions,
            1,
            Arc::new(ScanProgress::new()),
        )
        .unwrap();

        assert_eq!(report.stats.tracks_deleted, 1);
        assert_eq!(report.stats.albums_deleted, 1);
        assert_eq!(report.stats.tracks_inserted, 0);
        assert_eq!(report.stats.tracks_updated, 0);

        let track_n: i64 = conn
            .query_row("SELECT COUNT(*) FROM tracks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(track_n, 0);
        let album_n: i64 = conn
            .query_row("SELECT COUNT(*) FROM albums", [], |r| r.get(0))
            .unwrap();
        assert_eq!(album_n, 0);

        // last_scan_report has been persisted to server_state
        let json = state_kv::get(&conn, "last_scan_report").unwrap();
        assert!(json.is_some(), "scan report should be persisted");
        let json = json.unwrap();
        assert!(json.contains("\"tracks_deleted\":1"));
        assert!(json.contains("\"albums_deleted\":1"));
    }

    fn empty_stats() -> ScanStats {
        ScanStats::default()
    }

    #[test]
    fn b1_should_bump_when_inserted() {
        let mut s = empty_stats();
        s.tracks_inserted = 1;
        assert!(should_bump_system_update_id(&s));
    }

    #[test]
    fn b2_should_bump_when_deleted() {
        let mut s = empty_stats();
        s.tracks_deleted = 1;
        assert!(should_bump_system_update_id(&s));
    }

    #[test]
    fn b3_should_bump_when_updated() {
        let mut s = empty_stats();
        s.tracks_updated = 1;
        assert!(should_bump_system_update_id(&s));
    }

    #[test]
    fn b4_should_not_bump_when_all_zero() {
        assert!(!should_bump_system_update_id(&empty_stats()));
    }

    #[test]
    fn b5_should_not_bump_when_only_unchanged_or_tag_failed() {
        let mut s = empty_stats();
        s.tracks_unchanged = 100;
        s.tag_read_failed = 5;
        s.files_enumerated = 105;
        assert!(!should_bump_system_update_id(&s));
    }

    #[test]
    fn b6_scan_with_deletion_bumps_system_update_id() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("scan.db");
        let pool = crate::db::pool(&db_path).unwrap();
        let mut conn = pool.get().unwrap();
        seed_db_with_one_track(&conn);
        // Seed system_update_id = 1 (matches main.rs initialization)
        state_kv::set(&conn, "system_update_id", "1").unwrap();

        let music = TempDir::new().unwrap();
        let extensions = vec!["flac".to_string()];
        let report = run(
            &mut conn,
            music.path(),
            &extensions,
            1,
            Arc::new(ScanProgress::new()),
        )
        .unwrap();
        assert_eq!(report.stats.tracks_deleted, 1);

        let id: u32 = state_kv::get(&conn, "system_update_id")
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(id, 2);
    }

    #[test]
    fn rl1_relativize_path_strips_library_root_prefix() {
        // Normal case: file under library_root → relative path only
        let rel = super::relativize_path(
            std::path::Path::new("/music/library/jazz/coltrane.flac"),
            std::path::Path::new("/music/library"),
        );
        assert_eq!(rel, "jazz/coltrane.flac");
    }

    #[test]
    fn rl2_relativize_path_falls_back_to_filename_when_prefix_mismatch() {
        // Abnormal case: root not a prefix → hide host fs structure, return file name only
        let rel = super::relativize_path(
            std::path::Path::new("/etc/passwd"),
            std::path::Path::new("/music/library"),
        );
        assert_eq!(rel, "passwd");
    }

    // ── SPEC §4.5: skip tag read on mtime match (`tracks_unchanged` count)
    #[test]
    fn mt1_partition_by_mtime_matches_skip_else_re_read() {
        let mut db = std::collections::HashMap::new();
        db.insert("/m/a.flac".to_string(), 100);
        db.insert("/m/b.flac".to_string(), 200);
        // /a matches db mtime → skip, /b mismatched → re-read, /c new → re-read
        let enumerated = vec![("/m/a.flac", 100i64), ("/m/b.flac", 999), ("/m/c.flac", 50)];
        let (needs, unchanged) = partition_by_mtime(&enumerated, &db);
        assert_eq!(unchanged, 1);
        assert_eq!(needs, vec![1, 2]);
    }

    #[test]
    fn mt2_partition_all_matched_skips_everything() {
        let mut db = std::collections::HashMap::new();
        db.insert("/m/x.flac".to_string(), 1);
        db.insert("/m/y.flac".to_string(), 2);
        let enumerated = vec![("/m/x.flac", 1i64), ("/m/y.flac", 2)];
        let (needs, unchanged) = partition_by_mtime(&enumerated, &db);
        assert_eq!(unchanged, 2);
        assert!(needs.is_empty());
    }

    #[test]
    fn mt3_partition_all_new_re_reads_everything() {
        let db = std::collections::HashMap::new();
        let enumerated = vec![("/m/x.flac", 1i64), ("/m/y.flac", 2)];
        let (needs, unchanged) = partition_by_mtime(&enumerated, &db);
        assert_eq!(unchanged, 0);
        assert_eq!(needs, vec![0, 1]);
    }

    #[test]
    fn pf1_write_failure_report_persists_error() {
        // ops §P1: even when scan early-returns via `?` mid-run, a failure
        // marker remains in last_scan_report so the cause can be traced
        // via /admin/scan-report. Here we call the partial-report writer
        // helper directly to verify the JSON includes the error and that
        // the existing server_state row is overwritten.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("scan.db");
        let pool = crate::db::pool(&db_path).unwrap();
        let conn = pool.get().unwrap();
        // Pre-seed a "successful scan" report
        state_kv::set(&conn, "last_scan_report", r#"{"scan_id":"old","ok":true}"#).unwrap();

        let started = SystemTime::now();
        let started_secs = to_unix_secs(started);
        write_failure_report(&conn, "scan-xyz", started, started_secs, "boom!");

        let json = state_kv::get(&conn, "last_scan_report").unwrap().unwrap();
        assert!(json.contains(r#""scan_id":"scan-xyz""#));
        assert!(json.contains(r#""error":"boom!""#));
        assert!(
            !json.contains("\"ok\":true"),
            "old report must be overwritten"
        );
    }

    #[test]
    fn pf2_successful_scan_does_not_write_error_field() {
        // Success path: error is None and is omitted from the JSON (backward compatible)
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("scan.db");
        let pool = crate::db::pool(&db_path).unwrap();
        let mut conn = pool.get().unwrap();
        let music = TempDir::new().unwrap();
        let extensions = vec!["flac".to_string()];
        let report = run(
            &mut conn,
            music.path(),
            &extensions,
            1,
            Arc::new(ScanProgress::new()),
        )
        .unwrap();
        assert!(report.error.is_none());
        let json = state_kv::get(&conn, "last_scan_report").unwrap().unwrap();
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn b7_no_op_scan_does_not_bump_system_update_id() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("scan.db");
        let pool = crate::db::pool(&db_path).unwrap();
        let mut conn = pool.get().unwrap();
        state_kv::set(&conn, "system_update_id", "7").unwrap();

        // Scan against empty library (insert 0, delete 0, update 0)
        let music = TempDir::new().unwrap();
        let extensions = vec!["flac".to_string()];
        let report = run(
            &mut conn,
            music.path(),
            &extensions,
            1,
            Arc::new(ScanProgress::new()),
        )
        .unwrap();
        assert_eq!(report.stats.tracks_inserted, 0);
        assert_eq!(report.stats.tracks_deleted, 0);
        assert_eq!(report.stats.tracks_updated, 0);

        let id: u32 = state_kv::get(&conn, "system_update_id")
            .unwrap()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(id, 7, "system_update_id must not change on no-op scan");
    }
}

//! Filesystem watch → debounced full rescan (#30, SPEC §4.4).
//!
//! inotify events (via `notify`) are filtered down to changes a scan could
//! observe, collapsed by a trailing debounce with a max-wait cap, and fed
//! into the same detached scan path as `POST /admin/rescan`. Not incremental
//! on purpose: the mtime skip (SPEC §4.5) already makes an unchanged rescan
//! cheap, so a full scan is the simplest correct reaction to any change.
//!
//! Network mounts (SMB / NFS) usually deliver no events; there the watcher is
//! simply silent and the external triggers in SPEC §4.4 remain the way to
//! rescan.

use std::path::{Component, Path};
use std::sync::Arc;
use std::time::Duration;

use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;

use crate::scan::report::ScanReport;
use crate::scan::walker::has_audio_extension;
use crate::state::AppState;

/// Timing knobs from `[scan]` (`watch_debounce_secs` / `watch_max_wait_secs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchSettings {
    /// Quiet period: a scan starts once no relevant event arrived for this long.
    pub debounce: Duration,
    /// Upper bound from the first pending event, so a never-quiet stream of
    /// events still produces a scan.
    pub max_wait: Duration,
}

impl WatchSettings {
    /// `debounce` is clamped to ≥ 1 s and `max_wait` to ≥ `debounce`.
    pub fn new(debounce_secs: u64, max_wait_secs: u64) -> Self {
        let debounce = Duration::from_secs(debounce_secs.max(1));
        let max_wait = Duration::from_secs(max_wait_secs).max(debounce);
        Self { debounce, max_wait }
    }
}

/// Start watching `state.library_root` on a background task. Never fails the
/// caller: setup errors (watch limit, unsupported FS) are logged and the task
/// exits, leaving manual / external rescans as the fallback.
pub fn spawn(
    state: AppState,
    settings: WatchSettings,
    shutdown: broadcast::Receiver<()>,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            // Capacity 1 is the coalescing mechanism: while one wake-up is
            // pending (e.g. a scan is in flight), further events are dropped
            // instead of queueing more scans.
            let (tx, rx) = mpsc::channel(1);
            let root = state.library_root.clone();
            let extensions = state.extensions.clone();
            // Registering one inotify watch per directory walks the whole
            // tree; keep it off the async workers and off the startup path.
            let started =
                tokio::task::spawn_blocking(move || start_watcher(&root, extensions, tx)).await;
            let watcher = match started {
                Ok(Ok(w)) => w,
                Ok(Err(e)) => {
                    tracing::warn!(
                        error = %e,
                        "filesystem watch unavailable (if the watch limit was hit, raise \
                         fs.inotify.max_user_watches); rescans need POST /admin/rescan"
                    );
                    return;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "filesystem watch setup task failed");
                    return;
                }
            };
            tracing::info!(
                debounce_secs = settings.debounce.as_secs(),
                max_wait_secs = settings.max_wait.as_secs(),
                "watching library for changes"
            );

            let scan_lock = state.scan_lock.clone();
            run_loop(rx, shutdown, scan_lock, settings, |permit| {
                let scan_id = ScanReport::new_id();
                tracing::info!(scan_id = %scan_id, "library change detected; starting scan");
                crate::scan::trigger::spawn_detached(
                    state.clone(),
                    permit,
                    scan_id.clone(),
                    tracing::info_span!("watch_scan", scan_id = %scan_id),
                );
            })
            .await;
            drop(watcher);
        }
        .instrument(tracing::info_span!("fs_watch")),
    )
}

/// Create the recursive watcher. `notify` adds watches for directories
/// created (or moved in) later on its own.
fn start_watcher(
    root: &Path,
    extensions: Arc<Vec<String>>,
    tx: mpsc::Sender<()>,
) -> notify::Result<RecommendedWatcher> {
    let filter_root = root.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        let relevant = match res {
            Ok(event) => is_relevant(&event, &filter_root, &extensions),
            Err(e) => {
                // An error may mean lost events; a rescan is the safe answer.
                tracing::warn!(error = %e, "filesystem watch error; scheduling a rescan");
                true
            }
        };
        if relevant {
            // `Full` means a wake-up is already pending — nothing to add.
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(watcher)
}

/// Whether a raw `notify` event can change what a scan sees.
///
/// The scan opens every changed file to read tags, and `notify`'s inotify
/// backend reports opens (`Access(Open)`) and read-only closes. Accepting
/// access events would make every scan trigger the next one, so only content
/// and namespace changes pass. Metadata-only changes (`IN_ATTRIB`) are
/// ignored too: tag edits rewrite data and emit data events.
fn is_relevant(event: &Event, root: &Path, extensions: &[String]) -> bool {
    // Kernel queue overflow: events were lost, so rescan.
    if event.need_rescan() {
        return true;
    }
    let path_matches: fn(&Path, &[String]) -> bool = match event.kind {
        EventKind::Create(CreateKind::Folder)
        | EventKind::Remove(RemoveKind::Folder | RemoveKind::Any | RemoveKind::Other) => {
            |_, _| true
        }
        EventKind::Create(_)
        | EventKind::Remove(RemoveKind::File)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Any)
        | EventKind::Access(AccessKind::Close(AccessMode::Write)) => {
            |p, exts| has_audio_extension(p, exts) || p.is_dir()
        }
        // A path that no longer exists was moved out; it can't be stat'ed to
        // tell file from folder, and a moved-out album folder must rescan.
        EventKind::Modify(ModifyKind::Name(_)) => {
            |p, exts| has_audio_extension(p, exts) || p.is_dir() || !p.exists()
        }
        _ => return false,
    };
    event
        .paths
        .iter()
        .any(|p| !is_hidden(p, root) && path_matches(p, extensions))
}

/// Mirrors the walker's pruning (SPEC §4.8): any component below `root`
/// starting with `.` is invisible to the scan. Also covers rsync's
/// `.name.XXXXXX` temp files.
fn is_hidden(path: &Path, root: &Path) -> bool {
    path.strip_prefix(root).unwrap_or(path).components().any(
        |c| matches!(c, Component::Normal(s) if s.to_str().is_some_and(|s| s.starts_with('.'))),
    )
}

/// Trailing debounce with a max-wait cap. Pure state so the timing rules are
/// testable without a runtime.
#[derive(Debug)]
struct Debouncer {
    settings: WatchSettings,
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debouncer {
    fn new(settings: WatchSettings) -> Self {
        Self {
            settings,
            first: None,
            last: None,
        }
    }

    fn on_event(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    /// When the pending batch should fire; `None` when nothing is pending.
    fn deadline(&self) -> Option<Instant> {
        let (first, last) = (self.first?, self.last?);
        Some((last + self.settings.debounce).min(first + self.settings.max_wait))
    }

    fn reset(&mut self) {
        self.first = None;
        self.last = None;
    }
}

/// Wake-up → debounce → acquire `scan_lock` (waiting if a scan is in flight)
/// → `start(permit)`. Wake-ups that arrive during the wait or the scan stay
/// in the capacity-1 channel and produce exactly one follow-up scan.
async fn run_loop<F>(
    mut rx: mpsc::Receiver<()>,
    mut shutdown: broadcast::Receiver<()>,
    scan_lock: Arc<Semaphore>,
    settings: WatchSettings,
    mut start: F,
) where
    F: FnMut(OwnedSemaphorePermit),
{
    let mut debouncer = Debouncer::new(settings);
    loop {
        let deadline = debouncer.deadline();
        tokio::select! {
            biased;
            _ = shutdown.recv() => return,
            msg = rx.recv() => match msg {
                Some(()) => debouncer.on_event(Instant::now()),
                None => return,
            },
            _ = sleep_until_opt(deadline) => {
                debouncer.reset();
                // Shutdown must win over waiting for an in-flight scan, or
                // the watcher would start a new scan mid-shutdown.
                let permit = tokio::select! {
                    biased;
                    _ = shutdown.recv() => return,
                    p = scan_lock.clone().acquire_owned() => match p {
                        Ok(p) => p,
                        Err(_) => return,
                    },
                };
                start(permit);
            }
        }
    }
}

async fn sleep_until_opt(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, Flag, MetadataKind, RenameMode};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;

    fn exts() -> Vec<String> {
        vec!["flac".to_string(), "mp3".to_string()]
    }

    fn ev(kind: EventKind, path: &Path) -> Event {
        Event::new(kind).add_path(path.to_path_buf())
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    // ── WatchSettings / Debouncer ─────────────────────────────────

    #[test]
    fn ws1_settings_clamp_debounce_and_max_wait() {
        assert_eq!(WatchSettings::new(0, 0).debounce, secs(1));
        assert_eq!(WatchSettings::new(30, 5).max_wait, secs(30));
        assert_eq!(WatchSettings::new(10, 300).max_wait, secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn db1_single_event_fires_after_debounce() {
        let mut d = Debouncer::new(WatchSettings::new(10, 300));
        assert_eq!(d.deadline(), None);
        let t0 = Instant::now();
        d.on_event(t0);
        assert_eq!(d.deadline(), Some(t0 + secs(10)));
    }

    #[tokio::test(start_paused = true)]
    async fn db2_later_event_rearms_until_max_wait() {
        let mut d = Debouncer::new(WatchSettings::new(10, 30));
        let t0 = Instant::now();
        d.on_event(t0);
        d.on_event(t0 + secs(8));
        assert_eq!(d.deadline(), Some(t0 + secs(18)));
        d.on_event(t0 + secs(25));
        assert_eq!(d.deadline(), Some(t0 + secs(30)), "capped by max_wait");
        d.reset();
        assert_eq!(d.deadline(), None);
    }

    // ── is_relevant ───────────────────────────────────────────────

    #[test]
    fn ir1_access_events_from_scan_reads_are_ignored() {
        // Regression guard for the scan → event → scan loop.
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("a.flac");
        std::fs::write(&f, b"x").unwrap();
        for kind in [
            EventKind::Access(AccessKind::Open(AccessMode::Any)),
            EventKind::Access(AccessKind::Read),
            EventKind::Access(AccessKind::Close(AccessMode::Read)),
            EventKind::Modify(ModifyKind::Metadata(MetadataKind::Any)),
            EventKind::Other,
            EventKind::Any,
        ] {
            assert!(!is_relevant(&ev(kind, &f), tmp.path(), &exts()), "{kind:?}");
        }
    }

    #[test]
    fn ir2_audio_content_changes_are_relevant() {
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("Album").join("01 Track.FLAC");
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Remove(RemoveKind::File),
        ] {
            assert!(is_relevant(&ev(kind, &f), tmp.path(), &exts()), "{kind:?}");
        }
    }

    #[test]
    fn ir3_non_audio_files_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let jpg = tmp.path().join("Album").join("Folder.jpg");
        std::fs::create_dir_all(jpg.parent().unwrap()).unwrap();
        std::fs::write(&jpg, b"x").unwrap();
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Access(AccessKind::Close(AccessMode::Write)),
            EventKind::Remove(RemoveKind::File),
            EventKind::Modify(ModifyKind::Name(RenameMode::To)),
        ] {
            assert!(
                !is_relevant(&ev(kind, &jpg), tmp.path(), &exts()),
                "{kind:?}"
            );
        }
    }

    #[test]
    fn ir4_hidden_paths_and_rsync_temp_files_are_ignored() {
        let tmp = TempDir::new().unwrap();
        let temp = tmp.path().join("Album").join(".01 Track.flac.Ab12Cd");
        let in_hidden_dir = tmp.path().join(".Trash").join("x.flac");
        for p in [&temp, &in_hidden_dir] {
            assert!(!is_relevant(
                &ev(EventKind::Create(CreateKind::File), p),
                tmp.path(),
                &exts()
            ));
        }
    }

    #[test]
    fn ir5_hidden_check_is_relative_to_root() {
        // A library under a dot-directory must still be watched.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join(".music");
        let f = root.join("a.flac");
        assert!(is_relevant(
            &ev(EventKind::Create(CreateKind::File), &f),
            &root,
            &exts()
        ));
    }

    #[test]
    fn ir6_folder_create_remove_and_moves_are_relevant() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("New Album (2024)");
        std::fs::create_dir(&dir).unwrap();
        let gone = tmp.path().join("Moved Away vol. 2");
        for e in [
            ev(EventKind::Create(CreateKind::Folder), &dir),
            ev(EventKind::Remove(RemoveKind::Folder), &gone),
            // Folder moved in: no extension to go on, but it is a directory.
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::To)), &dir),
            // Folder moved out: can't be stat'ed any more.
            ev(EventKind::Modify(ModifyKind::Name(RenameMode::From)), &gone),
            // Generic create of a directory (dotted name, no audio extension).
            ev(EventKind::Create(CreateKind::Any), &dir),
        ] {
            assert!(is_relevant(&e, tmp.path(), &exts()), "{e:?}");
        }
    }

    #[test]
    fn ir7_temp_rename_to_audio_is_relevant() {
        let tmp = TempDir::new().unwrap();
        let from = tmp.path().join("x.part");
        let to = tmp.path().join("x.flac");
        let e = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(from)
            .add_path(to);
        assert!(is_relevant(&e, tmp.path(), &exts()));
    }

    #[test]
    fn ir8_queue_overflow_forces_rescan() {
        let e = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert!(is_relevant(&e, Path::new("/music"), &exts()));
    }

    // ── run_loop (paused clock) ───────────────────────────────────

    struct Harness {
        tx: mpsc::Sender<()>,
        shutdown: broadcast::Sender<()>,
        lock: Arc<Semaphore>,
        starts: Arc<AtomicUsize>,
        task: JoinHandle<()>,
    }

    fn harness(settings: WatchSettings) -> Harness {
        let (tx, rx) = mpsc::channel(1);
        let (shutdown, sd_rx) = broadcast::channel(1);
        let lock = Arc::new(Semaphore::new(1));
        let starts = Arc::new(AtomicUsize::new(0));
        let counter = starts.clone();
        let task = tokio::spawn(run_loop(
            rx,
            sd_rx,
            lock.clone(),
            settings,
            move |_permit| {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        ));
        Harness {
            tx,
            shutdown,
            lock,
            starts,
            task,
        }
    }

    impl Harness {
        fn starts(&self) -> usize {
            self.starts.load(Ordering::SeqCst)
        }
    }

    #[tokio::test(start_paused = true)]
    async fn rl1_burst_collapses_into_one_scan_after_quiet_period() {
        let h = harness(WatchSettings::new(10, 300));
        // Events at t = 0, 2, 4, 6, 8.
        for i in 0..5 {
            if i > 0 {
                tokio::time::sleep(secs(2)).await;
            }
            h.tx.send(()).await.unwrap();
        }
        tokio::time::sleep(secs(9)).await; // t = 17 < 8 + 10
        assert_eq!(h.starts(), 0);
        tokio::time::sleep(secs(2)).await; // t = 19
        assert_eq!(h.starts(), 1);
        tokio::time::sleep(secs(600)).await;
        assert_eq!(h.starts(), 1, "no further scans without new events");
    }

    #[tokio::test(start_paused = true)]
    async fn rl2_never_quiet_stream_fires_at_max_wait() {
        let h = harness(WatchSettings::new(10, 31));
        // Events every 3 s (t = 0 … 27) never leave a 10 s quiet gap.
        for _ in 0..10 {
            let _ = h.tx.try_send(());
            tokio::time::sleep(secs(3)).await;
        }
        assert_eq!(h.starts(), 0); // t = 30
        tokio::time::sleep(secs(2)).await; // t = 32 > first event + 31
        assert_eq!(h.starts(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rl3_events_during_in_flight_scan_produce_exactly_one_follow_up() {
        let h = harness(WatchSettings::new(10, 300));
        let held = h.lock.clone().try_acquire_owned().unwrap(); // a scan is running
        h.tx.send(()).await.unwrap();
        tokio::time::sleep(secs(11)).await;
        assert_eq!(h.starts(), 0, "waits for the in-flight scan");
        for _ in 0..5 {
            let _ = h.tx.try_send(());
            tokio::time::sleep(secs(1)).await;
        }
        drop(held);
        tokio::time::sleep(secs(1)).await;
        assert_eq!(h.starts(), 1);
        tokio::time::sleep(secs(600)).await;
        assert_eq!(h.starts(), 2, "pending events coalesce into one follow-up");
    }

    #[tokio::test(start_paused = true)]
    async fn rl4_shutdown_while_waiting_for_lock_starts_nothing() {
        let h = harness(WatchSettings::new(10, 300));
        let _held = h.lock.clone().try_acquire_owned().unwrap();
        h.tx.send(()).await.unwrap();
        tokio::time::sleep(secs(11)).await;
        h.shutdown.send(()).unwrap();
        tokio::time::timeout(secs(1), h.task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(h.starts.load(Ordering::SeqCst), 0);
    }

    // ── real inotify ──────────────────────────────────────────────

    async fn expect_wake(rx: &mut mpsc::Receiver<()>, what: &str) {
        tokio::time::timeout(secs(5), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("expected a wake-up for {what}"));
    }

    async fn expect_quiet(rx: &mut mpsc::Receiver<()>, what: &str) {
        assert!(
            tokio::time::timeout(Duration::from_millis(500), rx.recv())
                .await
                .is_err(),
            "unexpected wake-up for {what}"
        );
    }

    #[tokio::test]
    async fn iw1_real_watcher_sees_new_files_and_new_subdirectories() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let (tx, mut rx) = mpsc::channel(1);
        let _w = start_watcher(&root, Arc::new(exts()), tx).unwrap();

        std::fs::write(root.join("a.flac"), b"x").unwrap();
        expect_wake(&mut rx, "new file").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        while rx.try_recv().is_ok() {}

        // A directory created after startup gets its own watch.
        let album = root.join("New Album");
        std::fs::create_dir(&album).unwrap();
        expect_wake(&mut rx, "new folder").await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        while rx.try_recv().is_ok() {}
        std::fs::write(album.join("01.flac"), b"x").unwrap();
        expect_wake(&mut rx, "file in new folder").await;
    }

    #[tokio::test]
    async fn iw2_scan_does_not_retrigger_the_watcher() {
        let tmp = TempDir::new().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let album = root.join("Album");
        std::fs::create_dir(&album).unwrap();
        std::fs::write(album.join("01.flac"), b"not really flac").unwrap();
        std::fs::write(album.join("Folder.jpg"), b"x").unwrap();

        let (tx, mut rx) = mpsc::channel(1);
        let _w = start_watcher(&root, Arc::new(exts()), tx).unwrap();
        // macOS FSEvents can deliver the setup writes above after the stream
        // starts; let them settle and drain so only the scan is under test.
        tokio::time::sleep(Duration::from_secs(1)).await;
        while rx.try_recv().is_ok() {}

        let dbdir = TempDir::new().unwrap();
        let pool = crate::db::pool(&dbdir.path().join("t.db")).unwrap();
        let scan_root = root.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = pool.get().unwrap();
            crate::scan::run(
                &mut conn,
                &scan_root,
                &exts(),
                1,
                Arc::new(crate::scan::progress::ScanProgress::new()),
                ScanReport::new_id(),
            )
            .unwrap();
        })
        .await
        .unwrap();
        expect_quiet(&mut rx, "a scan that only reads the library").await;
    }
}

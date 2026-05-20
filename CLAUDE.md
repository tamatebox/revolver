# CLAUDE.md

## Operating principles

- Before writing code, consult the relevant section of [SPEC.md](SPEC.md) (the WHAT) and [ARCHITECTURE.md](ARCHITECTURE.md) (the HOW).
- Run `cargo test` and `cargo clippy --all-targets` before declaring a change done.

## About

**revolver** is a Rust lightweight UPnP/DLNA MediaServer. Single binary, SQLite backend, LAN-only. Verified on Linn DSM/2 (real device).

## Commands

```sh
cargo build --release      # LTO + strip
cargo test
cargo clippy --all-targets
cargo fmt
```

Admin UI: `http://localhost:8200/admin/ui` (scan / reshuffle / stats / settings / scan progress).

One-time per clone, enable the in-repo git hooks so `cargo fmt --check` + `cargo clippy -- -D warnings` run before every commit (same checks as CI):

```sh
git config core.hooksPath .githooks
```

## Key design decisions

Things you cannot grasp by reading one file:

- **Two-table model: `albums` + `tracks`** (SPEC §3). `album_id` is the primary key for UPnP ObjectIDs and all queries. Album identity = `(effective_album_artist, album, compilation)` — `album_id` stays stable across tag fixes as long as identity doesn't change.
- **`effective_album_artist` is computed at scan time** and stored: `compilation` → "Various Artists"; else `album_artist` → `artist` → "Unknown Artist". Do not recompute in queries.
- **`albums.quality` is bulk-UPDATEd after scan** (SPEC §4.6). Tiers: `flac/alac/pcm` + (>48kHz or >16bit) → `hires`; else `lossless` / `lossy` / `mixed` / `unknown`. Browse hits it via index.
- **ObjectID stability is the cross-client compatibility key** (SPEC §6.1, §10.4). `alb:{album_id}` / `trk:{track_id}` are auto-increment based and permanent. `aa:{base64}` / `ar:` / `gn:` are URL-safe base64 of the name (`-` `_`, no padding). Never break IDs on rescan.
- **`cat:recent` is a flat album list** (SPEC §6.7, #16). Sorted by `albums.last_added_at` DESC, capped by `browse.recently_added_limit` + optional `browse.recently_added_max_age_days`. The earlier time-range cascade (`day` / `week` / `year:YYYY` / `all`) was dropped after real-device use showed the two-click hop was friction without value. Future Views default to flat — avoid sub-container cascades.
- **SystemUpdateID increments only on structural change** (SPEC §5.1). `scan::should_bump_system_update_id` decides; bump on scan complete + fan-out propchange NOTIFY to CD subscribers. Playback and random reshuffle do NOT bump (avoid trashing Linn's Browse cache). Currently also bumps on `tracks_updated > 0` — slightly over-bumpy but harmless.
- **`added_at` = "when the server first saw this path"** (SPEC §4.2), not file btime. First scan: btime/mtime fallback. After: now(). Never overwrite on upsert. Recently Added orders by `MAX(added_at) by album_id`, so a new track on an existing album re-floats the album.
- **Skip tag read when mtime matches** (SPEC §4.5). Reported as `tracks_unchanged`. Makes "unchanged rescan" finish in seconds.
- **M4A: detect ALAC vs AAC via `lofty`** and store in `codec` (SPEC §14). Both are `audio/mp4` in protocolInfo, but `bitsPerSample` is emitted only for lossless.
- **Cross-client strategy: UPnP AV 1.0 standard only** (SPEC §10). No Linn `X_MAP_*` or Sonos extensions. Recently Added / Played / Random / Quality use virtual containers (SortCriteria is unused by Linn/Kazoo UI — SPEC §6.7-6.8).
- **Random Albums is an in-memory `Mutex<Vec<i64>>`** (SPEC §6.6). Reshuffled at startup / scan complete / `POST /admin/reshuffle`. Not persisted — a fresh order each launch is fine.
- **Recently Played counts stream hits** (SPEC §6.8). Only `Range` absent or `bytes=0-N` increments `play_count` + `last_played_at`. `bytes=N-` (N>0) and suffix `bytes=-N` are excluded (reject pre-fetch). Same rule for all clients — no Linn-specific heuristics.
- **Album art: on-demand extract + bytes-budget cache** (SPEC §8.3). Full clear past 100MB (no LRU — keep it simple).
- **Scan runs inside `tokio::task::spawn_blocking`** with rayon inside. Never blocks a tokio worker.
- **DB uses r2d2 connection pool** (rusqlite + spawn_blocking). SQLite WAL mode allows Browse during scan.
- **Schema migration: idempotent ALTER via `ensure_column`**. After `CREATE TABLE IF NOT EXISTS`, check columns via `PRAGMA table_info`. `CREATE INDEX IF NOT EXISTS idx_trk_played` must run AFTER `ensure_column` — the index fails if the target column doesn't exist on older DBs.
- **`BrowseContext` centralizes view-wide deps** (`conn` / `art_base_url` / `stream_base_url` / `random_state` / `now_secs`). The caller (`content_directory.rs`) injects the clock so tests are time-independent.
- **Shutdown broadcasts via `tokio::sync::broadcast::channel::<()>(1)`** — deliberately avoiding `CancellationToken` for a single signal.
- **SOAP fault = HTTP 500 + `<UPnPError>` body** (UPnP convention). Do not leak internal detail to clients; log via `tracing::error` only.
- **`config.toml` is bootstrap-only; `config_overrides` table is the runtime source of truth** (#13). At startup, the toml values are captured as defaults; user edits go to `config_overrides` (JSON-valued KV), and `AppState.browse` is rebuilt from `catalog::build_browse_settings`. `GET/POST/DELETE /admin/config` drives this. The key catalog lives in `src/config_catalog.rs` — add new keys there with a `ReloadTier` (`Runtime` / `Reload` / `Restart`) and a validator.
- **Search dispatches by `upnp:class derivedfrom`** (#5+#10, SPEC §5.4). The parser produces `ClassFilter + Predicate` tree (AND / OR / parens / `[@role="..."]`). Album-class returns `alb:{id}` containers, Artist-class returns `aa:{base64}` containers, Track-class returns track items via 4-field OR. `role` is captured but ignored at SQL until #9 lands COMPOSER.
- **`ScanProgress` is a lock-free atomic snapshot** (#12). `scan::run` updates `phase` / `current` / `total` as it moves through walk → tag_read → upsert → postprocess. Tag-read phase spawns a 5-s ticker thread that polls every 500ms (short shutdown latency). `/admin/scan-progress` reads via `Atomic*::load`, no locks.
- **`POST /admin/rescan` is async** (#18). The handler acquires the `scan_lock` permit and returns **202 Accepted** + `{ scan_id, started_at }` immediately; the scan plus post-scan side effects (SystemUpdateID broadcast + random reshuffle) run on a detached `tokio::spawn` whose closure holds the permit until completion. Callers poll `/admin/scan-progress` for progress and read `/admin/scan-report` for the final report. 409 Conflict if a scan is already in flight. Never re-introduce blocking behavior — long scans must not hang HTTP clients (especially rsync post-hooks).

## Active pitfalls

- **Gapless playback breaks if Range handling is off** in `/stream/{track_id}`. Both suffix `bytes=-N` and `bytes=N-` must work. Play-count attribution counts only `Range` absent or `start=0`.
- **Compilation albums: trust the compilation flag** (M4A `cpil` / MP3 `TCMP` / Vorbis `COMPILATION`) over Album Artist text. If set, force "Various Artists".
- **Emit `sampleFrequency` / `bitsPerSample` / `nrAudioChannels` correctly** — wrong values are why Linn drops 24/192 streams.
- **NAS over SMB/NFS has no btime** — fall back to mtime.
- **File move = path change**: DELETE → INSERT, so `added_at` becomes "now". Intentional (SPEC §4.2).
- **`MAX(added_at) by album_id` for Recently Added is intentional** — a per-album `first_seen_at` would not re-float on new tracks (SPEC §6.4).

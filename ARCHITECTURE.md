# Architecture

Implementation guide. This document covers the **how** — module layout,
dependency direction, data flow, and concurrency model. The **what** —
data model, protocol, design decisions — lives in [SPEC.md](SPEC.md).

---

## 1. Module Layout

```
src/
├── main.rs               Entry point: CLI args + config + tokio runtime + task spawn
├── lib.rs                Library entry (so integration tests can call `revolver::*`)
├── config.rs             `config.toml` schema and deserialization
├── error.rs              Unified `thiserror`-based error type
├── state.rs              AppState (Arc<...>): db pool / scan_lock / UUID / friendly_name /
│                            local_ip / subscriptions / notify_tasks / notify_client /
│                            art_cache / random_state / scan_progress / started_at /
│                            ssdp_listener_active / ssdp_advertiser_active /
│                            browse: RwLock<BrowseSettings> (#13) / config_defaults
├── random.rs             `Mutex<Vec<i64>>`-backed Random Albums state (SPEC §6.6)
├── normalize.rs          NFKD + combining-marks strip + lowercase + katakana→hiragana
│                            (#6). One function (`for_search`) used by both the
│                            shadow-column populator (upsert / migrate) and the
│                            Search query side.
├── config_catalog.rs     User-editable config key registry (#13). Each entry
│                            has a default-from-toml, validator, and ReloadTier
│                            (Runtime / Reload / Restart).
│
├── db/                   # ─── Persistence layer ───────────────────────
│   ├── mod.rs            r2d2 connection pool, PRAGMAs, migration entry
│   ├── schema.rs         `CREATE TABLE` / `CREATE INDEX` + idempotent
│   │                        `ensure_column` migrations
│   ├── albums.rs         albums: upsert / delete_orphans / recalc_counts /
│   │                        recalc_quality / recalc_last_added_at /
│   │                        recalc_last_played_at / bump_album_last_played_at /
│   │                        get_representative_track_path. `upsert` populates
│   │                        `album_norm` / `effective_album_artist_norm` (#6).
│   ├── tracks.rs         tracks: upsert / detect_deleted / get_mtimes /
│   │                        lookup_by_id. `upsert` populates the six `*_norm`
│   │                        shadow columns (#6), `year` (#2), and the four
│   │                        ReplayGain values (#11) alongside the raw fields.
│   ├── config_overrides.rs `config_overrides` KV (#13): get / set / delete +
│   │                        list_all for the admin config endpoints.
│   └── state_kv.rs       server_state key-value (uuid, system_update_id, last_scan_report)
│
├── scan/                 # ─── Library scan ────────────────────────────
│   ├── mod.rs            Scan orchestrator (SPEC §4.1 step 1-12, including
│   │                        quality recalc + `albums.last_added_at` /
│   │                        `last_played_at` denormalization recalcs).
│   ├── walker.rs         walkdir-based enumeration with extension / hidden-file filtering
│   │                        (SPEC §4.8)
│   ├── tagger.rs         lofty-based tag + codec + audio-properties reader.
│   │                        Reads composer / conductor / performer (#9), release
│   │                        year (#2, `parse_year`), and ReplayGain track / album
│   │                        gain & peak (#11, `parse_rg` handles "-7.34 dB" /
│   │                        "0.987654").
│   ├── matcher.rs        Computes `effective_album_artist` and `added_at`
│   │                        (SPEC §3.2, §4.2)
│   ├── progress.rs       Lock-free `ScanProgress` snapshot (#12). Powers
│   │                        `/admin/scan-progress`.
│   └── report.rs         `ScanReport` struct and JSON serialization (SPEC §4.7)
│
├── art/                  # ─── Album art extraction + cache (SPEC §8.3) ─
│   ├── mod.rs
│   ├── extract.rs        Embedded image (lofty) + folder image
│   │                        (case-insensitive priority order)
│   └── cache.rs          Bytes-budget memory cache (clear-all over 100MB,
│                            `Arc<Vec<u8>>` for zero-copy sharing)
│
├── upnp/                 # ─── UPnP protocol layer ─────────────────────
│   ├── mod.rs
│   ├── device.rs             Builds `/description.xml` (with `<iconList>`)
│   ├── scpd.rs               `/scpd/cd.xml`, `/scpd/cm.xml`
│   │                            (separate files, embedded via `include_str!`)
│   ├── icon.rs               `assets/icon-{48,120}.png` embedded via `include_bytes!`
│   ├── soap.rs               SOAP envelope parse (quick-xml) / encode + `SoapFault`
│   ├── content_directory.rs  Browse / Search / GetSystemUpdateID /
│   │                            GetSearchCapabilities / GetSortCapabilities.
│   │                            Builds a `BrowseContext` and dispatches to `browse::*`.
│   ├── connection_manager.rs GetProtocolInfo / GetCurrentConnectionIDs /
│   │                            GetCurrentConnectionInfo (SPEC §5.5)
│   ├── didl.rs               DIDL-Lite Container / Item XML generation (SPEC §7)
│   ├── object_id.rs          ObjectID parse / encode (URL-safe base64, no padding).
│   │                            Variants: Root / Cat{Aa,Ar,Al,Gn,Recent,Played,Random,
│   │                            Hires,Lossy,Mixed,Cm,Cn,Pf,Yr,Dec} +
│   │                            AlbumArtist / Artist / Genre / Composer / Conductor /
│   │                            Performer / Year(i32) / Decade(i32) / Album / Track /
│   │                            Disc{album_id,disc}.
│   │                            (The pre-#16 `RecentRange` enum was dropped when
│   │                            `cat:recent` was flattened to a single album list.)
│   ├── search.rs             SearchCriteria parser (SPEC §5.4).
│   │                            `read_string` slices the `&str` source to preserve
│   │                            multibyte UTF-8 in query values (#6).
│   ├── gena.rs               GENA subscriptions store + notify-tasks tracker
│   └── usn.rs                The five SSDP USN / NT variants (SPEC §9.3)
│
├── browse/               # ─── Browse view → SQL mapping (SPEC §6.4) ───
│   ├── mod.rs            BrowseContext + browse_metadata / browse_children dispatch
│   ├── categories.rs     Root (selection + order from `browse.top_level`, #8) +
│   │                        cat:aa/ar/al/gn + cat:cm/cn/pf (#9) + cat:yr/dec (#2)
│   │                        facets. Container builders (plain / person / genre /
│   │                        year). Classical and year facets self-hide via
│   │                        `facet_has_any` when the underlying column is empty.
│   ├── albums.rs         `alb:id` metadata + album list under each aa/ar/gn/cm/cn/pf
│   │                        facet (`WHERE EXISTS` semi-join) + `yr:Y` / `dec:D`
│   │                        filters (#2, year EXISTS / BETWEEN).
│   ├── tracks.rs         `trk:id` metadata + track list under `alb:id` +
│   │                        DIDL Item builder
│   ├── recent.rs         `cat:recent` — flat album list ordered by
│   │                        `albums.last_added_at DESC`, capped by
│   │                        `recently_added_limit` + optional
│   │                        `recently_added_max_age_days` (SPEC §6.7, #16).
│   │                        (The pre-#16 sub-container cascade is gone.)
│   ├── random.rs         `cat:random` — fetches albums from `random_state.page()`
│   ├── quality.rs        `cat:hires` / `cat:lossy` / `cat:mixed` — filtered by `albums.quality`
│   ├── played.rs         `cat:played` — `MAX(last_played_at) DESC`, never-played excluded
│   │                        (SPEC §6.8)
│   └── search.rs         DB query for ContentDirectory `Search`. Uses `*_norm`
│                            shadow columns + normalized search input (#6, SPEC §5.4).
│
├── ssdp.rs               SSDP discovery (SPEC §9.1-9.3).
│                            Listener and advertiser tasks are defined in one file.
│
└── http/                 # ─── HTTP / axum router (SPEC §8) ────────────
    ├── mod.rs            Router construction, endpoint registration, `HttpError`,
    │                        `ConcurrencyLimitLayer` (256 concurrent connections)
    ├── upnp.rs           `GET /description.xml`, `/scpd/cd.xml`, `/scpd/cm.xml`,
    │                        `/icon/48.png`, `/icon/120.png`
    ├── soap_ctrl.rs      `POST /control/cd`, `/control/cm`
    ├── stream.rs         `GET /stream/{track_id}` + Range (SPEC §8.2) +
    │                        play-stats counter (Range absent or `start=0` only +1,
    │                        SPEC §6.8)
    ├── art.rs            `GET /art/{album_id}` + cache (SPEC §8.3)
    ├── gena.rs           `SUBSCRIBE` / `UNSUBSCRIBE` on `/event/cd`, `/event/cm`
    │                        (SPEC §9.4-9.5)
    ├── admin.rs          `/admin/scan-report`, `rescan` (#18, async 202),
    │                        `reshuffle`, `stats` (incl. `tracks_with_replaygain`,
    │                        #11), `scan-progress` (#12), `ui` (SPEC §8.4-8.5).
    ├── admin_config.rs   `/admin/config` (#13): GET / POST / DELETE driving the
    │                        `config_overrides` table + the `config_catalog`
    │                        validator pipeline.
    └── admin_ui.html     Single-page web admin UI
                             (embedded into the binary via `include_str!`)
```

### Dependency Direction

```
                   ┌─────────────┐
                   │   main.rs   │
                   └──────┬──────┘
                          │ owns
                          ▼
                   ┌─────────────┐
              ┌───▶│  AppState   │◀───┐
              │    └─────────────┘    │
              │                       │
        ┌─────┴────────┐         ┌────┴─────┐
        │ http / ssdp  │         │   scan   │
        │  upnp / gena │         │   (W)    │
        └──────┬───────┘         └────┬─────┘
               │                      │
               ▼                      ▼
        ┌──────────────┐       ┌──────────────┐
        │ upnp / browse│       │   art        │
        │ (pure logic) │       │   random     │
        └──────┬───────┘       └──────┬───────┘
               │                      │
               ▼                      ▼
            ┌──────────────────────┐
            │         db/          │
            └──────────────────────┘
                       │
                       ▼
                  rusqlite + fs
```

- **`db/` depends only on `error` and external crates.** Schema and SQL are isolated
  here.
- **`upnp/` and `browse/` stay pure logic.** All I/O goes through `db/`, which makes
  them straightforward to test.
- **`scan/` is the only write-heavy path.** Concentrating writes here minimizes
  writer-lock contention under SQLite WAL mode.
- **`AppState` is shared across all layers via `Arc<...>`.** Current fields:
  `db_pool`, `library_root`, `extensions`, `scan_parallel`,
  `scan_lock: Arc<Semaphore>`, `browse: Arc<RwLock<BrowseSettings>>` (#13,
  hot-swap from `/admin/config`), `config_defaults: Arc<DefaultsMap>` (toml
  defaults snapshot, immutable after startup), `uuid`, `friendly_name`,
  `http_port`, `local_ip`, `subscriptions: Arc<Subscriptions>`,
  `notify_tasks: Arc<NotifyTasks>`, `notify_client: reqwest::Client`,
  `art_cache: Arc<ArtCache>`, `random_state: Arc<RandomState>`,
  `scan_progress: Arc<ScanProgress>` (#12),
  `ssdp_{listener,advertiser}_active: Arc<AtomicBool>` (observability,
  ops §P1), `started_at: i64`.
- **`BrowseContext` collects cross-view dependencies** (db connection, URL bases,
  random state, `now_secs`, and a snapshot of `BrowseSettings`). It is built in
  `content_directory.rs` and passed into each browse view, which lets tests
  inject a fixed `now_secs` and pinned settings (e.g. a custom `top_level`).
- **No circular dependencies.**

---

## 2. Data Flow

### Flow A — Library Scan (startup or `POST /admin/rescan`)

```
       fs (music root)
              │
              ▼
       scan/walker  ──filter (ext/hidden)──▶  Vec<PathBuf>
              │
              │   rayon parallel (the entire scan task runs inside
              │                       `tokio::task::spawn_blocking`,
              │                       and the rayon scope runs inside that)
              ▼
       scan/tagger (lofty)
              │   → (tags, codec, audio_props) per path
              ▼
       scan/matcher
              │   → compute effective_album_artist
              │   → decide whether this is the initial scan (tracks table empty)
              │   → decide added_at (initial: min(btime, mtime); subsequent: now())
              ▼
       ┌──────────────────────────────────────────┐
       │  SPEC §4.1 step 5-12, in order           │
       └──────────────────────────────────────────┘
              │
              ▼
       db/albums.upsert ────▶ album_id
              │
              ▼
       db/tracks.upsert (UNIQUE on path; collisions preserve added_at)
              │   (batch commit every 1000 rows, SPEC §4.1)
              ▼
       db/tracks.detect_deleted    DELETE rows whose path was not enumerated
              │
              ▼
       db/albums.delete_orphans    DELETE albums that no longer have any tracks
              │
              ▼
       db/albums.recalc_counts     track_count / total_duration_ms
              │
              ▼
       db/albums.recalc_quality    bulk UPDATE from tracks' codec / sample-rate /
              │                       bit-depth (SPEC §4.6)
              ▼
       db/albums.recalc_last_added_at   denormalize MAX(tracks.added_at) onto
       db/albums.recalc_last_played_at  the album row (cat:recent / cat:played
              │                          read these directly, no GROUP BY hot path)
              ▼
       state.system_update_id += 1  (only if there was a structural change)
              │
              ▼
       upnp/gena.broadcast_propchange(SystemUpdateID = new_value)
              │
              ▼
       random.reshuffle(conn)      Re-shuffle Random Albums after scan
              │                       (SPEC §6.6)
              ▼
       db/state_kv.save_scan_report (JSON, keeps the most recent entry only)
```

Triggered from `POST /admin/rescan`, the request **returns 202 Accepted
immediately** with `{ scan_id, started_at }` while the scan + post-scan side
effects run on a detached `tokio::spawn` (#18). The detached closure holds
the `scan_lock` permit until completion. Callers poll
`/admin/scan-progress` (#12, lock-free `Atomic*::load`) and read the final
report from `/admin/scan-report`. 409 Conflict is returned immediately if a
scan is already in flight.

Notes:

- **Rayon parallelism is bounded by `config.scan.parallel`.** Tag reading
  (CPU-bound) runs in parallel; DB writes funnel through a single writer.
- **Unchanged files are skipped at the walker stage via mtime comparison**, so
  tag reading is bypassed entirely (SPEC §4.5). The skip count surfaces as
  `tracks_unchanged` in the scan report.
- **`system_update_id` is incremented only on structural changes.** Play-count
  bumps and reshuffles do not trigger an increment.

### Flow B — Browse Request

```
   Control point (Linn App, etc.)
        │ POST /control/cd
        │ SOAPAction: ContentDirectory#Browse
        ▼
   http/soap_ctrl
        │ receive body → spawn_blocking
        ▼
   upnp/soap.parse_envelope  ──▶  SoapRequest { action, args }
        │
        ▼
   upnp/content_directory.handle
        │  Build BrowseContext (now_secs / random_state / URL bases /
        │  BrowseSettings snapshot — top_level, recently_added_*, etc.)
        │
        ├─▶ upnp/object_id.parse(ObjectID)  ──▶  enum ObjectId
        │     - Root / CatAa / CatAr / CatAl / CatGn
        │     - CatRecent / CatPlayed / CatRandom / CatHires / CatLossy / CatMixed
        │     - CatCm / CatCn / CatPf (#9) / CatYr / CatDec (#2)
        │     - AlbumArtist / Artist / Genre / Composer / Conductor / Performer
        │     - Year(i32) / Decade(i32) (#2) / Album(id) / Track(id) /
        │       Disc { album_id, disc } (#17)
        │
        ▼
   browse::browse_metadata / browse_children
        │  → categories (root + cat:*) / albums (alb:* + per-facet listings) /
        │    tracks (trk:* + alb:* children) / recent / random / quality / played /
        │    search
        │
        ├─▶ DB SELECT + COUNT (SPEC §6.4). Search predicates run against
        │     `*_norm` shadow columns (#6).
        │
        ▼
   result → DidlOutput { containers, items } + total_matches
        │
        ▼
   upnp/didl.build_didl
        │  (sets `<upnp:albumArtURI>` to `/art/{album_id}`)
        ▼
   upnp/soap.build_response_body
        │
        ▼
   HTTP 200 + body  ──▶  control point
```

Notes:

- **`BrowseMetadata` and `BrowseDirectChildren` have separate dispatch paths.**
- **The `UpdateID` response field carries the current `system_update_id`**
  (SPEC §6.5).
- **`RequestedCount = 0` means "all"**, clamped to a hard cap of 1000.
- **`SortCriteria` is ignored.** Control-point UIs (Linn App, Kazoo) do not send
  it, so ordering is dictated by virtual-container hierarchy instead
  (SPEC §6.7).

### Flow C — Audio Stream (Range Request)

```
   Control point
     │ GET /stream/{track_id}
     │ Range: bytes=N-M  (or bytes=N-, bytes=-N, or absent)
     ▼
   http/stream
     │
     ├─▶ db/tracks.lookup_by_id(track_id)
     │      └─▶ path, file_size, mime_type
     │
     ├─▶ path_within_library check (canonicalize and verify under library_root)
     │
     ├─▶ Parse Range header
     │
     ├─▶ Play-stats counter (SPEC §6.8):
     │      When Range is absent OR start=0:
     │        UPDATE tracks SET play_count = play_count + 1,
     │                          last_played_at = now
     │        (logs warn on failure but does not interrupt the stream)
     │
     ▼
   Branch on parsed Range:
     ├── absent   ──▶  open + stream::full       ──▶  200, Content-Length, Accept-Ranges
     ├── N-M      ──▶  open + seek(N) + take(L)  ──▶  206, Content-Range: bytes N-M/TOTAL
     ├── N-       ──▶  open + seek(N)            ──▶  206, Content-Range: bytes N-(TOTAL-1)/TOTAL
     ├── -N       ──▶  open + seek(TOTAL-N)      ──▶  206, Content-Range: bytes (TOTAL-N)-(TOTAL-1)/TOTAL
     └── invalid  ──▶                            ──▶  416, Content-Range: bytes */TOTAL
                                                       (Content-Type / Accept-Ranges are returned for 200/206/416)
```

Notes:

- **Both suffix Range (`-N`) and open-ended Range (`N-`) must work** for gapless
  playback (SPEC §8.2, §14).
- **Play counts are recorded only on Range-absent or `start=0` requests**
  (SPEC §6.8). This single rule applies to every client.
- **`tokio::io::AsyncSeekExt` + `AsyncReadExt::take` produce a chunk stream**
  that is handed to `axum::body::Body::from_stream`.

### Flow D — Album Art

```
   Control point
     │ GET /art/{album_id}?v=...
     ▼
   http/art
     │
     ├─▶ state.art_cache.get(album_id)               ── cache hit → respond immediately
     │     └─▶ Some(CachedArt) → response with mime + bytes + Cache-Control
     │
     │  On miss:
     │
     ├─▶ spawn_blocking(fetch representative track + extract):
     │      ├─▶ db/albums.get_representative_track_path
     │      │     (selected by disc_num → track_num → path, LIMIT 1)
     │      │
     │      ├─▶ art/extract.extract_embedded(lofty)
     │      │     (PictureType: CoverFront → Other → first, JPEG/PNG only)
     │      │
     │      └─▶ art/extract.extract_folder
     │            (cover.* → folder.* → front.* → others, case-insensitive)
     │
     ├─▶ Some(CachedArt) → state.art_cache.put(...)
     │     (clear-all when total exceeds 100MB)
     │
     ▼
   200 + image/{jpeg|png} + Cache-Control: public, max-age=86400
   or 404
```

### Flow E — Discovery → Description → Subscription

```
   Control point
     │ M-SEARCH * HTTP/1.1   (multicast UDP 239.255.255.250:1900)
     ▼
   ssdp::listener
     │  ─▶ unicast UDP response (with USN / LOCATION)
     │
     ▼ (control point fetches the Location URL)
     │
   ┌──────────────────────────────────┐
   │ GET /description.xml             │ ──▶  http → upnp/device
   │ GET /scpd/cd.xml                 │ ──▶  http → upnp/scpd
   │ GET /scpd/cm.xml                 │ ──▶  http → upnp/scpd
   └──────────────────────────────────┘
     │
     ▼
   SUBSCRIBE /event/cd
            HOST / CALLBACK / NT / TIMEOUT
     ▼
   http/gena ──▶ upnp/gena.subscriptions.add
                       │   Subscription { sid, callback_url, expires_at, seq: 0 }
                       │   (CALLBACK accepted only for private/loopback IPs,
                       │    SSRF defense)
                       ▼
              initial NOTIFY (current SystemUpdateID) ──▶ control point
```

In parallel, `ssdp::advertiser` multicasts `ssdp:alive` on startup, again every
900 seconds, and `ssdp:byebye` on shutdown.

### Flow F — State Change → GENA NOTIFY

```
   End of Flow A, or any other event that bumps system_update_id
        │
        ▼
   upnp/gena.broadcast_propchange
        │   (one spawned HTTP NOTIFY per CD subscriber, in parallel)
        │   (in-flight tasks are tracked in AppState.notify_tasks)
        │
        ▼
   per subscriber:
        │
        ├─▶ HTTP NOTIFY to callback_url
        │     ├─ success ──▶ subscription.seq += 1
        │     └─ failure ──▶ one retry, then give up (logs warn)
        │
        ▼
   subscriptions.sweep_expired()
        │   timer task at 60s interval that drops expired subscriptions
```

### Flow G — Recently Added / Played

```
   Browse cat:recent  (flat album list since #16 — SPEC §6.7)
        │  browse::recent::recent_root_children(ctx, start, count)
        │
        ├─▶ Optional age cap: ctx.settings.recently_added_max_age_days
        │     adds `WHERE last_added_at >= now - days*86400`
        ├─▶ Hard item cap: ctx.settings.recently_added_limit
        │     (also caps SOAP RequestedCount even when the client asks for more)
        │
        ▼
   SELECT albums ORDER BY last_added_at DESC, id DESC LIMIT/OFFSET → album list
   (`albums.last_added_at` is denormalized by `recalc_last_added_at` post-scan,
    so no GROUP BY at Browse time)


   Browse cat:played
        │  browse::played::played_albums_children(ctx, start, count)
        │
        ▼
   SELECT albums WHERE last_played_at IS NOT NULL
   ORDER BY last_played_at DESC, id DESC LIMIT/OFFSET → album list
   (never-played albums are excluded; `albums.last_played_at` denormalized
    by stream hot path + post-scan recalc)
```

**History**: the pre-#16 `cat:recent` exposed a sub-container cascade
(`day` / `week` / `month` / `3months` / `year:YYYY` / `all`) with dynamic
hiding. Real-device use on Linn showed the two-click hop was friction
without value, so it was dropped in favor of the flat list above. Future
views default to flat — avoid sub-container cascades (see CLAUDE.md).

### Flow H — Search

```
   Control point
        │ POST /control/cd  (SOAPAction: ContentDirectory#Search)
        ▼
   http/soap_ctrl → upnp/content_directory.handle
        │
        ├─▶ upnp/search.parse_criteria(SearchCriteria)
        │     ──▶  SearchExpr { class: ClassFilter, predicate: Predicate }
        │     - ClassFilter::{Album,Artist,Track,Any} from `upnp:class derivedfrom`
        │     - Predicate tree (Contains / And / Or / DerivedFrom / True)
        │
        ▼
   browse::search::search_tracks(ctx, expr, start, count)
        │  dispatched by ClassFilter:
        │
        ├── Album  ──▶ search_albums
        │     WHERE on album_norm / effective_album_artist_norm (#6)
        │
        ├── Artist ──▶ search_artists
        │     If `[@role="Composer|Conductor|Performer"]` is present (#9):
        │     ──▶ search_classical_facet — DISTINCT t.{column} where
        │         the `*_norm` shadow column matches; returns cm:/cn:/pf:
        │         containers
        │     Otherwise: DISTINCT effective_album_artist with norm column
        │
        └── Track / Any ──▶ search_track_items
              tracks JOIN albums, 4-field OR (title / album / artist / genre)
              with role-routed artist → composer / conductor / performer
        │
        ▼
   The Contains leaf builds `col_norm LIKE '%norm(value)%'` — both column
   value and search input flow through `normalize::for_search`
   (NFKD → strip marks → lowercase → katakana→hiragana).
        ▼
   DidlOutput → soap response → control point
```

---

## 3. Concurrency Model

Top-level tasks spawned from `main.rs`:

| Task | Role | Shutdown |
|---|---|---|
| HTTP server | axum, serves every endpoint in SPEC §8.1 | `ctrl_c` → graceful shutdown |
| SSDP listener | Listens on UDP port 1900, responds to `M-SEARCH` (`ssdp.rs`) | broadcast shutdown |
| SSDP advertiser | `ssdp:alive` on startup, periodic re-announce, `ssdp:byebye` on exit (`ssdp.rs`) | broadcast shutdown (sends byebye first) |
| GENA sweep | Drops expired subscriptions every 60s | broadcast shutdown |
| Scan worker | Triggered on startup (`scan.on_startup`) and from admin endpoints. Runs rayon inside `spawn_blocking`. Re-entry is blocked by `tokio::sync::Semaphore::new(1)` | Short-lived; completes per invocation |
| NOTIFY senders (many) | Short-lived tasks spawned per `broadcast_propchange`. Tracked in `AppState.notify_tasks` and aborted on shutdown | Aborted on shutdown |

Shutdown is signaled by `tokio::signal::ctrl_c()` firing
`tokio::sync::broadcast::channel::<()>(1)`. Each long-running task receives the
broadcast and exits gracefully. axum additionally hooks
`.with_graceful_shutdown(...)`.

# revolver — UPnP MediaServer Specification

A simple UPnP/DLNA MediaServer for personal music libraries.

---

## 1. Goals and Scope

### 1.1 Goals

- A focused, lightweight UPnP MediaServer.
- UPnP AV 1.0 compliant — discoverable and playable from common UPnP control
  points.
- Handle libraries of several tens of thousands of tracks without degrading.
- Small resident memory, fast startup, stable over long uptimes.

### 1.2 In Scope

- UPnP AV 1.0 MediaServer device.
- ContentDirectory:1 service (Browse / Search).
- ConnectionManager:1 service (minimal implementation).
- SSDP discovery (`M-SEARCH` response, `NOTIFY` advertisement).
- HTTP file serving with Range Request support.
- DIDL-Lite XML generation.
- **Browse facets**: Album Artist / Artist / Album / Genre.
- **Recently Added Albums** (based on `added_at`, preserved across rescans).
- **Random Albums** (shuffled at startup, stable within the session).
- **Supported formats**: FLAC / WAV / AIFF (lossless), ALAC / M4A (AAC) /
  MP3 (container + lossy).
- High-resolution audio up to 24-bit / 192 kHz (FLAC / WAV / AIFF / ALAC).
- Album art (embedded artwork with folder image fallback).
- LAN only.

### 1.3 Out of Scope (later or not planned)

| Item | Decision |
|---|---|
| OpenHome extensions | Not applicable (renderer-side feature, unrelated to the server). |
| Vendor-specific control-point extensions (`X_MAP_*`, etc.) | Not implemented. |
| External access / Subsonic API | Out of scope. Use a VPN (e.g., Tailscale) if needed. |
| Last-played / play history | Stream-hit counting is implemented (§6.8); full play history via OpenHome Info subscribe is future work. |
| Transcoding | Not planned. Files are served as-is. |
| Multiple libraries | Not planned. One root directory. |
| Tag editing | Not planned. Read-only. |
| Genre cleanup / tag normalization | Not planned. Tag values are used as-is. |
| Composer / Conductor / Orchestra facet | Implemented (§6.2, #9). |
| Year / Decade facet | Implemented (§6.2, #2). |

---

## 2. Tech Stack

- **Language**: Rust
- **Database**: SQLite (`rusqlite`)
- **HTTP**: `axum`
- **Tag reading**: `lofty`
- **File enumeration**: `walkdir` + `rayon`
- **XML**: `quick-xml`
- **Logging**: `tracing`
- **Distribution**: single binary

---

## 3. Data Model

### 3.1 SQLite Schema

**Design**: albums are first-class entities. `album_id` is the primary key and
forms the basis for UPnP object IDs and queries. As long as the identity tuple
`(effective_album_artist, album, compilation)` is unchanged, `album_id` is
stable even if surface tag values change.

```sql
-- Album-level entity
CREATE TABLE albums (
  id                    INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Identity tuple
  effective_album_artist TEXT NOT NULL,  -- compilation → 'Various Artists',
                                          -- else album_artist or artist
  album                 TEXT NOT NULL,
  compilation           INTEGER NOT NULL DEFAULT 0,
  -- Metadata
  album_artist_raw      TEXT,             -- original tag value (informational, nullable)
  first_seen_at         INTEGER NOT NULL, -- when this album first entered the DB
  -- Cached aggregates (derivable from tracks; precomputed for speed)
  track_count           INTEGER NOT NULL DEFAULT 0,
  total_duration_ms     INTEGER NOT NULL DEFAULT 0,
  -- Quality classification (computed from tracks during scan, §4.6)
  quality               TEXT NOT NULL DEFAULT 'unknown',
                        -- 'hires' | 'lossless' | 'lossy' | 'mixed' | 'unknown'
  UNIQUE(effective_album_artist, album, compilation)
);

CREATE INDEX idx_alb_aa       ON albums(effective_album_artist);
CREATE INDEX idx_alb_first    ON albums(first_seen_at DESC);
CREATE INDEX idx_alb_quality  ON albums(quality);

-- Track-level entity
CREATE TABLE tracks (
  id            INTEGER PRIMARY KEY AUTOINCREMENT,
  album_id      INTEGER NOT NULL REFERENCES albums(id) ON DELETE CASCADE,
  path          TEXT NOT NULL UNIQUE,
  title         TEXT,
  artist        TEXT,                    -- per-track performer (raw tag)
  genre         TEXT,
  track_num     INTEGER,
  disc_num      INTEGER,
  composer      TEXT,                    -- #9: COMPOSER / TCOM / ©wrt
  conductor     TEXT,                    -- #9: CONDUCTOR / TPE3
  performer     TEXT,                    -- #9: PERFORMER / TOPE / ©prf (orchestra / ensemble)
  duration_ms   INTEGER,
  -- Audio properties (for protocolInfo / DIDL-Lite res attributes)
  sample_rate   INTEGER,
  bit_depth     INTEGER,                 -- NULL for lossy (MP3/AAC)
  channels      INTEGER,
  bitrate       INTEGER,                 -- bps
  codec         TEXT,                    -- 'flac' | 'alac' | 'aac' | 'mp3' | 'pcm'
  mime_type     TEXT,
  file_size     INTEGER,
  -- Timestamps
  added_at      INTEGER NOT NULL,        -- decided per the logic in §4.2
  mtime         INTEGER NOT NULL,        -- for incremental-scan diffing
  -- Play stats (for Recently Played, §6.8)
  play_count    INTEGER NOT NULL DEFAULT 0,
  last_played_at INTEGER,                  -- unix seconds, nullable
  -- #11: ReplayGain. dB for gain, linear amplitude for peak. All nullable.
  rg_track_gain REAL,
  rg_track_peak REAL,
  rg_album_gain REAL,
  rg_album_peak REAL
);

CREATE INDEX idx_trk_album    ON tracks(album_id);
CREATE INDEX idx_trk_artist   ON tracks(artist);
CREATE INDEX idx_trk_genre    ON tracks(genre);
CREATE INDEX idx_trk_added    ON tracks(added_at DESC);
CREATE INDEX idx_trk_played   ON tracks(last_played_at DESC);  -- §6.8
CREATE INDEX idx_trk_composer ON tracks(composer);              -- #9
CREATE INDEX idx_trk_conductor ON tracks(conductor);            -- #9
CREATE INDEX idx_trk_performer ON tracks(performer);            -- #9

-- Server's own state
CREATE TABLE server_state (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
-- Keys:
--   'uuid'                : the server's device UUID (generated on first run)
--   'first_run_at'        : Unix timestamp of the server's first run
--   'system_update_id'    : ContentDirectory SystemUpdateID (changes per scan)
--   'last_full_scan_at'   : timestamp of the most recent full scan
```

### 3.2 effective_album_artist Computation

Computed from each track at scan time and stored on the album row:

```
compilation flag set                  → 'Various Artists'
album_artist tag is non-empty         → album_artist
otherwise                              → artist
artist also empty                      → 'Unknown Artist'
```

Storing the computed value avoids a `CASE` expression in every query, lets the
index do its job, and keeps query code simple.

### 3.3 SQLite Settings

```sql
PRAGMA journal_mode = WAL;       -- Browse stays responsive during scans
PRAGMA synchronous = NORMAL;     -- indexes are regenerable, prefer speed
PRAGMA foreign_keys = ON;        -- required for ON DELETE CASCADE
PRAGMA cache_size = -64000;      -- 64MB cache
```

---

## 4. Library Scan

### 4.1 Scan Pipeline

1. Enumerate files under the root directory via `walkdir` (skip hidden files
   `.*`, follow symlinks).
2. Pick audio files (`.flac`, `.wav`, `.aiff`, `.aif`, `.m4a`, `.mp3`). Others
   are skipped and recorded with a reason in the scan report (§4.7).
3. Read tags in parallel via `rayon` (`lofty`). **For M4A containers,
   determine whether the codec is ALAC or AAC** from lofty's codec info and
   save it to the `codec` column.
4. For each track, compute `effective_album_artist` (§3.2).
5. Upsert into `albums`, obtaining `album_id`.
6. Upsert into `tracks` (using `album_id`).
7. **Deletion detection**: paths present in the DB but absent in this scan are
   deleted from `tracks`. The delete count is recorded in the scan report.
8. Recompute `track_count` / `total_duration_ms` on `albums`.
9. **Orphan album cleanup**: delete albums whose `track_count = 0`.
10. Recompute `albums.quality` (§4.6).
11. If the scan contained structural changes, increment `system_update_id`
    (§5.1).
12. Save the scan report to `server_state` (§4.7).

### 4.2 added_at Logic

**Definition**: `added_at` is **"the time the server first recognized this
track path"** — not the file creation time. As a result:

- Initial scan of an existing library → `min(btime, mtime)` (gives a sensible
  relative ordering).
- Files newly observed on subsequent scans → `now()` (so files copied or
  rsync'd with stale btimes still count as "added today").

```rust
fn determine_added_at(meta: &Metadata, server_first_run_at: SystemTime) -> SystemTime {
    let now = SystemTime::now();
    let file_origin = match (meta.created().ok(), meta.modified().ok()) {
        (Some(b), Some(m)) => b.min(m),
        (Some(b), None) => b,
        (None, Some(m)) => m,
        (None, None) => now,
    };

    // On the initial scan, use file_origin to give a sensible
    // relative ordering across the existing library.
    if is_initial_scan {
        file_origin
    } else {
        // Paths newly observed on later scans → "added now".
        // (If file_origin were in the future due to a broken tag,
        //  clamp to now().)
        now
    }
}
```

The `is_initial_scan` test: at scan start, the `tracks` table is empty →
initial. (Alternatively, record `server_state.first_run_at` and compare against
the current clock.)

**Note**: importing a library from another machine via rsync registers the
import time as `added_at` for every track. This is intentional.

### 4.3 Upsert Logic

**albums**:

```sql
INSERT INTO albums (effective_album_artist, album, compilation, album_artist_raw, first_seen_at)
VALUES (?, ?, ?, ?, ?)
ON CONFLICT(effective_album_artist, album, compilation) DO UPDATE SET
  album_artist_raw = excluded.album_artist_raw
  -- first_seen_at is never overwritten
RETURNING id;
```

**tracks**:

```sql
INSERT INTO tracks (album_id, path, title, artist, ..., added_at, mtime)
VALUES (?, ?, ?, ?, ..., ?, ?)
ON CONFLICT(path) DO UPDATE SET
  album_id = excluded.album_id,
  title = excluded.title,
  artist = excluded.artist,
  -- ...
  mtime = excluded.mtime;
  -- added_at is never overwritten
```

### 4.4 Scan Triggers

- At server startup (initial run, or when `on_startup = true`).
- Manual trigger via `POST /admin/rescan`.
- **File-system watching** (`scan.watch`, §12): on local filesystems, watch
  `library.root` via the `notify` crate
  (`inotify` / `FSEvents` / `ReadDirectoryChangesW`) and trigger a rescan after
  a 5-second debounce (so bulk imports collapse into a single scan). On
  network filesystems (NFS / SMB / CIFS / FUSE) the watcher is automatically
  disabled because events are not reliably delivered; fall back to
  `rescan_interval_minutes` or manual rescan. Backend errors are logged and
  demoted to `watch_status = "error"` (never panic). Watch status is exposed
  via `/admin/stats` (§8.5).
- **Periodic rescan** (`scan.rescan_interval_minutes`, §12): runs a full scan
  on the configured interval. Set to `0` to disable. Useful as a fallback when
  watching is unavailable.

**NAS detection**: at startup, `statfs(2)` (`GetDriveType` on Windows) is
called on `library.root`. The filesystem is classified as "network" if:

| OS | Indicator |
|---|---|
| Linux | `f_type` ∈ {`NFS_SUPER_MAGIC` 0x6969, `SMB_SUPER_MAGIC` 0x517B, `CIFS_MAGIC_NUMBER` 0xFF534D42, `FUSE_SUPER_MAGIC` 0x65735546} |
| macOS | `f_fstypename` ∈ {`smbfs`, `nfs`, `webdav`, `afpfs`} |
| Windows | `GetDriveType` returns `DRIVE_REMOTE` |

**Symlinks**: only paths under `library.root` are watched. Symlink targets
outside the root are not watched — rely on the next periodic / manual rescan.

### 4.5 Incremental Scan Optimization

A full scan every time is path-count × tag-read cost. Skip-by-mtime cuts the
common case:

```rust
// If FS mtime equals DB mtime, skip tag reading.
// album/track row updates are also skipped; only albums.track_count is
// recomputed at the end.
```

This makes a "no-change rescan" finish in a few seconds.

### 4.6 Album Quality Calculation

After scanning, compute each album's `quality` by aggregating its tracks.

**Per-track tier**:

| Condition | Tier |
|---|---|
| `codec IN ('flac', 'alac', 'pcm')` and (`sample_rate > 48000` or `bit_depth > 16`) | `hires` |
| `codec IN ('flac', 'alac', 'pcm')`, otherwise | `lossless` |
| `codec IN ('mp3', 'aac')` | `lossy` |
| Otherwise | `unknown` |

**Album aggregation**:

- All tracks have the same tier → that tier.
- Multiple distinct tiers (excluding `unknown`) → `mixed`.
- All tracks `unknown` → `unknown`.
- Some `unknown` + the rest share a tier → that tier (ignore `unknown`).

SQL sketch:

```sql
UPDATE albums SET quality = (
  SELECT CASE
    WHEN COUNT(DISTINCT tier) > 1 THEN 'mixed'
    ELSE MAX(tier)
  END
  FROM (
    SELECT album_id,
      CASE
        WHEN codec IN ('flac','alac','pcm') AND (sample_rate > 48000 OR bit_depth > 16) THEN 'hires'
        WHEN codec IN ('flac','alac','pcm') THEN 'lossless'
        WHEN codec IN ('mp3','aac') THEN 'lossy'
        ELSE 'unknown'
      END AS tier
    FROM tracks
    WHERE album_id = albums.id AND tier != 'unknown'
  )
);
```

(Conceptual — in practice it is one bulk UPDATE rather than one statement
per album.)

### 4.7 Scan Report

Without a way to verify "did the new release get picked up?", operations
break down. Each scan produces a **structured report**, the most recent of
which is stored as JSON in `server_state` and exposed via
`GET /admin/scan-report`.

```jsonc
{
  "scan_id": "uuid",
  "started_at": 1716800000,
  "completed_at": 1716800032,
  "duration_ms": 32145,
  "is_initial": false,
  "stats": {
    "files_enumerated": 50234,
    "tracks_inserted": 12,        // newly discovered
    "tracks_updated": 3,          // tag changes
    "tracks_unchanged": 50214,    // skipped by mtime
    "tracks_deleted": 5,          // file disappeared
    "albums_inserted": 1,
    "albums_deleted": 0
  },
  "issues": [                     // playable but needs attention
    {"path": "...", "issue": "missing_album_artist"},
    {"path": "...", "issue": "missing_album"},
    {"path": "...", "issue": "no_duration"}
  ],
  "skipped": [                    // not scanned
    {"path": "...", "reason": "unsupported_extension"},
    {"path": "...", "reason": "zero_size"},
    {"path": "...", "reason": "tag_read_failed", "error": "..."}
  ]
}
```

**Persistence**: only the most recent scan is kept in `server_state` (no
historical archive).

**Relationship to logs**: `tracing` emits detailed info during the scan; the
report is the post-hoc summary.

### 4.8 Filesystem Rules

- Hidden files (paths starting with `.`) are **skipped** (`.DS_Store`,
  `.git/`, etc.).
- **Symlinks are followed** (so organizational symlinks that aggregate music
  from elsewhere work).
- Files with extensions outside the supported list (`.txt`, `.pdf`, …) are
  recorded in the report with a reason.
- **Character encoding**: paths and tags are assumed UTF-8. Shift-JIS or
  Latin-1 tags are accepted as-is from `lofty` without conversion. If
  display is garbled, fix the tags upstream.

---

## 5. UPnP Services

### 5.1 SystemUpdateID

The core ContentDirectory state. Control points use changes to this value to
invalidate their Browse cache.

- **Persistence**: stored in `server_state.system_update_id` (survives
  restart).
- **Initial value**: `1` on first run.
- **Increment conditions**: any of:
  - A new track is inserted (scan).
  - A track is deleted (scan).
  - A track's tags change in a way that affects album/artist structure (e.g.,
    `album_id` changes, `effective_album_artist` changes).
  - Pure-audio-property changes (bitrate, etc.) **do not** trigger an
    increment (avoids needless cache invalidation).
- **GENA event**: when incremented, NOTIFY subscribing control points of the
  new `SystemUpdateID`.
- **Browse/Search responses**: every response embeds the current value in the
  `UpdateID` field.

### 5.2 Device Description

- Device type: `urn:schemas-upnp-org:device:MediaServer:1`
- Services:
  - `ContentDirectory:1`
  - `ConnectionManager:1`
- `friendlyName` is configurable (default: `"Revolver"`).
- UUID is persistent (`server_state.uuid`, v4 generated on first run).
- `<iconList>` advertises two PNGs embedded in the binary, served from
  `/icon/48.png` (48×48) and `/icon/120.png` (120×120). Source SVG lives at
  `assets/icon.svg`; PNGs are pre-rendered and committed under `assets/`.

### 5.3 ContentDirectory:1

Actions implemented:

| Action | Required | Approach |
|---|---|---|
| `Browse` | ◎ | Full implementation (BrowseMetadata + BrowseDirectChildren). |
| `GetSearchCapabilities` | ◎ | Reports supported properties (§5.4). |
| `GetSortCapabilities` | ◎ | Returns `dc:title`, `dc:date`, `upnp:originalTrackNumber`. |
| `GetSystemUpdateID` | ◎ | Reads from `server_state`. |
| `Search` | ○ | Minimal implementation (§5.4). |

### 5.4 Search Implementation

The Search action is implemented to match the subset of the UPnP search
grammar that real control points (notably Linn / Kazoo, observed via #4)
actually send. The parser produces a tagged predicate tree; the dispatcher
routes queries by the `upnp:class derivedfrom` filter and runs a `LIKE
'%X%'` search against NFKD-folded shadow columns (`*_norm`, #6).

**Fuzzy matching (#6).** Each searchable field is mirrored in a `*_norm`
shadow column populated at upsert / migration time via
`normalize::for_search`. The pipeline applies:

1. NFKD decomposition (decomposes accents and folds fullwidth Latin /
   halfwidth katakana to their canonical forms).
2. Strip combining marks (`café` → `cafe`, `Björk` → `Bjork`).
3. Lowercase (replaces the prior `COLLATE NOCASE`).
4. Katakana → hiragana (`ミユキ` ⇔ `みゆき`, including halfwidth `ﾐﾕｷ`).

The search input runs through the same function, so `LIKE '%norm(input)%'`
against `column_norm` matches regardless of which side the variation is on.

Out of scope (separate follow-ups): romaji conversion, edit-distance
matching, FTS5 trigram.

**Supported grammar:**

- `upnp:class derivedfrom "OBJECT-CLASS"` — class filter. Recognized prefixes:
  - `object.container.album...`              → Album search
  - `object.container.person.musicArtist...` → Artist search
  - `object.item.audioItem...`               → Track search
- `contains` operator on `dc:title` / `upnp:album` / `upnp:artist` /
  `upnp:genre`.
- `upnp:artist[@role="Composer"]` (or `Conductor` / `Performer`) — routed
  to `tracks.composer` / `tracks.conductor` / `tracks.performer` (#9). For
  Artist-class searches the response containers also switch to the matching
  facet (`cm:` / `cn:` / `pf:`). Unknown roles fall through to the default
  effective_album_artist match.
- `and`, `or`, parentheses — full AND/OR composition (Linn's Track / global
  search uses an OR across the 4 fields).
- `*` and the empty string are explicit no-ops (return empty without
  hitting the DB).

**Class-based dispatch** (the key behavior change from earlier minimal
versions):

| Class filter | Table queried | Returned objects |
|---|---|---|
| `Album`  | `albums` — `dc:title`/`upnp:album` map to `albums.album`, `upnp:artist` to `effective_album_artist` | `alb:{id}` containers |
| `Artist` | `albums` — `dc:title`/`upnp:artist` map to `effective_album_artist` (distinct) | `aa:{base64}` containers |
| `Track`  | `tracks JOIN albums` — `title`/`album`/`artist`/`genre` columns | track items |
| `Any`    | Treated as Track | track items |

**Anything outside the supported subset** (unknown property, malformed
quoting, unrecognized `derivedfrom` class) collapses to an empty result.
Preferring inaction over misbehavior keeps control-point caches consistent.

`GetSearchCapabilities` returns `dc:title,upnp:artist,upnp:album` — the
properties most clients probe for. Well-behaved control points use it to
decide what to send.

**Example queries Linn DSM/2 sends (observed):**

```text
# Album field
upnp:class derivedfrom "object.container.album" and dc:title contains "X"

# Artist field
upnp:class derivedfrom "object.container.person.musicArtist" and dc:title contains "X"

# Track / global
upnp:class derivedfrom "object.item.audioItem" and
( dc:title contains "X" or upnp:album contains "X"
  or upnp:artist contains "X" or upnp:genre contains "X" )

# Composer
upnp:class derivedfrom "object.container.person.musicArtist" and
upnp:artist[@role="Composer"] contains "X"
```

### 5.5 ConnectionManager:1

Minimal implementation:

- `GetProtocolInfo` (required; enumerates Source protocolInfo from §7.3).
- `GetCurrentConnectionIDs` (required; always `"0"`).
- `GetCurrentConnectionInfo` (required; fixed values).

### 5.6 GENA Events

Evented state variables on ContentDirectory:

| Variable | Trigger |
|---|---|
| `SystemUpdateID` | Incremented per §5.1. |
| `ContainerUpdateIDs` | (Optional, fine-grained per-container updates. Not implemented; `SystemUpdateID` is sufficient.) |

SUBSCRIBE / NOTIFY details: see §9.

---

## 6. Browse Tree and ID Design

### 6.1 ID Design Principles

UPnP ObjectIDs are strings. Design them to be **stable across scans** so that
control-point favorites / history / playlists do not break.

Scheme:

| ObjectID form | Meaning | Stability |
|---|---|---|
| `0` | Root | Fixed |
| `cat:aa` | Album Artist category | Fixed |
| `cat:ar` | Artist category | Fixed |
| `cat:al` | Album category | Fixed |
| `cat:gn` | Genre category | Fixed |
| `cat:recent` | Recently Added | Fixed |
| `cat:random` | Random | Fixed |
| `cat:hires` | Hi-Res Albums (quality category) | Fixed |
| `cat:lossy` | Lossy Albums (quality category) | Fixed |
| `cat:mixed` | Mixed Quality Albums | Fixed |
| `cat:cm` | Composer category (#9, surfaced only when populated) | Fixed |
| `cat:cn` | Conductor category (#9) | Fixed |
| `cat:pf` | Performer category (#9) | Fixed |
| `aa:{base64(name)}` | A specific Album Artist | Stable as long as the displayed name doesn't change |
| `ar:{base64(name)}` | A specific Artist | Same |
| `gn:{base64(name)}` | A specific Genre | Same |
| `cm:{base64(name)}` | A specific Composer (#9) | Same |
| `cn:{base64(name)}` | A specific Conductor (#9) | Same |
| `pf:{base64(name)}` | A specific Performer (#9) | Same |
| `alb:{album_id}` | A specific Album | Tied to `albums.id`, stable across tag edits |
| `trk:{track_id}` | A specific Track | Tied to `tracks.id` |

Name segments are **URL-safe base64** to avoid `/`, spaces, non-ASCII, etc.

### 6.2 Top Level (ObjectID = "0")

```
"0" (object.container, "Music Library")
├── "cat:aa"      Album Artist
├── "cat:ar"      Artist
├── "cat:al"      Album
├── "cat:gn"      Genre
├── "cat:recent"  Recently Added       ← flat album list (§6.7)
├── "cat:played"  Recently Played      ← stream-hit counting (§6.8)
├── "cat:random"  Random
├── "cat:hires"   Hi-Res Albums        ← quality category
├── "cat:lossy"   Lossy Albums         ← quality category
├── "cat:mixed"   Mixed Quality        ← quality category
├── "cat:cm"      Composer             ← #9, surfaced only when populated
├── "cat:cn"      Conductor            ← #9
├── "cat:pf"      Performer            ← #9
├── "cat:yr"      Year                 ← #2, surfaced only when populated
└── "cat:dec"     Decade               ← #2, surfaced only when populated
```

`cat:recent` returns an **album list directly**, sorted by
`MAX(tracks.added_at) by album_id` DESC. Two settings cap what shows up:

- `browse.recently_added_limit` — max items returned.
- `browse.recently_added_max_age_days` — albums older than N days are
  excluded. `null` (default) means no age cap (show everything by recency).

Both are exposed via the config API (#13) so per-user tuning is one POST away.

> **History**: prior versions of revolver exposed `cat:recent:day` /
> `cat:recent:week` / `cat:recent:month` / `cat:recent:3months` /
> `cat:recent:year:YYYY` / `cat:recent:all` sub-containers under
> `cat:recent`, with a dynamic-hiding rule (a wider range was elided when its
> COUNT matched the next-shorter range). On real-device usage (Linn) the
> two-click cascade was friction without much value, so it was dropped in
> issue #16. Old sub-container IDs no longer parse and a control point that
> cached one gets "no such object" — control points re-fetch on the next
> `SystemUpdateID` bump.

**Design notes**:

- No category for regular CD-quality lossless. Plain `cat:al` covers it.
- `cat:lossy` and `cat:mixed` exist as diagnostic views ("is anything not
  lossless?" / "any mixed-quality albums?").
- `cat:hires` is for "just the hi-res stuff, please."
- These are standard ContentDirectory containers, so they look the same in
  every compliant control point.

**Configurable selection and order** (#8). The set above is the default;
`browse.top_level` overrides both selection and order:

```toml
[browse]
top_level = [
  "cat:aa", "cat:al",
  "cat:recent", "cat:played",
  "cat:hires",
]
```

Rules:

- Iteration order follows the array.
- Unknown IDs are silently dropped (so adding new facets in a future version
  does not require a config rewrite).
- Hi-Res / Lossy / Mixed Quality are surfaced solely by this list — drop
  the `cat:hires` / `cat:lossy` / `cat:mixed` entries to hide them.
- `cat:cm` / `cat:cn` / `cat:pf` still self-hide on libraries with no
  populated rows (#9 keeps the root clean on non-classical collections).
- Duplicates after the first occurrence are dropped.
- Editable at runtime via `/admin/config` (`ReloadTier::Runtime`).

### 6.3 Container object class

| ObjectID | dc:title | upnp:class |
|---|---|---|
| `0` | "Music Library" | `object.container` |
| `cat:aa` | "Album Artist" | `object.container` |
| `cat:ar` | "Artist" | `object.container` |
| `cat:al` | "Album" | `object.container` |
| `cat:gn` | "Genre" | `object.container.genre.musicGenre` (jumpgate) or `object.container` |
| `cat:recent` | "Recently Added" | `object.container` |
| `cat:recent:day` | "Last day" | `object.container` |
| `cat:recent:week` | "Last week" | `object.container` |
| `cat:recent:month` | "Last month" | `object.container` |
| `cat:recent:3months` | "Last 3 months" | `object.container` |
| `cat:recent:year:YYYY` | "YYYY" | `object.container` |
| `cat:recent:all` | "Show All" | `object.container` |
| `cat:played` | "Recently Played" | `object.container` |
| `cat:random` | "Random Albums" | `object.container` |
| `cat:hires` | "Hi-Res Albums" | `object.container` |
| `cat:lossy` | "Lossy Albums" | `object.container` |
| `cat:mixed` | "Mixed Quality" | `object.container` |
| `aa:...` | Album Artist name | `object.container.person.musicArtist` |
| `ar:...` | Artist name | `object.container.person.musicArtist` |
| `gn:...` | Genre name | `object.container.genre.musicGenre` |
| `cm:...` | Composer name (#9) | `object.container.person.musicArtist` |
| `cn:...` | Conductor name (#9) | `object.container.person.musicArtist` |
| `pf:...` | Performer name (#9) | `object.container.person.musicArtist` |
| `cat:yr` | "Year" (#2) | `object.container` |
| `cat:dec` | "Decade" (#2) | `object.container` |
| `yr:YYYY` | "YYYY" (#2) | `object.container` |
| `dec:YYYY` | "YYYYs" (#2) | `object.container` |
| `alb:...` | Album name | `object.container.album.musicAlbum` |

Track items use `object.item.audioItem.musicTrack`.

### 6.4 View Queries

| View (ObjectID) | Query |
|---|---|
| children of `cat:aa` | `SELECT DISTINCT effective_album_artist FROM albums ORDER BY effective_album_artist LIMIT ? OFFSET ?` |
| children of `aa:{name}` | `SELECT id, album FROM albums WHERE effective_album_artist = ? ORDER BY album LIMIT ? OFFSET ?` |
| children of `cat:ar` | `SELECT DISTINCT artist FROM tracks WHERE artist IS NOT NULL AND artist != '' ORDER BY artist LIMIT ? OFFSET ?` |
| children of `ar:{name}` | Albums on which this artist performs. `SELECT DISTINCT a.id, a.album FROM albums a JOIN tracks t ON t.album_id = a.id WHERE t.artist = ? ORDER BY a.album LIMIT ? OFFSET ?` |
| children of `cat:al` | `SELECT id, album, effective_album_artist FROM albums ORDER BY album LIMIT ? OFFSET ?` |
| children of `cat:gn` | `SELECT DISTINCT genre FROM tracks WHERE genre IS NOT NULL AND genre != '' ORDER BY genre LIMIT ? OFFSET ?` |
| children of `gn:{name}` | `SELECT DISTINCT a.id, a.album FROM albums a JOIN tracks t ON t.album_id = a.id WHERE t.genre = ? ORDER BY a.album LIMIT ? OFFSET ?` |
| children of `alb:{id}` | `SELECT * FROM tracks WHERE album_id = ? ORDER BY disc_num, track_num` |
| children of `cat:recent` | Time-range sub-container list (`day` / `week` / `month` / `3months` / `year:YYYY` / `all`) constructed via the dynamic hiding rule (§6.2). |
| children of `cat:recent:{range}` | `SELECT a.id, a.album, a.effective_album_artist FROM albums a JOIN (SELECT album_id, MAX(added_at) AS aa FROM tracks GROUP BY album_id) m ON m.album_id = a.id WHERE m.aa >= ? ORDER BY m.aa DESC LIMIT ? OFFSET ?` (`?` is the range lower bound; `all` omits the `WHERE`; `year:YYYY` uses `WHERE m.aa BETWEEN ? AND ?`.) |
| children of `cat:played` | `SELECT id, album, effective_album_artist FROM albums a JOIN (SELECT album_id, MAX(last_played_at) AS lp FROM tracks WHERE last_played_at IS NOT NULL GROUP BY album_id) m ON m.album_id = a.id ORDER BY m.lp DESC LIMIT ? OFFSET ?` |
| children of `cat:random` | Sliced from a shuffled-at-startup `album_id` array. |
| children of `cat:hires` | `SELECT id, album, effective_album_artist FROM albums WHERE quality = 'hires' ORDER BY effective_album_artist, album LIMIT ? OFFSET ?` |
| children of `cat:cm` (#9) | `SELECT DISTINCT composer FROM tracks WHERE composer IS NOT NULL AND composer != '' ORDER BY composer COLLATE NOCASE LIMIT ? OFFSET ?` |
| children of `cm:{name}` (#9) | `SELECT a.id, a.album, a.effective_album_artist, a.track_count FROM albums a WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.composer = ?) ORDER BY a.album LIMIT ? OFFSET ?` |
| `cat:cn` / `cn:{name}` (#9) | Same shape as composer, against `tracks.conductor`. |
| `cat:pf` / `pf:{name}` (#9) | Same shape as composer, against `tracks.performer`. |
| children of `cat:yr` (#2) | `SELECT DISTINCT year FROM tracks WHERE year IS NOT NULL ORDER BY year DESC LIMIT ? OFFSET ?` |
| children of `yr:{Y}` (#2) | `SELECT a.id, a.album, a.effective_album_artist, a.track_count FROM albums a WHERE EXISTS (SELECT 1 FROM tracks t WHERE t.album_id = a.id AND t.year = ?) ORDER BY a.album LIMIT ? OFFSET ?` |
| children of `cat:dec` (#2) | `SELECT DISTINCT (year/10)*10 AS d FROM tracks WHERE year IS NOT NULL ORDER BY d DESC LIMIT ? OFFSET ?` |
| children of `dec:{D}` (#2) | Same shape as `yr:`, predicate `t.year BETWEEN ? AND ?+9`. |
| children of `cat:lossy` | `SELECT id, album, effective_album_artist FROM albums WHERE quality = 'lossy' ORDER BY effective_album_artist, album LIMIT ? OFFSET ?` |
| children of `cat:mixed` | `SELECT id, album, effective_album_artist FROM albums WHERE quality = 'mixed' ORDER BY effective_album_artist, album LIMIT ? OFFSET ?` |

### 6.5 Paging

- `RequestedCount = 0` means "all" (per the UPnP spec). The implementation
  clamps to a hard cap (e.g., 1000).
- `TotalMatches` is returned via a separate `COUNT(*)` query.
- `UpdateID` in the response carries the current `system_update_id`.

### 6.6 Random Albums Implementation

```rust
struct RandomState {
    album_ids: Mutex<Vec<i64>>,
}

impl RandomState {
    fn reshuffle(&self, conn: &Connection) {
        let mut ids: Vec<i64> = conn.query("SELECT id FROM albums")...;
        ids.shuffle(&mut thread_rng());
        *self.album_ids.lock().unwrap() = ids;
    }
}
```

Timing:

- Shuffled at server startup.
- Manually reshuffled via `POST /admin/reshuffle`.
- Reshuffled automatically after a scan that altered the album set
  (full reshuffle; new albums are not just appended, otherwise they would
  always end up at the bottom).

### 6.7 Recently Added

The children of `cat:recent` are **a flat list of albums** sorted by recency:

```sql
SELECT id, album, effective_album_artist, track_count
FROM albums
WHERE last_added_at IS NOT NULL
  AND (?lower_bound IS NULL OR last_added_at >= ?lower_bound)
ORDER BY last_added_at DESC, id DESC
LIMIT ?count OFFSET ?start
```

Two settings (both in `[browse]`, both editable via the config API of #13):

| Setting | Default | Effect |
|---|---|---|
| `recently_added_limit` | `50` | Hard cap on items returned (also caps SOAP `RequestedCount`). |
| `recently_added_max_age_days` | `null` (no cap) | Lower bound = `now - N*86400`. When `null` the WHERE clause has no age predicate. |

`albums.last_added_at` is denormalized (`MAX(tracks.added_at) GROUP BY
album_id`), bulk-recalced after every scan. Adding a new track to an
existing album updates this field, so the album re-floats to the top of
`cat:recent` — the "resurface on new track" behavior is preserved.

> **History**: earlier versions exposed a sub-container hierarchy
> (`cat:recent:day` / `week` / `month` / `3months` / `year:YYYY` / `all`)
> with a dynamic-hiding rule. It was dropped in issue #16 after real-device
> use on Linn showed the two-click cascade added friction without proportional
> value. The denormalized `last_added_at` column was originally added to make
> that hierarchy fast and is still useful as the single sort key for the flat
> list.

### 6.8 Recently Played Implementation

A request with no `Range` header, or with a `Range` whose start is 0
(`bytes=0-N` / `bytes=0-`), counts as a playback start. Suffix ranges
(`bytes=-N`) and `N-` ranges with `N > 0` (typical pre-fetch) are excluded.
**No client-specific heuristics** — the same rule applies to every client.

DB schema (additions to §3.1):

```sql
ALTER TABLE tracks ADD COLUMN play_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tracks ADD COLUMN last_played_at INTEGER;  -- unix seconds, nullable
```

`/stream/{track_id}` handler:

```rust
// In addition to the existing stream logic:
if range.is_none() || range.is_some_and(|r| r.start == 0) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let conn = pool.get()?;
    conn.execute(
        "UPDATE tracks SET play_count = play_count + 1, last_played_at = ?1 WHERE id = ?2",
        params![now, track_id],
    )?;
}
```

**Accepted noise**:

- A 30-second preview or a skip still counts as one play.
- The same track played simultaneously on multiple renderers counts multiple
  times.
- Any client that issues a Range-absent or `start=0` request twice for the
  same track counts twice.

This is the trade-off for not relying on client-specific signals. A more
accurate "actually played for N seconds" measurement would require OpenHome
Info subscribe (future work).

**Update trigger**: `cat:played` only contains albums with a track that has
`play_count > 0`. The contents of `cat:played` change as plays accumulate, but
`SystemUpdateID` is **not** bumped (a play is not a structural change — see
§5.1).

---

## 7. DIDL-Lite

### 7.1 Container

```xml
<container id="..." parentID="..." childCount="..." restricted="1">
  <dc:title>...</dc:title>
  <upnp:class>object.container</upnp:class>
</container>
```

Album containers use `object.container.album.musicAlbum`.

### 7.2 Track (Item)

```xml
<item id="..." parentID="..." restricted="1">
  <dc:title>...</dc:title>
  <upnp:class>object.item.audioItem.musicTrack</upnp:class>
  <upnp:artist>...</upnp:artist>
  <upnp:album>...</upnp:album>
  <upnp:genre>...</upnp:genre>
  <upnp:originalTrackNumber>...</upnp:originalTrackNumber>
  <upnp:originalDiscNumber>...</upnp:originalDiscNumber>  <!-- multi-disc albums only -->
  <upnp:author role="Composer">...</upnp:author>           <!-- #9, when tag present -->
  <upnp:author role="Conductor">...</upnp:author>          <!-- #9, when tag present -->
  <upnp:author role="Performer">...</upnp:author>          <!-- #9, when tag present -->
  <upnp:albumArtURI>http://.../art/{album_id}</upnp:albumArtURI>
  <res
    protocolInfo="http-get:*:audio/flac:*"
    size="..."
    duration="HH:MM:SS.fff"
    bitrate="..."
    sampleFrequency="96000"
    bitsPerSample="24"
    nrAudioChannels="2"
  >http://.../stream/{track_id}</res>
</item>
```

**For lossy formats (MP3 / AAC)**: omit `bitsPerSample`. Always include
`bitrate`.

### 7.2.1 Disc Divider (multi-disc albums only)

A multi-disc album's child list includes `<container>` dividers
interleaved between disc groups, so Linn (which doesn't render disc
separation from `<upnp:originalDiscNumber>` alone) shows visual disc
boundaries:

```xml
<container id="disc:{album_id}:{N}" parentID="alb:{album_id}"
           childCount="..." restricted="1">
  <dc:title>>> Disc N</dc:title>
  <upnp:class>object.container</upnp:class>
</container>
```

The divider's children resolve to that disc's tracks. Single-disc albums
emit **no** divider.

### 7.3 protocolInfo Mapping

| Format | Codec | Extension | MIME type | protocolInfo |
|---|---|---|---|---|
| FLAC | flac | .flac | `audio/flac` | `http-get:*:audio/flac:*` |
| WAV | pcm | .wav | `audio/x-wav` | `http-get:*:audio/x-wav:*` |
| AIFF | pcm | .aiff / .aif | `audio/x-aiff` | `http-get:*:audio/x-aiff:*` |
| ALAC | alac | .m4a | `audio/mp4` | `http-get:*:audio/mp4:*` |
| AAC | aac | .m4a | `audio/mp4` | `http-get:*:audio/mp4:*` |
| MP3 | mp3 | .mp3 | `audio/mpeg` | `http-get:*:audio/mpeg:*` |

**Notes**:

- **ALAC and AAC share the `.m4a` extension and `audio/mp4` MIME**. Only the
  container payload differs. Renderers open the container to determine the
  codec, so `audio/mp4` is correct for both.
- `bitsPerSample` is emitted **only for lossless formats**
  (FLAC / WAV / AIFF / ALAC). Omit it for MP3 / AAC.
- `sampleFrequency` is emitted for every format.

**Important**: emit `sampleFrequency`, `bitsPerSample`, and
`nrAudioChannels` accurately. Renderers that drop 24/192 streams often do so
because of bad values here.

---

## 8. HTTP Server

### 8.1 Endpoints

| Path | Purpose |
|---|---|
| `GET /description.xml` | UPnP device description |
| `GET /scpd/cd.xml` | ContentDirectory SCPD |
| `GET /scpd/cm.xml` | ConnectionManager SCPD |
| `POST /control/cd` | ContentDirectory SOAP |
| `POST /control/cm` | ConnectionManager SOAP |
| `SUBSCRIBE /event/cd` | GENA event subscription |
| `GET /stream/{track_id}` | Audio file stream (Range support) |
| `GET /art/{album_id}` | Album art |
| `GET /` | Web admin UI (HTML, §8.4) |
| `GET /admin/stats` | Library statistics (JSON, §8.5) |
| `GET /admin/scan-report` | Most recent scan report (JSON, §4.7) |
| `GET /admin/scan-progress` | Live in-flight scan counter (JSON, #12) |
| `POST /admin/rescan` | Schedule a scan — **202 Accepted** with `{ scan_id, started_at }`, runs in the background (#18). Poll `/admin/scan-progress` for completion; `/admin/scan-report` for the final report. 409 if a scan is already in flight. |
| `POST /admin/reshuffle` | Reshuffle random albums |
| `GET /admin/config` | List effective config + defaults + source (JSON, #13) |
| `POST /admin/config` | Partial config update (#13) |
| `DELETE /admin/config/{key}` | Reset a single key to its toml default (#13) |

### 8.2 Range Request

The audio endpoint `/stream/{track_id}` strictly supports:

**Range forms**:

| Range header | Meaning |
|---|---|
| `bytes=N-M` | Bytes N through M, inclusive |
| `bytes=N-` | Byte N through end of file |
| `bytes=-N` | Last N bytes (suffix range) |

**Responses**:

- Valid range → `206 Partial Content`, `Content-Range: bytes N-M/TOTAL`,
  `Content-Length: M-N+1`.
- No `Range` header → `200 OK`, whole file, `Content-Length: TOTAL`.
- Out of range (`N >= TOTAL`, etc.) → `416 Range Not Satisfiable`,
  `Content-Range: bytes */TOTAL`.

**Always-returned headers** (200 / 206 / 416 alike):

- `Accept-Ranges: bytes`
- `Content-Type: <track's mime_type>`

**Notes**:

- For gapless playback, control points often pre-fetch the tail of the
  current track and the head of the next. **Suffix ranges and `N-` ranges
  must both work** or you will hear glitches at track boundaries.
- `If-Range` is ignored (always served as a fresh Range request) — safe for
  this use case.

### 8.3 Album Art

Endpoint: `GET /art/{album_id}?v={version}`.

**Selection priority**:

1. **Embedded artwork** in the representative track (disc 1 / track 1, or the
   smallest path if untracked):
   - If multiple images are present (ID3 / M4A `covr`):
     `Front Cover` → `Other` → first.
2. Album-folder files (in order):
   - `cover.jpg` / `cover.png`
   - `folder.jpg` / `folder.png`
   - `front.jpg` / `front.png`
   - Any other `*.jpg` / `*.png` (lexicographic).
3. None → `404`.

**Cache**:

- Extracted images are kept in memory with a simple bytes-budget cache
  (clear-all when total exceeds ~100MB).
- Cache key: `(album_id, source_signature)`, where `source_signature` is the
  mtime of the representative track (embedded) or art file (folder).
- During scan, entries whose source_signature changed are dropped.

**Versioning**:

- The art URL exposed via DIDL `albumArtURI` carries `?v={signature}`. Since
  control points cache by URL, changing the signature triggers a refetch.

### 8.4 Web Admin UI

`GET /` returns a single HTML page. The UI is designed to work from a phone
(quick checks while away from the desk).

**Contents**:

- Library statistics (fetched from `/admin/stats`):
  - Total albums, total tracks, total duration, breakdown by quality.
- Most recent scan result (from `/admin/scan-report`):
  - Inserted / updated / deleted counts, files with issues.
  - Details collapsible.
- Action buttons:
  - **Rescan** (`POST /admin/rescan`).
  - **Reshuffle Random** (`POST /admin/reshuffle`).
- Server info:
  - friendly_name, UUID, listening port.
  - DB file size, memory usage.
  - Start time, uptime.

**Constraints**:

- Single HTML file (CSS inlined, JS only does `fetch`).
- No external dependencies (no CDN, must work offline).
- No authentication (LAN-only; expose via VPN if needed).
- Mobile-friendly (viewport meta, minimum tap target 44px).
- ~100–200 lines total.

### 8.5 Stats Endpoint

`GET /admin/stats` JSON shape:

```jsonc
{
  "library": {
    "album_count": 3247,
    "track_count": 41892,
    "total_duration_ms": 18234567890,
    "total_file_size_bytes": 1234567890123
  },
  "quality_breakdown": {
    "hires": 412,
    "lossless": 2389,
    "lossy": 312,
    "mixed": 87,
    "unknown": 47
  },
  "scan": {
    "last_full_scan_at": 1716800000,
    "last_scan_duration_ms": 32145,
    "watch_status": "active"  // "active" | "disabled_nas" | "disabled_config" | "error"
  },
  "runtime": {
    "uptime_seconds": 86400,
    "memory_rss_bytes": 142000000,
    "db_file_size_bytes": 56000000,
    "first_run_at": 1700000000
  },
  "server": {
    "version": "0.1.0",
    "uuid": "...",
    "friendly_name": "Revolver"
  }
}
```

---

## 9. SSDP and GENA

### 9.1 SSDP Receive

- Listen on multicast `239.255.255.250:1900`.
- On `M-SEARCH * HTTP/1.1`, respond by unicast UDP according to the `ST`:
  - `ST: ssdp:all` → root device + every service (multiple responses).
  - `ST: upnp:rootdevice` → root device.
  - `ST: urn:schemas-upnp-org:device:MediaServer:1` → self.
  - `ST: urn:schemas-upnp-org:service:ContentDirectory:1` → ContentDirectory.
  - `ST: urn:schemas-upnp-org:service:ConnectionManager:1` → ConnectionManager.
- Delay the response randomly within the `MX` header bound (flood protection).

### 9.2 SSDP Send (NOTIFY)

- Multicast `ssdp:alive` at startup (root device + every service).
- Periodically re-announce (interval ≤ half of `CACHE-CONTROL: max-age`;
  recommended `max-age = 1800`, re-announce every 900s).
- Send `ssdp:byebye` at shutdown.

### 9.3 USN Format

USNs follow the UPnP spec:

| NT/ST | USN |
|---|---|
| `upnp:rootdevice` | `uuid:{DEVICE_UUID}::upnp:rootdevice` |
| `uuid:{DEVICE_UUID}` | `uuid:{DEVICE_UUID}` |
| `urn:schemas-upnp-org:device:MediaServer:1` | `uuid:{DEVICE_UUID}::urn:schemas-upnp-org:device:MediaServer:1` |
| `urn:schemas-upnp-org:service:ContentDirectory:1` | `uuid:{DEVICE_UUID}::urn:schemas-upnp-org:service:ContentDirectory:1` |

### 9.4 GENA SUBSCRIBE

Control points send `SUBSCRIBE`:

```
SUBSCRIBE /event/cd HTTP/1.1
HOST: ...
CALLBACK: <http://...>
NT: upnp:event
TIMEOUT: Second-1800
```

Server response:

```
HTTP/1.1 200 OK
SID: uuid:{NEW_SUBSCRIPTION_UUID}
TIMEOUT: Second-1800
```

In-memory state:

```rust
struct Subscription {
    sid: String,              // "uuid:..." form
    callback_url: String,     // URL extracted from <URL>
    service: ServiceId,       // ContentDirectory | ConnectionManager
    expires_at: SystemTime,   // now + timeout
    seq: u32,                 // NOTIFY sequence, starts at 0
}
```

### 9.5 GENA Renewal and Expiration

- **Renewal**: a `SUBSCRIBE` carrying an existing `SID` (and no `NT` /
  `CALLBACK`) refreshes `expires_at` and responds 200 OK with the same SID.
- **Expiration**: subscriptions past `expires_at` are deleted automatically;
  no further NOTIFYs are sent.
- **Cancellation**: `UNSUBSCRIBE` deletes the subscription explicitly.

### 9.6 GENA NOTIFY

When an evented variable changes, send HTTP NOTIFY to each subscriber's
callback URL:

```
NOTIFY /path/from/callback HTTP/1.1
HOST: ...
CONTENT-TYPE: text/xml; charset="utf-8"
NT: upnp:event
NTS: upnp:propchange
SID: uuid:...
SEQ: {seq}

<?xml version="1.0"?>
<e:propertyset xmlns:e="urn:schemas-upnp-org:event-1-0">
  <e:property>
    <SystemUpdateID>123</SystemUpdateID>
  </e:property>
</e:propertyset>
```

- On the initial SUBSCRIBE, **send a NOTIFY immediately** with the current
  values of all evented variables. This is required by the UPnP spec.
- `SEQ` is per-subscription, starting at 0 and incrementing.
- Repeated NOTIFY failures (e.g., `connection refused`) cause the subscription
  to be dropped.

### 9.7 ContentDirectory Evented Variables

Only `SystemUpdateID` is evented. `ContainerUpdateIDs` is not implemented.

---

## 10. Cross-Client Compatibility

The implementation stays inside **UPnP AV 1.0** so that any compliant control
point or renderer can use the server. Vendor-specific extensions
(`X_MAP_*`, Sonos `r:` namespace, etc.) are not implemented.

### 10.1 What is Standards-Compliant

| Element | Approach |
|---|---|
| `upnp:class` | Standard classes only: `object.container.album.musicAlbum`, `object.container.person.musicArtist`, `object.container.genre.musicGenre`, `object.item.audioItem.musicTrack`, `object.container`. |
| DIDL-Lite `res` attributes | Fully populated: `protocolInfo`, `duration`, `size`, `bitrate`, `sampleFrequency`, `bitsPerSample` (lossless only), `nrAudioChannels`. |
| `upnp:albumArtURI` | **Absolute URL** (`http://host:port/art/...`). Some clients cannot resolve relative URIs. |
| ContentDirectory hierarchy | Standard only. Feature views (e.g., quality categories) are realized via the container tree. |
| MIME types | Use mainstream values: `audio/flac`, `audio/mp4`, `audio/mpeg`, `audio/x-wav`, `audio/x-aiff`. |

### 10.2 Extensions Not Used

- **No vendor-specific Media Server extensions** (`X_MAP_BROWSE_ENTRY`, etc.):
  irrelevant to other clients, and pure maintenance burden.
- **No Sonos `r:` namespace** (Sonos-only). This is why **ReplayGain values
  are stored but not surfaced in DIDL-Lite** (#11) — no standard UPnP
  element exists for gain/peak. The values live on `tracks.rg_*` and are
  exposed only via `/admin/stats` (`tracks_with_replaygain`) until a
  standard emerges.
- **`upnp:longDescription` is not used for important data**: rendering is
  inconsistent across clients. If information matters, put it in `dc:title`
  or in a dedicated container.

### 10.3 protocolInfo Flexibility

Including a DLNA profile name (`DLNA.ORG_PN=FLAC` and so on) pleases some
DLNA-certified hardware but is irrelevant to the typical UPnP control point.

**Approach**:

- Wildcard form `http-get:*:audio/flac:*` (accepted by every client).
- If a specific renderer requires it, add the PN later — no per-client
  profile branching, keep things simple.

### 10.4 ObjectID Stability

Many control points (e.g., favorites, recents, playlists) **cache ObjectIDs
locally**. Breaking IDs on rescan turns those caches into orphans.

The design in §6.1 (`alb:{album_id}` based on a persistent
auto-increment column) meets this requirement.

### 10.5 Quality-Display Strategy

**Options compared**:

| Approach | Compatibility | Result |
|---|---|---|
| Dedicated top-level category (`cat:hires`, etc.) | All UPnP clients | **Adopted.** |
| `dc:title` suffix decoration (`Album [Hi-Res]`) | All UPnP clients | Opt-in feature flag. |
| `upnp:longDescription` | Inconsistent rendering | Not used. |
| Custom XML | None | Not used. |

**Decision**: ship the dedicated top-level categories (§6.2). The title
decoration is an opt-in flag (default off, format and tier set configurable).

Reasoning:

- The category approach works uniformly for every client and only **adds**
  to the top level — it does not interfere with users who do not care about
  quality views.
- Title decoration is more visible but can feel noisy; it is opt-in so
  preferences can differ across deployments without code changes.

---

## 11. Performance Targets

| Metric | Target |
|---|---|
| Full scan (30k–50k tracks, initial) | < 60 s |
| Incremental scan (no changes) | < 5 s |
| Startup (warm cache) | < 3 s |
| Browse response | < 100 ms |
| Resident memory | < 200 MB |
| Concurrent streams | ≥ 4 |

---

## 12. Configuration File

`config.toml`:

```toml
[server]
friendly_name = "Revolver"
http_port = 8200
uuid = "auto"  # generated on first run and persisted

[library]
root = "/path/to/music"
extensions = ["flac", "wav", "aiff", "aif", "m4a", "mp3"]

[scan]
on_startup = true
parallel = 8  # rayon thread count

# File-system watching (§4.4).
# "auto"     → enabled on local FS, disabled on NAS (NFS / SMB / CIFS / FUSE).
# "enabled"  → force on (use if you trust your NAS's event delivery).
# "disabled" → force off.
watch = "auto"

# Periodic full rescan in minutes. 0 disables.
# Useful as a fallback when `watch` is disabled (e.g., on a NAS).
rescan_interval_minutes = 0

[browse]
recently_added_limit = 50
random_albums_limit = 100

# Selection and order of top-level facets at ObjectID "0" (#8, §6.2).
# Default: the full canonical list. Drop entries (including Hi-Res / Lossy /
# Mixed if you don't want quality categories) to hide them.
# top_level = ["cat:aa", "cat:al", "cat:recent", "cat:played", "cat:hires"]

# Optional: decorate album titles with a quality tag.
# Example: "Album Name [Hi-Res 24/96]"
quality_in_title = false
quality_in_title_format = "[{q}]"            # "{q}" is replaced by the label
quality_in_title_include = ["hires", "lossy", "mixed"]  # CD-quality lossless is always unmarked
quality_in_title_show_specs = true           # include numeric specs like "Hi-Res 24/96"
```

---

## 13. Roadmap

### Implemented Functionality

1. Library scanner + SQLite (two-table model: `albums` + `tracks`).
2. Deletion detection and orphan-album cleanup (§4.1 step 7, 9).
3. Scan report generation and persistence (§4.7) + structured `tracing` logs.
4. SSDP discovery and device description.
5. ContentDirectory `Browse` (Album Artist / Artist / Album / Genre).
6. Minimal ContentDirectory `Search` (§5.4).
7. HTTP file serving with strict Range Request support (including suffix
   ranges).
8. DIDL-Lite generation across FLAC / WAV / AIFF / ALAC / AAC / MP3 (CD
   quality through hi-res).
9. GENA SUBSCRIBE/NOTIFY + `SystemUpdateID` propagation.
10. Admin endpoints: `/admin/scan-report`, `/admin/rescan`, `/admin/reshuffle`,
    `/admin/stats` (§8.5).
11. `added_at` logic + Recently Added view.
12. Random Albums view (§6.6).
13. ALAC / AAC / 24/192 hi-res support (protocolInfo / DIDL refinements).
14. Album art (embedded + folder fallback, in-memory cache, §8.3).
15. Album quality calculation (§4.6) + Hi-Res / Lossy / Mixed top-level
    categories.
16. Recently Added time-range submenu (§6.7).
17. Recently Played view via stream-hit counting (`cat:played`, §6.8).
18. Web admin UI (§8.4).
19. Composer / Conductor / Performer classical facets (#9). New nullable
    `tracks.{composer,conductor,performer}` columns read via lofty
    (`COMPOSER` / `TCOM` / `©wrt`, `CONDUCTOR` / `TPE3`, `PERFORMER` /
    `TOPE` / `©prf`). New top-level facets `cat:cm` / `cat:cn` / `cat:pf`
    surface only when the library has populated rows (hidden on
    non-classical collections). Per-track DIDL emits
    `<upnp:author role="Composer|Conductor|Performer">name</upnp:author>`.
    Search routes `upnp:artist[@role="..."]` to the matching column and
    returns the matching `cm:` / `cn:` / `pf:` containers.

20. Multi-disc albums (`MAX(disc_num) > 1`) emit:
    - `<upnp:originalDiscNumber>` on each track (for spec-compliant control
      points such as BubbleUPnP / JRiver), and
    - **disc-divider containers** (`disc:{album_id}:{disc}` with title
      `">> Disc N"`) interleaved between disc boundaries in the album's
      child list, because Linn ignores `<upnp:originalDiscNumber>` in UI
      rendering. The divider is itself a `<container>`; tapping it browses
      the disc's tracks. MinimServer ships the same pattern (§7.2, §14).
      Single-disc albums skip both — no divider, no `originalDiscNumber`.

21. Year / Decade facets (#2). New nullable `tracks.year` column read via
    lofty (`Year` / `RecordingDate` → DATE / YEAR / TDRC / ©day, parsed
    from "YYYY" and "YYYY-MM-DD" forms). New top-level facets `cat:yr`
    (per release year, newest first) and `cat:dec` (10-year buckets,
    bucket = `(year/10)*10`). Both self-hide on libraries with zero
    populated rows.

22. Search fuzzy matching (#6). NFKD-folded shadow columns
    (`tracks.{title,artist,genre,composer,conductor,performer}_norm`,
    `albums.{album,effective_album_artist}_norm`) populated by
    `normalize::for_search` (NFKD → strip combining marks → lowercase →
    katakana→hiragana). One-time migrate backfill fills existing rows;
    `tracks::upsert` / `albums::upsert` keep them in sync on new writes.
    Search's `LIKE '%X%'` runs against the shadow columns with the
    search input fed through the same pipeline. SearchCriteria's
    `read_string` now slices the source `&str` so non-ASCII query
    values survive intact.

23. ReplayGain capture (#11). New nullable `tracks.{rg_track_gain,
    rg_track_peak, rg_album_gain, rg_album_peak}` REAL columns. Tagger
    reads `REPLAYGAIN_TRACK_GAIN` / `REPLAYGAIN_TRACK_PEAK` /
    `REPLAYGAIN_ALBUM_GAIN` / `REPLAYGAIN_ALBUM_PEAK` via lofty's
    `ItemKey::ReplayGain*` variants (Vorbis / TXXX / iTunes mp4 atoms);
    `parse_rg` strips the optional `" dB"` suffix and drops non-finite
    values. `/admin/stats` exposes `tracks_with_replaygain` (count of
    rows with `rg_track_gain IS NOT NULL`) as a coverage diagnostic.
    DIDL exposure is deferred — see §10.2.

### Future Work

- Verify gapless playback on additional renderers (real-hardware testing).
- Memory and startup-time tuning.
- Error-handling polish; structured logging refinements.
- File-system watching with NAS auto-detection and configurable periodic
  rescan (`scan.watch`, `scan.rescan_interval_minutes`; spec'd in §4.4 / §8.5
  / §12, not yet implemented).
- `dc:title` quality decoration (opt-in, §10.5).
- OpenHome Info subscribe for accurate playback timing (refines the stream-
  hit counter in §6.8).

---

## 14. Known Pitfalls

- **Renderer-specific quirks**: some renderers fail to play 24/192 streams or
  glitch during gapless transitions when DIDL `res` attributes are slightly
  off. Capturing traffic with Wireshark against a known-good server is the
  fastest way to diagnose.
- **btime on NAS mounts**: SMB / NFS often does not expose `btime`. Fall back
  to `mtime` (§4.2).
- **Tag variants for the same name**: e.g., `"Eloy, Jean-Claude"` vs
  `"Jean-Claude Eloy"`. No normalization is performed; reconcile upstream
  by editing the tags.
- **`added_at` is not the file's creation time**: it is the time the *server*
  first observed the path (§4.2). Importing a library from another machine
  via rsync sets every track's `added_at` to the import moment. Initial scans
  use a `btime` / `mtime` fallback; subsequent discoveries use `now()`.
- **Album identity tuple**: `(effective_album_artist, album, compilation)`. A
  tag fix that changes any of the three components produces a new `album_id`.
  This propagates to control points as an ObjectID change, but
  `SystemUpdateID` is incremented at the same time so caches are invalidated.
- **M4A codec detection**: `.m4a` can hold ALAC or AAC. The extension alone
  is not enough — `lofty` is used to read the codec from the container and
  store it in the `codec` column. The `audio/mp4` MIME is the same for both,
  but `bitsPerSample` is emitted only for ALAC.
- **iTunes / Music.app and empty Album Artist**: ripping with Apple tools
  often leaves Album Artist blank. The `effective_album_artist` fallback in
  §3.2 covers this. **Compilation flags** (M4A `cpil` / MP3 `TCMP` / Vorbis
  `COMPILATION`) take precedence: when set, the Album Artist text is ignored
  and the value is unified to `Various Artists` (so `"VA"`, `"V.A."`,
  `"Various"`, and other variants all collapse).
- **Browse timeouts**: control points typically time out after a few seconds.
  Libraries with 1000+ Album Artists rely on the indexes in §3.1 to keep
  initial responses fast.
- **Large embedded artwork**: multi-MB embedded images are slow to read on
  every request — caching is required (§8.3).
- **Over-bumping `SystemUpdateID`**: incrementing on pure-audio-property
  changes (bitrate, etc.) causes control points to invalidate their Browse
  cache too often. **Increment only on structural changes**
  (album / artist / genre / path; §5.1).
- **Multi-disc albums**: identical `(effective_album_artist, album,
  compilation)` with different `disc_num` is automatically aggregated as a
  single album (the schema handles it). `ORDER BY disc_num, track_num`
  produces the right order. When `MAX(disc_num) > 1` for that album, two
  things happen: (1) `<upnp:originalDiscNumber>` is emitted on each track,
  and (2) a `<container>` divider (`disc:{album_id}:{disc}`, title
  `">> Disc N"`) is **interleaved between disc groups in the album's child
  list**. The divider is the Linn fallback — Linn parses
  `originalDiscNumber` but does not visually separate discs from it,
  leaving disc-2 tracks looking like duplicates of disc-1 tracks. The
  divider container is tappable; drilling in returns just that disc's
  tracks (a redundant subset of the parent flat view, but it keeps
  navigation coherent). Single-disc albums skip both — `dc:title` is
  **never** modified for disc info, so single-disc browsing is unchanged.
  When tags carry album names like `"Album [Disc 1]"`, the result is two
  separate albums; fix this in tags. Automatic disc merging based on
  parsed-out suffixes is **not implemented** (intentional — the parsing
  rules are too lossy). Non-order-preserving control points (VLC,
  foobar2000, MediaMonkey) may bunch the divider containers separately
  from tracks; this is acceptable degradation since Linn is the primary
  target and other CPs at least don't drop the response.
- **DB cleanup on file deletion**: paths missing from the filesystem during a
  scan are deleted from `tracks`. File moves or renames also look like
  delete-then-insert, which resets `added_at` to "now" for those tracks.
  **A file move is treated as a path change** by design. If accidental
  deletion happens, the scan report shows it and can be recovered manually.

---

## 15. Reference Specifications

- UPnP Device Architecture 1.1
- UPnP MediaServer:1 Device Template
- UPnP ContentDirectory:1 Service Template
- UPnP ConnectionManager:1 Service Template
- DIDL-Lite XML Schema

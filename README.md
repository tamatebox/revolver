# revolver

A lightweight UPnP/DLNA MediaServer written in Rust.
Single binary, SQLite-backed, LAN-only.

## Features

- **Library scanner** — FLAC / WAV / AIFF / ALAC / M4A (AAC) / MP3. High-resolution audio (up to 24-bit / 192 kHz) supported. Compilation detection, deletion detection, automatic quality classification, and ReplayGain capture (coverage surfaced via `/admin/stats`).
- **UPnP/DLNA discovery** — Announced over SSDP, visible to standard UPnP control points.
- **Browse** — Top-level facets (selection + order configurable from the admin UI):
  - Album Artist / Artist / Album / Genre
  - Recently Added (flat album list, optionally capped by count + age in days)
  - Recently Played (counted by stream hits)
  - Random Albums (reshuffled on startup, after each scan, or on demand)
  - Hi-Res / Lossy / Mixed Quality
  - Composer / Conductor / Performer — classical-music facets, surfaced only when the library has matching tags
  - Year / Decade — release-year facets, surfaced only when any track carries a year tag
- **Search** — class-aware: Album search returns album containers, Artist search returns artist containers, Track / global search ORs across `dc:title` / `upnp:album` / `upnp:artist` / `upnp:genre`. Recognizes `upnp:class derivedfrom`, `and` / `or` / parens, and the `upnp:artist[@role="..."]` attribute filter. **Fuzzy matching** folds accents, halfwidth / fullwidth Latin, and katakana ↔ hiragana so `Bjork` matches `Björk`, `Beatles` matches `Ｂｅａｔｌｅｓ`, and `みゆき` matches `ミユキ`.
- **HTTP streaming with Range Request** — Strict support for all Range forms (`bytes=N-M`, `N-`, `-N` suffix). Gapless playback works.
- **Album art** — Embedded artwork first, then folder images (`cover.*` / `folder.*` / `front.*` / etc., case-insensitive). On-demand extraction with a small in-memory cache.
- **GENA events** — `SUBSCRIBE` / `NOTIFY` with `SystemUpdateID` auto-increment, so control points refresh automatically after rescans.
- **Web admin UI** — Single-page UI at `/` with scan trigger, reshuffle, live stats, in-flight scan progress, and runtime settings editor (backed by a REST config API). No external dependencies.

## Compatibility

Tested with:

- Linn DSM/2 (via Linn App on iOS)

revolver targets UPnP AV 1.0 and avoids vendor extensions (`X_MAP_*` and similar), so any compliant control point or renderer should work. Verification beyond the hardware above is limited — bug reports from other setups are welcome.

## Quick Start

```sh
# 1. Prepare config
cp config.toml.example config.toml
$EDITOR config.toml   # set library.root to your music directory

# 2. Build and run
cargo run --release -- --config config.toml
```

On first launch:

- `revolver.db` (SQLite) is created and the library scan starts.
- A device UUID is generated and persisted to `server_state.uuid` (the same UUID is reused on subsequent runs).
- An SSDP `NOTIFY` is multicast so the server appears in UPnP control point lists on the LAN.

Verify:

```sh
# Device description
curl http://localhost:8200/description.xml

# Stats JSON
curl -s http://localhost:8200/admin/stats | jq

# Web UI (scan, reshuffle, stats)
open http://localhost:8200/
```

`Ctrl-C` triggers a graceful shutdown (an SSDP `byebye` is sent before exit).

## Configuration

Edit `config.toml`. The most relevant fields:

| Field | Description |
|---|---|
| `server.friendly_name` | Display name shown in control points |
| `server.http_port` | HTTP port (default `8200`) |
| `server.bind_address` | Bind address (default `0.0.0.0`) |
| `library.root` | Path to your music library |
| `library.extensions` | File extensions to scan |
| `scan.on_startup` | Run a library scan on startup |
| `scan.parallel` | Rayon thread count for tag reading |

See [`config.toml.example`](config.toml.example) for the full schema.

### Runtime browse settings

Browse-side tuning (Recently Added count + age caps, Random Albums cap, top-level facet selection / order) lives in the admin UI (`/` → Settings) rather than `config.toml`. All keys default to "no cap / full canonical list", so a fresh install needs zero toml plumbing — open the Settings panel only when you want to dial something down.

Same edits via the REST API:

```sh
curl http://localhost:8200/admin/config                                # list with defaults / source / restart_required
curl -X POST http://localhost:8200/admin/config \
     -H content-type:application/json \
     -d '{"browse.recently_added_limit": 100}'                         # partial update; `null` = no cap
curl -X DELETE http://localhost:8200/admin/config/browse.recently_added_limit   # reset to default (no cap)
```

Edits land in the SQLite-backed `config_overrides` table and persist across restarts.

## Security

revolver is designed for **LAN-only deployment**. SSDP discovery requires LAN multicast, and there is no authentication. Do not expose it directly to the public Internet:

- All endpoints, including `/admin/*`, are **unauthenticated** (LAN trust is assumed).
- `/stream/{id}` only serves files under `library.root` (symlink targets are canonicalized and verified).
- GENA `SUBSCRIBE` callbacks are restricted to private / loopback / link-local IPs (SSRF defense).
- Subscription and concurrent-request counts are capped (DoS mitigation).
- If you need access beyond the LAN, put revolver behind a reverse proxy with authentication.

## Building

```sh
cargo build --release         # release build (LTO + strip enabled)
cargo test                    # run the test suite
cargo clippy --all-targets
cargo fmt
```

Optional — enable the in-repo pre-commit hook (`cargo fmt --check` + `cargo clippy -- -D warnings`, mirrors CI) once per clone:

```sh
git config core.hooksPath .githooks
```

## Running with Docker

revolver also runs in a container, **but SSDP discovery requires
`network_mode: host`**. Docker Desktop on macOS / Windows runs containers
inside a Linux VM, so multicast traffic never reaches the host LAN — the
container will not be discoverable by UPnP control points there. Use Docker
only on a Linux host.

### docker compose

```sh
# 1. Prepare your data directory and copy the sample config.
mkdir data
cp config.toml.example data/config.toml
# Edit data/config.toml so that library.root = "/music"

# 2. Point the bind-mount in docker-compose.yml at your music library.
$EDITOR docker-compose.yml

# 3. Build and start.
docker compose up -d
```

### docker run

```sh
docker build -t revolver .
docker run -d \
  --name revolver \
  --network host \
  --restart unless-stopped \
  -v /path/to/music:/music:ro \
  -v "$(pwd)/data":/data \
  revolver
```

Mount points:

- `/music` — your music library (read-only is fine; revolver never writes here).
- `/data` — SQLite DB, scan reports, server UUID, and `config.toml`.

Once the project is published with a tagged release, the GitHub Actions workflow
builds and pushes a multi-arch image (`linux/amd64`, `linux/arm64`) to
`ghcr.io/<owner>/revolver`, so users can skip the local build.

## License

MIT. See [`Cargo.toml`](Cargo.toml).

---

For deeper technical details, see [`SPEC.md`](SPEC.md) (data model, protocol, design decisions) and [`ARCHITECTURE.md`](ARCHITECTURE.md) (module layout and data flow).
The [`CLAUDE.md`](CLAUDE.md) file is a working guide for the Claude Code CLI.

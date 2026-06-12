# light-maps — Project Plan

A lightweight, self-hostable map-tile API that website owners deploy next to their
own site. Users upload their own geospatial data; the API bakes it into an optimized
tile store and streams **Mapbox Vector Tiles (MVT)** back to the browser, the same way
Mapbox/MapTiler do — but as a single small binary that runs locally, with a hard
priority on **resource efficiency and time-to-first-rendered-map**.

> **Guiding principle:** *Fast and light first, features second.* Every design decision
> below is biased toward fewer bytes on the wire, fewer CPU cycles per request, and the
> smallest possible memory/idle footprint. Where a feature trades against those, it is
> deferred or made opt-in.

---

## 0. Build workflow at a glance

Read this first. It's the order things get built, *why* that order, and where to stop and
prove the speed/quality before moving on. The driving rule — **prove the hot path is fast
before building breadth on top of it** — so the architecture is validated by measurement,
not by hope, while it's still cheap to change.

Each step lists: **what gets built**, the **stop-and-verify gate** that must pass before the
next step, and **why the order**. Gates are non-negotiable checkpoints — if a gate fails, you
fix it (or revisit the design) before continuing, because every later step assumes it holds.

### Step A — Walking skeleton + benchmark harness *(the riskiest assumption first)*
- **Build:** `lm-core` enough to *read* an existing PMTiles file (mmap + directory lookup);
  a stub `serve` that streams one hardcoded tile over HTTP. No baking yet — use a sample
  PMTiles file as input. Stand up the criterion bench + a load-test profile.
- **🛑 Verify (SPEED gate):** measure cold-start, idle RSS, per-tile lookup time, and p50/p99
  under load against the §5 budget. **This is the make-or-break gate** — if zero-copy mmap
  serving isn't sub-millisecond and light here, the whole premise is wrong and we want to
  know now, before any pipeline code exists.
- **Why first:** the entire design bets on "lookup + copy is nearly free." Test that bet
  before building anything that depends on it.

### Step B — Minimal bake pipeline (GeoJSON → MVT → PMTiles, one layer)
- **Build:** GeoJSON ingest → reproject → per-zoom simplify → clip → quantize → MVT encode →
  write PMTiles + manifest. Single layer, gzip only. Wire `serve` to read real baked output
  and emit TileJSON.
- **🛑 Verify (CORRECTNESS gate):** unit tests for simplification, MVT round-trip, PMTiles
  read/write, reprojection; **golden-tile** byte comparisons; an **end-to-end test**: bake a
  sample GeoJSON, serve it, confirm MapLibre renders it in the browser. Re-run the Step A
  speed bench on *real* tiles and confirm tile-size distribution is sane.
- **Why here:** this is the first genuinely usable slice — a user can upload a GeoJSON and see
  a map. Lock correctness with golden tiles now so later pipeline changes can't silently
  corrupt output. **← First releasable milestone (Phase 1).**

### Step C — Breadth: more formats, layers, tilesets, compression
- **Build:** Shapefile / GeoPackage / CSV / MBTiles-import adapters; multi-layer + multi-
  tileset serving; brotli; tile dedup + empty-tile elision; `inspect` command; per-zoom
  tuning knobs.
- **🛑 Verify (REGRESSION gate):** golden tiles + benches become **CI gates that fail the
  build** on size/latency regression beyond a threshold. Add fuzzing on malformed uploads.
  Each new format gets a round-trip test. Confirm dedup/brotli measurably shrink archives
  *without* slowing the hot path.
- **Why here:** only widen the funnel once the core is correct and measured. The CI gates are
  what keep "fast first" true as surface area grows.

### Step D — Ergonomics, hardening, deploy
- **Build:** `light-maps.js` helper + auto-generated example styles; CORS/auth; opt-in
  metrics; backpressure/limits; systemd + Docker recipes; docs.
- **🛑 Verify (PRODUCTION gate):** integration tests for headers/encoding/304/range/auth/CORS;
  load test at target concurrency recording p99 + RSS; security pass on the request surface.
- **Why last:** these make it pleasant and safe to run, but none of them matter until the
  core is fast, correct, and broad. They ride on top of a proven base.

### Step E — Stretch *(only if it doesn't regress the gates)*
- Incremental re-bake, GDAL feature flag, object-storage/CDN serving, optional raster output.
- **Rule:** any stretch item that trips a SPEED or REGRESSION gate is rejected or deferred.

**Stop-and-check cadence, in one line:** *prove serving is fast (A) → prove baking is correct
(B) → widen formats behind regression gates (C) → harden for production (D) → extend only
within the gates (E).* Steps map onto the phased roadmap in §10; this section is the "why this
order / when to test" view, that section is the deliverables view.

---

## 1. Locked decisions (scope)

| Axis | Decision | Consequence |
|------|----------|-------------|
| Render model | **Vector tiles (MVT)** | Client renders with WebGL; tiny payloads; infinite zoom; styling lives on the client. |
| Deploy target | **Single self-hosted static binary** | Near-zero runtime deps; trivial to run beside any web server; tiny RAM footprint. |
| Data lifecycle | **Static, pre-processed once** | A one-time build step bakes an immutable tile store; the server is read-only at runtime → trivial concurrency, max speed. |
| Styling | **Client-side** | API ships geometry + attributes only. Presentation is the website's job via a MapLibre style JSON. |

These four together define the shape of the whole system: an **offline baker** + a
**read-only streaming server** over an **immutable, memory-mappable tile archive**.

---

## 2. Technology choices (and why)

### Language / runtime: **Rust**
- Single static binary, no GC pauses, no runtime to install — directly serves the
  "lightweight on servers" and "self-hosted binary" goals.
- Best-in-class geospatial + tiling ecosystem in a systems language (see crates below).
- Predictable, low memory; safe concurrency for the read-heavy serving path.
- *Alternative considered:* Go (simpler, also single-binary, great stdlib HTTP) — viable,
  but Rust wins on memory footprint, zero-copy tile serving, and the maturity of `geozero`/
  `mvt` tooling. *C++* rejected on safety/dev-velocity grounds.

### Tile archive format: **PMTiles**
- A single-file, cloud-/disk-friendly, **immutable** archive of pre-baked tiles with an
  embedded directory index. Designed precisely for "bake once, serve reads cheaply."
- Memory-mappable: the OS page cache does the caching for us → low heap, hot tiles stay
  resident, cold start is instant. No external DB, no daemon.
- HTTP range-request native, so the same artifact can later be served from object storage
  or a CDN with zero re-encoding if the user ever outgrows local hosting.
- *Alternative considered:* **MBTiles** (SQLite). Great tooling, but pulls in SQLite,
  has more per-request overhead, and is less mmap-friendly. We support **importing** MBTiles
  but serve from PMTiles.

### Tile encoding: **Mapbox Vector Tile spec v2** (protobuf), gzip/br pre-compressed at bake time.

### Wire transport: **HTTP/1.1 + HTTP/2**, tiles served with `Content-Encoding` already
applied (pre-compressed in the archive) so the server never compresses at request time.

### Client SDK: **MapLibre GL JS** (open-source, no token, drop-in `<script>`), plus a thin
optional `light-maps` JS wrapper for one-line embedding. MapLibre already speaks MVT + style
JSON, so "acts like Mapbox" is mostly free on the client.

### Key crates (indicative, not a lockfile)
- HTTP: `axum` + `hyper` + `tower` (lean, async, great backpressure story).
- Async runtime: `tokio` (single tunable worker pool; can run multi-thread or current-thread).
- Tiling/geo: `geozero`, `geo`, `mvt`/`geozero-mvt`, `geojson`, `shapefile`, `gdal` (optional
  feature flag for exotic formats), `pmtiles`.
- Serialization: `prost` (protobuf), `serde` (config/manifests).
- Compression: `flate2`/`zlib-ng` and `brotli` (bake-time only).

---

## 3. High-level architecture

```
                       BAKE TIME (offline, one-shot CLI)
  ┌───────────────────────────────────────────────────────────────┐
  │  user data (GeoJSON / Shapefile / GeoPackage / CSV / MBTiles) │
  │        │                                                      │
  │        ▼                                                      │
  │  Ingest → Normalize (reproject to WebMercator) → Validate     │
  │        │                                                      │
  │        ▼                                                      │
  │  Tile pipeline: clip, simplify per-zoom, quantize, build MVT  │
  │        │                                                      │
  │        ▼                                                      │
  │  Compress (gzip/br) → write PMTiles archive + manifest.json   │
  └───────────────────────────────────────────────────────────────┘
                                  │  immutable artifact
                                  ▼
                       RUN TIME (long-lived, read-only server)
  ┌───────────────────────────────────────────────────────────────┐
  │  Browser (MapLibre GL JS)                                     │
  │     │  GET /tiles/{set}/{z}/{x}/{y}.mvt                       │
  │     ▼                                                         │
  │  light-maps server (Rust, axum)                               │
  │   • route → lookup tile offset in mmap'd PMTiles directory    │
  │   • zero-copy slice the pre-compressed tile bytes             │
  │   • set caching/encoding headers → stream out                 │
  │   (no decode, no re-encode, no DB, OS page cache = the cache) │
  └───────────────────────────────────────────────────────────────┘
```

Two cleanly separated programs sharing one core library:
- `light-maps bake` — the heavy, offline, CPU-bound tile generator. Run rarely.
- `light-maps serve` — the hot, light, IO-bound read server. Runs forever.

This split is the single most important performance decision: **all expensive work happens
once, offline.** At request time the server does little more than an index lookup and a
memory copy.

---

## 4. Component breakdown

### 4.1 Ingest layer (`bake`)
- **Format adapters** behind one trait: GeoJSON, GeoJSONSeq/NDJSON, Shapefile (+.dbf),
  GeoPackage, CSV with lat/lon, and **MBTiles passthrough** (already-tiled input).
  GDAL is an *optional* compile feature for everything else (KML, GML, FlatGeobuf…), so the
  default binary stays dependency-free and small.
- **Reprojection** to EPSG:3857 (Web Mercator) up front; store source CRS in the manifest.
- **Validation & repair**: fix winding order, drop/repair invalid geometries, warn on
  unsupported types. Fail loud with a per-feature error report rather than producing a
  silently broken map.
- **Attribute schema inference**: collect property keys/types per layer for the manifest so
  the client knows what fields exist for styling/filtering.

### 4.2 Tiling pipeline (`bake`) — *the core IP*
- **Pyramid generation** from `minzoom` to `maxzoom` (configurable, sensible defaults).
- **Per-zoom generalization**: Douglas–Peucker / Visvalingam simplification with a tolerance
  derived from the tile's pixel resolution → low zooms carry far fewer vertices. This is the
  biggest lever on payload size and render speed.
- **Geometry clipping** to tile bounds (+ small buffer to avoid seams).
- **Coordinate quantization** to the 4096-unit MVT tile grid (integerized, delta-encoded).
- **Feature dropping / coalescing** at low zooms (e.g. drop sub-pixel polygons, merge dense
  points) with deterministic, attribute-aware rules.
- **Layer assignment**: one MVT layer per source dataset/layer; configurable.
- **Parallelism**: tile generation is embarrassingly parallel by tile → saturate all cores
  with `rayon`. Deterministic output regardless of thread count.
- **Compression**: each tile gzip- and/or brotli-compressed at bake time and stored
  pre-compressed.

### 4.3 Archive writer (`bake`)
- Emits a single **PMTiles** file + a sibling **`manifest.json`** (tileset metadata: bounds,
  center, min/max zoom, layers, attribute schema, attribution, generated style hints).
- **Deduplication**: identical tiles (very common for empty ocean/blank areas) stored once
  and referenced — PMTiles supports this natively, big size win.
- Writes an integrity hash; archive is content-addressable for cache-busting.

### 4.4 Serving layer (`serve`)
- **Memory-maps** the PMTiles archive at startup; reads the root directory into RAM (small),
  leaves tile data paged on demand.
- **Routes**
  - `GET /tiles/{set}/{z}/{x}/{y}.mvt` — the hot path. Index lookup → zero-copy byte slice →
    stream. Honors `Accept-Encoding` to pick the pre-stored gzip/br variant; never compresses
    inline.
  - `GET /tilesets/{set}/tile.json` — TileJSON so MapLibre auto-configures (URLs, zooms,
    bounds, attribution). Generated from the manifest.
  - `GET /tilesets/{set}/manifest.json` — full layer/attribute schema.
  - `GET /healthz`, `GET /metrics` (opt-in).
  - Optional static hosting of a demo `index.html` for instant smoke-testing.
- **Headers for speed**: long-lived `Cache-Control: public, immutable, max-age=...` (archive
  is content-addressed, so this is safe), strong `ETag`, `304` support, range requests.
- **Backpressure & limits**: connection/concurrency caps, per-request timeouts, max in-flight
  bytes — so a misbehaving client can't blow up memory.
- **Multi-tileset**: one server can mount several archives (e.g. one site, several maps)
  under different `{set}` names.

### 4.5 Client SDK (`web/`)
- Recommend **MapLibre GL JS** directly; ship a ~few-KB `light-maps.js` helper:
  `LightMaps.mount('#map', { url: '/tilesets/mymap/tile.json', style: myStyle })`.
- Ships an **example style JSON** generated from the manifest so a user sees *something*
  immediately, then customizes. (Styling stays 100% client-side per the design.)

### 4.6 CLI / config
- `light-maps bake <inputs...> -o map.pmtiles [--minzoom --maxzoom --layer-name …]`
- `light-maps serve [./*.pmtiles] [--addr --workers --cors …]`
- `light-maps inspect map.pmtiles` (print manifest, tile counts, size histogram).
- Config via flags + optional `light-maps.toml`. **Zero required config** for the happy path.

---

## 5. Performance strategy (the actual priority)

Concrete techniques, mapped to the goal of *fast + light first*:

1. **Do the work once.** All CPU-heavy tiling is offline; runtime is lookup + copy. This is
   the headline decision.
2. **Zero-copy serving.** mmap + slice pre-compressed bytes → no per-request allocation,
   decode, or re-encode. The kernel's page cache is our cache (no app-level cache to tune or
   bloat heap).
3. **Pre-compressed payloads.** Brotli/gzip done at bake time; the server sets the header and
   streams. Saves CPU on every single request.
4. **Aggressive per-zoom simplification + quantization.** Smaller tiles = less bandwidth, less
   client GPU work, faster first paint. Biggest win for *time-to-rendered-map*.
5. **Tile dedup + empty-tile elision.** Don't ship or store blank tiles.
6. **HTTP/2 + immutable caching.** Parallel tile fetch on first load; near-zero work on
   revisits (304 / browser cache).
7. **Small idle footprint.** No DB, no background workers; an idle server is basically an
   mmap and a listening socket — RAM in the low single-digit MBs.
8. **Tunable concurrency.** Default to a small worker pool; scale only if the host has cores
   to spare. Backpressure prevents memory blowups under load.
9. **Optional client niceties** (deferred): tile prefetch around the viewport, low-zoom
   "overview" tile inlined into TileJSON for instant first frame.

### Performance budget / targets (to validate, not promises)
- Cold start to first served tile: **< 50 ms**.
- Idle RSS: **single-digit MB**; loaded-and-serving RSS dominated by OS page cache, tunable.
- Per-tile server CPU: **a few µs** (lookup + slice), excluding kernel IO.
- p99 tile latency from local disk cache: **sub-millisecond** server-side.
These become benchmark gates in CI (§8).

---

## 6. API surface (summary)

| Method | Path | Purpose |
|--------|------|---------|
| GET | `/tiles/{set}/{z}/{x}/{y}.mvt` | Stream a vector tile (hot path). |
| GET | `/tilesets/{set}/tile.json` | TileJSON for MapLibre auto-config. |
| GET | `/tilesets/{set}/manifest.json` | Layer + attribute schema, bounds, attribution. |
| GET | `/tilesets` | List mounted tilesets. |
| GET | `/healthz` | Liveness. |
| GET | `/metrics` | Prometheus metrics (opt-in). |

Cross-cutting: CORS (configurable allowlist), optional bearer/API-key gate for private maps,
range requests, conditional GET (`ETag`/`If-None-Match`), gzip/br content negotiation.

---

## 7. Repository layout (proposed)

```
light-maps/
├─ crates/
│  ├─ lm-core/      # shared: geo types, MVT build, PMTiles read/write, manifest
│  ├─ lm-bake/      # ingest + tiling pipeline (CPU-bound, offline)
│  └─ lm-serve/     # axum read server (IO-bound, runtime)
├─ src/main.rs      # unified `light-maps` CLI dispatch (bake|serve|inspect)
├─ web/             # light-maps.js helper + example style + demo index.html
├─ examples/        # sample datasets + end-to-end "upload→bake→serve" walkthrough
├─ benches/         # criterion benches: tile lookup, bake throughput
├─ docs/            # deploy guide, format support matrix, API reference
└─ PLAN.md
```

---

## 8. Testing, benchmarking & quality gates
- **Unit**: geometry simplification correctness, MVT encoding round-trips, PMTiles
  read/write, reprojection accuracy.
- **Golden tiles**: bake known inputs, byte-compare against checked-in expected tiles to
  catch pipeline regressions.
- **Integration**: spin up `serve` over a fixture archive, assert headers, encodings,
  304s, range behavior, and that MapLibre can consume the TileJSON.
- **Benchmarks (CI gates)**: criterion micro-benches for the hot lookup path; a macro bench
  for bake throughput and tile-size distribution. **Fail CI on regression** beyond a threshold
  — this is how "fast first" stays true over time.
- **Load test**: `oha`/`wrk` profile for tile fetch under concurrency; record p50/p99 and RSS.
- **Fuzzing**: malformed uploads into `bake`; malformed requests into `serve`.

---

## 9. Security & operational concerns
- Runtime server is **read-only** over an immutable artifact → tiny attack surface.
- Strict request validation, hard limits on z/x/y ranges, request size/time caps.
- Bake-time input sandboxing: cap memory/time, reject pathological geometries gracefully.
- Optional API-key/bearer auth + CORS allowlist for private deployments.
- No telemetry by default; `/metrics` strictly opt-in.
- Ships with `systemd` unit + Docker examples; single binary makes supply chain auditing easy.

---

## 10. Phased roadmap

**Phase 0 — Skeleton & spike (proof the core is fast).**
`lm-core` PMTiles read + a stub `serve` that streams one hardcoded tile via mmap. Establish
the benchmark harness. Goal: prove sub-ms zero-copy serving before building anything else.

**Phase 1 — Minimum viable pipeline.**
`bake` for GeoJSON → simplify → MVT → PMTiles, single layer. `serve` full hot path +
TileJSON. MapLibre demo renders an uploaded GeoJSON end-to-end. **This is the first usable
release.**

**Phase 2 — Breadth.**
More input formats (Shapefile, GeoPackage, CSV, MBTiles import), multi-layer/multi-tileset,
per-zoom tuning knobs, brotli, tile dedup, `inspect`.

**Phase 3 — Polish & ergonomics.**
`light-maps.js` helper, auto-generated example styles, CORS/auth, metrics, deploy recipes,
docs site, prefetch/overview-tile client optimizations.

**Phase 4 — Stretch (only if it doesn't compromise §5).**
Incremental re-bake, GDAL feature flag for exotic formats, optional object-storage/CDN
serving of the same PMTiles artifact, raster-tile output as a separate opt-in.

---

## 11. Explicit non-goals (to protect the priority)
- No runtime tile generation, no dynamic editing of data (static-by-design).
- No server-side styling or rendering (client owns presentation).
- No built-in database, user accounts, or multi-tenant management plane.
- No geocoding / routing / directions (out of scope; could be sibling tools later).
- Not a hosted SaaS — it's a binary you run next to your site.

---

## 12. Open questions to resolve before/at Phase 1
1. Default `maxzoom` and simplification tolerances — pick conservative defaults, expose knobs.
2. Brotli vs gzip default (size vs. universal support) — likely store both, negotiate.
3. Exact PMTiles spec version / crate to target; confirm dedup + directory-in-RAM behavior.
4. Minimum browser/MapLibre version to support and document.
5. Bundle MapLibre or load from CDN in the demo (offline-friendliness vs. bundle size).
```

# light-maps

A lightweight, self-hostable vector map-tile server in Rust. Bake your geodata once
into an immutable [PMTiles](https://github.com/protomaps/PMTiles) archive, then serve
it as Mapbox Vector Tiles from a single static binary — no database, no daemon, tiny
idle footprint.

The design splits cleanly into two programs over one core library:

- **`lm-bake`** — offline, CPU-bound pipeline: ingest → reproject → per-zoom simplify →
  clip → MVT encode → compress → write PMTiles.
- **`lm-serve`** — long-lived, read-only server: memory-maps the archive and streams
  pre-compressed tiles with a zero-copy index lookup.

## Quick start

Run the built-in demo (bakes sample data and serves it, no config or files needed):

```
cargo run -p lm-example -- --open
```

Opens a MapLibre demo at `http://localhost:3000/demo`.

## Workflow

**1. Bake** geodata into a PMTiles archive (GeoJSON, GeoJSONSeq, Shapefile, CSV, MBTiles import):

```
lm-bake roads.geojson --layer roads --max-zoom 14 -o roads.pmtiles
lm-bake points.csv    --layer stops --max-zoom 12 -o stops.pmtiles
lm-bake a.geojson b.geojson --layers roads,buildings -o city.pmtiles
```

**2. Serve** the archive:

```
lm-serve roads.pmtiles --cors "*"
```

Point MapLibre (or any MVT client) at `http://localhost:3000/tile.json`.

## Endpoints

| Path | Purpose |
|------|---------|
| `GET /tiles/{set}/{z}/{x}/{y}` | Vector tile (hot path) |
| `GET /tilesets/{set}/tile.json` | TileJSON for MapLibre auto-config |
| `GET /tilesets/{set}/manifest.json` | Layer + attribute schema |
| `GET /tilesets` | List mounted tilesets |
| `GET /healthz` | Liveness |
| `GET /metrics` | Prometheus metrics (opt-in via `--metrics`) |

`lm-serve` flags: `--addr`, `--base-url`, `--api-key`, `--cors`, `--timeout`,
`--max-in-flight`, `--max-zoom`, `--metrics`.

## Layout

```
crates/lm-core    shared geo types, MVT build, PMTiles read/write, manifest
crates/lm-bake    ingest + tiling pipeline (CLI)
crates/lm-serve   axum read server (CLI)
crates/lm-example demo / local-testing server
web/              light-maps.js helper + demo page
deploy/           systemd unit + Dockerfile
wiki/             docs
```

## Docs

- [Baking geodata](wiki/baking.md) — formats, zoom ranges, multi-layer, compression
- [Serving](wiki/serving.md) — flags, auth, CORS, metrics
- [MapLibre integration](wiki/maplibre.md) — `light-maps.js`, plain JS, React
- [Deployment](wiki/deployment.md) — systemd, Docker, reverse proxy
- [PLAN.md](PLAN.md) — design rationale and roadmap

## License

MIT

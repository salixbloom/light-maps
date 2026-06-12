# Serving

`lm-serve` is a single static binary that mmaps one or more PMTiles archives and serves them as Mapbox Vector Tiles over HTTP.

## Quick start

```sh
lm-serve roads.pmtiles
# → listening on 0.0.0.0:3000
```

Multiple archives are served under their filename stem:

```sh
lm-serve roads.pmtiles admin.pmtiles stops.pmtiles
```

## Flags

| Flag | Default | Description |
|---|---|---|
| `--addr <host:port>` | `0.0.0.0:3000` | Address to listen on |
| `--base-url <url>` | `http://<addr>` | Public URL used in TileJSON `tiles` array (set this behind a reverse proxy) |
| `--api-key <token>` | — | Require `Authorization: Bearer <token>` on every request |
| `--cors <origin>` | — | Add an allowed CORS origin. Repeat for multiple. Pass `*` to allow all |
| `--timeout <secs>` | `10` | Per-request timeout |
| `--max-in-flight <n>` | `512` | Maximum concurrent in-flight requests |
| `--max-zoom <z>` | `24` | Reject tile requests above this zoom with 400 |
| `--metrics` | off | Expose Prometheus metrics at `GET /metrics` |

## Routes

| Route | Description |
|---|---|
| `GET /tiles/<set>/<z>/<x>/<y>.mvt` | Tile bytes (pre-compressed, zero-copy) |
| `GET /tilesets/<set>/tile.json` | TileJSON 3.0 for a specific tileset |
| `GET /tile.json` | TileJSON for the first loaded tileset |
| `GET /tilesets/<set>/manifest.json` | Raw metadata JSON from the archive |
| `GET /tilesets` | JSON list of loaded tileset names |
| `GET /healthz` | `200 ok` — use for load-balancer health checks |
| `GET /metrics` | Prometheus metrics (only if `--metrics` is set) |

## Caching headers

Every tile response includes:

```
Cache-Control: public, max-age=86400, immutable
ETag: "lm-<minzoom>-<maxzoom>-<tilecount>"
Vary: Accept-Encoding
```

Clients that send `If-None-Match` get a `304 Not Modified` with no body when the ETag matches — no tile data is read from disk.

## Auth

When `--api-key` is set, every request (including `/healthz`) must carry the token:

```
Authorization: Bearer your-secret-token
```

The comparison is constant-time to prevent timing attacks. For production use, pass the key via an environment file rather than the shell so it doesn't appear in `ps` output:

```sh
# /etc/light-maps/env
LM_API_KEY=your-secret-token
```

```sh
lm-serve tiles.pmtiles --api-key "$LM_API_KEY"
```

## Prometheus metrics

Run with `--metrics` and scrape `GET /metrics`:

```
lm_tile_requests_total{set="roads", status="200"}
lm_tile_request_duration_seconds{set="roads"}
lm_archive_tiles_total{set="roads"}
```

## Environment variable for log level

```sh
RUST_LOG=lm_serve=debug lm-serve tiles.pmtiles
```

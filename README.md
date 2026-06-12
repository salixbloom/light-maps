# light-maps

Lightweight, self-hostable map tile server. Upload your own geodata, bake it once into a [PMTiles](https://github.com/protomaps/PMTiles) archive, serve it as Mapbox Vector Tiles with a single static binary.

## Try it in 30 seconds

```
cargo run -p lm-example
```

Opens a demo at `http://localhost:3000/demo` — no config, no external files.

## The two-step workflow

**1. Bake** your geodata into a PMTiles archive (one-time, offline):

```
# GeoJSON
lm-bake mydata.geojson --layer roads --max-zoom 14 -o roads.pmtiles

# CSV with lat/lon columns
lm-bake points.csv --layer stops --max-zoom 12 -o stops.pmtiles

# Shapefile
lm-bake boundaries.shp --layer admin --max-zoom 8 -o admin.pmtiles
```

**2. Serve** the archive:

```
lm-serve roads.pmtiles --cors "*"
```

Point MapLibre (or any MVT client) at `http://localhost:3000/tile.json`.

## Docs

- [Baking geodata](wiki/baking.md) — formats, zoom ranges, multi-layer archives, compression
- [Serving](wiki/serving.md) — all `lm-serve` flags, auth, CORS, metrics
- [MapLibre integration](wiki/maplibre.md) — `light-maps.js` helper, plain JS, React snippet
- [Deployment](wiki/deployment.md) — systemd, Docker, reverse proxy

## License

MIT

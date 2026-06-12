# Baking geodata

`lm-bake` converts geodata into a [PMTiles v3](https://github.com/protomaps/PMTiles) archive. The archive is immutable — run `lm-bake` again whenever your source data changes.

## Supported input formats

| Format | Extension | Notes |
|---|---|---|
| GeoJSON | `.geojson`, `.json` | FeatureCollection, Feature, or bare Geometry |
| GeoJSON Seq | `.geojsonl`, `.geojsons` | Newline-delimited; RS (`0x1E`) byte optional |
| CSV | `.csv` | Needs `lat`/`latitude` and `lon`/`lng`/`longitude` columns (case-insensitive) |
| Shapefile | `.shp` | Pass the `.shp` path; `.dbf` must be alongside it |
| MBTiles | `.mbtiles` | Imports an existing tile archive directly |

## Basic usage

```sh
lm-bake input.geojson \
  --layer  roads      \   # name used in the vector tile source-layer
  --min-zoom 5        \
  --max-zoom 14       \
  -o roads.pmtiles
```

## Flags

| Flag | Default | Description |
|---|---|---|
| `--layer <name>` | file stem | Layer name written into the vector tile |
| `--min-zoom <z>` | `0` | Lowest zoom level to generate |
| `--max-zoom <z>` | `14` | Highest zoom level to generate |
| `--compression gzip\|brotli` | `gzip` | Tile compression codec |
| `--attribution <text>` | — | Attribution string stored in archive metadata |
| `-o <path>` | `out.pmtiles` | Output path |

## Multi-layer archives

Pass multiple input files to merge them into a single archive:

```sh
lm-bake roads.geojson buildings.geojson water.geojson \
  --layers roads,buildings,water \
  --max-zoom 14 \
  -o city.pmtiles
```

Each file becomes one named layer. Tiles that share the same z/x/y are merged at the protobuf level — no extra memory overhead.

## Zoom range advice

| Use case | Suggested max-zoom |
|---|---|
| Country/continent outlines | 6 |
| Admin boundaries | 8 |
| Road networks | 14 |
| Building footprints | 16 |
| Point datasets | 12–14 |

Generating beyond z14 for polygon/line data rarely adds visible detail and grows the archive quickly. Points are cheap — z14 is usually fine.

## Compression

`gzip` is universally supported by browsers and CDNs. `brotli` produces archives ~15% smaller and is supported by all modern browsers, but some older CDN configs may not pass the `Content-Encoding: br` header through correctly.

## Inspecting an archive

```sh
lm-bake inspect roads.pmtiles
```

Prints zoom range, tile count, metadata JSON, and archive size.

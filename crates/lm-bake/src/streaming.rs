/// Memory-bounded streaming bake.
///
/// The in-memory `bake_layer` path holds every feature (and a reprojected clone)
/// in RAM, which OOMs on multi-million-feature inputs. This path instead:
///
///   1. streams features from a reader, reprojecting each to Web Mercator and
///      spilling it to an on-disk [`feature_store`] — only a small fixed-size
///      index entry (bbox + offset) stays in RAM;
///   2. for each zoom, bins feature *indices* into the tiles their bbox covers,
///      then encodes each populated tile by reading just its features back from
///      the store via mmap.
///
/// Peak RAM is the index (~40 B/feature) plus the features of the tiles being
/// encoded — not the whole dataset.
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use geo::BoundingRect;
use geo_types::Geometry;
use rayon::prelude::*;

use lm_core::{tile_id::tile_to_id, writer::TileEntry};

use crate::{
    error::BakeError,
    feature_store::{FeatureMeta, StoreReader, StoreWriter},
    manifest::{FieldInfo, LayerInfo},
    pipeline::{
        compress_tile, print_open_line, spawn_progress_thread, write_archive, BakeConfig,
        BakeOutput, MIN_FEATURE_PIXELS,
    },
    reproject::{merc_x_to_tile, merc_y_to_tile, tile_bbox, to_mercator},
    simplify::{simplify_for_zoom, simplify_tolerance},
    tile_clip::clip_to_tile,
    tile_encode::{encode_tile, EncodableFeature},
};

type PropMap = serde_json::Map<String, geojson::JsonValue>;

/// Safety ceiling on how many features a single tile may materialize at once.
/// Beyond this we stop adding features to that tile — a tile already this dense
/// is visually saturated, and the cap bounds peak memory regardless of how the
/// data is distributed. (Tippecanoe applies an analogous per-tile limit.)
const MAX_FEATURES_PER_TILE: usize = 200_000;

/// Bake a layer from a streaming GeoJSON-Seq reader without holding all features
/// in memory. `reader` yields one GeoJSON Feature per line (already WGS84).
pub fn bake_layer_streaming<R: Read>(
    reader: R,
    config: &BakeConfig,
    store_path: PathBuf,
) -> Result<BakeOutput, BakeError> {
    // ── pass 1: stream → feature store + in-RAM index ─────────────────────────
    let (store, index, bounds, layer_info) = build_store(reader, config, store_path)?;

    if index.is_empty() {
        return Err(BakeError::Empty);
    }

    // ── pass 2: per-zoom tile binning + encode (read features from store) ──────
    let n_features = index.len();
    let n_zooms = (config.max_zoom - config.min_zoom + 1) as usize;
    let total = n_features * n_zooms;

    let processed = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

    print_open_line(&config.layer_name, n_features, n_zooms, config, is_tty);
    let progress = spawn_progress_thread(
        Arc::clone(&processed),
        Arc::clone(&done),
        config.layer_name.clone(),
        total,
        is_tty,
    );

    let mut entries: Vec<TileEntry> = Vec::new();
    for z in config.min_zoom..=config.max_zoom {
        let zoom_entries = bake_zoom_streaming(&store, &index, z, config, &processed)?;
        entries.extend(zoom_entries);
    }

    done.store(true, Ordering::Relaxed);
    let _ = progress.join();

    // ── assemble archive ──────────────────────────────────────────────────────
    write_archive(entries, layer_info, bounds, config)
}

// ── pass 1 ──────────────────────────────────────────────────────────────────────

#[allow(clippy::type_complexity)]
fn build_store<R: Read>(
    reader: R,
    config: &BakeConfig,
    store_path: PathBuf,
) -> Result<(StoreReader, Vec<FeatureMeta>, [f64; 4], LayerInfo), BakeError> {
    let buf = BufReader::with_capacity(1 << 20, reader);
    let mut writer = StoreWriter::create(store_path)?;
    let mut index: Vec<FeatureMeta> = Vec::new();

    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut field_types: HashMap<String, HashSet<String>> = HashMap::new();
    let mut geom_types: HashSet<String> = HashSet::new();

    for line in buf.lines() {
        let line = line?;
        let trimmed = line.trim_start_matches('\x1E').trim();
        if trimmed.is_empty() {
            continue;
        }
        let feat: geojson::Feature = trimmed
            .parse()
            .map_err(|e: geojson::Error| BakeError::GeoJson(e.to_string()))?;

        let geom_wgs: Geometry<f64> = match feat.geometry {
            Some(g) => g
                .try_into()
                .map_err(|e: geojson::Error| BakeError::GeoJson(e.to_string()))?,
            None => continue,
        };

        // WGS84 bbox for manifest bounds + schema.
        if let Some(rect) = geom_wgs.bounding_rect() {
            min_lon = min_lon.min(rect.min().x);
            min_lat = min_lat.min(rect.min().y);
            max_lon = max_lon.max(rect.max().x);
            max_lat = max_lat.max(rect.max().y);
        }
        geom_types.insert(geom_type_name(&geom_wgs).to_owned());

        let props = feat.properties.unwrap_or_default();
        for (k, v) in &props {
            field_types
                .entry(k.clone())
                .or_default()
                .insert(json_type_name(v).to_owned());
        }

        // Reproject once and store. Index keeps the mercator bbox.
        let geom_merc = to_mercator(geom_wgs);
        let mbox = match geom_merc.bounding_rect() {
            Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
            None => continue,
        };
        let meta = writer.append(&geom_merc, mbox, &props)?;
        index.push(meta);
    }

    let store = writer.finish()?;

    if index.is_empty() {
        return Err(BakeError::Empty);
    }

    let bounds = [
        min_lon.max(-180.0),
        min_lat.max(-90.0),
        max_lon.min(180.0),
        max_lat.min(90.0),
    ];
    let fields = field_types
        .into_iter()
        .map(|(name, types)| FieldInfo {
            name,
            field_type: if types.len() == 1 {
                types.into_iter().next().unwrap()
            } else {
                "mixed".to_owned()
            },
        })
        .collect();
    let layer_info = LayerInfo {
        name: config.layer_name.clone(),
        fields,
        geometry_types: geom_types.into_iter().collect(),
    };

    Ok((store, index, bounds, layer_info))
}

// ── pass 2 ──────────────────────────────────────────────────────────────────────

/// Bake one zoom: bin feature indices into tiles, then encode each populated
/// tile by reading its features from the store.
fn bake_zoom_streaming(
    store: &StoreReader,
    index: &[FeatureMeta],
    z: u8,
    config: &BakeConfig,
    processed: &Arc<AtomicUsize>,
) -> Result<Vec<TileEntry>, BakeError> {
    let max_tile = (1u32 << z) - 1;

    // Sub-pixel drop threshold for this zoom, in mercator metres. A feature
    // whose bbox is smaller than this on both axes can't render distinctly here.
    let drop_size = simplify_tolerance(z) * MIN_FEATURE_PIXELS;

    // Bin feature indices into the tiles their bbox covers. Holding indices
    // (u32) rather than geometries keeps this map small. Sub-pixel features are
    // dropped up front — this is what bounds memory at low zooms, where every
    // feature's bbox would otherwise land in the same one or two tiles.
    let mut buckets: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
    for (i, meta) in index.iter().enumerate() {
        processed.fetch_add(1, Ordering::Relaxed);

        let [min_x, min_y, max_x, max_y] = meta.bbox;

        // Drop features that are sub-pixel at this zoom (both dimensions tiny).
        let w = max_x - min_x;
        let h = max_y - min_y;
        if (w > 0.0 || h > 0.0) && w < drop_size && h < drop_size {
            continue;
        }

        let x_min = merc_x_to_tile(min_x, z).saturating_sub(1);
        let x_max = (merc_x_to_tile(max_x, z) + 1).min(max_tile);
        let y_min = merc_y_to_tile(max_y, z).saturating_sub(1); // y flipped
        let y_max = (merc_y_to_tile(min_y, z) + 1).min(max_tile);

        for x in x_min..=x_max {
            for y in y_min..=y_max {
                let bucket = buckets.entry((x, y)).or_default();
                // Per-tile cap: once a tile is saturated, stop adding to it so a
                // pathological hotspot can't blow up memory.
                if bucket.len() < MAX_FEATURES_PER_TILE {
                    bucket.push(i as u32);
                }
            }
        }
    }

    let layer_name = &config.layer_name;
    let compression = config.compression;

    // Encode populated tiles in parallel. Each task reads only the features in
    // its tile back from the mmap'd store.
    let entries: Vec<TileEntry> = buckets
        .into_par_iter()
        .filter_map(|((x, y), feat_ids)| {
            let (tmin_x, tmin_y, tmax_x, tmax_y) = tile_bbox(z, x, y, 0.0);
            let mut tile_feats: Vec<EncodableFeature> = Vec::new();

            for fid in feat_ids {
                let (geom, props) = store.read(&index[fid as usize]).ok()?;
                let clipped = match clip_to_tile(geom, tmin_x, tmin_y, tmax_x, tmax_y) {
                    Some(g) => g,
                    None => continue,
                };
                let s = match simplify_for_zoom(clipped, z) {
                    Some(g) => g,
                    None => continue,
                };
                if let Some(g) = clip_to_tile(s, tmin_x, tmin_y, tmax_x, tmax_y) {
                    tile_feats.push(EncodableFeature { geom: g, props });
                }
            }

            if tile_feats.is_empty() {
                return None;
            }
            let raw = encode_tile(layer_name, &tile_feats, tmin_x, tmin_y, tmax_x, tmax_y).ok()?;
            let compressed = compress_tile(&raw, compression).ok()?;
            Some(TileEntry { tile_id: tile_to_id(z, x, y), data: compressed })
        })
        .collect();

    Ok(entries)
}

// ── helpers (mirror ingest) ──────────────────────────────────────────────────────

fn geom_type_name(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::Line(_) | Geometry::LineString(_) => "LineString",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::Polygon(_) => "Polygon",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
        _ => "Unknown",
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Bool(_) => "Boolean",
        serde_json::Value::Number(_) => "Number",
        serde_json::Value::String(_) => "String",
        serde_json::Value::Null => "Null",
        _ => "mixed",
    }
}

// Silence unused import warning for PropMap when only referenced in signatures.
#[allow(dead_code)]
type _PropMapAlias = PropMap;

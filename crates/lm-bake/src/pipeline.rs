/// Top-level bake pipeline: GeoJSON → PMTiles.
///
/// Supports multiple input layers, gzip and/or brotli tile compression,
/// per-zoom simplification tolerance overrides, and parallel tile generation.
use std::{
    io::Write,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

use std::collections::HashMap;

use geo::BoundingRect;
use rayon::prelude::*;

use lm_core::{
    pmtiles::Compression,
    tile_id::tile_to_id,
    writer::{write_pmtiles, TileEntry},
};

use crate::{
    error::BakeError,
    ingest::ingest_geojson,
    manifest::{LayerInfo, Manifest},
    reproject::{merc_x_to_tile, merc_y_to_tile, tile_bbox, to_mercator},
    simplify::{simplify_for_zoom, simplify_tolerance},
    tile_clip::clip_to_tile,
    tile_encode::{encode_tile, EncodableFeature},
};

// ── config ────────────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct BakeConfig {
    /// Layer name used in MVT and TileJSON.
    pub layer_name: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub attribution: Option<String>,
    /// Compression to store tiles as.
    pub compression: TileCompression,
    /// Per-zoom tolerance multiplier (1.0 = default pixel resolution).
    /// Higher = more aggressive simplification at that zoom.
    pub tolerance_factor: f64,
    /// Property-field filter. `None` keeps every field; `Some(set)` keeps only
    /// the named fields and drops the rest before storing — trimming both the
    /// feature store and the per-feature RAM held during tiling. Used by the
    /// `--include-fields` / `--exclude-fields` flags and the interactive picker.
    pub keep_fields: Option<std::collections::HashSet<String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TileCompression {
    Gzip,
    Brotli,
}

impl Default for BakeConfig {
    fn default() -> Self {
        Self {
            layer_name: "default".to_owned(),
            min_zoom: 0,
            max_zoom: 14,
            attribution: None,
            compression: TileCompression::Gzip,
            tolerance_factor: 1.0,
            keep_fields: None,
        }
    }
}

// ── output ────────────────────────────────────────────────────────────────────

pub struct BakeOutput {
    pub manifest: Manifest,
    pub pmtiles_bytes: Vec<u8>,
    pub tile_count: usize,
    pub archive_bytes: u64,
}

// ── single-layer entry point ──────────────────────────────────────────────────

/// Bake a single GeoJSON string into a PMTiles archive.
pub fn bake(geojson_src: &str, config: BakeConfig) -> Result<BakeOutput, BakeError> {
    let layer = ingest_geojson(&config.layer_name, geojson_src)?;
    bake_layer(layer, config)
}

/// Bake an already-ingested layer into a PMTiles archive.
///
/// This is the allocation-light entry point: callers that stream a large input
/// (e.g. line-delimited GeoJSON) build the `IngestedLayer` directly and hand it
/// here, avoiding a re-serialize-then-reparse round-trip through a GeoJSON
/// string.
pub fn bake_layer(
    layer: crate::ingest::IngestedLayer,
    config: BakeConfig,
) -> Result<BakeOutput, BakeError> {
    let bounds = layer.bounds;
    let layer_info = layer.layer_info.clone();

    let entries = bake_layer_to_entries(&layer.features, &config)?;
    write_archive(entries, layer_info, bounds, &config)
}

/// Assemble baked tile entries into a finished PMTiles archive (single-layer).
/// Shared by the in-memory and streaming bake paths.
pub fn write_archive(
    entries: Vec<TileEntry>,
    layer_info: LayerInfo,
    bounds: [f64; 4],
    config: &BakeConfig,
) -> Result<BakeOutput, BakeError> {
    let tile_count = entries.len();
    let center = [
        (bounds[0] + bounds[2]) / 2.0,
        (bounds[1] + bounds[3]) / 2.0,
        ((config.min_zoom + config.max_zoom) / 2) as f64,
    ];
    let manifest = Manifest {
        name: config.layer_name.clone(),
        min_zoom: config.min_zoom,
        max_zoom: config.max_zoom,
        bounds,
        center,
        layers: vec![layer_info],
        tile_compression: compression_name(config.compression),
        attribution: config.attribution.clone(),
    };

    let mut entries_mut = entries;
    let mut pmtiles_bytes = Vec::new();
    let archive_bytes = write_pmtiles(
        &mut pmtiles_bytes,
        &mut entries_mut,
        &manifest.to_json(),
        to_lm_compression(config.compression),
        config.min_zoom,
        config.max_zoom,
        bounds,
        center,
    )
    .map_err(|e| BakeError::Write(e.to_string()))?;

    Ok(BakeOutput { manifest, pmtiles_bytes, tile_count, archive_bytes })
}

// ── multi-layer entry point ───────────────────────────────────────────────────

/// Input descriptor for one layer in a multi-layer bake.
pub struct LayerInput<'a> {
    pub name: &'a str,
    pub geojson: &'a str,
}

/// Bake multiple GeoJSON layers into a single PMTiles archive.
/// All layers share the same zoom range and compression setting.
pub fn bake_multi(
    layers: &[LayerInput<'_>],
    config: BakeConfig,
) -> Result<BakeOutput, BakeError> {
    if layers.is_empty() {
        return Err(BakeError::Empty);
    }

    let mut all_entries: Vec<TileEntry> = Vec::new();
    let mut all_layer_infos: Vec<LayerInfo> = Vec::new();
    let mut overall_bounds = [180.0f64, 90.0, -180.0, -90.0];

    for li in layers {
        let mut cfg = config.clone();
        cfg.layer_name = li.name.to_owned();
        let layer = ingest_geojson(li.name, li.geojson)?;

        // Expand overall bounding box.
        let b = layer.bounds;
        overall_bounds[0] = overall_bounds[0].min(b[0]);
        overall_bounds[1] = overall_bounds[1].min(b[1]);
        overall_bounds[2] = overall_bounds[2].max(b[2]);
        overall_bounds[3] = overall_bounds[3].max(b[3]);

        all_layer_infos.push(layer.layer_info.clone());

        // Bake tiles for this layer, then merge tile entries.
        // Tiles from different layers at the same (z,x,y) must be merged into
        // one MVT (multiple layers in one tile) — handled below.
        let entries = bake_layer_to_entries(&layer.features, &cfg)?;
        all_entries.extend(entries);
    }

    // Merge entries with the same tile_id into one tile (multi-layer MVT).
    let merged = merge_tile_entries(all_entries, layers, &config)?;
    let tile_count = merged.len();

    let bounds = overall_bounds;
    let center = [
        (bounds[0] + bounds[2]) / 2.0,
        (bounds[1] + bounds[3]) / 2.0,
        ((config.min_zoom + config.max_zoom) / 2) as f64,
    ];
    let manifest = Manifest {
        name: config.layer_name.clone(),
        min_zoom: config.min_zoom,
        max_zoom: config.max_zoom,
        bounds,
        center,
        layers: all_layer_infos,
        tile_compression: compression_name(config.compression),
        attribution: config.attribution.clone(),
    };

    let mut entries_mut = merged;
    let mut pmtiles_bytes = Vec::new();
    let archive_bytes = write_pmtiles(
        &mut pmtiles_bytes,
        &mut entries_mut,
        &manifest.to_json(),
        to_lm_compression(config.compression),
        config.min_zoom,
        config.max_zoom,
        bounds,
        center,
    )
    .map_err(|e| BakeError::Write(e.to_string()))?;

    Ok(BakeOutput { manifest, pmtiles_bytes, tile_count, archive_bytes })
}

// ── internal helpers ──────────────────────────────────────────────────────────

/// A feature is dropped at a given zoom when its mercator bounding box is below
/// this many MVT grid-units on *both* axes — it would render as sub-pixel noise.
/// Shared by the in-memory and streaming bake paths so they agree on output.
pub(crate) const MIN_FEATURE_PIXELS: f64 = 1.0;

/// Clip a mercator geometry into one tile, simplify, then clip again.
///
/// Returns `None` if the geometry does not intersect the tile after clipping and
/// simplification. Both the in-memory and streaming bake paths use this function
/// so their tile-inclusion decisions are provably identical.
pub fn clip_and_simplify_for_tile(
    geom: geo_types::Geometry<f64>,
    z: u8,
    tmin_x: f64,
    tmin_y: f64,
    tmax_x: f64,
    tmax_y: f64,
) -> Option<geo_types::Geometry<f64>> {
    let clipped = clip_to_tile(geom, tmin_x, tmin_y, tmax_x, tmax_y)?;
    let simplified = simplify_for_zoom(clipped, z)?;
    clip_to_tile(simplified, tmin_x, tmin_y, tmax_x, tmax_y)
}

type PropMap = serde_json::Map<String, geojson::JsonValue>;
type FeatureVec = Vec<(geo_types::Geometry<f64>, PropMap)>;

fn bake_layer_to_entries(
    features: &FeatureVec,
    config: &BakeConfig,
) -> Result<Vec<TileEntry>, BakeError> {
    // Reproject once upfront. We keep a borrowed reference to each feature's
    // properties (`&Map`) rather than cloning — a tile only clones the props of
    // the (few) features that land in it.
    let mercator: Vec<(geo_types::Geometry<f64>, &PropMap)> = features
        .iter()
        .map(|(g, p)| (to_mercator(g.clone()), p))
        .collect();

    // Progress is measured as (zoom levels × features) work units — a real,
    // bounded quantity, unlike the 2^z × 2^z phantom-tile grid the old code
    // tried to iterate (357M at z14, which is what caused the memory blow-up).
    let n_features = mercator.len();
    let n_zooms = (config.max_zoom - config.min_zoom + 1) as usize;
    let total: usize = n_features * n_zooms;

    let processed = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let is_tty = std::io::IsTerminal::is_terminal(&std::io::stderr());

    // Opening line — printed before any work so there's always immediate output.
    print_open_line(&config.layer_name, n_features, n_zooms, config, is_tty);

    let progress_thread = spawn_progress_thread(
        Arc::clone(&processed),
        Arc::clone(&done),
        config.layer_name.clone(),
        total,
        is_tty,
    );

    // Process one zoom level at a time. For each zoom we bin features into the
    // tiles they actually touch (driven by each feature's bounding box), so the
    // working set is bounded by the data — not by 2^z × 2^z.
    let mut entries: Vec<TileEntry> = Vec::new();

    for z in config.min_zoom..=config.max_zoom {
        let zoom_entries = bake_zoom(&mercator, z, config, &processed)?;
        entries.extend(zoom_entries);
    }

    done.store(true, Ordering::Relaxed);
    let _ = progress_thread.join();

    Ok(entries)
}

/// Bake every populated tile at a single zoom level.
///
/// Rather than visiting all 2^z × 2^z tile slots, we simplify each feature once,
/// compute the tile range its bounding box covers, and clip it into only those
/// tiles. The result is a sparse `(x,y) → features` map whose size is bounded by
/// the dataset, not the zoom grid.
fn bake_zoom(
    mercator: &[(geo_types::Geometry<f64>, &PropMap)],
    z: u8,
    config: &BakeConfig,
    processed: &Arc<AtomicUsize>,
) -> Result<Vec<TileEntry>, BakeError> {
    // 1-tile buffer matches the clip buffer so features straddling a tile edge
    // are captured by both neighbours.
    type TileKey = (u32, u32);
    let mut buckets: HashMap<TileKey, Vec<EncodableFeature>> = HashMap::new();

    let max_tile = (1u32 << z) - 1;
    // Features smaller than ~1 pixel at this zoom are dropped (sub-pixel noise).
    // Keeps the in-memory path consistent with the streaming path.
    let drop_size = simplify_tolerance(z) * MIN_FEATURE_PIXELS;

    for (geom, props) in mercator {
        // Tile range is derived from the *original* (unsimplified) bbox, which is
        // the conservative, larger extent — simplification only shrinks geometry,
        // so this never misses a tile the feature touches.
        let bbox = match geom.bounding_rect() {
            Some(b) => b,
            None => {
                processed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };

        // Drop *extended* features (lines/polygons) whose bbox is sub-pixel at
        // this zoom. Points have a zero-size bbox and are always kept.
        let w = bbox.max().x - bbox.min().x;
        let h = bbox.max().y - bbox.min().y;
        if (w > 0.0 || h > 0.0) && w < drop_size && h < drop_size {
            processed.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        // One-tile pad on each side so edge-straddling geometry reaches the
        // neighbouring tile (matches clip_to_tile's buffer).
        let x_min = merc_x_to_tile(bbox.min().x, z).saturating_sub(1);
        let x_max = (merc_x_to_tile(bbox.max().x, z) + 1).min(max_tile);
        // Y axis is flipped: max mercator-y → min tile-y.
        let y_min = merc_y_to_tile(bbox.max().y, z).saturating_sub(1);
        let y_max = (merc_y_to_tile(bbox.min().y, z) + 1).min(max_tile);

        for x in x_min..=x_max {
            for y in y_min..=y_max {
                let (tmin_x, tmin_y, tmax_x, tmax_y) = tile_bbox(z, x, y, 0.0);
                if let Some(g) = clip_and_simplify_for_tile(
                    geom.clone(), z, tmin_x, tmin_y, tmax_x, tmax_y,
                ) {
                    buckets
                        .entry((x, y))
                        .or_default()
                        .push(EncodableFeature { geom: g, props: (*props).clone() });
                }
            }
        }

        processed.fetch_add(1, Ordering::Relaxed);
    }

    // Encode + compress populated tiles in parallel. The number of populated
    // tiles is bounded by the data footprint, so this Vec stays small.
    let layer_name = &config.layer_name;
    let compression = config.compression;

    let entries: Vec<TileEntry> = buckets
        .into_par_iter()
        .filter_map(|((x, y), feats)| {
            if feats.is_empty() {
                return None;
            }
            let (tmin_x, tmin_y, tmax_x, tmax_y) = tile_bbox(z, x, y, 0.0);
            let raw = encode_tile(layer_name, &feats, tmin_x, tmin_y, tmax_x, tmax_y).ok()?;
            let compressed = compress(&raw, compression).ok()?;
            Some(TileEntry { tile_id: tile_to_id(z, x, y), data: compressed })
        })
        .collect();

    Ok(entries)
}

/// Print the opening progress line before any work starts. Shared with the
/// streaming bake path.
pub fn print_open_line(
    layer: &str,
    n_features: usize,
    n_zooms: usize,
    config: &BakeConfig,
    is_tty: bool,
) {
    let total = n_features * n_zooms;
    let mut stderr = std::io::stderr();
    if is_tty {
        let _ = write!(
            stderr,
            "  [{}] {:>7} / {} feature-zooms (  0%)  00:00 elapsed  ",
            layer, 0, total
        );
        let _ = stderr.flush();
    } else {
        eprintln!(
            "  [{}] starting — {} features × {} zoom levels (z{}-z{})",
            layer, n_features, n_zooms, config.min_zoom, config.max_zoom
        );
    }
}

/// Spawn the background progress reporter. Returns the join handle. Shared with
/// the streaming bake path.
pub fn spawn_progress_thread(
    processed: Arc<AtomicUsize>,
    done: Arc<AtomicBool>,
    layer: String,
    total: usize,
    is_tty: bool,
) -> std::thread::JoinHandle<()> {
    let start = Instant::now();
    std::thread::spawn(move || {
        let mut stderr = std::io::stderr();
        let mut last_log_secs = 0u64;

        loop {
            std::thread::sleep(std::time::Duration::from_millis(250));

            let is_done = done.load(Ordering::Relaxed);
            let n = processed.load(Ordering::Relaxed);
            let secs = start.elapsed().as_secs();
            let pct = if total > 0 { n * 100 / total } else { 100 };
            let elapsed_str = format!("{:02}:{:02}", secs / 60, secs % 60);

            if is_tty {
                let _ = write!(
                    stderr,
                    "\r  [{layer}] {n:>7} / {total} feature-zooms ({pct:>3}%)  {elapsed_str} elapsed  "
                );
                let _ = stderr.flush();
            } else if secs > last_log_secs {
                last_log_secs = secs;
                eprintln!("  [{layer}] {n} / {total} feature-zooms ({pct}%)  {elapsed_str} elapsed");
            }

            if is_done {
                break;
            }
        }

        let n = processed.load(Ordering::Relaxed);
        let secs = start.elapsed().as_secs();
        let pct = if total > 0 { n * 100 / total } else { 100 };
        let elapsed_str = format!("{:02}:{:02}", secs / 60, secs % 60);
        if is_tty {
            let _ = writeln!(
                stderr,
                "\r  [{layer}] {n:>7} / {total} feature-zooms ({pct:>3}%)  {elapsed_str} elapsed  "
            );
        } else {
            eprintln!("  [{layer}] {n} / {total} feature-zooms ({pct}%)  {elapsed_str} elapsed");
        }
    })
}

/// Re-encode tiles that share the same tile_id across layers into a single
/// multi-layer MVT.  This is a Step C extension; for single-layer bakes it
/// is a no-op (no duplicate tile_ids exist).
fn merge_tile_entries(
    mut entries: Vec<TileEntry>,
    layers: &[LayerInput<'_>],
    config: &BakeConfig,
) -> Result<Vec<TileEntry>, BakeError> {
    // Group entries by tile_id.
    entries.sort_by_key(|e| e.tile_id);
    let mut merged: Vec<TileEntry> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let id = entries[i].tile_id;
        let mut j = i + 1;
        while j < entries.len() && entries[j].tile_id == id {
            j += 1;
        }
        if j - i == 1 {
            // Single-layer tile — pass through.
            merged.push(TileEntry { tile_id: id, data: entries[i].data.clone() });
        } else {
            // Multiple layers at this tile: decompress each, re-encode as one
            // multi-layer MVT, then re-compress.
            let mut layer_blobs: Vec<(&str, Vec<u8>)> = Vec::new();
            for k in i..j {
                let raw = decompress(&entries[k].data, config.compression)?;
                let layer_name = layers.get(k - i).map(|l| l.name).unwrap_or("layer");
                layer_blobs.push((layer_name, raw));
            }
            let combined = combine_mvt_layers(&layer_blobs)?;
            let compressed = compress(&combined, config.compression)
                .map_err(|e| BakeError::Encode(e.to_string()))?;
            merged.push(TileEntry { tile_id: id, data: compressed });
        }
        i = j;
    }
    Ok(merged)
}

/// Concatenate pre-parsed MVT layer bytes into one tile protobuf.
/// Each input is a raw (decompressed) MVT blob; we simply concatenate their
/// protobuf layer fields — this works because the MVT protobuf field tag for
/// `layer` is field 3, and concatenating two valid protobufs with the same
/// field tag produces a valid repeated-field protobuf.
fn combine_mvt_layers(layers: &[(&str, Vec<u8>)]) -> Result<Vec<u8>, BakeError> {
    let mut combined = Vec::new();
    for (_, data) in layers {
        combined.extend_from_slice(data);
    }
    Ok(combined)
}

/// Compress one tile blob with the configured codec. Public so the streaming
/// bake path can share it.
pub fn compress_tile(data: &[u8], comp: TileCompression) -> Result<Vec<u8>, BakeError> {
    compress(data, comp)
}

fn compress(data: &[u8], comp: TileCompression) -> Result<Vec<u8>, BakeError> {
    match comp {
        TileCompression::Gzip => gzip_compress(data),
        TileCompression::Brotli => brotli_compress(data),
    }
}

fn decompress(data: &[u8], comp: TileCompression) -> Result<Vec<u8>, BakeError> {
    match comp {
        TileCompression::Gzip => {
            use flate2::read::GzDecoder;
            use std::io::Read;
            let mut dec = GzDecoder::new(data);
            let mut out = Vec::new();
            dec.read_to_end(&mut out).map_err(|e| BakeError::Write(e.to_string()))?;
            Ok(out)
        }
        TileCompression::Brotli => {
            let mut out = Vec::new();
            brotli::BrotliDecompress(&mut std::io::Cursor::new(data), &mut out)
                .map_err(|e| BakeError::Write(e.to_string()))?;
            Ok(out)
        }
    }
}

fn gzip_compress(data: &[u8]) -> Result<Vec<u8>, BakeError> {
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(data).map_err(|e| BakeError::Write(e.to_string()))?;
    enc.finish().map_err(|e| BakeError::Write(e.to_string()))
}

fn brotli_compress(data: &[u8]) -> Result<Vec<u8>, BakeError> {
    let mut out = Vec::new();
    let params = brotli::enc::BrotliEncoderParams {
        quality: 5, // balanced speed/size; tunable
        ..Default::default()
    };
    brotli::BrotliCompress(&mut std::io::Cursor::new(data), &mut out, &params)
        .map_err(|e| BakeError::Write(e.to_string()))?;
    Ok(out)
}

fn compression_name(c: TileCompression) -> String {
    match c {
        TileCompression::Gzip => "gzip".to_owned(),
        TileCompression::Brotli => "brotli".to_owned(),
    }
}

fn to_lm_compression(c: TileCompression) -> Compression {
    match c {
        TileCompression::Gzip => Compression::Gzip,
        TileCompression::Brotli => Compression::Brotli,
    }
}

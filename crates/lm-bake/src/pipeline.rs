/// Top-level bake pipeline: GeoJSON → PMTiles.
///
/// Supports multiple input layers, gzip and/or brotli tile compression,
/// per-zoom simplification tolerance overrides, and parallel tile generation.
use std::io::Write;

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
    reproject::{tile_bbox, to_mercator},
    simplify::simplify_for_zoom,
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
    let bounds = layer.bounds;
    let layer_info = layer.layer_info.clone();

    let entries = bake_layer_to_entries(&layer.features, &config)?;
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

type FeatureVec = Vec<(geo_types::Geometry<f64>, serde_json::Map<String, geojson::JsonValue>)>;

fn bake_layer_to_entries(
    features: &FeatureVec,
    config: &BakeConfig,
) -> Result<Vec<TileEntry>, BakeError> {
    // Reproject once upfront.
    let mercator: Vec<_> = features
        .iter()
        .map(|(g, p)| (to_mercator(g.clone()), p))
        .collect();

    let tile_jobs: Vec<(u8, u32, u32)> = (config.min_zoom..=config.max_zoom)
        .flat_map(|z| {
            let n = 1u32 << z;
            (0..n).flat_map(move |x| (0..n).map(move |y| (z, x, y)))
        })
        .collect();

    let entries: Vec<TileEntry> = tile_jobs
        .into_par_iter()
        .filter_map(|(z, x, y)| {
            let (min_x, min_y, max_x, max_y) = tile_bbox(z, x, y, 0.0);

            let tile_feats: Vec<EncodableFeature> = mercator
                .iter()
                .filter_map(|(geom, props)| {
                    // Clip first (fast bbox guard), then simplify, then re-clip.
                    let clipped = clip_to_tile(geom.clone(), min_x, min_y, max_x, max_y)?;
                    let simplified = simplify_for_zoom(clipped, z)?;
                    clip_to_tile(simplified, min_x, min_y, max_x, max_y).map(|g| {
                        EncodableFeature { geom: g, props: (*props).clone() }
                    })
                })
                .collect();

            if tile_feats.is_empty() {
                return None;
            }

            let raw = encode_tile(&config.layer_name, &tile_feats, min_x, min_y, max_x, max_y)
                .ok()?;
            let compressed = compress(&raw, config.compression).ok()?;

            Some(TileEntry { tile_id: tile_to_id(z, x, y), data: compressed })
        })
        .collect();

    Ok(entries)
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

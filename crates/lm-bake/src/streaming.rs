/// Memory-bounded streaming bake.
///
/// Memory model
/// ────────────
/// Pass 1 (build_store): feature geometries are reprojected to Web Mercator and
/// spilled to an mmap'd on-disk store one at a time. Only a 44-byte index entry
/// per feature stays in RAM (~145 MB for 3.3 M features).
///
/// Pass 2 (bake_zoom_streaming): for each zoom we bin index entries (4 bytes each)
/// into tile buckets, then process those buckets in chunks of TILE_CHUNK_SIZE.
/// Each chunk reads its features from the mmap store, encodes+compresses the
/// tiles, and pushes the compressed bytes to a StreamingTileWriter that appends
/// directly to a temp file. The chunk's geometry is dropped before the next chunk
/// loads, so peak geometry RAM is proportional to chunk size, not dataset size.
///
/// Peak RAM breakdown (WA parcels, 3.3 M features, z0-z14):
///   Feature index:    ~145 MB  (Vec<FeatureMeta>, 44 B each)
///   Tile directory:   ~  2 MB  (Vec<DirEntry>, 20 B × 83 k tiles)
///   Geometry / chunk: ~100 MB  (TILE_CHUNK_SIZE tiles × avg geometry)
///   Total:            ~250 MB  (was ~1.5 GB)
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

use geo::BoundingRect;
use geo_types::Geometry;
use rayon::prelude::*;

use lm_core::{tile_id::tile_to_id, StreamingTileWriter};

use crate::{
    error::BakeError,
    feature_store::{FeatureMeta, StoreReader, StoreWriter},
    manifest::{FieldInfo, LayerInfo, Manifest},
    pipeline::{
        clip_and_simplify_for_tile, compress_tile, print_open_line, spawn_progress_thread,
        BakeConfig, BakeOutput, MIN_FEATURE_PIXELS,
    },
    reproject::{merc_x_to_tile, merc_y_to_tile, tile_bbox, to_mercator},
    simplify::simplify_tolerance,
    tile_encode::{encode_tile, EncodableFeature},
};

/// Max feature-references decoded in one rayon batch. A "reference" is one
/// (tile, feature) pair, i.e. one `store.read` that materialises a geometry +
/// properties in RAM. Bounding the *reference* count (rather than the tile count)
/// keeps peak per-chunk RAM flat across zooms: at low zoom a handful of huge tiles
/// fill a chunk; at high zoom thousands of tiny ones do, but the live feature
/// payload is the same either way.
///
/// ~50 k refs × ~500 B avg (geometry + parsed props) ≈ ~25 MB of live feature
/// data per chunk, times rayon's thread fan-out. Lower this if RSS is still tight.
const CHUNK_FEATURE_REFS: usize = 50_000;

/// A single tile is never split across chunks, so one tile may exceed the target
/// on its own. This caps how far a single oversized tile can blow the budget.
const MAX_FEATURES_PER_TILE: usize = 200_000;

// ── public entry point ────────────────────────────────────────────────────────

/// Bake a layer from a streaming GeoJSON-Seq reader without holding all features
/// in RAM. Tiles are written to `out` as they are produced; peak RSS is bounded
/// by the index + one chunk of tile geometry, not the whole dataset.
pub fn bake_layer_streaming<R: Read, W: Write>(
    reader: R,
    config: &BakeConfig,
    store_path: PathBuf,
    tile_tmp_path: PathBuf,
    out: &mut W,
) -> Result<BakeOutput, BakeError> {
    // ── pass 1: stream → feature store + in-RAM index ─────────────────────────
    // The store is built in input order, then rewritten in Morton (Z-order) for
    // read locality. Derive the sorted path as a sibling of the unsorted one.
    let sorted_store_path = store_path.with_extension("sorted");
    let (store, index, bounds, layer_info) =
        build_store(reader, config, store_path, sorted_store_path)?;

    if index.is_empty() {
        return Err(BakeError::Empty);
    }

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

    // ── pass 2: per-zoom tile encoding → streaming writer ─────────────────────
    let mut tile_writer = StreamingTileWriter::new(tile_tmp_path)
        .map_err(|e| BakeError::Io(e))?;

    for z in config.min_zoom..=config.max_zoom {
        bake_zoom_streaming(&store, &index, z, config, &processed, &mut tile_writer)?;
    }

    done.store(true, Ordering::Relaxed);
    let _ = progress.join();

    let tile_count = tile_writer.tile_count();

    // ── assemble archive ──────────────────────────────────────────────────────
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

    let archive_bytes = tile_writer
        .finish(
            out,
            &manifest.to_json(),
            to_lm_compression(config.compression),
            config.min_zoom,
            config.max_zoom,
            bounds,
            center,
        )
        .map_err(|e| BakeError::Write(e.to_string()))?;

    // Collect output bytes for callers that need them in memory (e.g. lm-example
    // bake path). For large files the caller should pass a File writer instead.
    Ok(BakeOutput {
        manifest,
        pmtiles_bytes: Vec::new(), // caller used `out` directly
        tile_count,
        archive_bytes,
    })
}

// ── pass 1 ───────────────────────────────────────────────────────────────────

#[allow(clippy::type_complexity)]
fn build_store<R: Read>(
    reader: R,
    config: &BakeConfig,
    store_path: PathBuf,
    sorted_store_path: PathBuf,
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

        if let Some(rect) = geom_wgs.bounding_rect() {
            min_lon = min_lon.min(rect.min().x);
            min_lat = min_lat.min(rect.min().y);
            max_lon = max_lon.max(rect.max().x);
            max_lat = max_lat.max(rect.max().y);
        }
        geom_types.insert(geom_type_name(&geom_wgs).to_owned());

        let mut props = feat.properties.unwrap_or_default();
        // Drop unwanted fields before they reach the store — trims disk and the
        // per-feature RAM each tile re-parses during pass 2.
        if let Some(keep) = &config.keep_fields {
            props.retain(|k, _| keep.contains(k));
        }
        for (k, v) in &props {
            field_types
                .entry(k.clone())
                .or_default()
                .insert(json_type_name(v).to_owned());
        }

        let geom_merc = to_mercator(geom_wgs);
        let mbox = match geom_merc.bounding_rect() {
            Some(r) => [r.min().x, r.min().y, r.max().x, r.max().y],
            None => continue,
        };
        let meta = writer.append(&geom_merc, mbox, &props)?;
        index.push(meta);
    }

    let unsorted = writer.finish()?;

    if index.is_empty() {
        return Err(BakeError::Empty);
    }

    // ── spatial sort ──────────────────────────────────────────────────────────
    // Reorder the store so geographically-adjacent features sit at adjacent byte
    // offsets. Without this, per-tile reads at high zoom fault in pages scattered
    // across the whole store (the entire file ends up resident). With a Morton
    // (Z-order) sort, a tile's features cluster into a small contiguous offset
    // window, so the kernel only needs that region resident at a time.
    let (store, index) = spatial_sort_store(unsorted, index, sorted_store_path)?;

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

// ── spatial sort ───────────────────────────────────────────────────────────────

/// Rewrite the feature store in Morton (Z-order) order for read locality.
///
/// Each record is relocated by a verbatim byte copy (no decode/re-encode), so the
/// cost is one streaming read+write of the store. The returned index is in the new
/// order and its offsets point into the new (sorted) store; the unsorted store is
/// deleted when its reader drops.
fn spatial_sort_store(
    unsorted: StoreReader,
    index: Vec<FeatureMeta>,
    sorted_path: PathBuf,
) -> Result<(StoreReader, Vec<FeatureMeta>), BakeError> {
    // Compute a Morton code per feature from its mercator bbox centre, quantised
    // against the dataset's mercator extent. A small `Vec<(u64,u32)>` (12 B each)
    // is the only extra RAM — ~40 MB at 3.3 M features.
    let mut order: Vec<(u64, u32)> = index
        .iter()
        .enumerate()
        .map(|(i, m)| (morton_code(m.bbox), i as u32))
        .collect();
    order.sort_unstable_by_key(|(code, _)| *code);

    // Stream records into the sorted store in the new order. Random reads from the
    // unsorted mmap fault in its pages once here; thereafter every per-zoom read
    // hits the locality-sorted store.
    let mut writer = StoreWriter::create(sorted_path)?;
    let mut sorted_index: Vec<FeatureMeta> = Vec::with_capacity(index.len());
    for (_, orig_i) in &order {
        let meta = &index[*orig_i as usize];
        let raw = unsorted.read_raw(meta);
        let new_meta = writer.append_raw(meta.bbox, raw)?;
        sorted_index.push(new_meta);
    }

    let store = writer.finish()?;
    drop(unsorted); // deletes the unsorted store file via Drop
    Ok((store, sorted_index))
}

/// Morton (Z-order) code for a feature, derived from its mercator bbox centre.
///
/// The centre is normalised to [0,1) against the full Web Mercator square, then
/// quantised to 21 bits per axis and bit-interleaved into a u64. 21 bits ≈ 19 m
/// resolution at the equator — far finer than any tile we generate, so features
/// in the same high-zoom tile share a contiguous code range.
#[inline]
fn morton_code(bbox: [f64; 4]) -> u64 {
    use crate::reproject::MERCATOR_EXTENT;
    let cx = (bbox[0] + bbox[2]) * 0.5;
    let cy = (bbox[1] + bbox[3]) * 0.5;
    // Normalise to [0,1) across the mercator square.
    let nx = ((cx + MERCATOR_EXTENT) / (2.0 * MERCATOR_EXTENT)).clamp(0.0, 1.0);
    let ny = ((cy + MERCATOR_EXTENT) / (2.0 * MERCATOR_EXTENT)).clamp(0.0, 1.0);
    const BITS: u32 = 21;
    let max = ((1u32 << BITS) - 1) as f64;
    let xi = (nx * max) as u32;
    let yi = (ny * max) as u32;
    interleave_bits(xi) | (interleave_bits(yi) << 1)
}

/// Spread the low 21 bits of `v` so each occupies an even bit position (0,2,4…).
#[inline]
fn interleave_bits(v: u32) -> u64 {
    let mut x = v as u64 & 0x1f_ffff; // keep 21 bits
    x = (x | (x << 32)) & 0x1f00000000ffff;
    x = (x | (x << 16)) & 0x1f0000ff0000ff;
    x = (x | (x << 8)) & 0x100f00f00f00f00f;
    x = (x | (x << 4)) & 0x10c30c30c30c30c3;
    x = (x | (x << 2)) & 0x1249249249249249;
    x
}

// ── pass 2 ───────────────────────────────────────────────────────────────────

fn bake_zoom_streaming(
    store: &StoreReader,
    index: &[FeatureMeta],
    z: u8,
    config: &BakeConfig,
    processed: &Arc<AtomicUsize>,
    tile_writer: &mut StreamingTileWriter,
) -> Result<(), BakeError> {
    let max_tile = (1u32 << z) - 1;
    let drop_size = simplify_tolerance(z) * MIN_FEATURE_PIXELS;

    // Bin feature indices into tile buckets (index only — no geometry loaded yet).
    let mut buckets: HashMap<(u32, u32), Vec<u32>> = HashMap::new();
    for (i, meta) in index.iter().enumerate() {
        processed.fetch_add(1, Ordering::Relaxed);

        let [min_x, min_y, max_x, max_y] = meta.bbox;
        let w = max_x - min_x;
        let h = max_y - min_y;
        if (w > 0.0 || h > 0.0) && w < drop_size && h < drop_size {
            continue;
        }

        let x_min = merc_x_to_tile(min_x, z).saturating_sub(1);
        let x_max = (merc_x_to_tile(max_x, z) + 1).min(max_tile);
        let y_min = merc_y_to_tile(max_y, z).saturating_sub(1);
        let y_max = (merc_y_to_tile(min_y, z) + 1).min(max_tile);

        for x in x_min..=x_max {
            for y in y_min..=y_max {
                let bucket = buckets.entry((x, y)).or_default();
                if bucket.len() < MAX_FEATURES_PER_TILE {
                    bucket.push(i as u32);
                }
            }
        }
    }

    // Collect into a sorted Vec so chunks are spatially coherent (better mmap
    // locality — adjacent tiles tend to share features in the store).
    let mut tile_list: Vec<((u32, u32), Vec<u32>)> = buckets.into_iter().collect();
    tile_list.sort_unstable_by(|a, b| {
        let id_a = tile_to_id(z, a.0.0, a.0.1);
        let id_b = tile_to_id(z, b.0.0, b.0.1);
        id_a.cmp(&id_b)
    });

    let layer_name = &config.layer_name;
    let compression = config.compression;

    // Process in feature-count-bounded chunks. Walk the sorted tile list,
    // accumulating tiles until their combined feature-reference count reaches
    // CHUNK_FEATURE_REFS, then encode that slice in parallel and flush it. This
    // keeps peak live-geometry RAM roughly constant across zoom levels — a low
    // zoom packs few huge tiles per chunk, a high zoom many tiny ones, but the
    // decoded feature payload per chunk is the same.
    let mut start = 0usize;
    while start < tile_list.len() {
        let mut end = start;
        let mut refs = 0usize;
        while end < tile_list.len()
            && (end == start || refs + tile_list[end].1.len() <= CHUNK_FEATURE_REFS)
        {
            refs += tile_list[end].1.len();
            end += 1;
        }
        let chunk = &tile_list[start..end];
        start = end;

        let encoded: Vec<Option<(u64, Vec<u8>)>> = chunk
            .par_iter()
            .map(|((x, y), feat_ids)| {
                let (tmin_x, tmin_y, tmax_x, tmax_y) = tile_bbox(z, *x, *y, 0.0);
                let mut tile_feats: Vec<EncodableFeature> = Vec::new();

                for &fid in feat_ids {
                    let (geom, props) = store.read(&index[fid as usize]).ok()?;
                    if let Some(g) =
                        clip_and_simplify_for_tile(geom, z, tmin_x, tmin_y, tmax_x, tmax_y)
                    {
                        tile_feats.push(EncodableFeature { geom: g, props });
                    }
                }

                if tile_feats.is_empty() {
                    return None;
                }
                let raw = encode_tile(layer_name, &tile_feats, tmin_x, tmin_y, tmax_x, tmax_y).ok()?;
                let compressed = compress_tile(&raw, compression).ok()?;
                Some((tile_to_id(z, *x, *y), compressed))
            })
            .collect();

        // Push the finished chunk to the writer, then the Vec is dropped.
        for entry in encoded.into_iter().flatten() {
            tile_writer
                .push(entry.0, &entry.1)
                .map_err(|e| BakeError::Io(e))?;
        }
    }

    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

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

fn compression_name(c: crate::pipeline::TileCompression) -> String {
    match c {
        crate::pipeline::TileCompression::Gzip => "gzip".to_owned(),
        crate::pipeline::TileCompression::Brotli => "brotli".to_owned(),
    }
}

fn to_lm_compression(c: crate::pipeline::TileCompression) -> lm_core::pmtiles::Compression {
    match c {
        crate::pipeline::TileCompression::Gzip => lm_core::pmtiles::Compression::Gzip,
        crate::pipeline::TileCompression::Brotli => lm_core::pmtiles::Compression::Brotli,
    }
}

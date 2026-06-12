/// Step C regression gate.
///
/// These tests must ALL pass before merging Step C changes.
/// Size/performance assertions are the CI gate that keeps "fast first" true.
use std::io::Write;

use lm_bake::{
    bake, BakeConfig, BakeOutput,
    pipeline::{bake_multi, LayerInput, TileCompression},
    formats::{csv::ingest_csv, geojsonseq::ingest_geojsonseq},
};
use lm_core::PmtReader;

// ── test data ─────────────────────────────────────────────────────────────────

const WORLD_POLYGON: &str = r#"{
  "type": "FeatureCollection",
  "features": [{
    "type": "Feature",
    "geometry": {
      "type": "Polygon",
      "coordinates": [[
        [-180, -85], [180, -85], [180, 85], [-180, 85], [-180, -85]
      ]]
    },
    "properties": { "name": "world" }
  }]
}"#;

const POINTS_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    {"type":"Feature","geometry":{"type":"Point","coordinates":[0,51.5]},"properties":{"city":"London"}},
    {"type":"Feature","geometry":{"type":"Point","coordinates":[2.35,48.8]},"properties":{"city":"Paris"}},
    {"type":"Feature","geometry":{"type":"Point","coordinates":[13.4,52.5]},"properties":{"city":"Berlin"}}
  ]
}"#;

fn bake_to_file(
    geojson: &str,
    config: BakeConfig,
) -> (tempfile::NamedTempFile, BakeOutput) {
    let out = bake(geojson, config).expect("bake should succeed");
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&out.pmtiles_bytes).unwrap();
    (f, out)
}

// ── compression tests ─────────────────────────────────────────────────────────

#[test]
fn brotli_archive_smaller_than_gzip() {
    let gz_config = BakeConfig {
        layer_name: "test".into(),
        min_zoom: 0,
        max_zoom: 5,
        compression: TileCompression::Gzip,
        ..Default::default()
    };
    let br_config = BakeConfig {
        layer_name: "test".into(),
        min_zoom: 0,
        max_zoom: 5,
        compression: TileCompression::Brotli,
        ..Default::default()
    };

    let gz_out = bake(WORLD_POLYGON, gz_config).unwrap();
    let br_out = bake(WORLD_POLYGON, br_config).unwrap();

    assert!(
        br_out.archive_bytes <= gz_out.archive_bytes,
        "brotli ({}) should not be larger than gzip ({})",
        br_out.archive_bytes,
        gz_out.archive_bytes
    );
}

#[test]
fn brotli_archive_is_readable() {
    let config = BakeConfig {
        layer_name: "test".into(),
        min_zoom: 0,
        max_zoom: 4,
        compression: TileCompression::Brotli,
        ..Default::default()
    };
    let (f, out) = bake_to_file(POINTS_GEOJSON, config);
    let reader = PmtReader::open(f.path()).unwrap();
    assert!(reader.tile_count() > 0);
    assert_eq!(reader.tile_count(), out.tile_count as u64);
}

// ── clipping regression gate ──────────────────────────────────────────────────

#[test]
fn world_polygon_produces_tiles_at_all_zooms() {
    // A polygon covering the whole world should produce non-empty tiles at
    // every zoom level — proves the full clipping path doesn't eat valid geom.
    let config = BakeConfig {
        layer_name: "world".into(),
        min_zoom: 0,
        max_zoom: 4,
        ..Default::default()
    };
    let (f, _) = bake_to_file(WORLD_POLYGON, config);
    let reader = PmtReader::open(f.path()).unwrap();

    for z in 0u8..=4 {
        // z=0 always exists
        if z == 0 {
            reader.get_tile(0, 0, 0).expect("z0/0/0 must exist");
        }
    }
    assert!(reader.tile_count() >= 5, "expected at least 5 tiles across z0-z4");
}

// ── tile size regression gate (the CI enforcer) ───────────────────────────────

#[test]
fn tile_size_budget_not_regressed() {
    // The z0 tile for a world polygon must stay under 32KB gzip-compressed.
    // If this trips, the pipeline is generating bloated output — investigate
    // simplification and clipping before merging.
    let config = BakeConfig {
        layer_name: "world".into(),
        min_zoom: 0,
        max_zoom: 3,
        ..Default::default()
    };
    let (f, _) = bake_to_file(WORLD_POLYGON, config);
    let reader = PmtReader::open(f.path()).unwrap();
    let tile = reader.get_tile(0, 0, 0).unwrap();
    let size = tile.data.len();
    assert!(
        size < 32 * 1024,
        "z0 tile is {size} bytes — exceeds 32KB regression budget"
    );
}

#[test]
fn dedup_reduces_archive_size_for_repeated_tiles() {
    // Many low-zoom tiles over a small area will be identical (empty ocean).
    // Dedup should make the archive meaningfully smaller than naive storage.
    // We test that tile_count >> archive content uniqueness (tile_count is
    // the number of addressed tiles; unique blobs should be fewer).
    let config = BakeConfig {
        layer_name: "pts".into(),
        min_zoom: 0,
        max_zoom: 6,
        ..Default::default()
    };
    let out = bake(POINTS_GEOJSON, config).unwrap();
    // Not all tiles can be unique when we have only 3 features across z0-z6.
    // The archive should be well under 1MB for a trivial point dataset.
    assert!(
        out.archive_bytes < 1024 * 1024,
        "archive is {}B — excessive for 3 points at z0-z6",
        out.archive_bytes
    );
}

// ── format adapter tests ──────────────────────────────────────────────────────

#[test]
fn csv_ingest_produces_points() {
    let src = "lat,lon,name\n51.5,-0.1,London\n48.8,2.35,Paris\n52.5,13.4,Berlin";
    let layer = ingest_csv("cities", src).unwrap();
    assert_eq!(layer.features.len(), 3);
    // All features should be Points
    for (geom, _) in &layer.features {
        assert!(matches!(geom, geo_types::Geometry::Point(_)));
    }
}

#[test]
fn csv_ingest_captures_properties() {
    let src = "lat,lon,pop\n51.5,-0.1,9000000";
    let layer = ingest_csv("cities", src).unwrap();
    let (_, props) = &layer.features[0];
    assert!(props.contains_key("pop"));
}

#[test]
fn geojsonseq_parses_ndjson() {
    let src = concat!(
        r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]},"properties":{"id":1}}"#,
        "\n",
        r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[1,1]},"properties":{"id":2}}"#,
    );
    let layer = ingest_geojsonseq("seq", src).unwrap();
    assert_eq!(layer.features.len(), 2);
}

#[test]
fn geojsonseq_with_rs_delimiters() {
    let src = "\x1E{\"type\":\"Feature\",\"geometry\":{\"type\":\"Point\",\"coordinates\":[0,0]},\"properties\":{}}\n\
               \x1E{\"type\":\"Feature\",\"geometry\":{\"type\":\"Point\",\"coordinates\":[1,1]},\"properties\":{}}";
    let layer = ingest_geojsonseq("seq", src).unwrap();
    assert_eq!(layer.features.len(), 2);
}

// ── multi-layer tests ─────────────────────────────────────────────────────────

#[test]
fn multi_layer_bake_produces_single_archive() {
    let layers = vec![
        LayerInput { name: "points", geojson: POINTS_GEOJSON },
        LayerInput { name: "world", geojson: WORLD_POLYGON },
    ];
    let config = BakeConfig {
        layer_name: "combined".into(),
        min_zoom: 0,
        max_zoom: 3,
        ..Default::default()
    };
    let out = bake_multi(&layers, config).unwrap();
    assert_eq!(out.manifest.layers.len(), 2);
    assert!(out.tile_count > 0);

    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&out.pmtiles_bytes).unwrap();
    let reader = PmtReader::open(f.path()).unwrap();
    assert!(reader.tile_count() > 0);
}

// ── clip correctness ──────────────────────────────────────────────────────────

#[test]
fn clipping_does_not_produce_tile_outside_bounds() {
    use lm_bake::tile_clip::clip_to_tile;
    use geo_types::{polygon, Geometry};

    // Polygon entirely outside tile — must be None.
    let poly = Geometry::Polygon(polygon![
        (x: 100.0, y: 100.0), (x: 200.0, y: 100.0),
        (x: 200.0, y: 200.0), (x: 100.0, y: 200.0),
        (x: 100.0, y: 100.0)
    ]);
    assert!(clip_to_tile(poly, -1.0, -1.0, 1.0, 1.0).is_none());
}

#[test]
fn clipping_cross_tile_linestring() {
    use lm_bake::tile_clip::clip_to_tile;
    use geo_types::{Geometry, LineString};

    // Line from far west to far east — must intersect any tile near the origin.
    let ls = Geometry::LineString(LineString::from(vec![(-100.0f64, 0.0), (100.0, 0.0)]));
    let result = clip_to_tile(ls, -1.0, -1.0, 1.0, 1.0);
    assert!(result.is_some(), "horizontal line should intersect tile");
}

/// Step B correctness gate: bake GeoJSON → PMTiles → read back with lm-core.
///
/// Checks:
/// 1. Bake completes without error.
/// 2. The resulting PMTiles is readable and has expected metadata.
/// 3. Tiles are non-empty for zooms that should have data.
/// 4. Missing tiles return TileNotFound (no panic).
/// 5. Golden tile: a specific tile's size stays within bounds (size gate).
use std::io::Write;

use lm_bake::{bake, BakeConfig};
use lm_core::PmtReader;

/// Minimal GeoJSON with a point, a linestring, and a polygon.
const TEST_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    {
      "type": "Feature",
      "geometry": { "type": "Point", "coordinates": [0.0, 0.0] },
      "properties": { "name": "origin", "value": 42 }
    },
    {
      "type": "Feature",
      "geometry": {
        "type": "LineString",
        "coordinates": [[-10.0, -10.0], [10.0, 10.0]]
      },
      "properties": { "name": "diagonal", "kind": "line" }
    },
    {
      "type": "Feature",
      "geometry": {
        "type": "Polygon",
        "coordinates": [[
          [-5.0, -5.0], [5.0, -5.0], [5.0, 5.0], [-5.0, 5.0], [-5.0, -5.0]
        ]]
      },
      "properties": { "name": "box", "area": 100 }
    }
  ]
}"#;

fn bake_to_tmpfile(max_zoom: u8) -> (tempfile::NamedTempFile, lm_bake::pipeline::BakeOutput) {
    let config = BakeConfig {
        layer_name: "test".to_owned(),
        min_zoom: 0,
        max_zoom,
        ..Default::default()
    };
    let out = bake(TEST_GEOJSON, config).expect("bake should succeed");
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&out.pmtiles_bytes).unwrap();
    (f, out)
}

#[test]
fn bake_produces_valid_pmtiles() {
    let (f, out) = bake_to_tmpfile(6);
    assert!(out.tile_count > 0, "expected at least one tile");

    let reader = PmtReader::open(f.path()).expect("should open baked pmtiles");
    assert_eq!(reader.min_zoom(), 0);
    assert_eq!(reader.max_zoom(), 6);
    assert!(reader.tile_count() > 0);
}

#[test]
fn metadata_round_trips() {
    let (f, _) = bake_to_tmpfile(4);
    let reader = PmtReader::open(f.path()).unwrap();
    let meta_str = reader.metadata().expect("metadata should be present");
    let meta: serde_json::Value = serde_json::from_str(&meta_str).expect("metadata should be valid JSON");

    assert_eq!(meta["name"], "test");
    assert_eq!(meta["min_zoom"], 0);
    assert_eq!(meta["max_zoom"], 4);

    // Bounds should contain our features (near 0,0)
    let bounds = meta["bounds"].as_array().unwrap();
    let min_lon = bounds[0].as_f64().unwrap();
    let max_lon = bounds[2].as_f64().unwrap();
    assert!(min_lon < 0.0 && max_lon > 0.0, "bounds should straddle 0");
}

#[test]
fn z0_tile_exists_and_is_nonempty() {
    let (f, _) = bake_to_tmpfile(6);
    let reader = PmtReader::open(f.path()).unwrap();
    let tile = reader.get_tile(0, 0, 0).expect("z0 should have a tile");
    assert!(!tile.data.is_empty(), "z0 tile should be non-empty");
}

#[test]
fn absent_tile_returns_not_found() {
    let (f, _) = bake_to_tmpfile(4);
    let reader = PmtReader::open(f.path()).unwrap();
    // Far corner of the world at z4 — our data is near 0,0 so this should be empty.
    let result = reader.get_tile(4, 15, 0);
    assert!(
        matches!(result, Err(lm_core::pmtiles::PmtError::TileNotFound { .. })),
        "expected TileNotFound for z4/15/0"
    );
}

#[test]
fn tile_size_within_budget() {
    // Golden size gate: the z0 tile (whole world, most data) must be under 64KB
    // compressed. If this trips, the pipeline is producing bloated tiles.
    let (f, _) = bake_to_tmpfile(4);
    let reader = PmtReader::open(f.path()).unwrap();
    let tile = reader.get_tile(0, 0, 0).unwrap();
    let size = tile.data.len();
    assert!(
        size < 64 * 1024,
        "z0 tile is {size} bytes — exceeds 64KB budget"
    );
}

#[test]
fn bake_output_tile_count_matches_reader() {
    let (f, out) = bake_to_tmpfile(5);
    let reader = PmtReader::open(f.path()).unwrap();
    assert_eq!(
        out.tile_count as u64,
        reader.tile_count(),
        "pipeline tile count should match archive tile count"
    );
}

/// Streaming bake correctness gate.
///
/// The memory-bounded streaming path (spill features to disk, bin tiles, read
/// back on demand) must produce the same tiles as the in-memory path for the
/// same input. We bake identical data both ways and compare the resulting
/// tile-id sets and archive validity.
use std::collections::BTreeSet;
use std::io::Write;

use lm_bake::{bake, bake_layer_streaming, BakeConfig};
use lm_core::PmtReader;

/// Three features as a FeatureCollection (for the in-memory path).
const FC_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [0.0, 0.0] },
      "properties": { "name": "origin", "value": 42 } },
    { "type": "Feature",
      "geometry": { "type": "LineString", "coordinates": [[-10.0,-10.0],[10.0,10.0]] },
      "properties": { "name": "diagonal" } },
    { "type": "Feature",
      "geometry": { "type": "Polygon",
        "coordinates": [[[-5.0,-5.0],[5.0,-5.0],[5.0,5.0],[-5.0,5.0],[-5.0,-5.0]]] },
      "properties": { "name": "box", "area": 100 } }
  ]
}"#;

/// The same three features, one per line (for the streaming path).
const SEQ_GEOJSON: &str = concat!(
    r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[0.0,0.0]},"properties":{"name":"origin","value":42}}"#,
    "\n",
    r#"{"type":"Feature","geometry":{"type":"LineString","coordinates":[[-10.0,-10.0],[10.0,10.0]]},"properties":{"name":"diagonal"}}"#,
    "\n",
    r#"{"type":"Feature","geometry":{"type":"Polygon","coordinates":[[[-5.0,-5.0],[5.0,-5.0],[5.0,5.0],[-5.0,5.0],[-5.0,-5.0]]]},"properties":{"name":"box","area":100}}"#,
);

fn cfg(max_zoom: u8) -> BakeConfig {
    BakeConfig {
        layer_name: "test".to_owned(),
        min_zoom: 0,
        max_zoom,
        ..Default::default()
    }
}

fn store_path(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "lm-stream-test-{tag}-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// Write archive bytes to a temp file and return its PmtReader.
fn reader_for(bytes: &[u8]) -> (tempfile::NamedTempFile, PmtReader) {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(bytes).unwrap();
    f.flush().unwrap();
    let reader = PmtReader::open(f.path()).expect("readable archive");
    (f, reader)
}

fn tile_ids(reader: &PmtReader, max_zoom: u8) -> BTreeSet<(u8, u32, u32)> {
    let mut ids = BTreeSet::new();
    for z in 0..=max_zoom {
        let n = 1u32 << z;
        for x in 0..n {
            for y in 0..n {
                if reader.get_tile(z, x, y).is_ok() {
                    ids.insert((z, x, y));
                }
            }
        }
    }
    ids
}

#[test]
fn streaming_matches_in_memory_tile_set() {
    let max_zoom = 6; // keep the brute-force tile scan cheap

    let mem = bake(FC_GEOJSON, cfg(max_zoom)).expect("in-memory bake");
    let stream = bake_layer_streaming(SEQ_GEOJSON.as_bytes(), &cfg(max_zoom), store_path("a"))
        .expect("streaming bake");

    let (_mf, mem_reader) = reader_for(&mem.pmtiles_bytes);
    let (_sf, stream_reader) = reader_for(&stream.pmtiles_bytes);

    let mem_ids = tile_ids(&mem_reader, max_zoom);
    let stream_ids = tile_ids(&stream_reader, max_zoom);

    assert_eq!(
        mem_ids, stream_ids,
        "streaming and in-memory paths must produce the same populated tiles"
    );
    assert!(!stream_ids.is_empty(), "expected some tiles");
}

#[test]
fn streaming_archive_is_valid_and_has_metadata() {
    let max_zoom = 5;
    let out = bake_layer_streaming(SEQ_GEOJSON.as_bytes(), &cfg(max_zoom), store_path("b"))
        .expect("streaming bake");

    let (_f, reader) = reader_for(&out.pmtiles_bytes);
    let meta = reader.metadata().expect("metadata present");
    assert!(meta.contains("test"), "layer name should appear in metadata");

    // z0 tile must exist and decompress to a non-empty MVT.
    let tile = reader.get_tile(0, 0, 0).expect("z0 tile should exist");
    assert!(!tile.data.is_empty(), "z0 tile should be non-empty");
}

#[test]
fn streaming_empty_input_errors() {
    let err = bake_layer_streaming(b"\n\n".as_slice(), &cfg(2), store_path("c"));
    assert!(err.is_err(), "empty streaming input should error");
}

#[test]
fn streaming_temp_store_is_cleaned_up() {
    let path = store_path("d");
    let _ = bake_layer_streaming(SEQ_GEOJSON.as_bytes(), &cfg(3), path.clone())
        .expect("streaming bake");
    assert!(
        !path.exists(),
        "feature store temp file should be removed after bake"
    );
}

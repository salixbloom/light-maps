/// Streaming bake correctness gate.
use std::collections::BTreeSet;
use std::io::Write;

use lm_bake::{bake, bake_layer_streaming, BakeConfig};
use lm_core::PmtReader;

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

const SEQ_GEOJSON: &str = concat!(
    "{\"type\":\"Feature\",\"geometry\":{\"type\":\"Point\",\"coordinates\":[0.0,0.0]},\"properties\":{\"name\":\"origin\",\"value\":42}}",
    "\n",
    "{\"type\":\"Feature\",\"geometry\":{\"type\":\"LineString\",\"coordinates\":[[-10.0,-10.0],[10.0,10.0]]},\"properties\":{\"name\":\"diagonal\"}}",
    "\n",
    "{\"type\":\"Feature\",\"geometry\":{\"type\":\"Polygon\",\"coordinates\":[[[-5.0,-5.0],[5.0,-5.0],[5.0,5.0],[-5.0,5.0],[-5.0,-5.0]]]},\"properties\":{\"name\":\"box\",\"area\":100}}",
);

fn cfg(max_zoom: u8) -> BakeConfig {
    BakeConfig { layer_name: "test".to_owned(), min_zoom: 0, max_zoom, ..Default::default() }
}

fn tmp_path(tag: &str, kind: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "lm-stream-test-{tag}-{kind}-{}-{}.bin",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn stream_to_vec(tag: &str, max_zoom: u8) -> Vec<u8> {
    let mut buf = Vec::new();
    bake_layer_streaming(
        SEQ_GEOJSON.as_bytes(),
        &cfg(max_zoom),
        tmp_path(tag, "store"),
        tmp_path(tag, "tiles"),
        &mut buf,
    )
    .expect("streaming bake");
    buf
}

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
    let max_zoom = 6;
    let mem = bake(FC_GEOJSON, cfg(max_zoom)).expect("in-memory bake");
    let stream_bytes = stream_to_vec("a", max_zoom);
    let (_mf, mr) = reader_for(&mem.pmtiles_bytes);
    let (_sf, sr) = reader_for(&stream_bytes);
    assert_eq!(tile_ids(&mr, max_zoom), tile_ids(&sr, max_zoom),
        "streaming and in-memory paths must produce the same populated tiles");
    assert_eq!(mr.tile_count(), sr.tile_count());
}

#[test]
fn streaming_archive_is_valid_and_has_metadata() {
    let bytes = stream_to_vec("b", 5);
    let (_f, reader) = reader_for(&bytes);
    let meta = reader.metadata().expect("metadata present");
    assert!(meta.contains("test"), "layer name should appear in metadata");
    let tile = reader.get_tile(0, 0, 0).expect("z0 tile should exist");
    assert!(!tile.data.is_empty(), "z0 tile should be non-empty");
}

#[test]
fn streaming_empty_input_errors() {
    let mut buf = Vec::new();
    let err = bake_layer_streaming(
        b"\n\n".as_slice(), &cfg(2),
        tmp_path("c", "store"), tmp_path("c", "tiles"), &mut buf,
    );
    assert!(err.is_err(), "empty streaming input should error");
}

#[test]
fn streaming_temp_store_is_cleaned_up() {
    let store = tmp_path("d", "store");
    let tiles = tmp_path("d", "tiles");
    let mut buf = Vec::new();
    bake_layer_streaming(SEQ_GEOJSON.as_bytes(), &cfg(3), store.clone(), tiles.clone(), &mut buf)
        .expect("streaming bake");
    assert!(!store.exists(), "feature store temp file should be removed");
    assert!(!tiles.exists(), "tile temp file should be removed");
}

use lm_core::{fixture::build_fixture, PmtReader};

fn make_test_archive() -> (tempfile::NamedTempFile, Vec<(u8, u32, u32, Vec<u8>)>) {
    let tiles = vec![
        (0u8, 0u32, 0u32, b"tile-z0-x0-y0".to_vec()),
        (1, 0, 0, b"tile-z1-x0-y0".to_vec()),
        (1, 0, 1, b"tile-z1-x0-y1".to_vec()),
        (1, 1, 0, b"tile-z1-x1-y0".to_vec()),
        (1, 1, 1, b"tile-z1-x1-y1".to_vec()),
        (2, 0, 0, b"tile-z2-x0-y0".to_vec()),
        (2, 2, 2, b"tile-z2-x2-y2".to_vec()),
    ];
    let data = build_fixture(&tiles);
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, &data).unwrap();
    (f, tiles)
}

#[test]
fn open_and_basic_metadata() {
    let (f, _) = make_test_archive();
    let r = PmtReader::open(f.path()).unwrap();
    assert_eq!(r.min_zoom(), 0);
    assert_eq!(r.max_zoom(), 2);
    assert_eq!(r.tile_count(), 7);
}

#[test]
fn fetch_present_tiles() {
    let (f, tiles) = make_test_archive();
    let r = PmtReader::open(f.path()).unwrap();

    for (z, x, y, expected) in &tiles {
        let td = r.get_tile(*z, *x, *y).expect(&format!("z{z}/{x}/{y} should exist"));
        assert_eq!(td.data.as_ref(), expected.as_slice(), "z{z}/{x}/{y} data mismatch");
    }
}

#[test]
fn missing_tile_returns_not_found() {
    let (f, _) = make_test_archive();
    let r = PmtReader::open(f.path()).unwrap();
    assert!(
        matches!(
            r.get_tile(2, 3, 3),
            Err(lm_core::pmtiles::PmtError::TileNotFound { .. })
        ),
        "expected TileNotFound for z2/3/3"
    );
}

#[test]
fn metadata_is_valid_json() {
    let (f, _) = make_test_archive();
    let r = PmtReader::open(f.path()).unwrap();
    let meta = r.metadata().unwrap();
    serde_json::from_str::<serde_json::Value>(&meta).unwrap();
}

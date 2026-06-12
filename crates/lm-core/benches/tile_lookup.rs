/// Criterion benchmark: hot-path tile lookup from an in-memory fixture.
///
/// Run with:   cargo bench -p lm-core
/// For a real file: set LM_BENCH_FILE=/path/to/real.pmtiles before running.
use std::io::Write;

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use lm_core::{fixture::build_fixture, PmtReader};

fn make_fixture_file() -> tempfile::NamedTempFile {
    // A small grid of tiles across z0–z2.
    let mut tiles = vec![(0u8, 0u32, 0u32, vec![0u8; 64])];
    for x in 0..2u32 {
        for y in 0..2u32 {
            tiles.push((1, x, y, vec![0u8; 128]));
        }
    }
    for x in 0..4u32 {
        for y in 0..4u32 {
            tiles.push((2, x, y, vec![0u8; 256]));
        }
    }
    let data = build_fixture(&tiles);
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(&data).unwrap();
    f
}

fn bench_tile_lookup(c: &mut Criterion) {
    // Prefer an external real file if provided; fall back to fixture.
    let _fixture_file; // keep alive for the lifetime of the bench
    let path: String = match std::env::var("LM_BENCH_FILE") {
        Ok(p) => p,
        Err(_) => {
            _fixture_file = make_fixture_file();
            _fixture_file.path().to_str().unwrap().to_owned()
        }
    };

    let reader = PmtReader::open(&path).expect("open pmtiles");
    let zoom = reader.min_zoom().max(1);
    let mid = (1u32 << (zoom - 1)).saturating_sub(1);

    let mut group = c.benchmark_group("tile_lookup");

    // Hit: tile present — this is the production hot path.
    group.bench_with_input(
        BenchmarkId::new("get_tile_hit", format!("z{zoom}/{mid}/{mid}")),
        &(zoom, mid, mid),
        |b, &(z, x, y)| {
            b.iter(|| {
                let _ = black_box(reader.get_tile(z, x, y));
            });
        },
    );

    // Miss: tile absent — exercises the not-found binary-search path.
    group.bench_function("get_tile_miss", |b| {
        b.iter(|| {
            let _ = black_box(reader.get_tile(30, 0, 0));
        });
    });

    group.finish();
}

criterion_group!(benches, bench_tile_lookup);
criterion_main!(benches);

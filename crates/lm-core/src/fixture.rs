/// Build a minimal but fully valid PMTiles v3 archive in memory for testing.
///
/// Writes N tiles of synthetic MVT data so we can test the reader and bench
/// the hot path without needing an external file.
pub fn build_fixture(tiles: &[(u8, u32, u32, Vec<u8>)]) -> Vec<u8> {
    // --- encode one directory entry per tile (no runs, no leaves) ----
    // Directory format (all varints):
    //   num_entries
    //   tile_id deltas...
    //   run_lengths...  (1 = single tile)
    //   lengths...
    //   offsets...      (absolute for first, 0-delta after)

    let ids: Vec<u64> = tiles
        .iter()
        .map(|&(z, x, y, _)| crate::tile_id::tile_to_id(z, x, y))
        .collect();
    // Sort by tile ID (required by spec).
    let mut order: Vec<usize> = (0..tiles.len()).collect();
    order.sort_by_key(|&i| ids[i]);
    let sorted: Vec<(u64, &Vec<u8>)> =
        order.iter().map(|&i| (ids[i], &tiles[i].3)).collect();

    // Compute tile data layout first so we know offsets.
    let mut tile_data: Vec<u8> = Vec::new();
    let mut offsets: Vec<u64> = Vec::new();
    let mut lengths: Vec<u32> = Vec::new();
    for (_, data) in &sorted {
        offsets.push(tile_data.len() as u64);
        lengths.push(data.len() as u32);
        tile_data.extend_from_slice(data);
    }

    // Encode directory.
    let mut dir: Vec<u8> = Vec::new();
    write_varint(&mut dir, sorted.len() as u64);
    // tile_id deltas
    let mut last = 0u64;
    for &(id, _) in &sorted {
        write_varint(&mut dir, id - last);
        last = id;
    }
    // run_lengths (all 1)
    for _ in &sorted {
        write_varint(&mut dir, 1);
    }
    // lengths
    for &l in &lengths {
        write_varint(&mut dir, l as u64);
    }
    // offsets (absolute for first, then 0-delta means "follow prev")
    for (i, &off) in offsets.iter().enumerate() {
        if i == 0 {
            write_varint(&mut dir, off);
        } else {
            // Check if contiguous with previous entry.
            let prev_end = offsets[i - 1] + lengths[i - 1] as u64;
            if off == prev_end {
                write_varint(&mut dir, 0); // delta=0 means "contiguous"
            } else {
                write_varint(&mut dir, off);
            }
        }
    }

    // Gzip the directory (internal_compression = Gzip = 2).
    let dir_gz = gzip_compress(&dir);

    // Empty metadata.
    let meta_gz = gzip_compress(b"{}");

    // Build header.
    let root_dir_offset: u64 = 127;
    let root_dir_len: u64 = dir_gz.len() as u64;
    let metadata_offset: u64 = root_dir_offset + root_dir_len;
    let metadata_len: u64 = meta_gz.len() as u64;
    let leaf_dirs_offset: u64 = metadata_offset + metadata_len;
    let leaf_dirs_len: u64 = 0;
    let tile_data_offset: u64 = leaf_dirs_offset + leaf_dirs_len;
    let tile_data_len: u64 = tile_data.len() as u64;

    let mut header = [0u8; 127];
    header[0..7].copy_from_slice(b"PMTiles");
    header[7] = 3; // version
    write_u64_le(&mut header, 8, root_dir_offset);
    write_u64_le(&mut header, 16, root_dir_len);
    write_u64_le(&mut header, 24, metadata_offset);
    write_u64_le(&mut header, 32, metadata_len);
    write_u64_le(&mut header, 40, leaf_dirs_offset);
    write_u64_le(&mut header, 48, leaf_dirs_len);
    write_u64_le(&mut header, 56, tile_data_offset);
    write_u64_le(&mut header, 64, tile_data_len);
    write_u64_le(&mut header, 72, sorted.len() as u64); // addressed_tiles
    write_u64_le(&mut header, 80, sorted.len() as u64); // tile_entries
    write_u64_le(&mut header, 88, sorted.len() as u64); // tile_contents
    header[96] = 1; // clustered = true
    header[97] = 2; // internal_compression = gzip
    header[98] = 1; // tile_compression = none (raw bytes in fixture)
    header[99] = 1; // tile_type = MVT
    header[100] = 0; // min_zoom
    header[101] = 2; // max_zoom
    // bounds / center left as zero

    let mut out = Vec::with_capacity(127 + dir_gz.len() + meta_gz.len() + tile_data.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&dir_gz);
    out.extend_from_slice(&meta_gz);
    out.extend_from_slice(&tile_data);
    out
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}

fn write_u64_le(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn gzip_compress(data: &[u8]) -> Vec<u8> {
    use flate2::{write::GzEncoder, Compression};
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    std::io::Write::write_all(&mut enc, data).unwrap();
    enc.finish().unwrap()
}

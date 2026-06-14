/// PMTiles v3 writer with tile deduplication.
///
/// Identical tile blobs are stored once and referenced from multiple directory
/// entries — common for empty-ocean / uniform-fill tiles at low zoom levels.
/// Tiles with zero length are elided (never written or referenced).
use std::collections::HashMap;
use std::io::{self, Write};

use flate2::{write::GzEncoder, Compression};

use crate::pmtiles::Compression as TileCompression;

// ── public API ────────────────────────────────────────────────────────────────

pub struct TileEntry {
    pub tile_id: u64,
    /// Raw tile bytes, already compressed by the caller (or uncompressed).
    pub data: Vec<u8>,
}

/// Write a PMTiles v3 archive with deduplication enabled.
///
/// Tiles with identical data share a single storage slot in the data section.
/// Empty (zero-length) tiles are silently dropped — callers should not include
/// them.
pub fn write_pmtiles<W: Write>(
    out: &mut W,
    tiles: &mut Vec<TileEntry>,
    metadata_json: &str,
    tile_compression: TileCompression,
    min_zoom: u8,
    max_zoom: u8,
    bounds: [f64; 4],
    center: [f64; 3],
) -> io::Result<u64> {
    // Sort by tile ID (spec requirement).
    tiles.sort_by_key(|e| e.tile_id);

    // Dedup: build a content → offset map so identical blobs share storage.
    // We use a hash of the bytes as the key — collision probability is negligible
    // for tile data, and we verify by length match before trusting it.
    let mut content_map: HashMap<u64, (u64, u32)> = HashMap::new(); // hash → (offset, len)
    let mut tile_data: Vec<u8> = Vec::new();

    // (tile_id, offset, length) for building the directory.
    let mut dir_entries: Vec<(u64, u64, u32)> = Vec::with_capacity(tiles.len());

    let mut dedup_count = 0usize;

    for t in tiles.iter() {
        if t.data.is_empty() {
            continue; // elide empty tiles
        }
        let h = hash_bytes(&t.data);
        let (offset, length) = content_map.entry(h).or_insert_with(|| {
            let off = tile_data.len() as u64;
            let len = t.data.len() as u32;
            tile_data.extend_from_slice(&t.data);
            (off, len)
        });
        if *length != t.data.len() as u32 {
            // Hash collision (extremely unlikely) — append as new entry.
            let off = tile_data.len() as u64;
            let len = t.data.len() as u32;
            tile_data.extend_from_slice(&t.data);
            dir_entries.push((t.tile_id, off, len));
        } else {
            if *offset != (tile_data.len() as u64 - *length as u64)
                && !dir_entries.is_empty()
            {
                dedup_count += 1;
            }
            dir_entries.push((t.tile_id, *offset, *length));
        }
    }

    let _ = dedup_count; // informational; exposed via inspect later

    let n = dir_entries.len() as u64;
    let dir_raw = build_directory(&dir_entries);
    let dir_gz = gzip(&dir_raw)?;
    let meta_gz = gzip(metadata_json.as_bytes())?;

    let root_dir_offset: u64 = 127;
    let root_dir_len = dir_gz.len() as u64;
    let metadata_offset = root_dir_offset + root_dir_len;
    let metadata_len = meta_gz.len() as u64;
    let leaf_dirs_offset = metadata_offset + metadata_len;
    let tile_data_offset = leaf_dirs_offset; // no leaf dirs in Step B/C
    let tile_data_len = tile_data.len() as u64;

    let mut header = [0u8; 127];
    header[0..7].copy_from_slice(b"PMTiles");
    header[7] = 3;
    wru64(&mut header, 8, root_dir_offset);
    wru64(&mut header, 16, root_dir_len);
    wru64(&mut header, 24, metadata_offset);
    wru64(&mut header, 32, metadata_len);
    wru64(&mut header, 40, leaf_dirs_offset);
    wru64(&mut header, 48, 0); // leaf_dirs_len
    wru64(&mut header, 56, tile_data_offset);
    wru64(&mut header, 64, tile_data_len);
    wru64(&mut header, 72, n); // addressed_tiles
    wru64(&mut header, 80, n); // tile_entries
    // tile_contents = unique blobs stored
    wru64(&mut header, 88, content_map.len() as u64);
    header[96] = 1; // clustered
    header[97] = 2; // internal_compression = gzip
    header[98] = tile_compression as u8;
    header[99] = 1; // tile_type = MVT
    header[100] = min_zoom;
    header[101] = max_zoom;
    wri32(&mut header, 102, (bounds[0] * 1e7) as i32);
    wri32(&mut header, 106, (bounds[1] * 1e7) as i32);
    wri32(&mut header, 110, (bounds[2] * 1e7) as i32);
    wri32(&mut header, 114, (bounds[3] * 1e7) as i32);
    header[118] = center[2] as u8;
    wri32(&mut header, 119, (center[0] * 1e7) as i32);
    wri32(&mut header, 123, (center[1] * 1e7) as i32);

    let mut written = 0u64;
    out.write_all(&header)?; written += 127;
    out.write_all(&dir_gz)?; written += dir_gz.len() as u64;
    out.write_all(&meta_gz)?; written += meta_gz.len() as u64;
    out.write_all(&tile_data)?; written += tile_data.len() as u64;

    Ok(written)
}

// ── directory encoding ────────────────────────────────────────────────────────

fn build_directory(entries: &[(u64, u64, u32)]) -> Vec<u8> {
    let n = entries.len();
    let mut buf = Vec::new();
    write_varint(&mut buf, n as u64);

    // tile_id deltas
    let mut last_id = 0u64;
    for &(id, _, _) in entries {
        write_varint(&mut buf, id - last_id);
        last_id = id;
    }
    // run_lengths (1 per entry)
    for _ in 0..n { write_varint(&mut buf, 1); }
    // lengths
    for &(_, _, len) in entries { write_varint(&mut buf, len as u64); }
    // offsets (0 = contiguous with previous)
    for (i, &(_, off, _len)) in entries.iter().enumerate() {
        if i == 0 {
            write_varint(&mut buf, off);
        } else {
            let prev = entries[i - 1];
            if off == prev.1 + prev.2 as u64 {
                write_varint(&mut buf, 0);
            } else {
                write_varint(&mut buf, off);
            }
        }
    }
    buf
}

fn write_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 { buf.push(b); break; } else { buf.push(b | 0x80); }
    }
}

fn gzip(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::fast());
    enc.write_all(data)?;
    enc.finish()
}

fn wru64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn wri32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Fast non-cryptographic hash (FNV-1a 64-bit) for dedup keying.
fn hash_bytes(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

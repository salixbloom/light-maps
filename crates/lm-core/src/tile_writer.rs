/// Streaming PMTiles writer — tiles are appended to a temp file as they arrive;
/// the final archive is assembled in one pass at the end.
///
/// This eliminates the `Vec<TileEntry>` buffer that previously held ~1 GB of
/// compressed tile data in RAM for large datasets. Instead only the directory
/// (~20 bytes per tile) is kept in memory.
///
/// Usage:
///   let mut w = StreamingTileWriter::new(temp_path)?;
///   w.push(tile_id, compressed_bytes)?;   // call from rayon, behind a Mutex
///   let bytes_written = w.finish(out, metadata, compression, zoom, bounds, center)?;
use std::collections::HashMap;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use flate2::{write::GzEncoder, Compression};

use crate::pmtiles::Compression as TileCompression;

/// Directory entry kept in RAM: tile id, byte offset within tile-data section,
/// compressed byte length.
#[derive(Clone)]
struct DirEntry {
    tile_id: u64,
    offset: u64,
    len: u32,
}

/// Append-only tile sink that spills compressed tile blobs to a temp file.
/// Wrap in `Arc<Mutex<_>>` to push from rayon threads.
pub struct StreamingTileWriter {
    path: PathBuf,
    file: BufWriter<std::fs::File>,
    pos: u64,
    dir: Vec<DirEntry>,
    /// FNV hash → (offset, len) for dedup.
    dedup: HashMap<u64, (u64, u32)>,
}

impl StreamingTileWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        let file = std::fs::File::create(&path)?;
        Ok(Self {
            path,
            file: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
            dir: Vec::new(),
            dedup: HashMap::new(),
        })
    }

    /// Push one compressed tile. Deduplicates identical blobs (same compressed
    /// bytes → same storage slot). Safe to call from a single writer thread;
    /// callers that produce tiles in parallel should collect a batch then push.
    pub fn push(&mut self, tile_id: u64, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        let h = fnv1a(data);
        let (offset, len) = if let Some(&(off, l)) = self.dedup.get(&h) {
            // Verify not a hash collision (lengths must match).
            if l == data.len() as u32 {
                (off, l)
            } else {
                // Collision — store as new entry.
                let off = self.pos;
                let l = data.len() as u32;
                self.file.write_all(data)?;
                self.pos += l as u64;
                self.dedup.insert(h, (off, l));
                (off, l)
            }
        } else {
            let off = self.pos;
            let l = data.len() as u32;
            self.file.write_all(data)?;
            self.pos += l as u64;
            self.dedup.insert(h, (off, l));
            (off, l)
        };
        self.dir.push(DirEntry { tile_id, offset, len });
        Ok(())
    }

    /// Finalise: sort the directory, then write the complete PMTiles v3 archive
    /// into `out`. The temp file is deleted on return (success or error).
    pub fn finish<W: Write>(
        mut self,
        out: &mut W,
        metadata_json: &str,
        tile_compression: TileCompression,
        min_zoom: u8,
        max_zoom: u8,
        bounds: [f64; 4],
        center: [f64; 3],
    ) -> io::Result<u64> {
        // Flush tile data to disk.
        self.file.flush()?;
        let tile_data_len = self.pos;
        let n_contents = self.dedup.len() as u64;

        // Sort directory by tile_id (PMTiles spec requirement).
        self.dir.sort_unstable_by_key(|e| e.tile_id);
        let n = self.dir.len() as u64;

        let dir_raw = build_directory(&self.dir);
        let dir_gz = gzip(&dir_raw)?;
        let meta_gz = gzip(metadata_json.as_bytes())?;

        // PMTiles v3 layout (127-byte header, then data sections).
        let root_dir_offset: u64 = 127;
        let root_dir_len = dir_gz.len() as u64;
        let metadata_offset = root_dir_offset + root_dir_len;
        let metadata_len = meta_gz.len() as u64;
        let leaf_dirs_offset = metadata_offset + metadata_len;
        let tile_data_offset = leaf_dirs_offset;

        let mut header = [0u8; 127];
        header[0..7].copy_from_slice(b"PMTiles");
        header[7] = 3;
        wru64(&mut header, 8,  root_dir_offset);
        wru64(&mut header, 16, root_dir_len);
        wru64(&mut header, 24, metadata_offset);
        wru64(&mut header, 32, metadata_len);
        wru64(&mut header, 40, leaf_dirs_offset);
        wru64(&mut header, 48, 0); // no leaf dirs
        wru64(&mut header, 56, tile_data_offset);
        wru64(&mut header, 64, tile_data_len);
        wru64(&mut header, 72, n);           // n_addressed_tiles
        wru64(&mut header, 80, n);           // n_tile_entries
        wru64(&mut header, 88, n_contents);  // n_tile_contents (unique)
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
        out.write_all(&header)?;  written += 127;
        out.write_all(&dir_gz)?;  written += dir_gz.len() as u64;
        out.write_all(&meta_gz)?; written += meta_gz.len() as u64;

        // Copy tile data from the temp file into `out`.
        {
            let mut tile_file = std::fs::File::open(&self.path)?;
            tile_file.seek(SeekFrom::Start(0))?;
            let copied = io::copy(&mut tile_file, out)?;
            written += copied;
        }

        let _ = std::fs::remove_file(&self.path);
        Ok(written)
    }

    /// Number of tiles pushed so far (including duplicates).
    pub fn tile_count(&self) -> usize {
        self.dir.len()
    }
}

/// Thread-safe wrapper so rayon workers can push tiles concurrently.
/// Collect a sorted batch outside the lock, then call `push_batch`.
pub struct SharedTileWriter(pub Mutex<StreamingTileWriter>);

impl SharedTileWriter {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        Ok(Self(Mutex::new(StreamingTileWriter::new(path)?)))
    }

    /// Push a sorted batch of (tile_id, data) pairs under one lock acquisition.
    pub fn push_batch(&self, batch: Vec<(u64, Vec<u8>)>) -> io::Result<()> {
        let mut w = self.0.lock().unwrap();
        for (id, data) in batch {
            w.push(id, &data)?;
        }
        Ok(())
    }

    pub fn into_inner(self) -> StreamingTileWriter {
        self.0.into_inner().unwrap()
    }
}

// ── PMTiles directory encoder ────────────────────────────────────────────────

fn build_directory(entries: &[DirEntry]) -> Vec<u8> {
    let n = entries.len();
    let mut buf = Vec::new();
    write_varint(&mut buf, n as u64);

    let mut last_id = 0u64;
    for e in entries {
        write_varint(&mut buf, e.tile_id - last_id);
        last_id = e.tile_id;
    }
    for _ in 0..n { write_varint(&mut buf, 1); } // run_length = 1
    for e in entries { write_varint(&mut buf, e.len as u64); }
    for (i, e) in entries.iter().enumerate() {
        if i == 0 {
            write_varint(&mut buf, e.offset);
        } else {
            let prev = &entries[i - 1];
            if e.offset == prev.offset + prev.len as u64 {
                write_varint(&mut buf, 0); // contiguous
            } else {
                write_varint(&mut buf, e.offset);
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

fn fnv1a(data: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

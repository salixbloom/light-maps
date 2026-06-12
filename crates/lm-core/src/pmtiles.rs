/// PMTiles v3 reader — mmap-backed, zero-copy tile serving.
///
/// Layout (spec v3):
///   [0..127]   fixed header
///   [header.root_dir_offset .. +root_dir_len]   root directory (gzip-compressed)
///   [header.metadata_offset .. +metadata_len]   JSON metadata (gzip-compressed)
///   [header.leaf_dirs_offset .. +leaf_dirs_len]  leaf directories (gzip-compressed)
///   [header.tile_data_offset .. +tile_data_len]  raw tile bytes
///
/// The directory is a list of `Entry` values.  Each entry covers a run of
/// consecutive tile IDs and points to either a tile or a leaf directory.
use std::{
    fs::File,
    io::{Cursor, Read},
    path::Path,
};

use bytes::Bytes;
use thiserror::Error;

// ── public re-export ─────────────────────────────────────────────────────────

/// Tile bytes as returned by the reader.  Wraps the mmap slice in a `Bytes`
/// so callers get a cheap, refcounted handle with no extra allocation.
pub struct TileData {
    /// Raw (possibly pre-compressed) tile bytes.
    pub data: Bytes,
    /// The compression stored in the archive for this tile.
    pub compression: Compression,
}

// ── error type ───────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum PmtError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid magic bytes — not a PMTiles file")]
    BadMagic,
    #[error("unsupported PMTiles spec version {0} (only v3 supported)")]
    UnsupportedVersion(u8),
    #[error("tile not found: z={z} x={x} y={y}")]
    TileNotFound { z: u8, x: u32, y: u32 },
    #[error("corrupt directory entry")]
    CorruptDirectory,
    #[error("decompression failed: {0}")]
    Decompress(String),
}

// ── header ────────────────────────────────────────────────────────────────────

const MAGIC: &[u8] = b"PMTiles";
const HEADER_LEN: usize = 127;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum Compression {
    Unknown = 0,
    None = 1,
    Gzip = 2,
    Brotli = 3,
    Zstd = 4,
}

impl Compression {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::None,
            2 => Self::Gzip,
            3 => Self::Brotli,
            4 => Self::Zstd,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
struct Header {
    root_dir_offset: u64,
    root_dir_len: u64,
    metadata_offset: u64,
    metadata_len: u64,
    leaf_dirs_offset: u64,
    #[allow(dead_code)]
    leaf_dirs_len: u64,
    tile_data_offset: u64,
    #[allow(dead_code)]
    tile_data_len: u64,
    #[allow(dead_code)]
    addressed_tiles: u64,
    tile_entries: u64,
    #[allow(dead_code)]
    tile_contents: u64,
    #[allow(dead_code)]
    clustered: bool,
    internal_compression: Compression,
    tile_compression: Compression,
    _tile_type: u8,
    min_zoom: u8,
    max_zoom: u8,
    _min_lon_e7: i32,
    _min_lat_e7: i32,
    _max_lon_e7: i32,
    _max_lat_e7: i32,
    _center_zoom: u8,
    _center_lon_e7: i32,
    _center_lat_e7: i32,
}

fn read_u8(buf: &[u8], off: usize) -> u8 {
    buf[off]
}
fn read_u32_le(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}
fn read_u64_le(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().unwrap())
}

impl Header {
    fn parse(buf: &[u8]) -> Result<Self, PmtError> {
        if buf.len() < HEADER_LEN {
            return Err(PmtError::BadMagic);
        }
        if &buf[0..7] != MAGIC {
            return Err(PmtError::BadMagic);
        }
        let version = read_u8(buf, 7);
        if version != 3 {
            return Err(PmtError::UnsupportedVersion(version));
        }
        Ok(Header {
            root_dir_offset: read_u64_le(buf, 8),
            root_dir_len: read_u64_le(buf, 16),
            metadata_offset: read_u64_le(buf, 24),
            metadata_len: read_u64_le(buf, 32),
            leaf_dirs_offset: read_u64_le(buf, 40),
            leaf_dirs_len: read_u64_le(buf, 48),
            tile_data_offset: read_u64_le(buf, 56),
            tile_data_len: read_u64_le(buf, 64),
            addressed_tiles: read_u64_le(buf, 72),
            tile_entries: read_u64_le(buf, 80),
            tile_contents: read_u64_le(buf, 88),
            clustered: read_u8(buf, 96) == 1,
            internal_compression: Compression::from_u8(read_u8(buf, 97)),
            tile_compression: Compression::from_u8(read_u8(buf, 98)),
            _tile_type: read_u8(buf, 99),
            min_zoom: read_u8(buf, 100),
            max_zoom: read_u8(buf, 101),
            _min_lon_e7: read_u32_le(buf, 102) as i32,
            _min_lat_e7: read_u32_le(buf, 106) as i32,
            _max_lon_e7: read_u32_le(buf, 110) as i32,
            _max_lat_e7: read_u32_le(buf, 114) as i32,
            _center_zoom: read_u8(buf, 118),
            _center_lon_e7: read_u32_le(buf, 119) as i32,
            _center_lat_e7: read_u32_le(buf, 123) as i32,
        })
    }
}

// ── directory entries ─────────────────────────────────────────────────────────

/// A single decoded directory entry.
#[derive(Debug, Clone)]
struct Entry {
    tile_id: u64,
    offset: u64,
    length: u32,
    /// 0 = leaf directory; >0 = tile byte length.
    run_length: u32,
}

/// Decode a varint from `buf` starting at `*pos`, advancing `*pos`.
fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, PmtError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        if *pos >= buf.len() {
            return Err(PmtError::CorruptDirectory);
        }
        let byte = buf[*pos];
        *pos += 1;
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return Err(PmtError::CorruptDirectory);
        }
    }
    Ok(result)
}

fn decode_directory(data: &[u8]) -> Result<Vec<Entry>, PmtError> {
    let mut pos = 0usize;
    let num_entries = read_varint(data, &mut pos)? as usize;

    let mut entries = Vec::with_capacity(num_entries);

    // tile_id deltas
    let mut last_id: u64 = 0;
    for _ in 0..num_entries {
        let delta = read_varint(data, &mut pos)?;
        last_id += delta;
        entries.push(Entry { tile_id: last_id, offset: 0, length: 0, run_length: 0 });
    }
    // run_lengths
    for e in &mut entries {
        e.run_length = read_varint(data, &mut pos)? as u32;
    }
    // lengths
    for e in &mut entries {
        e.length = read_varint(data, &mut pos)? as u32;
    }
    // offsets (delta-encoded; 0 means "follow previous entry = contiguous")
    // First, read all raw values; then resolve the "follow" rule in a second pass
    // over a plain index to avoid borrow-checker conflicts.
    let mut raw_offsets = Vec::with_capacity(entries.len());
    for _ in 0..entries.len() {
        raw_offsets.push(read_varint(data, &mut pos)?);
    }
    let mut resolved = Vec::with_capacity(entries.len());
    for i in 0..entries.len() {
        let raw = raw_offsets[i];
        if raw == 0 && i > 0 {
            let prev_offset = resolved[i - 1];
            let prev_len = entries[i - 1].length as u64;
            resolved.push(prev_offset + prev_len);
        } else {
            resolved.push(raw);
        }
    }
    for (e, off) in entries.iter_mut().zip(resolved) {
        e.offset = off;
    }

    Ok(entries)
}

fn decompress_internal(data: &[u8], comp: Compression) -> Result<Vec<u8>, PmtError> {
    match comp {
        Compression::Gzip => {
            let mut dec = flate2::read::GzDecoder::new(Cursor::new(data));
            let mut out = Vec::new();
            dec.read_to_end(&mut out)
                .map_err(|e| PmtError::Decompress(e.to_string()))?;
            Ok(out)
        }
        Compression::None | Compression::Unknown => Ok(data.to_vec()),
        other => Err(PmtError::Decompress(format!(
            "unsupported internal compression {:?}",
            other
        ))),
    }
}

// ── reader ────────────────────────────────────────────────────────────────────

/// Mmapped, read-only PMTiles v3 reader.
///
/// The root directory is decoded into RAM at construction time (~KB).
/// All tile data is left in the mmap and sliced on demand — no copies
/// until the caller receives the `Bytes`.
pub struct PmtReader {
    mmap: memmap2::Mmap,
    header: Header,
    root_dir: Vec<Entry>,
}

impl PmtReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, PmtError> {
        let file = File::open(path)?;
        // SAFETY: we own the file and never mutate the mapping.
        let mmap = unsafe { memmap2::MmapOptions::new().map(&file)? };

        let header = Header::parse(&mmap)?;

        let raw_root = &mmap[header.root_dir_offset as usize
            ..(header.root_dir_offset + header.root_dir_len) as usize];
        let root_data = decompress_internal(raw_root, header.internal_compression)?;
        let root_dir = decode_directory(&root_data)?;

        Ok(Self { mmap, header, root_dir })
    }

    /// Fetch tile bytes for (z, x, y).  Returns pre-compressed bytes exactly
    /// as stored in the archive — zero extra allocations on the hot path.
    pub fn get_tile(
        &self,
        z: u8,
        x: u32,
        y: u32,
    ) -> Result<TileData, PmtError> {
        let id = crate::tile_id::tile_to_id(z, x, y);
        let entry = self.lookup(&self.root_dir, id, 0)?;
        let start = (self.header.tile_data_offset + entry.offset) as usize;
        let end = start + entry.length as usize;
        let data = Bytes::copy_from_slice(&self.mmap[start..end]);
        Ok(TileData { data, compression: self.header.tile_compression })
    }

    /// Metadata JSON (decompressed).
    pub fn metadata(&self) -> Result<String, PmtError> {
        let raw = &self.mmap[self.header.metadata_offset as usize
            ..(self.header.metadata_offset + self.header.metadata_len) as usize];
        let data = decompress_internal(raw, self.header.internal_compression)?;
        String::from_utf8(data).map_err(|e| PmtError::Decompress(e.to_string()))
    }

    pub fn min_zoom(&self) -> u8 { self.header.min_zoom }
    pub fn max_zoom(&self) -> u8 { self.header.max_zoom }
    pub fn tile_count(&self) -> u64 { self.header.tile_entries }

    // ── directory search ──────────────────────────────────────────────────────

    fn lookup(&self, dir: &[Entry], id: u64, depth: u8) -> Result<Entry, PmtError> {
        // Binary search: find the last entry whose tile_id <= id.
        let idx = dir.partition_point(|e| e.tile_id <= id);
        if idx == 0 {
            return Err(PmtError::TileNotFound { z: 0, x: 0, y: 0 });
        }
        let e = &dir[idx - 1];

        if e.run_length == 0 {
            // Leaf directory pointer — recurse once.
            if depth > 1 {
                return Err(PmtError::CorruptDirectory);
            }
            let leaf_start =
                (self.header.leaf_dirs_offset + e.offset) as usize;
            let leaf_end = leaf_start + e.length as usize;
            let raw = &self.mmap[leaf_start..leaf_end];
            let leaf_data = decompress_internal(raw, self.header.internal_compression)?;
            let leaf_dir = decode_directory(&leaf_data)?;
            return self.lookup(&leaf_dir, id, depth + 1);
        }

        // Tile entry: check it actually covers `id`.
        if e.tile_id == id || (e.run_length > 1 && id < e.tile_id + e.run_length as u64) {
            // Clustered: each run slot shifts the offset by `length`.
            let run_offset = if e.run_length > 1 {
                (id - e.tile_id) * e.length as u64
            } else {
                0
            };
            return Ok(Entry {
                tile_id: id,
                offset: e.offset + run_offset,
                length: e.length,
                run_length: 1,
            });
        }

        Err(PmtError::TileNotFound { z: 0, x: 0, y: 0 })
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_single_byte() {
        let buf = [0x01u8];
        let mut pos = 0;
        assert_eq!(read_varint(&buf, &mut pos).unwrap(), 1);
        assert_eq!(pos, 1);
    }

    #[test]
    fn varint_multi_byte() {
        // 300 = 0b100101100 → 0xAC 0x02
        let buf = [0xACu8, 0x02];
        let mut pos = 0;
        assert_eq!(read_varint(&buf, &mut pos).unwrap(), 300);
        assert_eq!(pos, 2);
    }

    #[test]
    fn bad_magic_rejected() {
        let buf = [0u8; 128];
        assert!(matches!(Header::parse(&buf), Err(PmtError::BadMagic)));
    }
}

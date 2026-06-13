/// On-disk feature store for memory-bounded baking.
///
/// During a streaming bake we cannot hold millions of reprojected features in
/// RAM. Instead each feature is serialized once to a temp file (append-only),
/// and a tiny fixed-size index entry (mercator bbox + byte offset/length) is
/// kept in memory. Tiles read the few features they need back from the store on
/// demand via an mmap, so peak RAM is the index plus whatever a handful of tiles
/// touch — not the whole dataset.
///
/// Serialization is a small hand-rolled binary codec (no extra deps): a geometry
/// tag byte, coordinate count(s) as u32, packed f64 little-endian coordinates,
/// then a u32-length-prefixed JSON blob for properties.
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use geo_types::{
    Geometry, LineString, MultiLineString, MultiPoint, MultiPolygon, Point, Polygon,
};
use memmap2::Mmap;

use crate::error::BakeError;

type PropMap = serde_json::Map<String, geojson::JsonValue>;

/// Fixed-size in-RAM index entry for one stored feature.
#[derive(Clone, Copy)]
pub struct FeatureMeta {
    /// Mercator bounding box (min_x, min_y, max_x, max_y).
    pub bbox: [f64; 4],
    /// Byte offset of the record in the store file.
    pub offset: u64,
    /// Byte length of the record.
    pub len: u32,
}

/// Append-only writer for the feature store.
pub struct StoreWriter {
    path: PathBuf,
    out: BufWriter<File>,
    pos: u64,
}

impl StoreWriter {
    pub fn create(path: PathBuf) -> Result<Self, BakeError> {
        let file = File::create(&path)?;
        Ok(Self {
            path,
            out: BufWriter::with_capacity(1 << 20, file),
            pos: 0,
        })
    }

    /// Serialize one feature, returning its index entry. Caller keeps the entry.
    pub fn append(
        &mut self,
        geom: &Geometry<f64>,
        bbox: [f64; 4],
        props: &PropMap,
    ) -> Result<FeatureMeta, BakeError> {
        let mut rec = Vec::with_capacity(256);
        encode_geometry(geom, &mut rec);

        // Properties as a length-prefixed JSON blob.
        let props_json = serde_json::to_vec(props).map_err(|e| BakeError::Encode(e.to_string()))?;
        write_u32(&mut rec, props_json.len() as u32);
        rec.extend_from_slice(&props_json);

        let offset = self.pos;
        let len = rec.len() as u32;
        self.out.write_all(&rec)?;
        self.pos += len as u64;

        Ok(FeatureMeta { bbox, offset, len })
    }

    /// Flush and finalize, returning a reader over the written data.
    pub fn finish(mut self) -> Result<StoreReader, BakeError> {
        self.out.flush()?;
        let file = self.out.into_inner().map_err(|e| BakeError::Io(e.into_error()))?;
        file.sync_all()?;
        drop(file);
        StoreReader::open(self.path)
    }
}

/// mmap-backed random-access reader for the feature store.
pub struct StoreReader {
    path: PathBuf,
    mmap: Mmap,
}

impl StoreReader {
    fn open(path: PathBuf) -> Result<Self, BakeError> {
        let file = File::open(&path)?;
        // SAFETY: the store file is written once and not mutated while mapped.
        let mmap = unsafe { Mmap::map(&file)? };
        Ok(Self { path, mmap })
    }

    /// Decode the feature at `meta` back into geometry + properties.
    pub fn read(&self, meta: &FeatureMeta) -> Result<(Geometry<f64>, PropMap), BakeError> {
        let start = meta.offset as usize;
        let end = start + meta.len as usize;
        let bytes = &self.mmap[start..end];
        let mut cur = 0usize;
        let geom = decode_geometry(bytes, &mut cur)?;
        let plen = read_u32(bytes, &mut cur)? as usize;
        let props_bytes = &bytes[cur..cur + plen];
        let props: PropMap =
            serde_json::from_slice(props_bytes).map_err(|e| BakeError::Encode(e.to_string()))?;
        Ok((geom, props))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for StoreReader {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

// ── geometry codec ──────────────────────────────────────────────────────────────
//
// Tags: 1 Point, 2 LineString, 3 Polygon, 4 MultiPoint, 5 MultiLineString,
//       6 MultiPolygon. (Line and GeometryCollection are normalized on encode.)

fn encode_geometry(geom: &Geometry<f64>, out: &mut Vec<u8>) {
    match geom {
        Geometry::Point(p) => {
            out.push(1);
            write_coord(out, p.x(), p.y());
        }
        Geometry::Line(l) => {
            // Normalize to a 2-point LineString.
            out.push(2);
            write_ring(out, &[(l.start.x, l.start.y), (l.end.x, l.end.y)]);
        }
        Geometry::LineString(ls) => {
            out.push(2);
            write_ls(out, ls);
        }
        Geometry::Polygon(p) => {
            out.push(3);
            write_polygon(out, p);
        }
        Geometry::MultiPoint(mp) => {
            out.push(4);
            write_u32(out, mp.0.len() as u32);
            for p in &mp.0 {
                write_coord(out, p.x(), p.y());
            }
        }
        Geometry::MultiLineString(mls) => {
            out.push(5);
            write_u32(out, mls.0.len() as u32);
            for ls in &mls.0 {
                write_ls(out, ls);
            }
        }
        Geometry::MultiPolygon(mp) => {
            out.push(6);
            write_u32(out, mp.0.len() as u32);
            for poly in &mp.0 {
                write_polygon(out, poly);
            }
        }
        Geometry::GeometryCollection(gc) => {
            // Flatten: store the first member, or a degenerate empty point.
            // (Collections are rare in tile sources; we don't lose the common
            // cases. Each member could be stored separately upstream if needed.)
            if let Some(first) = gc.0.first() {
                encode_geometry(first, out);
            } else {
                out.push(1);
                write_coord(out, 0.0, 0.0);
            }
        }
        _ => {
            out.push(1);
            write_coord(out, 0.0, 0.0);
        }
    }
}

fn decode_geometry(b: &[u8], cur: &mut usize) -> Result<Geometry<f64>, BakeError> {
    let tag = read_u8(b, cur)?;
    let g = match tag {
        1 => {
            let (x, y) = read_coord(b, cur)?;
            Geometry::Point(Point::new(x, y))
        }
        2 => Geometry::LineString(read_ls(b, cur)?),
        3 => Geometry::Polygon(read_polygon(b, cur)?),
        4 => {
            let n = read_u32(b, cur)? as usize;
            let mut pts = Vec::with_capacity(n);
            for _ in 0..n {
                let (x, y) = read_coord(b, cur)?;
                pts.push(Point::new(x, y));
            }
            Geometry::MultiPoint(MultiPoint(pts))
        }
        5 => {
            let n = read_u32(b, cur)? as usize;
            let mut lines = Vec::with_capacity(n);
            for _ in 0..n {
                lines.push(read_ls(b, cur)?);
            }
            Geometry::MultiLineString(MultiLineString(lines))
        }
        6 => {
            let n = read_u32(b, cur)? as usize;
            let mut polys = Vec::with_capacity(n);
            for _ in 0..n {
                polys.push(read_polygon(b, cur)?);
            }
            Geometry::MultiPolygon(MultiPolygon(polys))
        }
        other => return Err(BakeError::Encode(format!("bad geometry tag {other}"))),
    };
    Ok(g)
}

// ── piece encoders ──────────────────────────────────────────────────────────────

fn write_ls(out: &mut Vec<u8>, ls: &LineString<f64>) {
    write_u32(out, ls.0.len() as u32);
    for c in &ls.0 {
        write_coord(out, c.x, c.y);
    }
}

fn read_ls(b: &[u8], cur: &mut usize) -> Result<LineString<f64>, BakeError> {
    let n = read_u32(b, cur)? as usize;
    let mut coords = Vec::with_capacity(n);
    for _ in 0..n {
        let (x, y) = read_coord(b, cur)?;
        coords.push(geo_types::coord! { x: x, y: y });
    }
    Ok(LineString(coords))
}

fn write_ring(out: &mut Vec<u8>, coords: &[(f64, f64)]) {
    write_u32(out, coords.len() as u32);
    for &(x, y) in coords {
        write_coord(out, x, y);
    }
}

fn write_polygon(out: &mut Vec<u8>, p: &Polygon<f64>) {
    write_ls(out, p.exterior());
    write_u32(out, p.interiors().len() as u32);
    for ring in p.interiors() {
        write_ls(out, ring);
    }
}

fn read_polygon(b: &[u8], cur: &mut usize) -> Result<Polygon<f64>, BakeError> {
    let exterior = read_ls(b, cur)?;
    let n = read_u32(b, cur)? as usize;
    let mut interiors = Vec::with_capacity(n);
    for _ in 0..n {
        interiors.push(read_ls(b, cur)?);
    }
    Ok(Polygon::new(exterior, interiors))
}

// ── primitive read/write ────────────────────────────────────────────────────────

#[inline]
fn write_coord(out: &mut Vec<u8>, x: f64, y: f64) {
    out.extend_from_slice(&x.to_le_bytes());
    out.extend_from_slice(&y.to_le_bytes());
}

#[inline]
fn read_coord(b: &[u8], cur: &mut usize) -> Result<(f64, f64), BakeError> {
    let x = read_f64(b, cur)?;
    let y = read_f64(b, cur)?;
    Ok((x, y))
}

#[inline]
fn write_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

#[inline]
fn read_u8(b: &[u8], cur: &mut usize) -> Result<u8, BakeError> {
    let v = *b.get(*cur).ok_or_else(|| BakeError::Encode("store underrun".into()))?;
    *cur += 1;
    Ok(v)
}

#[inline]
fn read_u32(b: &[u8], cur: &mut usize) -> Result<u32, BakeError> {
    let end = *cur + 4;
    let slice = b.get(*cur..end).ok_or_else(|| BakeError::Encode("store underrun".into()))?;
    *cur = end;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

#[inline]
fn read_f64(b: &[u8], cur: &mut usize) -> Result<f64, BakeError> {
    let end = *cur + 8;
    let slice = b.get(*cur..end).ok_or_else(|| BakeError::Encode("store underrun".into()))?;
    *cur = end;
    Ok(f64::from_le_bytes(slice.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{polygon, LineString};

    fn roundtrip(g: Geometry<f64>) -> Geometry<f64> {
        let mut buf = Vec::new();
        encode_geometry(&g, &mut buf);
        let mut cur = 0;
        decode_geometry(&buf, &mut cur).unwrap()
    }

    #[test]
    fn point_roundtrip() {
        let g = Geometry::Point(Point::new(1.5, -2.5));
        match roundtrip(g) {
            Geometry::Point(p) => assert_eq!((p.x(), p.y()), (1.5, -2.5)),
            _ => panic!("type changed"),
        }
    }

    #[test]
    fn polygon_with_hole_roundtrip() {
        let p = polygon!(
            exterior: [(x:0.,y:0.),(x:4.,y:0.),(x:4.,y:4.),(x:0.,y:4.),(x:0.,y:0.)],
            interiors: [[(x:1.,y:1.),(x:2.,y:1.),(x:2.,y:2.),(x:1.,y:2.),(x:1.,y:1.)]],
        );
        match roundtrip(Geometry::Polygon(p.clone())) {
            Geometry::Polygon(q) => {
                assert_eq!(q.exterior().0.len(), 5);
                assert_eq!(q.interiors().len(), 1);
            }
            _ => panic!("type changed"),
        }
    }

    #[test]
    fn linestring_roundtrip() {
        let ls = LineString::from(vec![(0.0, 0.0), (1.0, 1.0), (2.0, 0.0)]);
        match roundtrip(Geometry::LineString(ls)) {
            Geometry::LineString(q) => assert_eq!(q.0.len(), 3),
            _ => panic!("type changed"),
        }
    }

    #[test]
    fn store_write_read_roundtrip() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("lm-store-test-{}.bin", std::process::id()));
        let mut w = StoreWriter::create(path).unwrap();
        let g = Geometry::Point(Point::new(10.0, 20.0));
        let mut props = PropMap::new();
        props.insert("k".into(), serde_json::json!("v"));
        let meta = w.append(&g, [10.0, 20.0, 10.0, 20.0], &props).unwrap();
        let reader = w.finish().unwrap();
        let (rg, rp) = reader.read(&meta).unwrap();
        assert!(matches!(rg, Geometry::Point(_)));
        assert_eq!(rp.get("k").unwrap(), "v");
        // reader drop removes the temp file
    }
}

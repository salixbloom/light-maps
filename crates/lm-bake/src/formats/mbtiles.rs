/// MBTiles import adapter.
///
/// MBTiles is a SQLite database where tiles are stored in a `tiles` table
/// as `(zoom_level, tile_column, tile_row, tile_data)`.  Row/Y indexing uses
/// TMS convention (y=0 at south), which is the inverse of XYZ (y=0 at north),
/// so we flip: `xyz_y = (2^z - 1) - tms_y`.
///
/// Returns `(tile_id, raw_tile_bytes)` pairs ready to write directly into a
/// PMTiles archive.  No re-encoding — tiles are passed through verbatim so
/// their existing compression is preserved.
use rusqlite::{Connection, OpenFlags};

use lm_core::{tile_id::tile_to_id, writer::TileEntry};

use crate::error::BakeError;

pub struct MbtilesInfo {
    pub entries: Vec<TileEntry>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// [min_lon, min_lat, max_lon, max_lat]
    pub bounds: [f64; 4],
    pub name: String,
    pub description: Option<String>,
    /// Tile compression as stored ("gzip", "none", …)
    pub tile_compression: String,
}

pub fn import_mbtiles(path: &str) -> Result<MbtilesInfo, BakeError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| BakeError::GeoJson(format!("mbtiles open: {e}")))?;

    // Read metadata table.
    let mut meta_stmt = conn
        .prepare("SELECT name, value FROM metadata")
        .map_err(|e| BakeError::GeoJson(format!("mbtiles metadata: {e}")))?;

    let mut meta: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let rows = meta_stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|e| BakeError::GeoJson(format!("mbtiles metadata read: {e}")))?;
    for r in rows {
        let (k, v) = r.map_err(|e| BakeError::GeoJson(e.to_string()))?;
        meta.insert(k, v);
    }

    let name = meta.get("name").cloned().unwrap_or_else(|| "imported".to_owned());
    let description = meta.get("description").cloned();
    let tile_compression = detect_compression(&conn);

    let bounds = parse_bounds(meta.get("bounds").map(String::as_str));
    let min_zoom: u8 = meta
        .get("minzoom")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let max_zoom: u8 = meta
        .get("maxzoom")
        .and_then(|v| v.parse().ok())
        .unwrap_or(14);

    // Read tiles.
    let mut tile_stmt = conn
        .prepare("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles")
        .map_err(|e| BakeError::GeoJson(format!("mbtiles tiles: {e}")))?;

    let mut entries = Vec::new();
    let tile_rows = tile_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .map_err(|e| BakeError::GeoJson(e.to_string()))?;

    for r in tile_rows {
        let (z, x, tms_y, data) =
            r.map_err(|e| BakeError::GeoJson(e.to_string()))?;
        let z = z as u8;
        let x = x as u32;
        // Flip TMS → XYZ y.
        let y = ((1u64 << z) - 1 - tms_y as u64) as u32;
        entries.push(TileEntry { tile_id: tile_to_id(z, x, y), data });
    }

    if entries.is_empty() {
        return Err(BakeError::Empty);
    }

    Ok(MbtilesInfo {
        entries,
        min_zoom,
        max_zoom,
        bounds,
        name,
        description,
        tile_compression,
    })
}

fn detect_compression(conn: &Connection) -> String {
    // Peek at the first tile's magic bytes to detect gzip.
    if let Ok(mut stmt) = conn.prepare("SELECT tile_data FROM tiles LIMIT 1") {
        if let Ok(mut rows) = stmt.query([]) {
            if let Ok(Some(row)) = rows.next() {
                if let Ok(data) = row.get::<_, Vec<u8>>(0) {
                    if data.len() >= 2 && data[0] == 0x1f && data[1] == 0x8b {
                        return "gzip".to_owned();
                    }
                }
            }
        }
    }
    "none".to_owned()
}

fn parse_bounds(s: Option<&str>) -> [f64; 4] {
    if let Some(s) = s {
        let parts: Vec<f64> = s.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        if parts.len() == 4 {
            return [parts[0], parts[1], parts[2], parts[3]];
        }
    }
    [-180.0, -85.0, 180.0, 85.0]
}

/// WGS84 (EPSG:4326) → Web Mercator (EPSG:3857) reprojection.
///
/// Uses the standard spherical Mercator formulas (same as every tile mapping
/// library). Operates in-place on geo-types geometries via coordinate mapping.
use geo::MapCoords;
use geo_types::Geometry;

use std::f64::consts::PI;

const EARTH_RADIUS: f64 = 6_378_137.0; // metres, WGS84 semi-major axis

#[inline]
pub fn lon_to_x(lon_deg: f64) -> f64 {
    lon_deg.to_radians() * EARTH_RADIUS
}

#[inline]
pub fn lat_to_y(lat_deg: f64) -> f64 {
    let lat = lat_deg.to_radians();
    EARTH_RADIUS * ((PI / 4.0 + lat / 2.0).tan()).ln()
}

/// Project a WGS84 lon/lat geometry into Web Mercator metres.
pub fn to_mercator(geom: Geometry<f64>) -> Geometry<f64> {
    geom.map_coords(|c| (lon_to_x(c.x), lat_to_y(c.y)).into())
}

/// Web Mercator extent: the full sphere maps to ±20037508.3427892 m.
pub const MERCATOR_EXTENT: f64 = 20_037_508.342_789_2;

/// Return the Web Mercator bounding box [min_x, min_y, max_x, max_y] for
/// a tile (z, x, y), with a fractional `buffer` in tile-units added on each side.
pub fn tile_bbox(z: u8, x: u32, y: u32, buffer: f64) -> (f64, f64, f64, f64) {
    let tiles = (1u64 << z) as f64;
    let size = 2.0 * MERCATOR_EXTENT / tiles;
    let buf = size * buffer;

    let min_x = -MERCATOR_EXTENT + x as f64 * size - buf;
    let max_x = min_x + size + 2.0 * buf;
    // Y axis is flipped: tile y=0 is the top (north)
    let max_y = MERCATOR_EXTENT - y as f64 * size + buf;
    let min_y = max_y - size - 2.0 * buf;

    (min_x, min_y, max_x, max_y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_projects_to_zero() {
        assert!((lon_to_x(0.0)).abs() < 1e-6);
        assert!((lat_to_y(0.0)).abs() < 1e-6);
    }

    #[test]
    fn antimeridian_extent() {
        let x = lon_to_x(180.0);
        assert!((x - MERCATOR_EXTENT).abs() < 1.0, "x={x}");
    }

    #[test]
    fn tile_bbox_z0_covers_world() {
        let (min_x, min_y, max_x, max_y) = tile_bbox(0, 0, 0, 0.0);
        assert!((min_x + MERCATOR_EXTENT).abs() < 1.0);
        assert!((max_x - MERCATOR_EXTENT).abs() < 1.0);
        assert!((min_y + MERCATOR_EXTENT).abs() < 1.0);
        assert!((max_y - MERCATOR_EXTENT).abs() < 1.0);
    }
}

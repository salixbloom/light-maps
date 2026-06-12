/// Per-zoom simplification using geo's Simplify (Douglas-Peucker).
///
/// Tolerance is derived from the tile's pixel resolution in Web Mercator metres:
///   tile_size = 2 * MERCATOR_EXTENT / 2^z
///   pixel_size = tile_size / MVT_EXTENT  (MVT_EXTENT = 4096 units)
///
/// We multiply by a tunable factor; 1.0 gives one-unit tolerance (the minimum
/// meaningful simplification for a 4096-grid tile). Higher values simplify more
/// aggressively at low zooms — good for file size, needs care for accuracy.
use geo::Simplify;
use geo_types::Geometry;

use crate::reproject::MERCATOR_EXTENT;

pub const MVT_EXTENT: u32 = 4096;

/// Tolerance factor: 1.0 = exactly one MVT grid unit. We use slightly above 1
/// so sub-pixel detail is removed without visible loss at each zoom level.
const TOLERANCE_FACTOR: f64 = 1.0;

/// Compute the simplification tolerance in Web Mercator metres for zoom `z`.
pub fn simplify_tolerance(z: u8) -> f64 {
    let tile_size = 2.0 * MERCATOR_EXTENT / (1u64 << z) as f64;
    let pixel_size = tile_size / MVT_EXTENT as f64;
    pixel_size * TOLERANCE_FACTOR
}

/// Simplify a Web Mercator geometry for zoom level `z`.
/// Points and small geometries are returned unchanged.
pub fn simplify_for_zoom(geom: Geometry<f64>, z: u8) -> Option<Geometry<f64>> {
    let tol = simplify_tolerance(z);
    match geom {
        Geometry::LineString(ls) => {
            let s = ls.simplify(&tol);
            if s.0.len() < 2 {
                None
            } else {
                Some(Geometry::LineString(s))
            }
        }
        Geometry::MultiLineString(mls) => {
            let s = mls.simplify(&tol);
            if s.0.is_empty() {
                None
            } else {
                Some(Geometry::MultiLineString(s))
            }
        }
        Geometry::Polygon(p) => {
            let s = p.simplify(&tol);
            // Drop polygons whose exterior collapses to < 4 coords (degenerate)
            if s.exterior().0.len() < 4 {
                None
            } else {
                Some(Geometry::Polygon(s))
            }
        }
        Geometry::MultiPolygon(mp) => {
            let s = mp.simplify(&tol);
            if s.0.is_empty() {
                None
            } else {
                Some(Geometry::MultiPolygon(s))
            }
        }
        // Points never simplified
        other => Some(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tolerance_decreases_with_zoom() {
        let t0 = simplify_tolerance(0);
        let t10 = simplify_tolerance(10);
        let t16 = simplify_tolerance(16);
        assert!(t0 > t10, "z0 tol={t0} should be > z10 tol={t10}");
        assert!(t10 > t16, "z10 tol={t10} should be > z16 tol={t16}");
    }

    #[test]
    fn z0_tolerance_is_world_scale() {
        // At z=0 the whole world is one tile; one pixel is ~9756m
        let t = simplify_tolerance(0);
        assert!(t > 9000.0 && t < 11000.0, "z0 tol={t}");
    }
}

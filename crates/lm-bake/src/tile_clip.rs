/// Clip Web Mercator geometries to a tile bounding box.
///
/// Uses geo's `BooleanOps` for polygons (proper intersection).
/// Uses a simple Cohen-Sutherland line clip for linestrings.
/// Points are filtered by containment.
use geo::{BooleanOps, BoundingRect, Intersects};
use geo_types::{Geometry, LineString, MultiLineString, MultiPolygon, Rect};

/// Buffer as a fraction of tile size added to each side.
const CLIP_BUFFER: f64 = 4.0 / 4096.0;

pub fn clip_to_tile(
    geom: Geometry<f64>,
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
) -> Option<Geometry<f64>> {
    let bw = (max_x - min_x) * CLIP_BUFFER;
    let bh = (max_y - min_y) * CLIP_BUFFER;
    let tile = Rect::new((min_x - bw, min_y - bh), (max_x + bw, max_y + bh));

    let geom_bbox = geom.bounding_rect()?;
    if !geom_bbox.intersects(&tile) {
        return None;
    }

    match geom {
        Geometry::Point(p) => {
            if tile.intersects(&p) { Some(Geometry::Point(p)) } else { None }
        }
        Geometry::MultiPoint(mp) => {
            let pts: Vec<_> = mp.0.into_iter().filter(|p| tile.intersects(p)).collect();
            if pts.is_empty() { None } else { Some(Geometry::MultiPoint(pts.into())) }
        }

        Geometry::LineString(ls) => {
            let segments = clip_linestring(&ls.0, &tile);
            match segments.len() {
                0 => None,
                1 => Some(Geometry::LineString(LineString(segments.into_iter().next().unwrap()))),
                _ => Some(Geometry::MultiLineString(MultiLineString(
                    segments.into_iter().map(LineString).collect(),
                ))),
            }
        }
        Geometry::Line(l) => {
            let ls = LineString(vec![l.start, l.end]);
            let segments = clip_linestring(&ls.0, &tile);
            if segments.is_empty() {
                None
            } else {
                Some(Geometry::MultiLineString(MultiLineString(
                    segments.into_iter().map(LineString).collect(),
                )))
            }
        }
        Geometry::MultiLineString(mls) => {
            let segs: Vec<LineString<f64>> = mls
                .0
                .into_iter()
                .flat_map(|ls| clip_linestring(&ls.0, &tile).into_iter().map(LineString))
                .collect();
            if segs.is_empty() { None } else { Some(Geometry::MultiLineString(MultiLineString(segs))) }
        }

        Geometry::Polygon(p) => {
            let tile_mp = MultiPolygon(vec![tile.into()]);
            let result = MultiPolygon(vec![p]).intersection(&tile_mp);
            match result.0.len() {
                0 => None,
                1 => Some(Geometry::Polygon(result.0.into_iter().next().unwrap())),
                _ => Some(Geometry::MultiPolygon(result)),
            }
        }
        Geometry::MultiPolygon(mp) => {
            let tile_mp = MultiPolygon(vec![tile.into()]);
            let result = mp.intersection(&tile_mp);
            if result.0.is_empty() { None } else { Some(Geometry::MultiPolygon(result)) }
        }

        Geometry::GeometryCollection(gc) => {
            let parts: Vec<_> = gc.0.into_iter()
                .filter_map(|g| clip_to_tile(g, min_x, min_y, max_x, max_y))
                .collect();
            if parts.is_empty() { None } else { Some(Geometry::GeometryCollection(parts.into())) }
        }

        other => Some(other),
    }
}

// ── Cohen-Sutherland line clip ─────────────────────────────────────────────

type Coord = geo_types::Coord<f64>;

fn clip_linestring(coords: &[Coord], tile: &Rect<f64>) -> Vec<Vec<Coord>> {
    if coords.len() < 2 {
        return vec![];
    }
    let xmin = tile.min().x;
    let ymin = tile.min().y;
    let xmax = tile.max().x;
    let ymax = tile.max().y;

    let mut segments: Vec<Vec<Coord>> = Vec::new();
    let mut current: Vec<Coord> = Vec::new();

    for w in coords.windows(2) {
        let (a, b) = (w[0], w[1]);
        if let Some((ca, cb)) = cohen_sutherland(a, b, xmin, ymin, xmax, ymax) {
            if current.is_empty() {
                current.push(ca);
            } else if (current.last().unwrap().x - ca.x).abs() > 1e-12
                || (current.last().unwrap().y - ca.y).abs() > 1e-12
            {
                // Gap in the clipped line — start a new segment.
                if current.len() >= 2 {
                    segments.push(std::mem::take(&mut current));
                } else {
                    current.clear();
                }
                current.push(ca);
            }
            current.push(cb);
        } else if !current.is_empty() {
            if current.len() >= 2 {
                segments.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
    }
    if current.len() >= 2 {
        segments.push(current);
    }
    segments
}

const INSIDE: u8 = 0;
const LEFT: u8 = 1;
const RIGHT: u8 = 2;
const BOTTOM: u8 = 4;
const TOP: u8 = 8;

fn outcode(p: Coord, xmin: f64, ymin: f64, xmax: f64, ymax: f64) -> u8 {
    let mut code = INSIDE;
    if p.x < xmin { code |= LEFT; }
    else if p.x > xmax { code |= RIGHT; }
    if p.y < ymin { code |= BOTTOM; }
    else if p.y > ymax { code |= TOP; }
    code
}

fn cohen_sutherland(
    mut a: Coord, mut b: Coord,
    xmin: f64, ymin: f64, xmax: f64, ymax: f64,
) -> Option<(Coord, Coord)> {
    let mut ca = outcode(a, xmin, ymin, xmax, ymax);
    let mut cb = outcode(b, xmin, ymin, xmax, ymax);
    loop {
        if ca | cb == 0 { return Some((a, b)); }     // both inside
        if ca & cb != 0 { return None; }             // trivially outside
        let out = if ca != 0 { ca } else { cb };
        let (x, y) = if out & TOP != 0 {
            (a.x + (b.x - a.x) * (ymax - a.y) / (b.y - a.y), ymax)
        } else if out & BOTTOM != 0 {
            (a.x + (b.x - a.x) * (ymin - a.y) / (b.y - a.y), ymin)
        } else if out & RIGHT != 0 {
            (xmax, a.y + (b.y - a.y) * (xmax - a.x) / (b.x - a.x))
        } else {
            (xmin, a.y + (b.y - a.y) * (xmin - a.x) / (b.x - a.x))
        };
        let p = geo_types::coord! { x: x, y: y };
        if out == ca { a = p; ca = outcode(a, xmin, ymin, xmax, ymax); }
        else         { b = p; cb = outcode(b, xmin, ymin, xmax, ymax); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::{point, polygon, LineString};

    #[test]
    fn point_inside_kept() {
        let p = Geometry::Point(point!(x: 0.0, y: 0.0));
        assert!(clip_to_tile(p, -1.0, -1.0, 1.0, 1.0).is_some());
    }

    #[test]
    fn point_outside_dropped() {
        let p = Geometry::Point(point!(x: 5.0, y: 5.0));
        assert!(clip_to_tile(p, -1.0, -1.0, 1.0, 1.0).is_none());
    }

    #[test]
    fn linestring_clipped() {
        let ls = Geometry::LineString(LineString::from(vec![(-5.0f64, 0.0), (5.0, 0.0)]));
        assert!(clip_to_tile(ls, -1.0, -1.0, 1.0, 1.0).is_some());
    }

    #[test]
    fn polygon_crossing_clipped() {
        let poly = Geometry::Polygon(polygon![
            (x: -2.0, y: -2.0), (x: 2.0, y: -2.0),
            (x: 2.0, y:  2.0), (x: -2.0, y:  2.0),
            (x: -2.0, y: -2.0)
        ]);
        assert!(clip_to_tile(poly, -1.0, -1.0, 1.0, 1.0).is_some());
    }

    #[test]
    fn polygon_outside_dropped() {
        let poly = Geometry::Polygon(polygon![
            (x: 10.0, y: 10.0), (x: 20.0, y: 10.0),
            (x: 20.0, y: 20.0), (x: 10.0, y: 20.0),
            (x: 10.0, y: 10.0)
        ]);
        assert!(clip_to_tile(poly, -1.0, -1.0, 1.0, 1.0).is_none());
    }
}

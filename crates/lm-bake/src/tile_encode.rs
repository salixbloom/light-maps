/// Encode a set of features into a Mapbox Vector Tile (MVT) protobuf blob.
///
/// Coordinate quantization: Web Mercator metres → [0, 4096] integer grid.
/// The MVT spec uses screen-space coordinates where y increases downward,
/// so we flip the y axis during quantization.
use std::collections::HashMap;

use geo_types::Geometry;

use crate::{error::BakeError, simplify::MVT_EXTENT};

/// A feature ready to encode: quantized geometry + string properties.
pub struct EncodableFeature {
    pub geom: Geometry<f64>,
    pub props: serde_json::Map<String, geojson::JsonValue>,
}

/// Encode features into a raw MVT tile (gzip-compressed protobuf bytes).
///
/// `layer_name`  – MVT layer name (one per baked dataset).
/// `features`    – features already clipped and simplified for this tile.
/// `tile_min_x/y, tile_max_x/y` – Web Mercator extent of this tile (no buffer).
pub fn encode_tile(
    layer_name: &str,
    features: &[EncodableFeature],
    tile_min_x: f64,
    tile_min_y: f64,
    tile_max_x: f64,
    tile_max_y: f64,
) -> Result<Vec<u8>, BakeError> {
    use geozero::mvt::{tile, Message, Tile};

    let tile_w = tile_max_x - tile_min_x;
    let tile_h = tile_max_y - tile_min_y;
    let extent = MVT_EXTENT as f64;

    // Quantize Web Mercator → integer MVT coordinates.
    // MVT y=0 is top (north); Mercator y increases northward → flip.
    let qx = |x: f64| -> i64 { ((x - tile_min_x) / tile_w * extent).round() as i64 };
    let qy = |y: f64| -> i64 { ((tile_max_y - y) / tile_h * extent).round() as i64 };

    let mut mvt_layer = tile::Layer {
        version: 2,
        name: layer_name.to_owned(),
        extent: Some(MVT_EXTENT),
        ..Default::default()
    };

    // Key/value tables shared across all features in the layer.
    let mut keys: Vec<String> = Vec::new();
    let mut values: Vec<tile::Value> = Vec::new();
    let mut key_idx: HashMap<String, u32> = HashMap::new();
    let mut val_idx: HashMap<String, u32> = HashMap::new();

    for feat in features {
        let mut mvt_feat = tile::Feature {
            r#type: Some(geom_type_id(&feat.geom) as i32),
            ..Default::default()
        };

        // Encode geometry as MVT commands.
        match encode_geometry(&feat.geom, &qx, &qy) {
            Ok(cmds) => mvt_feat.geometry = cmds,
            Err(_) => continue, // skip unrepresentable geometries
        }

        // Encode properties as interned key/value pairs.
        for (k, v) in &feat.props {
            let ki = *key_idx.entry(k.clone()).or_insert_with(|| {
                let i = keys.len() as u32;
                keys.push(k.clone());
                i
            });
            let vs = json_value_to_mvt(v);
            let vk = format!("{vs:?}");
            let vi = *val_idx.entry(vk).or_insert_with(|| {
                let i = values.len() as u32;
                values.push(vs);
                i
            });
            mvt_feat.tags.push(ki);
            mvt_feat.tags.push(vi);
        }

        mvt_layer.features.push(mvt_feat);
    }

    mvt_layer.keys = keys;
    mvt_layer.values = values;

    let mvt_tile = Tile {
        layers: vec![mvt_layer],
    };

    // Serialize to protobuf bytes.
    let mut buf = Vec::new();
    mvt_tile
        .encode(&mut buf)
        .map_err(|e| BakeError::Encode(e.to_string()))?;

    Ok(buf)
}

// ── geometry encoding ─────────────────────────────────────────────────────────

fn geom_type_id(geom: &Geometry<f64>) -> u32 {
    match geom {
        Geometry::Point(_) | Geometry::MultiPoint(_) => 1,       // POINT
        Geometry::Line(_)
        | Geometry::LineString(_)
        | Geometry::MultiLineString(_) => 2,                       // LINESTRING
        Geometry::Polygon(_) | Geometry::MultiPolygon(_) => 3,    // POLYGON
        _ => 1,
    }
}

fn encode_geometry(
    geom: &Geometry<f64>,
    qx: &impl Fn(f64) -> i64,
    qy: &impl Fn(f64) -> i64,
) -> Result<Vec<u32>, BakeError> {
    let mut cmds = Vec::new();
    match geom {
        Geometry::Point(p) => {
            encode_point(p.x(), p.y(), &mut cmds, qx, qy);
        }
        Geometry::MultiPoint(mp) => {
            // MoveTo command for each point in one batch.
            if mp.0.is_empty() {
                return Err(BakeError::Encode("empty multipoint".into()));
            }
            push_cmd(&mut cmds, 1, mp.0.len() as u32); // MoveTo × n
            let mut cx = 0i64;
            let mut cy = 0i64;
            for p in &mp.0 {
                let x = qx(p.x());
                let y = qy(p.y());
                push_zigzag(&mut cmds, x - cx);
                push_zigzag(&mut cmds, y - cy);
                cx = x;
                cy = y;
            }
        }
        Geometry::Line(l) => {
            encode_linestring_coords(
                &[l.start, l.end].map(|p| (p.x, p.y)),
                &mut cmds,
                qx,
                qy,
            );
        }
        Geometry::LineString(ls) => {
            let coords: Vec<(f64, f64)> = ls.0.iter().map(|c| (c.x, c.y)).collect();
            encode_linestring_coords(&coords, &mut cmds, qx, qy);
        }
        Geometry::MultiLineString(mls) => {
            for ls in &mls.0 {
                let coords: Vec<(f64, f64)> = ls.0.iter().map(|c| (c.x, c.y)).collect();
                encode_linestring_coords(&coords, &mut cmds, qx, qy);
            }
        }
        Geometry::Polygon(p) => {
            encode_ring(&p.exterior().0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), &mut cmds, qx, qy);
            for interior in p.interiors() {
                encode_ring(&interior.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), &mut cmds, qx, qy);
            }
        }
        Geometry::MultiPolygon(mp) => {
            for poly in &mp.0 {
                encode_ring(&poly.exterior().0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), &mut cmds, qx, qy);
                for interior in poly.interiors() {
                    encode_ring(&interior.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), &mut cmds, qx, qy);
                }
            }
        }
        _ => return Err(BakeError::Encode("unsupported geometry type".into())),
    }
    Ok(cmds)
}

fn encode_point(
    x: f64,
    y: f64,
    cmds: &mut Vec<u32>,
    qx: &impl Fn(f64) -> i64,
    qy: &impl Fn(f64) -> i64,
) {
    push_cmd(cmds, 1, 1); // MoveTo × 1
    push_zigzag(cmds, qx(x));
    push_zigzag(cmds, qy(y));
}

fn encode_linestring_coords(
    coords: &[(f64, f64)],
    cmds: &mut Vec<u32>,
    qx: &impl Fn(f64) -> i64,
    qy: &impl Fn(f64) -> i64,
) {
    if coords.len() < 2 {
        return;
    }
    let mut cx = 0i64;
    let mut cy = 0i64;

    // MoveTo first point
    push_cmd(cmds, 1, 1);
    let x0 = qx(coords[0].0);
    let y0 = qy(coords[0].1);
    push_zigzag(cmds, x0 - cx);
    push_zigzag(cmds, y0 - cy);
    cx = x0;
    cy = y0;

    // LineTo remaining points
    push_cmd(cmds, 2, (coords.len() - 1) as u32);
    for &(lx, ly) in &coords[1..] {
        let x = qx(lx);
        let y = qy(ly);
        push_zigzag(cmds, x - cx);
        push_zigzag(cmds, y - cy);
        cx = x;
        cy = y;
    }
}

fn encode_ring(
    coords: &[(f64, f64)],
    cmds: &mut Vec<u32>,
    qx: &impl Fn(f64) -> i64,
    qy: &impl Fn(f64) -> i64,
) {
    if coords.len() < 4 {
        return; // degenerate ring
    }
    // Rings are closed (first == last); encode all but the closing duplicate.
    let open: Vec<(f64, f64)> = coords[..coords.len() - 1].to_vec();
    if open.len() < 3 {
        return;
    }

    let mut cx = 0i64;
    let mut cy = 0i64;

    // MoveTo first point
    push_cmd(cmds, 1, 1);
    let x0 = qx(open[0].0);
    let y0 = qy(open[0].1);
    push_zigzag(cmds, x0 - cx);
    push_zigzag(cmds, y0 - cy);
    cx = x0;
    cy = y0;

    // LineTo remaining points
    push_cmd(cmds, 2, (open.len() - 1) as u32);
    for &(lx, ly) in &open[1..] {
        let x = qx(lx);
        let y = qy(ly);
        push_zigzag(cmds, x - cx);
        push_zigzag(cmds, y - cy);
        cx = x;
        cy = y;
    }

    // ClosePath
    push_cmd(cmds, 7, 1);
}

// ── MVT command helpers ───────────────────────────────────────────────────────

#[inline]
fn push_cmd(cmds: &mut Vec<u32>, id: u32, count: u32) {
    cmds.push((id & 0x7) | (count << 3));
}

#[inline]
fn push_zigzag(cmds: &mut Vec<u32>, v: i64) {
    cmds.push(((v << 1) ^ (v >> 63)) as u32);
}

// ── property encoding ─────────────────────────────────────────────────────────

fn json_value_to_mvt(v: &serde_json::Value) -> geozero::mvt::tile::Value {
    use geozero::mvt::tile::Value;
    match v {
        serde_json::Value::Bool(b) => Value { bool_value: Some(*b), ..Default::default() },
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value { sint_value: Some(i), ..Default::default() }
            } else if let Some(f) = n.as_f64() {
                Value { double_value: Some(f), ..Default::default() }
            } else {
                Value { string_value: Some(n.to_string()), ..Default::default() }
            }
        }
        serde_json::Value::String(s) => Value { string_value: Some(s.clone()), ..Default::default() },
        other => Value { string_value: Some(other.to_string()), ..Default::default() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::point;

    #[test]
    fn encode_single_point_tile() {
        let feat = EncodableFeature {
            geom: Geometry::Point(point!(x: 0.0, y: 0.0)),
            props: serde_json::Map::new(),
        };
        let bytes = encode_tile("test", &[feat], -1.0, -1.0, 1.0, 1.0).unwrap();
        assert!(!bytes.is_empty());
    }
}

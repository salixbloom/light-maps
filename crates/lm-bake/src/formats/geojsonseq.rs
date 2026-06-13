/// GeoJSON-Seq / NDJSON adapter.
///
/// Each line is an independent GeoJSON Feature object (RS-delimited or bare).
/// We parse them one at a time from a streaming reader so a multi-GB file is
/// never held in memory all at once — only the running feature list and bbox
/// grow, bounded by the data, not the file's serialized size.
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read};

use geo_types::Geometry;
use geojson::Feature;

use crate::{
    error::BakeError,
    ingest::IngestedLayer,
    manifest::{FieldInfo, LayerInfo},
};

/// Parse a GeoJSON-Seq string. Kept for the in-memory call sites (small inputs
/// and tests); delegates to the streaming reader.
pub fn ingest_geojsonseq(name: &str, src: &str) -> Result<IngestedLayer, BakeError> {
    ingest_geojsonseq_reader(name, src.as_bytes())
}

/// Parse GeoJSON-Seq from any reader, one line (one Feature) at a time.
///
/// This is the path used for large files: the reader is wrapped in a buffered
/// reader and consumed line-by-line, so peak memory is the parsed feature set
/// plus one line's worth of text — not the whole file.
pub fn ingest_geojsonseq_reader<R: Read>(
    name: &str,
    reader: R,
) -> Result<IngestedLayer, BakeError> {
    let buf = BufReader::with_capacity(1 << 20, reader);

    let mut features = Vec::new();
    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut field_types: HashMap<String, HashSet<String>> = HashMap::new();
    let mut geom_types: HashSet<String> = HashSet::new();

    for line in buf.lines() {
        let line = line?;
        // Strip optional RS (0x1E) record separators and surrounding whitespace.
        let trimmed = line.trim_start_matches('\x1E').trim();
        if trimmed.is_empty() {
            continue;
        }

        let feat: Feature = trimmed
            .parse()
            .map_err(|e: geojson::Error| BakeError::GeoJson(e.to_string()))?;

        let geom = match feat.geometry {
            Some(g) => {
                let geo: Geometry<f64> = g
                    .try_into()
                    .map_err(|e: geojson::Error| BakeError::GeoJson(e.to_string()))?;
                geo
            }
            None => continue, // skip null-geometry features
        };

        geom_types.insert(geom_type_name(&geom).to_owned());
        update_bbox(&geom, &mut min_lon, &mut min_lat, &mut max_lon, &mut max_lat);

        let props = feat.properties.unwrap_or_default();
        for (k, v) in &props {
            field_types
                .entry(k.clone())
                .or_default()
                .insert(json_type_name(v).to_owned());
        }
        features.push((geom, props));
    }

    if features.is_empty() {
        return Err(BakeError::Empty);
    }

    let bounds = [
        min_lon.max(-180.0),
        min_lat.max(-90.0),
        max_lon.min(180.0),
        max_lat.min(90.0),
    ];

    let fields = field_types
        .into_iter()
        .map(|(name, types)| FieldInfo {
            name,
            field_type: if types.len() == 1 {
                types.into_iter().next().unwrap()
            } else {
                "mixed".to_owned()
            },
        })
        .collect();

    let layer_info = LayerInfo {
        name: name.to_owned(),
        fields,
        geometry_types: geom_types.into_iter().collect(),
    };

    Ok(IngestedLayer {
        name: name.to_owned(),
        features,
        bounds,
        layer_info,
    })
}

// ── shared helpers (mirror ingest.rs) ──────────────────────────────────────────

fn geom_type_name(g: &Geometry<f64>) -> &'static str {
    match g {
        Geometry::Point(_) => "Point",
        Geometry::MultiPoint(_) => "MultiPoint",
        Geometry::Line(_) => "LineString",
        Geometry::LineString(_) => "LineString",
        Geometry::MultiLineString(_) => "MultiLineString",
        Geometry::Polygon(_) => "Polygon",
        Geometry::MultiPolygon(_) => "MultiPolygon",
        Geometry::GeometryCollection(_) => "GeometryCollection",
        _ => "Unknown",
    }
}

fn json_type_name(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Bool(_) => "Boolean",
        serde_json::Value::Number(_) => "Number",
        serde_json::Value::String(_) => "String",
        serde_json::Value::Null => "Null",
        _ => "mixed",
    }
}

fn update_bbox(
    geom: &Geometry<f64>,
    min_lon: &mut f64,
    min_lat: &mut f64,
    max_lon: &mut f64,
    max_lat: &mut f64,
) {
    use geo::BoundingRect;
    if let Some(rect) = geom.bounding_rect() {
        *min_lon = min_lon.min(rect.min().x);
        *min_lat = min_lat.min(rect.min().y);
        *max_lon = max_lon.max(rect.max().x);
        *max_lat = max_lat.max(rect.max().y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ndjson() {
        let src = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]},"properties":{}}
{"type":"Feature","geometry":{"type":"Point","coordinates":[1,1]},"properties":{}}"#;
        let layer = ingest_geojsonseq("test", src).unwrap();
        assert_eq!(layer.features.len(), 2);
    }

    #[test]
    fn empty_input_errors() {
        assert!(ingest_geojsonseq("test", "").is_err());
    }

    #[test]
    fn strips_rs_delimiters() {
        let src = "\x1E{\"type\":\"Feature\",\"geometry\":{\"type\":\"Point\",\"coordinates\":[2,2]},\"properties\":{}}";
        let layer = ingest_geojsonseq("test", src).unwrap();
        assert_eq!(layer.features.len(), 1);
    }

    #[test]
    fn streaming_reader_matches_string_path() {
        let src = r#"{"type":"Feature","geometry":{"type":"Point","coordinates":[0,0]},"properties":{"a":1}}
{"type":"Feature","geometry":{"type":"Point","coordinates":[1,1]},"properties":{"a":2}}"#;
        let layer = ingest_geojsonseq_reader("test", src.as_bytes()).unwrap();
        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.layer_info.fields.len(), 1);
    }
}

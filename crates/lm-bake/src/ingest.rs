/// GeoJSON ingest: read a FeatureCollection and produce a flat list of features
/// with their property maps, plus a derived bounding box and attribute schema.
use std::collections::{HashMap, HashSet};

use geo_types::Geometry;
use geojson::{Feature, FeatureCollection, GeoJson};

use crate::{
    error::BakeError,
    manifest::{FieldInfo, LayerInfo},
};

pub struct IngestedLayer {
    pub name: String,
    /// (geometry in WGS84, properties)
    pub features: Vec<(Geometry<f64>, serde_json::Map<String, geojson::JsonValue>)>,
    /// Bounding box [min_lon, min_lat, max_lon, max_lat]
    pub bounds: [f64; 4],
    pub layer_info: LayerInfo,
}

pub fn ingest_geojson(name: &str, src: &str) -> Result<IngestedLayer, BakeError> {
    let gj: GeoJson = src
        .parse()
        .map_err(|e: geojson::Error| BakeError::GeoJson(e.to_string()))?;

    let fc: FeatureCollection = match gj {
        GeoJson::FeatureCollection(fc) => fc,
        GeoJson::Feature(f) => FeatureCollection {
            bbox: None,
            features: vec![f],
            foreign_members: None,
        },
        GeoJson::Geometry(g) => FeatureCollection {
            bbox: None,
            features: vec![Feature {
                bbox: None,
                geometry: Some(g),
                id: None,
                properties: None,
                foreign_members: None,
            }],
            foreign_members: None,
        },
    };

    if fc.features.is_empty() {
        return Err(BakeError::Empty);
    }

    let mut features = Vec::with_capacity(fc.features.len());
    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut field_types: HashMap<String, HashSet<String>> = HashMap::new();
    let mut geom_types: HashSet<String> = HashSet::new();

    for feat in fc.features {
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

/// CSV adapter for point data.
///
/// Expects columns named `lat`/`lon` (or `latitude`/`longitude`).
/// All other columns become feature properties.
/// Geometry type is always Point.
use std::collections::HashSet;

use geo_types::Geometry;

use crate::{
    error::BakeError,
    ingest::IngestedLayer,
    manifest::{FieldInfo, LayerInfo},
};

pub fn ingest_csv(name: &str, src: &str) -> Result<IngestedLayer, BakeError> {
    let mut rdr = csv::ReaderBuilder::new()
        .trim(csv::Trim::All)
        .from_reader(src.as_bytes());

    let headers: Vec<String> = rdr
        .headers()
        .map_err(|e| BakeError::GeoJson(e.to_string()))?
        .iter()
        .map(|s| s.to_lowercase())
        .collect();

    let lat_col = headers
        .iter()
        .position(|h| h == "lat" || h == "latitude")
        .ok_or_else(|| BakeError::GeoJson("CSV must have 'lat' or 'latitude' column".into()))?;
    let lon_col = headers
        .iter()
        .position(|h| h == "lon" || h == "lng" || h == "longitude")
        .ok_or_else(|| BakeError::GeoJson("CSV must have 'lon'/'lng' or 'longitude' column".into()))?;

    let mut features = Vec::new();
    let mut min_lon = f64::MAX;
    let mut min_lat = f64::MAX;
    let mut max_lon = f64::MIN;
    let mut max_lat = f64::MIN;
    let mut field_types: std::collections::HashMap<String, HashSet<String>> =
        std::collections::HashMap::new();

    for result in rdr.records() {
        let record = result.map_err(|e| BakeError::GeoJson(e.to_string()))?;

        let lat: f64 = record
            .get(lat_col)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| BakeError::GeoJson("invalid lat value".into()))?;
        let lon: f64 = record
            .get(lon_col)
            .and_then(|v| v.parse().ok())
            .ok_or_else(|| BakeError::GeoJson("invalid lon value".into()))?;

        min_lat = min_lat.min(lat);
        max_lat = max_lat.max(lat);
        min_lon = min_lon.min(lon);
        max_lon = max_lon.max(lon);

        let mut props = serde_json::Map::new();
        for (i, col) in headers.iter().enumerate() {
            if i == lat_col || i == lon_col {
                continue;
            }
            if let Some(val) = record.get(i) {
                let jval: serde_json::Value = val
                    .parse::<f64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| serde_json::Value::String(val.to_owned()));
                field_types
                    .entry(col.clone())
                    .or_default()
                    .insert(json_type(& jval).to_owned());
                props.insert(col.clone(), jval);
            }
        }

        features.push((Geometry::Point(geo_types::Point::new(lon, lat)), props));
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
        .map(|(n, types)| FieldInfo {
            name: n,
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
        geometry_types: vec!["Point".to_owned()],
    };

    Ok(IngestedLayer { name: name.to_owned(), features, bounds, layer_info })
}

fn json_type(v: &serde_json::Value) -> &'static str {
    match v {
        serde_json::Value::Number(_) => "Number",
        serde_json::Value::Bool(_) => "Boolean",
        serde_json::Value::String(_) => "String",
        _ => "mixed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_csv() {
        let src = "lat,lon,name\n51.5,-0.1,London\n48.8,2.35,Paris";
        let layer = ingest_csv("cities", src).unwrap();
        assert_eq!(layer.features.len(), 2);
        let (geom, props) = &layer.features[0];
        assert!(matches!(geom, Geometry::Point(_)));
        assert_eq!(props["name"], serde_json::Value::String("London".into()));
    }

    #[test]
    fn missing_lat_errors() {
        let src = "x,y\n1,2";
        assert!(ingest_csv("test", src).is_err());
    }
}

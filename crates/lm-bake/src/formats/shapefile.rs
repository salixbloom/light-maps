/// Shapefile adapter using geozero's `with-shp` feature.
///
/// Drives `ShpReader::iter_features` with a `GeoJsonWriter` to convert
/// all features to a GeoJSON FeatureCollection, then delegates to the
/// standard GeoJSON ingestor.
use geozero::geojson::GeoJsonWriter;

use crate::{error::BakeError, ingest::IngestedLayer};

pub fn ingest_shapefile(name: &str, shp_path: &str) -> Result<IngestedLayer, BakeError> {
    let mut buf = Vec::<u8>::new();
    let mut writer = GeoJsonWriter::new(&mut buf);

    let shp = geozero::shp::ShpReader::from_path(shp_path)
        .map_err(|e| BakeError::GeoJson(format!("shapefile open: {e}")))?;

    let iter = shp
        .iter_features(&mut writer)
        .map_err(|e| BakeError::GeoJson(format!("shapefile iter: {e}")))?;

    let mut count = 0usize;
    for item in iter {
        item.map_err(|e| BakeError::GeoJson(format!("shapefile record: {e}")))?;
        count += 1;
    }

    if count == 0 {
        return Err(BakeError::Empty);
    }

    drop(writer);
    let gjson_str = String::from_utf8(buf)
        .map_err(|e| BakeError::GeoJson(e.to_string()))?;

    // GeoJsonWriter emits a FeatureCollection if dataset_begin/end is called,
    // otherwise we wrap it manually.
    let src = if gjson_str.trim_start().starts_with('{') {
        gjson_str
    } else {
        format!("{{\"type\":\"FeatureCollection\",\"features\":[{}]}}", gjson_str)
    };

    crate::ingest::ingest_geojson(name, &src)
}

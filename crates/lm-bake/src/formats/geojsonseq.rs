/// GeoJSON-Seq / NDJSON adapter.
///
/// Each line is an independent GeoJSON Feature object (RS-delimited or bare).
/// We collect them into a FeatureCollection and delegate to the main GeoJSON
/// ingestor.
use crate::{error::BakeError, ingest::IngestedLayer};

pub fn ingest_geojsonseq(name: &str, src: &str) -> Result<IngestedLayer, BakeError> {
    // Strip optional RS (0x1E) record separators and blank lines.
    let features: Vec<&str> = src
        .lines()
        .map(|l| l.trim_start_matches('\x1E').trim())
        .filter(|l| !l.is_empty())
        .collect();

    if features.is_empty() {
        return Err(BakeError::Empty);
    }

    // Wrap lines into a FeatureCollection and delegate.
    let fc = format!(
        "{{\"type\":\"FeatureCollection\",\"features\":[{}]}}",
        features.join(",")
    );
    crate::ingest::ingest_geojson(name, &fc)
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
}

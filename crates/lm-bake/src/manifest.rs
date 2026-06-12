use serde::{Deserialize, Serialize};

/// Written alongside the PMTiles archive as `<name>.json`.
/// Also embedded in the PMTiles metadata block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub name: String,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// [min_lon, min_lat, max_lon, max_lat] in WGS84 degrees
    pub bounds: [f64; 4],
    /// [lon, lat, zoom]
    pub center: [f64; 3],
    pub layers: Vec<LayerInfo>,
    pub tile_compression: String,
    pub attribution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerInfo {
    pub name: String,
    pub fields: Vec<FieldInfo>,
    pub geometry_types: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldInfo {
    pub name: String,
    #[serde(rename = "type")]
    pub field_type: String,
}

impl Manifest {
    /// Serialise to JSON string for embedding in PMTiles metadata block.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap()
    }

    /// Build a TileJSON 3.0 object for serving at `/tilesets/{name}/tile.json`.
    pub fn to_tilejson(&self, tiles_url: &str) -> serde_json::Value {
        serde_json::json!({
            "tilejson": "3.0.0",
            "name": self.name,
            "minzoom": self.min_zoom,
            "maxzoom": self.max_zoom,
            "bounds": self.bounds,
            "center": self.center,
            "tiles": [tiles_url],
            "attribution": self.attribution,
            "vector_layers": self.layers.iter().map(|l| serde_json::json!({
                "id": l.name,
                "minzoom": self.min_zoom,
                "maxzoom": self.max_zoom,
                "fields": l.fields.iter().map(|f| (&f.name, &f.field_type))
                    .collect::<std::collections::HashMap<_,_>>(),
            })).collect::<Vec<_>>(),
        })
    }
}

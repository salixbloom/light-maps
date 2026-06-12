/// Human-readable archive inspection.
use crate::{pmtiles::PmtError, PmtReader};

pub struct InspectReport {
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub tile_count: u64,
    pub metadata_json: String,
    pub archive_size_bytes: u64,
}

impl InspectReport {
    pub fn print(&self) {
        println!("PMTiles archive");
        println!("  zoom range : {}-{}", self.min_zoom, self.max_zoom);
        println!("  tiles      : {}", self.tile_count);
        println!("  archive    : {} bytes ({:.1} KB)", self.archive_size_bytes, self.archive_size_bytes as f64 / 1024.0);
        println!("  metadata   :");
        // Pretty-print JSON with 4-space indent.
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&self.metadata_json) {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        } else {
            println!("{}", self.metadata_json);
        }
    }
}

pub fn inspect(path: &str) -> Result<InspectReport, PmtError> {
    let reader = PmtReader::open(path)?;
    let metadata_json = reader.metadata().unwrap_or_else(|_| "{}".to_owned());
    let archive_size_bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(InspectReport {
        min_zoom: reader.min_zoom(),
        max_zoom: reader.max_zoom(),
        tile_count: reader.tile_count(),
        metadata_json,
        archive_size_bytes,
    })
}

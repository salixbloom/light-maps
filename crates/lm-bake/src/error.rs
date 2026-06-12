use thiserror::Error;

#[derive(Debug, Error)]
pub enum BakeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid GeoJSON: {0}")]
    GeoJson(String),
    #[error("empty feature collection — nothing to bake")]
    Empty,
    #[error("mvt encode: {0}")]
    Encode(String),
    #[error("write: {0}")]
    Write(String),
}

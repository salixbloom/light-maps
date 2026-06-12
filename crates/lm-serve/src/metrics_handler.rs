/// Opt-in Prometheus metrics endpoint at GET /metrics.
///
/// Tracks:
///   lm_tile_requests_total{set, status}   — counter
///   lm_tile_request_duration_seconds{set} — histogram
///   lm_archive_tiles_total{set}           — gauge (set at startup)
use metrics::{counter, gauge, histogram};

/// Record a completed tile request.
pub fn record_tile_request(set: &str, status: u16, duration_secs: f64) {
    counter!("lm_tile_requests_total", "set" => set.to_owned(), "status" => status.to_string())
        .increment(1);
    histogram!("lm_tile_request_duration_seconds", "set" => set.to_owned())
        .record(duration_secs);
}

/// Record archive tile count at startup (informational gauge).
pub fn record_archive_size(set: &str, count: u64) {
    gauge!("lm_archive_tiles_total", "set" => set.to_owned()).set(count as f64);
}

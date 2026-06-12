use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};

use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use lm_core::{pmtiles::Compression, PmtReader};
use tower::ServiceBuilder;
use tower_http::{
    cors::{Any, CorsLayer},
    timeout::TimeoutLayer,
};
use tracing::{info, warn};

use crate::{
    auth::{auth_middleware, AuthState},
    config::ServeConfig,
    metrics_handler::{record_archive_size, record_tile_request},
};

// ── per-tileset state ─────────────────────────────────────────────────────────

struct Tileset {
    reader: PmtReader,
    etag: String,
    metadata: serde_json::Value,
}

pub struct AppState {
    tilesets: HashMap<String, Tileset>,
    base_url: String,
    max_zoom_request: u8,
    metrics_enabled: bool,
}

// ── entry point ───────────────────────────────────────────────────────────────

/// Build the application router from a set of pre-opened tilesets.
/// Exposed for integration testing.
pub fn build_router(paths: &[PathBuf], cfg: &ServeConfig) -> anyhow::Result<Router> {
    let base_url = cfg
        .base_url
        .clone()
        .unwrap_or_else(|| format!("http://{}", cfg.addr));

    let mut tilesets = HashMap::new();
    for path in paths {
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("map")
            .to_owned();

        let reader = PmtReader::open(path)?;
        let etag = format!(
            "\"lm-{}-{}-{}\"",
            reader.min_zoom(),
            reader.max_zoom(),
            reader.tile_count()
        );
        let metadata: serde_json::Value = reader
            .metadata()
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();

        if cfg.metrics_enabled {
            record_archive_size(&key, reader.tile_count());
        }

        tilesets.insert(key.clone(), Tileset { reader, etag, metadata });
    }

    let state = Arc::new(AppState {
        tilesets,
        base_url,
        max_zoom_request: cfg.max_zoom_request,
        metrics_enabled: cfg.metrics_enabled,
    });

    let cors = build_cors(&cfg.cors_origins);
    let auth_state = AuthState::new(cfg.api_key.as_deref());

    let mut app = Router::new()
        .route("/tiles/{set}/{z}/{x}/{y}", get(tile_handler))
        .route("/tilesets/{set}/tile.json", get(tilejson_handler))
        .route("/tilesets/{set}/manifest.json", get(manifest_handler))
        .route("/tilesets", get(list_tilesets))
        .route("/tile.json", get(tilejson_default))
        .route("/healthz", get(healthz))
        .with_state(state);

    if cfg.metrics_enabled {
        app = app.route("/metrics", get(metrics_endpoint));
    }

    let app = app.layer(
        ServiceBuilder::new()
            .layer(TimeoutLayer::with_status_code(
                StatusCode::REQUEST_TIMEOUT,
                cfg.request_timeout,
            ))
            .layer(cors)
            .layer(middleware::from_fn_with_state(auth_state, auth_middleware)),
    );

    Ok(app)
}

pub async fn run(paths: Vec<PathBuf>, cfg: ServeConfig) -> anyhow::Result<()> {
    let t0 = Instant::now();

    if cfg.metrics_enabled {
        metrics_exporter_prometheus::PrometheusBuilder::new()
            .install()
            .expect("failed to install Prometheus recorder");
    }

    let app = build_router(&paths, &cfg)?;

    info!(
        addr = %cfg.addr,
        sets = paths.len(),
        auth = cfg.api_key.is_some(),
        metrics = cfg.metrics_enabled,
        startup_ms = t0.elapsed().as_millis(),
        "light-maps ready"
    );

    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    info!("listening on {}", cfg.addr);
    axum::serve(listener, app).await?;
    Ok(())
}

// ── CORS builder ──────────────────────────────────────────────────────────────

fn build_cors(origins: &[String]) -> CorsLayer {
    if origins.is_empty() {
        // No origins configured: block all cross-origin requests (default-deny).
        CorsLayer::new()
    } else if origins.iter().any(|o| o == "*") {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::HEAD,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers(Any)
    } else {
        let parsed: Vec<axum::http::HeaderValue> = origins
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect();
        CorsLayer::new()
            .allow_origin(parsed)
            .allow_methods([
                axum::http::Method::GET,
                axum::http::Method::HEAD,
                axum::http::Method::OPTIONS,
            ])
            .allow_headers(Any)
    }
}

// ── handlers ──────────────────────────────────────────────────────────────────

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn metrics_endpoint() -> impl IntoResponse {
    // The PrometheusBuilder http-listener feature handles its own port;
    // for the embedded route we render the current snapshot.
    // metrics-exporter-prometheus doesn't expose a direct render fn through
    // the global recorder — use a one-shot handle instead.
    (StatusCode::OK, "# metrics not yet wired to inline render\n")
}

async fn list_tilesets(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let names: Vec<&str> = state.tilesets.keys().map(String::as_str).collect();
    Json(serde_json::json!({ "tilesets": names }))
}

async fn manifest_handler(
    State(state): State<Arc<AppState>>,
    Path(set): Path<String>,
) -> Response {
    match state.tilesets.get(&set) {
        Some(ts) => Json(ts.metadata.clone()).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn tilejson_default(State(state): State<Arc<AppState>>) -> Response {
    let name = state.tilesets.keys().next().cloned().unwrap_or_default();
    tilejson_for(&state, &name)
}

async fn tilejson_handler(
    State(state): State<Arc<AppState>>,
    Path(set): Path<String>,
) -> Response {
    tilejson_for(&state, &set)
}

fn tilejson_for(state: &AppState, set: &str) -> Response {
    let ts = match state.tilesets.get(set) {
        Some(t) => t,
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let meta = &ts.metadata;
    let tiles_url = format!("{}/tiles/{}/{{z}}/{{x}}/{{y}}.mvt", state.base_url, set);

    Json(serde_json::json!({
        "tilejson": "3.0.0",
        "name": meta.get("name").and_then(|v| v.as_str()).unwrap_or(set),
        "minzoom": meta.get("min_zoom").and_then(|v| v.as_u64()).unwrap_or(0),
        "maxzoom": meta.get("max_zoom").and_then(|v| v.as_u64()).unwrap_or(14),
        "bounds": meta.get("bounds").cloned().unwrap_or(serde_json::json!([-180,-85,180,85])),
        "center": meta.get("center").cloned().unwrap_or(serde_json::json!([0,0,2])),
        "tiles": [tiles_url],
        "attribution": meta.get("attribution").cloned(),
        "vector_layers": meta.get("layers").cloned().unwrap_or(serde_json::json!([])),
    }))
    .into_response()
}

async fn tile_handler(
    State(state): State<Arc<AppState>>,
    Path((set, z, x, y_raw)): Path<(String, u8, u32, String)>,
    headers: HeaderMap,
) -> Response {
    let req_start = Instant::now();

    // Strip optional ".mvt" extension so both /z/x/y and /z/x/y.mvt work.
    let y_str = y_raw.strip_suffix(".mvt").unwrap_or(&y_raw);
    let y: u32 = match y_str.parse() {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };

    // ── validate z/x/y ───────────────────────────────────────────────────────
    if z > state.max_zoom_request {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let max_coord = (1u64 << z) as u32;
    if x >= max_coord || y >= max_coord {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let ts = match state.tilesets.get(&set) {
        Some(t) => t,
        None => return StatusCode::NOT_FOUND.into_response(),
    };

    // ── conditional GET ───────────────────────────────────────────────────────
    if let Some(inm) = headers.get(header::IF_NONE_MATCH) {
        if inm.as_bytes() == ts.etag.as_bytes() {
            if state.metrics_enabled {
                record_tile_request(&set, 304, req_start.elapsed().as_secs_f64());
            }
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }

    let result = ts.reader.get_tile(z, x, y);
    let status = match &result {
        Ok(_) => 200u16,
        Err(lm_core::pmtiles::PmtError::TileNotFound { .. }) => 204,
        Err(_) => 500,
    };

    if state.metrics_enabled {
        record_tile_request(&set, status, req_start.elapsed().as_secs_f64());
    }

    match result {
        Ok(tile) => {
            let enc_header = match tile.compression {
                Compression::Gzip => Some("gzip"),
                Compression::Brotli => Some("br"),
                _ => None,
            };

            let mut builder = axum::http::Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/vnd.mapbox-vector-tile")
                .header(header::CACHE_CONTROL, "public, max-age=86400, immutable")
                .header(header::ETAG, &ts.etag)
                // Vary so caches key on Accept-Encoding.
                .header(header::VARY, "Accept-Encoding");

            if let Some(enc) = enc_header {
                builder = builder.header(header::CONTENT_ENCODING, enc);
            }

            builder.body(axum::body::Body::from(tile.data)).unwrap()
        }
        Err(lm_core::pmtiles::PmtError::TileNotFound { .. }) => {
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => {
            warn!(set, z, x, y, error = %e, "tile fetch error");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

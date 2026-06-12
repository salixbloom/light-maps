/// lm-example — self-contained demo of the light-maps pipeline.
///
/// Steps:
///   1. Bakes a small embedded GeoJSON (world capitals) into a PMTiles archive.
///   2. Starts lm-serve on localhost:3000 serving that archive.
///   3. Prints a URL to open the bundled demo page.
///
/// Run with:
///   cargo run -p lm-example
///
/// Then open http://localhost:3000/demo in your browser,
/// or point any MapLibre map at http://localhost:3000/tile.json.
use std::{
    fs,
    net::TcpListener,
    path::PathBuf,
    time::Duration,
};

use anyhow::Context;
use lm_bake::{
    bake,
    BakeConfig,
    pipeline::TileCompression,
};
use lm_serve::{build_router, ServeConfig};
use tracing::info;

// ── embedded data ─────────────────────────────────────────────────────────────

/// A hand-picked selection of world capitals as a GeoJSON FeatureCollection.
/// Each feature carries a `name` and `country` property.
const CAPITALS_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-0.1276, 51.5074] }, "properties": { "name": "London",       "country": "United Kingdom" } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [2.3522, 48.8566]  }, "properties": { "name": "Paris",        "country": "France"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [13.4050, 52.5200] }, "properties": { "name": "Berlin",       "country": "Germany"        } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [37.6173, 55.7558] }, "properties": { "name": "Moscow",       "country": "Russia"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-77.0369, 38.9072]}, "properties": { "name": "Washington DC","country": "USA"            } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-43.1729,-22.9068]}, "properties": { "name": "Rio de Janeiro","country": "Brazil"        } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [116.4074, 39.9042]}, "properties": { "name": "Beijing",      "country": "China"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [139.6917, 35.6895]}, "properties": { "name": "Tokyo",        "country": "Japan"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [28.9784, 41.0082] }, "properties": { "name": "Istanbul",     "country": "Turkey"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [31.2357, 30.0444] }, "properties": { "name": "Cairo",        "country": "Egypt"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [18.4241,-33.9249] }, "properties": { "name": "Cape Town",    "country": "South Africa"   } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [72.8777, 19.0760] }, "properties": { "name": "Mumbai",       "country": "India"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [103.8198, 1.3521] }, "properties": { "name": "Singapore",    "country": "Singapore"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [151.2093,-33.8688]}, "properties": { "name": "Sydney",       "country": "Australia"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-58.3816,-34.6037]}, "properties": { "name": "Buenos Aires", "country": "Argentina"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-99.1332, 19.4326]}, "properties": { "name": "Mexico City",  "country": "Mexico"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [55.2708, 25.2048] }, "properties": { "name": "Dubai",        "country": "UAE"            } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [126.9780, 37.5665]}, "properties": { "name": "Seoul",        "country": "South Korea"    } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-47.9218,-15.7801]}, "properties": { "name": "Brasilia",     "country": "Brazil"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-3.7038, 40.4168] }, "properties": { "name": "Madrid",       "country": "Spain"          } }
  ]
}"#;

// ── embedded demo HTML ────────────────────────────────────────────────────────

/// Minimal self-contained demo page served at GET /demo.
/// Loads MapLibre from CDN, hits /tile.json, and renders capitals as circles.
const DEMO_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1.0"/>
  <title>light-maps demo</title>
  <link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.css"/>
  <style>
    * { margin:0; padding:0; box-sizing:border-box }
    body { background:#0d1117; color:#c9d1d9; font-family:system-ui,sans-serif }
    #map { width:100vw; height:100vh }
    #info {
      position:fixed; top:16px; left:16px; z-index:10;
      background:rgba(13,17,23,.85); border:1px solid #30363d;
      border-radius:8px; padding:14px 16px; max-width:260px;
      backdrop-filter:blur(6px);
    }
    #info h1 { font-size:15px; font-weight:600; color:#58a6ff; margin-bottom:6px }
    #info p  { font-size:12px; color:#8b949e; line-height:1.5 }
    .maplibregl-popup-content { background:#161b22; color:#c9d1d9; border:1px solid #30363d; border-radius:6px; padding:10px 14px; font-size:13px }
    .maplibregl-popup-tip { border-top-color:#30363d }
  </style>
</head>
<body>
  <div id="info">
    <h1>light-maps</h1>
    <p>World capitals served as vector tiles from a local PMTiles archive.<br><br>Click any dot for details.</p>
  </div>
  <div id="map"></div>
  <script src="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.js"></script>
  <script>
    const map = new maplibregl.Map({
      container: 'map',
      style: {
        version: 8,
        glyphs: 'https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf',
        sources: {
          capitals: {
            type: 'vector',
            url: '/tile.json',
          }
        },
        layers: [
          {
            id: 'background',
            type: 'background',
            paint: { 'background-color': '#0d1117' }
          },
          {
            id: 'capitals-circle',
            type: 'circle',
            source: 'capitals',
            'source-layer': 'capitals',
            paint: {
              'circle-radius': ['interpolate', ['linear'], ['zoom'], 0, 3, 6, 7],
              'circle-color': '#58a6ff',
              'circle-stroke-width': 1.5,
              'circle-stroke-color': '#161b22'
            }
          },
          {
            id: 'capitals-label',
            type: 'symbol',
            source: 'capitals',
            'source-layer': 'capitals',
            minzoom: 3,
            layout: {
              'text-field': ['get', 'name'],
              'text-font': ['Open Sans Regular'],
              'text-size': 11,
              'text-offset': [0, 1.2],
              'text-anchor': 'top',
            },
            paint: {
              'text-color': '#c9d1d9',
              'text-halo-color': '#0d1117',
              'text-halo-width': 1
            }
          }
        ]
      },
      center: [20, 20],
      zoom: 1.5,
    });

    map.on('click', 'capitals-circle', e => {
      const p = e.features[0].properties;
      new maplibregl.Popup()
        .setLngLat(e.lngLat)
        .setHTML(`<strong>${p.name}</strong><br/>${p.country}`)
        .addTo(map);
    });

    map.on('mouseenter', 'capitals-circle', () => map.getCanvas().style.cursor = 'pointer');
    map.on('mouseleave', 'capitals-circle', () => map.getCanvas().style.cursor = '');
  </script>
</body>
</html>"#;

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lm_example=info,lm_serve=info".into()),
        )
        .init();

    // ── 1. pick a free port (or default 3000) ─────────────────────────────────
    let port = free_port(3000);
    let addr = format!("127.0.0.1:{port}");

    // ── 2. bake the embedded GeoJSON into a temp PMTiles file ─────────────────
    info!("baking world capitals → PMTiles …");

    let output = bake(
        CAPITALS_GEOJSON,
        BakeConfig {
            layer_name: "capitals".into(),
            min_zoom: 0,
            max_zoom: 6,
            attribution: Some("light-maps example".into()),
            compression: TileCompression::Gzip,
            tolerance_factor: 1.0,
        },
    )
    .context("bake failed")?;

    info!(
        tiles = output.tile_count,
        bytes = output.archive_bytes,
        "bake complete"
    );

    // Write to a temp file — lm-serve needs a path it can mmap.
    let tmp_dir = std::env::temp_dir().join("lm-example");
    fs::create_dir_all(&tmp_dir)?;
    let pmtiles_path: PathBuf = tmp_dir.join("capitals.pmtiles");
    fs::write(&pmtiles_path, &output.pmtiles_bytes)
        .context("failed to write temp PMTiles file")?;

    info!(path = %pmtiles_path.display(), "written PMTiles archive");

    // ── 3. build the router with the demo /demo route appended ────────────────
    let cfg = ServeConfig {
        addr: addr.clone(),
        base_url: Some(format!("http://{addr}")),
        cors_origins: vec!["*".into()],
        request_timeout: Duration::from_secs(10),
        ..ServeConfig::default()
    };

    let router = build_router(&[pmtiles_path], &cfg)?;

    // Attach the /demo route directly on top of the tile router.
    let app = router.route("/demo", axum::routing::get(demo_handler));

    // ── 4. start the server ───────────────────────────────────────────────────
    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!();
    println!("  light-maps example server running");
    println!();
    println!("  Demo page  →  http://{addr}/demo");
    println!("  TileJSON   →  http://{addr}/tile.json");
    println!("  Tiles      →  http://{addr}/tiles/capitals/{{z}}/{{x}}/{{y}}.mvt");
    println!();
    println!("  Press Ctrl-C to stop.");
    println!();

    axum::serve(listener, app).await?;
    Ok(())
}

async fn demo_handler() -> impl axum::response::IntoResponse {
    axum::response::Html(DEMO_HTML)
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Return `preferred` if it is free, otherwise find a random free port.
fn free_port(preferred: u16) -> u16 {
    if TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return preferred;
    }
    // Bind to port 0 — OS assigns a free one.
    let l = TcpListener::bind("127.0.0.1:0").expect("no free port");
    l.local_addr().unwrap().port()
}

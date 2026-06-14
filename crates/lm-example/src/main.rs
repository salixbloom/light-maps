/// lm-example — demo / local-testing server for light-maps.
///
/// Modes
/// ─────
///   (default)           Bakes the built-in world-capitals GeoJSON and serves it.
///   --file <path>       Serve an existing .pmtiles file directly (no bake).
///   --bake <geojson>    Bake an arbitrary GeoJSON/GeoJSONSeq file and serve it.
///
/// Common flags
/// ────────────
///   --port <n>          Listen port (default: 3000, auto-increments if busy).
///   --layer <name>      Source-layer name for the demo page (default: inferred).
///   --max-zoom <n>      Max zoom when baking (default: 6).
///   --open              Open the demo page in the default browser after start.
///   --no-demo           Skip the /demo HTML page (serve tiles only).
///
/// Examples
/// ────────
///   cargo run -p lm-example
///   cargo run -p lm-example -- --file /tmp/wa_parcels.pmtiles --layer parcels --open
///   cargo run -p lm-example -- --bake ~/data/roads.geojson --layer roads --max-zoom 14
use std::{fs, net::TcpListener, path::PathBuf, time::Duration};

use anyhow::{bail, Context};
use clap::Parser;
use lm_bake::{bake, BakeConfig, pipeline::TileCompression};
use lm_serve::{build_router, ServeConfig};
use tracing::info;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "lm-example", about = "light-maps demo / local testing server")]
struct Cli {
    /// Serve an existing .pmtiles file (skips baking).
    #[arg(long, value_name = "PATH", conflicts_with = "bake")]
    file: Option<PathBuf>,

    /// Bake an arbitrary GeoJSON / GeoJSONSeq file then serve it.
    #[arg(long, value_name = "PATH", conflicts_with = "file")]
    bake: Option<PathBuf>,

    /// Layer name used in the demo page. Defaults to the file stem or "capitals".
    #[arg(long, value_name = "NAME")]
    layer: Option<String>,

    /// Listen port. Falls back to the next free port if busy.
    #[arg(long, default_value_t = 3000, value_name = "PORT")]
    port: u16,

    /// Maximum zoom level when baking (ignored with --file).
    #[arg(long, default_value_t = 6, value_name = "N")]
    max_zoom: u8,

    /// Minimum zoom level when baking (ignored with --file).
    #[arg(long, default_value_t = 0, value_name = "N")]
    min_zoom: u8,

    /// Open the demo page in the default browser after the server starts.
    #[arg(long)]
    open: bool,

    /// Only serve tiles; do not mount the /demo HTML page.
    #[arg(long)]
    no_demo: bool,
}

// ── embedded data ─────────────────────────────────────────────────────────────

const CAPITALS_GEOJSON: &str = r#"{
  "type": "FeatureCollection",
  "features": [
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-0.1276, 51.5074] }, "properties": { "name": "London",        "country": "United Kingdom" } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [2.3522, 48.8566]  }, "properties": { "name": "Paris",         "country": "France"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [13.4050, 52.5200] }, "properties": { "name": "Berlin",        "country": "Germany"        } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [37.6173, 55.7558] }, "properties": { "name": "Moscow",        "country": "Russia"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-77.0369, 38.9072]}, "properties": { "name": "Washington DC", "country": "USA"            } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-43.1729,-22.9068]}, "properties": { "name": "Rio de Janeiro","country": "Brazil"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [116.4074, 39.9042]}, "properties": { "name": "Beijing",       "country": "China"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [139.6917, 35.6895]}, "properties": { "name": "Tokyo",         "country": "Japan"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [28.9784, 41.0082] }, "properties": { "name": "Istanbul",      "country": "Turkey"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [31.2357, 30.0444] }, "properties": { "name": "Cairo",         "country": "Egypt"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [18.4241,-33.9249] }, "properties": { "name": "Cape Town",     "country": "South Africa"   } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [72.8777, 19.0760] }, "properties": { "name": "Mumbai",        "country": "India"          } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [103.8198, 1.3521] }, "properties": { "name": "Singapore",     "country": "Singapore"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [151.2093,-33.8688]}, "properties": { "name": "Sydney",        "country": "Australia"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-58.3816,-34.6037]}, "properties": { "name": "Buenos Aires",  "country": "Argentina"      } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-99.1332, 19.4326]}, "properties": { "name": "Mexico City",   "country": "Mexico"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [55.2708, 25.2048] }, "properties": { "name": "Dubai",         "country": "UAE"            } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [126.9780, 37.5665]}, "properties": { "name": "Seoul",         "country": "South Korea"    } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-47.9218,-15.7801]}, "properties": { "name": "Brasilia",      "country": "Brazil"         } },
    { "type": "Feature", "geometry": { "type": "Point", "coordinates": [-3.7038, 40.4168] }, "properties": { "name": "Madrid",        "country": "Spain"          } }
  ]
}"#;

// ── demo HTML template ────────────────────────────────────────────────────────

fn demo_html(layer: &str, addr: &str) -> String {
    // Render a generic polygon/line/point style that works for any layer.
    // The tile.json URL is absolute so the browser can reach it from the page.
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8"/>
  <meta name="viewport" content="width=device-width, initial-scale=1.0"/>
  <title>light-maps — {layer}</title>
  <link rel="stylesheet" href="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.css"/>
  <style>
    * {{ margin:0; padding:0; box-sizing:border-box }}
    body {{ background:#0d1117; color:#c9d1d9; font-family:system-ui,sans-serif }}
    #map {{ width:100vw; height:100vh }}
    #info {{
      position:fixed; top:16px; left:16px; z-index:10;
      background:rgba(13,17,23,.85); border:1px solid #30363d;
      border-radius:8px; padding:14px 16px; max-width:280px;
      backdrop-filter:blur(6px);
    }}
    #info h1 {{ font-size:15px; font-weight:600; color:#58a6ff; margin-bottom:4px }}
    #info p  {{ font-size:12px; color:#8b949e; line-height:1.5 }}
    #info code {{ font-size:11px; color:#79c0ff; background:#161b22; padding:1px 4px; border-radius:3px }}
    #jump {{
      margin-top:10px; padding-top:10px; border-top:1px solid #21262d;
    }}
    #jump label {{ display:block; font-size:11px; color:#8b949e; margin-bottom:5px }}
    #jump-row {{ display:flex; gap:5px }}
    #jump-input {{
      flex:1; min-width:0;
      background:#0d1117; border:1px solid #30363d; border-radius:5px;
      color:#c9d1d9; font-size:12px; padding:4px 8px;
      outline:none;
    }}
    #jump-input:focus {{ border-color:#58a6ff }}
    #jump-input.error {{ border-color:#f85149 }}
    #jump-btn {{
      background:#21262d; border:1px solid #30363d; border-radius:5px;
      color:#c9d1d9; font-size:12px; padding:4px 9px; cursor:pointer;
      white-space:nowrap;
    }}
    #jump-btn:hover {{ background:#30363d }}
    .maplibregl-popup-content {{
      background:#161b22; color:#c9d1d9; border:1px solid #30363d;
      border-radius:6px; padding:10px 14px; font-size:12px; max-width:280px;
      word-break:break-word;
    }}
    .maplibregl-popup-tip {{ border-top-color:#30363d }}
    #props {{ margin-top:6px; display:grid; grid-template-columns:auto 1fr; gap:2px 8px }}
    #props .k {{ color:#8b949e; white-space:nowrap }}
    #props .v {{ color:#c9d1d9 }}
  </style>
</head>
<body>
  <div id="info">
    <h1>light-maps</h1>
    <p id="layer-label">Loading…<br></p>
    <p><a href="http://{addr}/tile.json" style="color:#79c0ff;font-size:11px">tile.json</a></p>
    <p style="margin-top:4px;font-size:12px;color:#8b949e">Click any feature for properties.</p>
    <div id="jump">
      <label for="jump-input">Jump to coordinate</label>
      <div id="jump-row">
        <input id="jump-input" type="text" placeholder="lat, lng  or  lng, lat" spellcheck="false"/>
        <button id="jump-btn">Go</button>
      </div>
    </div>
  </div>
  <div id="map"></div>
  <script src="https://unpkg.com/maplibre-gl@4/dist/maplibre-gl.js"></script>
  <script>
    const TILE_JSON_URL = 'http://{addr}/tile.json';

    // Fetch tile.json first so we get the real layer names, bounds, and center
    // from the archive rather than guessing them from the filename.
    fetch(TILE_JSON_URL)
      .then(r => r.json())
      .then(tj => initMap(tj))
      .catch(e => document.getElementById('layer-label').textContent = 'Error: ' + e);

    function initMap(tj) {{
      const layers = (tj.vector_layers || []).map(l => l.id);
      const firstLayer = layers[0] || 'layer';

      document.getElementById('layer-label').innerHTML =
        layers.map(l => `Layer: <code>${{l}}</code>`).join('<br>');

      // Use center/bounds from tile.json if available.
      const center = tj.center ? [tj.center[0], tj.center[1]] : [0, 20];
      const initZoom = tj.center ? tj.center[2] : 1.5;

      // Build one set of style layers per vector layer in the archive.
      const PALETTE = ['#58a6ff','#f78166','#3fb950','#d2a8ff','#ffa657'];
      const styleLayers = [
        {{ id: 'background', type: 'background', paint: {{ 'background-color': '#0d1117' }} }},
      ];
      layers.forEach((lyr, i) => {{
        const color = PALETTE[i % PALETTE.length];
        styleLayers.push(
          {{ id: `fill-${{lyr}}`, type: 'fill', source: 'data', 'source-layer': lyr,
             paint: {{ 'fill-color': color, 'fill-opacity': 0.25 }} }},
          {{ id: `line-${{lyr}}`, type: 'line', source: 'data', 'source-layer': lyr,
             paint: {{ 'line-color': color, 'line-width': 1 }} }},
          {{ id: `circle-${{lyr}}`, type: 'circle', source: 'data', 'source-layer': lyr,
             filter: ['==', ['geometry-type'], 'Point'],
             paint: {{
               'circle-radius': ['interpolate', ['linear'], ['zoom'], 0, 3, 8, 7],
               'circle-color': color,
               'circle-stroke-width': 1.5,
               'circle-stroke-color': '#161b22'
             }} }},
        );
      }});

      const map = new maplibregl.Map({{
        container: 'map',
        style: {{
          version: 8,
          glyphs: 'https://demotiles.maplibre.org/font/{{fontstack}}/{{range}}.pbf',
          sources: {{ data: {{ type: 'vector', url: TILE_JSON_URL }} }},
          layers: styleLayers,
        }},
        center,
        zoom: initZoom,
      }});

      // Fit to bounds once the map is ready, if tile.json provides them.
      if (tj.bounds) {{
        map.on('load', () => {{
          map.fitBounds(
            [[tj.bounds[0], tj.bounds[1]], [tj.bounds[2], tj.bounds[3]]],
            {{ padding: 40, duration: 0 }}
          );
        }});
      }}

      map.addControl(new maplibregl.NavigationControl(), 'top-right');
      map.addControl(new maplibregl.ScaleControl(), 'bottom-right');

      const CLICKABLE = layers.flatMap(l => [`fill-${{l}}`, `line-${{l}}`, `circle-${{l}}`]);

      CLICKABLE.forEach(id => {{
        map.on('click', id, e => {{
          const p = e.features[0].properties;
          const rows = Object.entries(p)
            .map(([k, v]) => `<span class="k">${{k}}</span><span class="v">${{v}}</span>`)
            .join('');
          new maplibregl.Popup({{ maxWidth: '320px' }})
            .setLngLat(e.lngLat)
            .setHTML(`<div id="props">${{rows}}</div>`)
            .addTo(map);
        }});
        map.on('mouseenter', id, () => map.getCanvas().style.cursor = 'pointer');
        map.on('mouseleave', id, () => map.getCanvas().style.cursor = '');
      }});

      // ── coordinate jump ────────────────────────────────────────────────────
      const jumpInput = document.getElementById('jump-input');
      const jumpBtn   = document.getElementById('jump-btn');

      function parseCoord(raw) {{
        const parts = raw.trim().split(/[\s,]+/).filter(Boolean);
        if (parts.length < 2) return null;
        const a = parseFloat(parts[0]);
        const b = parseFloat(parts[1]);
        if (isNaN(a) || isNaN(b)) return null;
        if (Math.abs(a) > 90) return {{ lng: a, lat: b }};
        if (Math.abs(b) > 90) return {{ lng: b, lat: a }};
        return {{ lng: b, lat: a }};
      }}

      function doJump() {{
        const coord = parseCoord(jumpInput.value);
        if (!coord) {{
          jumpInput.classList.add('error');
          jumpInput.title = 'Enter two numbers, e.g. 47.6, -122.3';
          return;
        }}
        jumpInput.classList.remove('error');
        jumpInput.title = '';
        map.flyTo({{ center: [coord.lng, coord.lat], zoom: Math.max(map.getZoom(), 10), duration: 800 }});
      }}

      jumpBtn.addEventListener('click', doJump);
      jumpInput.addEventListener('keydown', e => {{ if (e.key === 'Enter') doJump(); }});
      jumpInput.addEventListener('input', () => jumpInput.classList.remove('error'));
    }} // end initMap
  </script>
</body>
</html>"#,
        layer = layer,
        addr = addr,
    )
}

// ── main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lm_example=info,lm_serve=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let port = free_port(cli.port);
    let addr = format!("127.0.0.1:{port}");

    // ── resolve the .pmtiles path ─────────────────────────────────────────────
    let (pmtiles_path, layer_name) = match (&cli.file, &cli.bake) {
        // ── mode 1: serve an existing .pmtiles file ───────────────────────────
        (Some(path), _) => {
            if !path.exists() {
                bail!("file not found: {}", path.display());
            }
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext != "pmtiles" {
                bail!(
                    "expected a .pmtiles file, got .{ext}. \
                     To bake first use --bake instead of --file."
                );
            }
            let layer = cli.layer.clone().unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("layer")
                    .to_owned()
            });
            info!(path = %path.display(), layer = %layer, "serving existing PMTiles archive");
            (path.clone(), layer)
        }

        // ── mode 2: bake an arbitrary GeoJSON / GeoJSONSeq file ──────────────
        (_, Some(src)) => {
            if !src.exists() {
                bail!("file not found: {}", src.display());
            }
            let layer = cli.layer.clone().unwrap_or_else(|| {
                src.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("layer")
                    .to_owned()
            });
            let geojson =
                fs::read_to_string(src).context("failed to read GeoJSON file")?;
            bake_to_temp(&geojson, &layer, cli.min_zoom, cli.max_zoom)?
        }

        // ── mode 3: default — bake embedded capitals ──────────────────────────
        (None, None) => {
            let layer = cli.layer.clone().unwrap_or_else(|| "capitals".into());
            bake_to_temp(CAPITALS_GEOJSON, &layer, cli.min_zoom, cli.max_zoom)?
        }
    };

    // ── build and start the server ────────────────────────────────────────────
    let cfg = ServeConfig {
        addr: addr.clone(),
        base_url: Some(format!("http://{addr}")),
        cors_origins: vec!["*".into()],
        request_timeout: Duration::from_secs(30),
        ..ServeConfig::default()
    };

    let router = build_router(&[pmtiles_path], &cfg)?;

    let app = if cli.no_demo {
        router
    } else {
        let html = demo_html(&layer_name, &addr);
        router.route(
            "/demo",
            axum::routing::get(move || async move { axum::response::Html(html) }),
        )
    };

    let listener = tokio::net::TcpListener::bind(&addr).await?;

    println!();
    println!("  light-maps  ·  layer: {layer_name}");
    println!();
    if !cli.no_demo {
        println!("  Demo page  →  http://{addr}/demo");
    }
    println!("  TileJSON   →  http://{addr}/tile.json");
    println!("  Tiles      →  http://{addr}/tiles/{layer_name}/{{z}}/{{x}}/{{y}}.mvt");
    println!();
    println!("  Press Ctrl-C to stop.");
    println!();

    if cli.open {
        let url = format!("http://{addr}/demo");
        open_browser(&url);
    }

    axum::serve(listener, app).await?;
    Ok(())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn bake_to_temp(
    geojson: &str,
    layer: &str,
    min_zoom: u8,
    max_zoom: u8,
) -> anyhow::Result<(PathBuf, String)> {
    info!(layer = %layer, min_zoom, max_zoom, "baking …");
    let output = bake(
        geojson,
        BakeConfig {
            layer_name: layer.into(),
            min_zoom,
            max_zoom,
            attribution: Some("light-maps example".into()),
            compression: TileCompression::Gzip,
            tolerance_factor: 1.0,
            keep_fields: None,
        },
    )
    .context("bake failed")?;

    info!(
        tiles = output.tile_count,
        bytes = output.archive_bytes,
        "bake complete"
    );

    let tmp_dir = std::env::temp_dir().join("lm-example");
    fs::create_dir_all(&tmp_dir)?;
    let path = tmp_dir.join(format!("{layer}.pmtiles"));
    fs::write(&path, &output.pmtiles_bytes)
        .context("failed to write temp PMTiles file")?;
    info!(path = %path.display(), "written PMTiles archive");

    Ok((path, layer.to_owned()))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "linux")]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/c", "start", url])
        .spawn();
}

/// Return `preferred` if free, else find a random free port.
fn free_port(preferred: u16) -> u16 {
    if TcpListener::bind(("127.0.0.1", preferred)).is_ok() {
        return preferred;
    }
    TcpListener::bind("127.0.0.1:0")
        .expect("no free port")
        .local_addr()
        .unwrap()
        .port()
}

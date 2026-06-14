use std::{path::PathBuf, process};

use std::collections::HashSet;

use lm_bake::{
    estimate::estimate_input,
    formats::{
        csv::ingest_csv,
        geojsonseq::{ingest_geojsonseq, ingest_geojsonseq_reader},
        shapefile::ingest_shapefile,
    },
    ingest::{ingest_geojson, IngestedLayer},
    interactive::{pick_fields, Choice},
    pipeline::{bake_layer, bake_multi, BakeConfig, LayerInput, TileCompression},
    prepare::{prepare_input, Prepared},
    streaming::bake_layer_streaming,
};
use lm_core::inspect::inspect;
use tracing::{error, info};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lm_bake=info".into()),
        )
        .init();

    if let Err(e) = run(std::env::args().skip(1).collect()) {
        error!("{e}");
        process::exit(1);
    }
}

fn run(args: Vec<String>) -> anyhow::Result<()> {
    // ── parse args ────────────────────────────────────────────────────────────
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        print_usage();
        return Ok(());
    }

    // `lm-bake inspect <file.pmtiles>`
    if args[0] == "inspect" {
        let path = args.get(1).ok_or_else(|| anyhow::anyhow!("inspect requires a path"))?;
        let report = inspect(path)?;
        report.print();
        return Ok(());
    }

    // `lm-bake <input> [input2 ...] [flags]`
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut layer_names: Vec<String> = Vec::new();
    let mut min_zoom: u8 = 0;
    let mut max_zoom: u8 = 14;
    let mut compression = TileCompression::Gzip;
    let mut attribution: Option<String> = None;
    let mut output: PathBuf = PathBuf::from("out.pmtiles");
    let mut tolerance_factor: f64 = 1.0;
    let mut interactive = false;
    let mut include_fields: Option<Vec<String>> = None;
    let mut exclude_fields: Vec<String> = Vec::new();

    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--layer" | "--layers" => {
                i += 1;
                let val = next_arg(&args, i, "--layer")?;
                // Accept comma-separated list: --layers roads,buildings
                for name in val.split(',') {
                    layer_names.push(name.trim().to_owned());
                }
            }
            "--min-zoom" => {
                i += 1;
                min_zoom = next_arg(&args, i, "--min-zoom")?.parse()
                    .map_err(|_| anyhow::anyhow!("--min-zoom must be 0-30"))?;
            }
            "--max-zoom" => {
                i += 1;
                max_zoom = next_arg(&args, i, "--max-zoom")?.parse()
                    .map_err(|_| anyhow::anyhow!("--max-zoom must be 0-30"))?;
            }
            "--compression" => {
                i += 1;
                compression = match next_arg(&args, i, "--compression")?.as_str() {
                    "gzip"   => TileCompression::Gzip,
                    "brotli" => TileCompression::Brotli,
                    other    => anyhow::bail!("--compression must be gzip or brotli, got {other}"),
                };
            }
            "--attribution" => {
                i += 1;
                attribution = Some(next_arg(&args, i, "--attribution")?);
            }
            "--tolerance" => {
                i += 1;
                tolerance_factor = next_arg(&args, i, "--tolerance")?.parse()
                    .map_err(|_| anyhow::anyhow!("--tolerance must be a number"))?;
            }
            "-o" | "--output" => {
                i += 1;
                output = PathBuf::from(next_arg(&args, i, "-o")?);
            }
            "--interactive" => {
                interactive = true;
            }
            "--include-fields" => {
                i += 1;
                let val = next_arg(&args, i, "--include-fields")?;
                include_fields = Some(
                    val.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect(),
                );
            }
            "--exclude-fields" => {
                i += 1;
                let val = next_arg(&args, i, "--exclude-fields")?;
                exclude_fields.extend(
                    val.split(',').map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()),
                );
            }
            flag if flag.starts_with('-') => {
                anyhow::bail!("unknown flag: {flag}");
            }
            path => {
                inputs.push(PathBuf::from(path));
            }
        }
        i += 1;
    }

    if inputs.is_empty() {
        print_usage();
        anyhow::bail!("no input files provided");
    }

    // Backfill layer names from file stems if not provided.
    while layer_names.len() < inputs.len() {
        let stem = inputs[layer_names.len()]
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("layer")
            .to_owned();
        layer_names.push(stem);
    }

    // ── prepare ─────────────────────────────────────────────────────────────────
    // Detect inputs that can't be baked memory-safely as-is (too large, or in a
    // non-WGS84 CRS) and normalize them to streamable WGS84 GeoJSON-Seq via
    // ogr2ogr. The returned `Prepared` owns any temp file (deleted on drop).
    info!("preparing {} input(s) …", inputs.len());
    let prepared: Vec<Prepared> = inputs
        .iter()
        .zip(&layer_names)
        .map(|(path, name)| prepare_input(path, name))
        .collect::<anyhow::Result<_>>()?;

    // ── field selection ─────────────────────────────────────────────────────────
    // Resolve which property fields to keep. Precedence:
    //   1. --interactive  → sample the input, show the picker, use its result
    //   2. --include-fields → keep exactly that set
    //   3. --exclude-fields → keep everything except that set (needs the field
    //      list, so we sample the input to discover field names)
    //   4. neither          → keep all (keep_fields = None)
    let keep_fields = resolve_keep_fields(
        &prepared[0].path,
        interactive,
        include_fields,
        &exclude_fields,
    )?;

    let cfg = BakeConfig {
        layer_name: layer_names[0].clone(),
        min_zoom,
        max_zoom,
        attribution,
        compression,
        tolerance_factor,
        keep_fields,
    };

    // ── bake ──────────────────────────────────────────────────────────────────
    info!(layers = inputs.len(), min_zoom, max_zoom, "baking …");

    let result = if prepared.len() == 1 {
        let path = &prepared[0].path;
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        if matches!(ext.as_str(), "geojsonl" | "geojsons" | "ndjson") {
            // Line-delimited input → memory-bounded streaming bake. Tiles are
            // written directly to the output file as produced; no Vec<TileEntry>
            // buffer grows in RAM.
            let input_file = std::fs::File::open(path)?;
            let store_path = streaming_store_path(&layer_names[0], "store");
            let tile_tmp   = streaming_store_path(&layer_names[0], "tiles");
            let mut out_file = std::fs::File::create(&output)?;
            let result = bake_layer_streaming(input_file, &cfg, store_path, tile_tmp, &mut out_file)
                .map_err(|e| anyhow::anyhow!("bake failed: {e}"))?;
            info!(
                tiles  = result.tile_count,
                bytes  = result.archive_bytes,
                output = %output.display(),
                "done"
            );
            // prepared (temp files) dropped here.
            return Ok(());
            #[allow(unreachable_code)]
            result
        } else {
            // Single small layer: ingest to an IngestedLayer and bake in memory —
            // no GeoJSON-string round-trip.
            let layer = ingest_to_layer(path, &layer_names[0])?;
            bake_layer(layer, cfg).map_err(|e| anyhow::anyhow!("bake failed: {e}"))?
        }
    } else {
        // Multi-layer: the layer-merge path needs GeoJSON strings. Read each
        // prepared input into a string and hand them to bake_multi.
        let geojson_strings: Vec<String> = prepared
            .iter()
            .zip(&layer_names)
            .map(|(p, name)| ingest_to_geojson(&p.path, name))
            .collect::<anyhow::Result<_>>()?;

        let layer_inputs: Vec<LayerInput<'_>> = layer_names
            .iter()
            .zip(&geojson_strings)
            .map(|(name, geojson)| LayerInput { name, geojson })
            .collect();

        bake_multi(&layer_inputs, cfg).map_err(|e| anyhow::anyhow!("bake failed: {e}"))?
    };

    // ── write output ──────────────────────────────────────────────────────────
    std::fs::write(&output, &result.pmtiles_bytes)?;

    info!(
        tiles  = result.tile_count,
        bytes  = result.archive_bytes,
        output = %output.display(),
        "done"
    );

    // `prepared` (and any temp files it owns) dropped here.
    Ok(())
}

/// Ingest any supported format directly into an `IngestedLayer`, streaming
/// line-delimited GeoJSON so large files never load whole.
fn ingest_to_layer(path: &PathBuf, layer_name: &str) -> anyhow::Result<IngestedLayer> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    info!(file = %path.display(), format = %ext, "reading");

    let layer = match ext.as_str() {
        // Streaming path — one feature per line, never the whole file in memory.
        "geojsonl" | "geojsons" | "ndjson" => {
            let file = std::fs::File::open(path)?;
            ingest_geojsonseq_reader(layer_name, file).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "geojson" | "json" => {
            let src = std::fs::read_to_string(path)?;
            ingest_geojson(layer_name, &src).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "csv" => {
            let src = std::fs::read_to_string(path)?;
            ingest_csv(layer_name, &src).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "shp" => {
            let path_str = path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;
            ingest_shapefile(layer_name, path_str).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        other => anyhow::bail!(
            "unsupported format '.{other}' — supported: geojson, geojsonl, csv, shp\n\
             Tip: convert first with `ogr2ogr -f GeoJSONSeq out.geojsonl input.{other}`"
        ),
    };

    Ok(layer)
}

/// Temp path for streaming bake scratch files (feature store or tile buffer).
fn streaming_store_path(layer_name: &str, kind: &str) -> PathBuf {
    let safe: String = layer_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("lm-bake-{kind}-{safe}-{pid}-{nanos}.bin"))
}

// ── format dispatch ───────────────────────────────────────────────────────────

/// Read any supported format and return its content as a GeoJSON string.
fn ingest_to_geojson(path: &PathBuf, layer_name: &str) -> anyhow::Result<String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("path is not valid UTF-8"))?;

    info!(file = %path.display(), format = %ext, "reading");

    let layer = match ext.as_str() {
        "geojson" | "json" => {
            let src = std::fs::read_to_string(path)?;
            ingest_geojson(layer_name, &src)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "geojsonl" | "geojsons" | "ndjson" => {
            let src = std::fs::read_to_string(path)?;
            ingest_geojsonseq(layer_name, &src)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "csv" => {
            let src = std::fs::read_to_string(path)?;
            ingest_csv(layer_name, &src)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
        "shp" => {
            ingest_shapefile(layer_name, path_str)
                .map_err(|e| anyhow::anyhow!("{e}"))?
        }
        other => anyhow::bail!(
            "unsupported format '.{other}' — supported: geojson, geojsonl, csv, shp\n\
             Tip: convert first with `ogr2ogr -f GeoJSON out.geojson input.{other}`"
        ),
    };

    // Re-serialise the ingested layer as GeoJSON so bake_multi can consume it.
    let features: Vec<serde_json::Value> = layer
        .features
        .into_iter()
        .map(|(geom, props)| {
            let geom_json: geojson::Geometry = geojson::Geometry::try_from(&geom)
                .unwrap_or_else(|_| geojson::Geometry::new(geojson::Value::Point(vec![0.0, 0.0])));
            serde_json::json!({
                "type": "Feature",
                "geometry": geom_json,
                "properties": props,
            })
        })
        .collect();

    let fc = serde_json::json!({
        "type": "FeatureCollection",
        "features": features,
    });

    Ok(fc.to_string())
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn next_arg(args: &[String], i: usize, flag: &str) -> anyhow::Result<String> {
    args.get(i)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))
}

/// Resolve the property-field keep-set from the CLI flags, sampling the input as
/// needed. Returns `None` to mean "keep all fields".
fn resolve_keep_fields(
    input: &PathBuf,
    interactive: bool,
    include_fields: Option<Vec<String>>,
    exclude_fields: &[String],
) -> anyhow::Result<Option<HashSet<String>>> {
    // Explicit include list wins and needs no sampling.
    if let Some(list) = include_fields {
        return Ok(Some(list.into_iter().collect()));
    }

    if interactive {
        if !std::io::IsTerminal::is_terminal(&std::io::stderr()) {
            anyhow::bail!("--interactive requires a terminal (stderr is not a TTY)");
        }
        let est = estimate_input(input)
            .ok_or_else(|| anyhow::anyhow!("could not sample input for estimate"))?;
        match pick_fields(&est) {
            Choice::Proceed(keep) => return Ok(Some(keep)),
            Choice::Abort => {
                info!("bake cancelled");
                std::process::exit(0);
            }
        }
    }

    // Exclude list: sample to learn the full field set, then subtract.
    if !exclude_fields.is_empty() {
        let est = estimate_input(input)
            .ok_or_else(|| anyhow::anyhow!("could not sample input to apply --exclude-fields"))?;
        let drop: HashSet<&str> = exclude_fields.iter().map(String::as_str).collect();
        let keep: HashSet<String> = est
            .fields
            .iter()
            .map(|f| f.name.clone())
            .filter(|name| !drop.contains(name.as_str()))
            .collect();
        return Ok(Some(keep));
    }

    Ok(None)
}

fn print_usage() {
    eprintln!(
        r#"lm-bake — convert geodata to a PMTiles vector tile archive

USAGE
  lm-bake <input> [input2 ...] [OPTIONS]
  lm-bake inspect <archive.pmtiles>

INPUTS
  .geojson / .json     GeoJSON FeatureCollection
  .geojsonl / .ndjson  Newline-delimited GeoJSON
  .csv                 CSV with lat/lon columns
  .shp                 Shapefile (.dbf must be alongside)

OPTIONS
  --layer  <name>          Layer name (default: file stem). Comma-separate for multi-layer.
  --min-zoom <z>           Minimum zoom to generate (default: 0)
  --max-zoom <z>           Maximum zoom to generate (default: 14)
  --compression gzip|brotli  Tile compression (default: gzip)
  --attribution <text>     Attribution string stored in metadata
  --tolerance <factor>     Simplification multiplier (default: 1.0)
  -o / --output <path>     Output path (default: out.pmtiles)
  --interactive            Sample the input, show an estimate, and pick which
                           property fields to keep before baking (TTY only)
  --include-fields <list>  Keep only these comma-separated property fields
  --exclude-fields <list>  Keep all fields except these comma-separated ones

EXAMPLES
  lm-bake roads.geojson --layer roads --max-zoom 14 -o roads.pmtiles
  lm-bake points.csv --layer stops --max-zoom 12 -o stops.pmtiles
  lm-bake roads.geojson buildings.geojson --layers roads,buildings -o city.pmtiles
  lm-bake parcels.geojsonl --interactive -o parcels.pmtiles
  lm-bake parcels.geojsonl --exclude-fields DATA_LINK,SUB_ADDRESS -o parcels.pmtiles
  lm-bake inspect roads.pmtiles
"#
    );
}

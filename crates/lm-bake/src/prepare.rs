/// Input preparation: detect inputs that can't be baked memory-safely as-is and
/// normalize them to streamable, WGS84 GeoJSON-Seq via `ogr2ogr`.
///
/// A file is "stream-ready" when it is already line-delimited GeoJSON
/// (`.geojsonl` / `.ndjson` / `.geojsons`) AND in WGS84 (EPSG:4326). Anything
/// else that is large or in a non-WGS84 CRS is converted to a temporary
/// `.geojsonl` so the streaming ingest path can consume it one feature at a
/// time. If conversion is needed but `ogr2ogr` is absent, we abort with an
/// install hint rather than OOM trying to parse the whole file.
use std::path::{Path, PathBuf};
use std::process::Command;

use tracing::info;

/// Inputs at or above this size are routed through `ogr2ogr` even when the CRS
/// looks fine, because parsing them whole would exhaust memory. 1 GiB.
const LARGE_FILE_BYTES: u64 = 1024 * 1024 * 1024;

/// How many bytes of a (potentially huge) GeoJSON file to scan for a `crs`
/// member. The CRS, if present, sits in the top-level object before `features`.
const CRS_SCAN_BYTES: usize = 64 * 1024;

/// Result of preparing an input for baking.
pub struct Prepared {
    /// Path to feed to the ingest pipeline (original, or a converted temp file).
    pub path: PathBuf,
    /// Set when `path` is a temp file we created and the caller should delete
    /// after baking.
    pub temp: Option<TempFile>,
}

/// A temp file that deletes itself on drop.
pub struct TempFile(PathBuf);

impl TempFile {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Decide whether `input` needs conversion and, if so, run `ogr2ogr` to produce
/// a streamable WGS84 `.geojsonl`. Returns the path to actually bake.
///
/// `layer_name` is only used to name the temp file readably.
pub fn prepare_input(input: &Path, layer_name: &str) -> anyhow::Result<Prepared> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();

    let size = std::fs::metadata(input)
        .map(|m| m.len())
        .unwrap_or(0);

    let already_streamable = matches!(ext.as_str(), "geojsonl" | "ndjson" | "geojsons");
    let is_large = size >= LARGE_FILE_BYTES;
    let crs = detect_crs(input, &ext);
    let non_wgs84 = matches!(&crs, Some(code) if !is_wgs84(code));

    // Stream-ready and nothing forces a rewrite → bake the file directly.
    if already_streamable && !non_wgs84 {
        return Ok(Prepared { path: input.to_path_buf(), temp: None });
    }
    // Small WGS84 non-streamable file (e.g. a normal .geojson) → the existing
    // in-memory ingest handles it fine; don't pay for a conversion.
    if !is_large && !non_wgs84 {
        return Ok(Prepared { path: input.to_path_buf(), temp: None });
    }

    // From here, conversion is required.
    let reason = match (&crs, is_large) {
        (Some(code), true) => format!("{:.1} GB and CRS {code}", size as f64 / 1e9),
        (Some(code), false) => format!("CRS {code} (not WGS84)"),
        (None, true) => format!("{:.1} GB (too large to parse whole)", size as f64 / 1e9),
        (None, false) => "needs normalization".to_owned(),
    };
    info!("input needs conversion: {reason}");

    ensure_ogr2ogr()?;

    let out = temp_geojsonl_path(layer_name);
    info!(
        "converting → {} (WGS84 GeoJSON-Seq via ogr2ogr) …",
        out.display()
    );
    run_ogr2ogr(input, &out)?;

    Ok(Prepared {
        path: out.clone(),
        temp: Some(TempFile(out)),
    })
}

/// Read the leading bytes of the file and try to find an EPSG code in a `crs`
/// member. Returns e.g. `Some("EPSG:2927")`. `None` means "no CRS declared",
/// which per the GeoJSON spec means WGS84 — so we do *not* force conversion on
/// CRS grounds in that case.
///
/// For non-text container formats (shp, gdb, gpkg, …) we consult `ogrinfo`
/// instead, since the CRS isn't grep-able from the bytes.
fn detect_crs(input: &Path, ext: &str) -> Option<String> {
    match ext {
        "geojson" | "json" | "geojsonl" | "ndjson" | "geojsons" => detect_crs_text(input),
        // Container formats: ask ogrinfo (best-effort; absence is non-fatal here
        // because the size/format checks may already force conversion).
        _ => detect_crs_ogrinfo(input),
    }
}

fn detect_crs_text(input: &Path) -> Option<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(input).ok()?;
    let mut head = vec![0u8; CRS_SCAN_BYTES];
    let n = f.read(&mut head).ok()?;
    let text = String::from_utf8_lossy(&head[..n]);

    // Look for the RFC 7946-deprecated named CRS form GDAL still emits:
    //   "crs": { ... "name": "urn:ogc:def:crs:EPSG::2927" ... }
    // Grab the trailing integer after the last "EPSG:" (single or double colon).
    let idx = text.find("\"crs\"")?;
    let rest = &text[idx..];
    let epsg_pos = rest.find("EPSG:")?;
    let after = &rest[epsg_pos + "EPSG:".len()..];
    let digits: String = after
        .chars()
        .skip_while(|c| *c == ':')
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("EPSG:{digits}"))
    }
}

fn detect_crs_ogrinfo(input: &Path) -> Option<String> {
    let out = Command::new("ogrinfo")
        .arg("-so")
        .arg("-al")
        .arg(input)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // ogrinfo prints e.g.  `ID["EPSG",2927]`  in the WKT block.
    let pos = text.find("\"EPSG\",")?;
    let after = &text[pos + "\"EPSG\",".len()..];
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        Some(format!("EPSG:{digits}"))
    }
}

fn is_wgs84(code: &str) -> bool {
    // EPSG:4326 is WGS84 lon/lat; CRS84 is the same axis-flipped — both are fine
    // for our purposes since we read lon,lat order.
    let c = code.to_ascii_uppercase();
    c == "EPSG:4326" || c == "OGC:CRS84" || c == "CRS84"
}

/// Verify `ogr2ogr` is on PATH; if not, abort with a platform-specific install
/// hint. We don't try to bake the file anyway — that's what was OOMing.
fn ensure_ogr2ogr() -> anyhow::Result<()> {
    let ok = Command::new("ogr2ogr")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        return Ok(());
    }
    anyhow::bail!(
        "this input needs converting with `ogr2ogr` (from GDAL), which was not found on PATH.\n\
         Install it and re-run:\n  \
           Debian/Ubuntu : sudo apt-get install gdal-bin\n  \
           Fedora/RHEL   : sudo dnf install gdal\n  \
           Arch          : sudo pacman -S gdal\n  \
           macOS (brew)  : brew install gdal"
    );
}

fn run_ogr2ogr(input: &Path, output: &Path) -> anyhow::Result<()> {
    let status = Command::new("ogr2ogr")
        .arg("-t_srs")
        .arg("EPSG:4326")
        .arg("-f")
        .arg("GeoJSONSeq")
        .arg(output)
        .arg(input)
        .status()
        .map_err(|e| anyhow::anyhow!("failed to launch ogr2ogr: {e}"))?;

    if !status.success() {
        // Clean up a partial/empty output before erroring out.
        let _ = std::fs::remove_file(output);
        anyhow::bail!("ogr2ogr conversion failed (exit {:?})", status.code());
    }

    let produced = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    if produced == 0 {
        let _ = std::fs::remove_file(output);
        anyhow::bail!("ogr2ogr produced an empty file — is the input a recognised geodata format?");
    }
    Ok(())
}

fn temp_geojsonl_path(layer_name: &str) -> PathBuf {
    let safe: String = layer_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("lm-bake-{safe}-{pid}-{nanos}.geojsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wgs84_codes_recognised() {
        assert!(is_wgs84("EPSG:4326"));
        assert!(is_wgs84("OGC:CRS84"));
        assert!(!is_wgs84("EPSG:2927"));
        assert!(!is_wgs84("EPSG:3857"));
    }

    #[test]
    fn detects_named_crs_in_text() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("lm-prep-test-{}.geojson", std::process::id()));
        std::fs::write(
            &p,
            r#"{ "type":"FeatureCollection",
                "crs": { "type":"name", "properties": { "name":"urn:ogc:def:crs:EPSG::2927" } },
                "features": [] }"#,
        )
        .unwrap();
        assert_eq!(detect_crs_text(&p).as_deref(), Some("EPSG:2927"));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn no_crs_member_returns_none() {
        let dir = std::env::temp_dir();
        let p = dir.join(format!("lm-prep-test-nocrs-{}.geojson", std::process::id()));
        std::fs::write(&p, r#"{ "type":"FeatureCollection", "features": [] }"#).unwrap();
        assert_eq!(detect_crs_text(&p), None);
        let _ = std::fs::remove_file(&p);
    }
}
